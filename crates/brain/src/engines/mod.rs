//! The seam between the coordinator and the engines that answer turns (architecture v1 §5.3).
//!
//! An engine plays one model round per [`Engine::start`]: it sends [`TurnEvent`]s and ends the
//! round with [`TurnEvent::Finished`]. The coordinator, not the engine, decides whether a round
//! ends its run. Each start gets its own pump and channel: the pump forwards the round's events
//! to the coordinator tagged with the run, closes after the first `Finished`, and reports a round
//! that ends without one, even after text, as `Finished(Truncated)`. Dropping the round's
//! [`Cancel`] aborts the pump and the engine's future with it, even while the pump waits on a
//! full coordinator queue, so a cancelled round sends nothing more and never reads as truncated.

pub(crate) mod fixture;
pub(crate) mod responses;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kumochi_protocol::message::{RunId, ThreadId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::context::{ModelRequest, Origin};
use crate::coordinator::{Input, JobKey, LeaseId, SendKey};

/// Events a round queues ahead of its pump; the coordinator's own queue does the buffering.
const ROUND_QUEUE: usize = 1;

/// What an engine is asked to answer: a request the coordinator admitted, and nothing else.
#[derive(Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The admitted build, or the background job, whose send asks to go out under it.
    pub(crate) key: SendKey,
    /// The run the round belongs to; every event it produces is tagged with it. A job's is the
    /// run whose end started it.
    pub run: RunId,
    /// The thread the turn was typed in.
    pub thread: ThreadId,
    /// Where the run started, which decides what its tools may reach (PR 13b).
    pub(crate) origin: Origin,
    /// The built request: the only content a round may send.
    pub(crate) request: ModelRequest,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunRequest")
            .field("key", &self.key)
            .field("run", &self.run)
            .field("thread", &self.thread)
            .field("origin", &self.origin)
            .field("request", &self.request)
            .finish()
    }
}

#[cfg(test)]
impl RunRequest {
    /// A Kumo request with `text` as its instructions and nothing else.
    pub(crate) fn for_tests(run: RunId, thread: ThreadId, text: &str) -> Self {
        Self {
            key: SendKey::Build(crate::coordinator::BuildKey {
                run,
                round: 1,
                attempt: 1,
            }),
            run,
            thread,
            origin: Origin::Kumo,
            request: ModelRequest {
                choice: None,
                instructions: text.to_owned(),
                input: Vec::new(),
                tools: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
impl SendGate {
    /// A gate whose asks reach nobody, so every answer is `Rebuild`.
    pub(crate) fn closed(key: SendKey) -> Self {
        let (inputs, _) = mpsc::unbounded_channel();
        Self::new(key, Asks::default(), inputs)
    }
}

/// One model round: it ends when the round does, and dropping it cancels the round.
pub type Round = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Something that answers turns. A direct-provider engine plays one round per start; the
/// coordinator starts the next round itself.
pub trait Engine: Send + Sync + fmt::Debug {
    /// Starts one model round for `request`, which sends its events to `events`. Right before
    /// any of the body goes to the network, and before every retry, the round asks `gate`, and
    /// it sends only under a granted lease, which it holds until no more of the body can go.
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round;
}

/// Asks waiting for the coordinator's answer, by build or job.
pub(crate) type Asks = Arc<Mutex<Vec<(SendKey, oneshot::Sender<Answer>)>>>;

/// How a round asks to send its body (docs/development/engines.md, "The send lease").
#[derive(Debug)]
pub struct SendGate {
    key: SendKey,
    asks: Asks,
    inputs: mpsc::UnboundedSender<Input>,
}

impl SendGate {
    pub(crate) fn new(key: SendKey, asks: Asks, inputs: mpsc::UnboundedSender<Input>) -> Self {
        Self { key, asks, inputs }
    }

    /// Waits for leave to send. A brain that's gone, or a coordinator that never answers
    /// because the round was dropped, means `Rebuild`: nothing is sent.
    pub async fn ask(&self) -> Answer {
        let (answer, answered) = oneshot::channel();
        lock(&self.asks).push((self.key, answer));
        if self
            .inputs
            .send(Input::SendReady { key: self.key })
            .is_err()
        {
            return Answer::Rebuild;
        }
        answered.await.unwrap_or(Answer::Rebuild)
    }
}

/// The coordinator's answer to a round that's about to send.
#[derive(Debug)]
pub enum Answer {
    /// Send, and hold the lease until no more of the body can go out.
    Grant(SendLease),
    /// Drop the body unsent.
    Rebuild,
}

/// A granted send. Dropping it, wherever it is, reports `SendSettled` on the store-reply
/// channel, which the brain always drains, so a lost grant settles too.
#[derive(Debug)]
#[must_use = "dropping a SendLease settles its send"]
pub struct SendLease {
    lease: LeaseId,
    inputs: mpsc::UnboundedSender<Input>,
}

impl SendLease {
    pub(crate) fn new(lease: LeaseId, inputs: mpsc::UnboundedSender<Input>) -> Self {
        Self { lease, inputs }
    }
}

impl Drop for SendLease {
    fn drop(&mut self) {
        let settled = Input::SendSettled { lease: self.lease };
        if self.inputs.send(settled).is_err() {
            tracing::debug!("a send settled after the brain ended");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Answers the round waiting on `key`, if it still waits. A grant nobody takes is dropped
/// here, which settles it.
pub(crate) fn answer(asks: &Asks, key: SendKey, answer: Answer) {
    let waiting = {
        let mut asks = lock(asks);
        asks.iter()
            .position(|(asked, _)| *asked == key)
            .map(|at| asks.swap_remove(at).1)
    };
    if let Some(waiting) = waiting
        && waiting.send(answer).is_err()
    {
        tracing::debug!("a round stopped before its answer");
    }
}

/// What an engine produces during a round.
#[derive(Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// More of the visible answer.
    TextDelta(String),
    /// A tool call began; it always comes before the call's first argument delta.
    ToolCallStarted {
        /// The provider's id for the call.
        call: CallId,
        /// The tool it calls.
        name: ToolName,
    },
    /// More of a started call's arguments.
    ToolArgsDelta {
        /// The call.
        call: CallId,
        /// The next bytes of its arguments.
        delta: String,
    },
    /// A call's arguments are complete.
    ToolCallReady {
        /// The call.
        call: CallId,
        /// Its whole arguments.
        args: String,
    },
    /// A completed native output item, such as reasoning or a call, kept for replay.
    OutputItem {
        /// Its place in the response's output, which replay follows.
        index: u32,
        /// The provider's item as JSON: the same value it sent, though not always the same bytes.
        item: Opaque,
    },
    /// The round ended.
    Finished(Finish),
}

impl fmt::Debug for TurnEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta(text) => f.debug_tuple("TextDelta").field(&text.len()).finish(),
            Self::ToolCallStarted { call, name } => f
                .debug_struct("ToolCallStarted")
                .field("call", call)
                .field("name", name)
                .finish(),
            Self::ToolArgsDelta { call, delta } => f
                .debug_struct("ToolArgsDelta")
                .field("call", call)
                .field("delta_bytes", &delta.len())
                .finish(),
            Self::ToolCallReady { call, args } => f
                .debug_struct("ToolCallReady")
                .field("call", call)
                .field("args_bytes", &args.len())
                .finish(),
            Self::OutputItem { index, item } => f
                .debug_struct("OutputItem")
                .field("index", index)
                .field("item", item)
                .finish(),
            Self::Finished(finish) => f.debug_tuple("Finished").field(finish).finish(),
        }
    }
}

/// How a round ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// The provider finished the round.
    Completed,
    /// The round's stream ended without saying how it finished.
    Truncated,
    /// The round failed.
    Failed(EngineFailure),
    /// The engine stopped the round itself. Only the coordinator interrupts runs, so this fails
    /// an active one.
    Interrupted,
}

/// Why a round failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineFailure {
    /// The engine's provider reported the round failed.
    Provider,
    /// The Responses stream failed, was refused, or broke its rules.
    Stream(crate::connections::model::error::Failure),
    /// The account gave no bearer for the run's connection; nothing was sent.
    Access,
}

/// A provider's id for one tool call: ASCII and at most [`CallId::MAX`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallId(String);

impl CallId {
    /// The longest id taken; a longer one is refused, never truncated.
    pub const MAX: usize = 64;

    /// Takes a provider's call id.
    ///
    /// # Errors
    /// [`Refused::CallId`] when `id` is empty, longer than [`CallId::MAX`] or not ASCII.
    pub fn new(id: &str) -> Result<Self, Refused> {
        if id.is_empty() || id.len() > Self::MAX || !id.is_ascii() {
            return Err(Refused::CallId);
        }
        Ok(Self(id.to_owned()))
    }

    /// The id's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The draw tool's name on the wire.
pub(crate) const DRAW: &str = "draw";

/// The tools a call can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolName {
    /// Draws on the overlay.
    Draw,
    /// A name Kumochi doesn't offer.
    Unknown,
}

/// A native output item's bytes, never parsed or logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Opaque(Box<[u8]>);

impl Opaque {
    /// The largest item taken [guess].
    pub const MAX: usize = 256 * 1024;

    /// Takes an item's bytes.
    ///
    /// # Errors
    /// [`Refused::OutputItem`] when `bytes` is over [`Opaque::MAX`].
    pub fn new(bytes: Vec<u8>) -> Result<Self, Refused> {
        if bytes.len() > Self::MAX {
            return Err(Refused::OutputItem);
        }
        Ok(Self(bytes.into_boxed_slice()))
    }

    /// How many bytes it holds.
    pub fn size(&self) -> usize {
        self.0.len()
    }

    /// Its bytes, to send back as the same JSON value.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Opaque").field(&self.0.len()).finish()
    }
}

/// A value an engine produced is out of bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// A call id is empty, over 64 bytes or not ASCII.
    #[error("a call id must be 1 to 64 ASCII bytes")]
    CallId,
    /// An output item is over 256 KiB.
    #[error("an output item is over 256 KiB")]
    OutputItem,
}

/// A started round; dropping it aborts the round's pump and the engine's future with it.
#[derive(Debug)]
#[must_use = "dropping a Cancel cancels its round"]
pub struct Cancel(AbortHandle);

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts one round of `engine` for `request` behind its own pump, which forwards its events
/// to `inputs`.
pub fn start(
    engine: &dyn Engine,
    request: RunRequest,
    inputs: mpsc::Sender<Input>,
    gate: SendGate,
) -> Cancel {
    let run = request.run;
    let (events_tx, events) = mpsc::channel(ROUND_QUEUE);
    let round = engine.start(request, events_tx, gate);
    Cancel(tokio::spawn(pump(run, round, events, inputs)).abort_handle())
}

/// Starts a background job's one round behind its own task, which takes the round's text and
/// reports what it came to as `JobDone` on `inputs`. Dropping the `Cancel` drops the round.
pub(crate) fn start_job(
    engine: &dyn Engine,
    request: RunRequest,
    key: JobKey,
    inputs: mpsc::UnboundedSender<Input>,
    gate: SendGate,
) -> Cancel {
    let (events_tx, events) = mpsc::channel(ROUND_QUEUE);
    let round = engine.start(request, events_tx, gate);
    Cancel(tokio::spawn(collect(key, round, events, inputs)).abort_handle())
}

/// Takes a title job's text until its round completes. Anything but text and native items,
/// more than [`crate::jobs::MAX_OUTPUT`] bytes, or a round that ends any other way fails it.
/// It logs the outcome, never the text.
async fn collect(
    key: JobKey,
    mut round: Round,
    mut events: mpsc::Receiver<TurnEvent>,
    inputs: mpsc::UnboundedSender<Input>,
) {
    let mut text = String::new();
    let mut playing = true;
    let title = loop {
        let event = tokio::select! {
            biased;
            event = events.recv() => event.unwrap_or(TurnEvent::Finished(Finish::Truncated)),
            () = &mut round, if playing => {
                playing = false;
                events.close();
                continue;
            }
        };
        match event {
            TurnEvent::TextDelta(delta) if text.len() + delta.len() <= crate::jobs::MAX_OUTPUT => {
                text.push_str(&delta);
            }
            TurnEvent::OutputItem { .. } => {}
            TurnEvent::Finished(Finish::Completed) => break crate::jobs::clean_title(&text),
            _ => break None,
        }
    };
    tracing::info!(
        thread = key.thread.0,
        serial = key.serial,
        titled = title.is_some(),
        "a title job ended"
    );
    if inputs.send(Input::JobDone { key, title }).is_err() {
        tracing::debug!("a title job ended after the brain");
    }
}

/// Forwards one round's events, waiting while `inputs` is full, until the first `Finished`.
async fn pump(
    run: RunId,
    mut round: Round,
    mut events: mpsc::Receiver<TurnEvent>,
    inputs: mpsc::Sender<Input>,
) {
    let mut playing = true;
    loop {
        let event = tokio::select! {
            biased;
            event = events.recv() => event.unwrap_or(TurnEvent::Finished(Finish::Truncated)),
            () = &mut round, if playing => {
                playing = false;
                // What the round queued before it ended still goes; nothing after does.
                events.close();
                continue;
            }
        };
        let finished = matches!(event, TurnEvent::Finished(_));
        if inputs.send(Input::Engine { run, event }).await.is_err() || finished {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kumochi_protocol::message::{RunId, ThreadId};
    use tokio::sync::mpsc;

    use super::{
        Answer, Asks, CallId, Cancel, Engine, Finish, Opaque, Refused, Round, RunRequest, SendGate,
        SendLease, ToolName, TurnEvent, answer, start,
    };
    use crate::coordinator::{Input, JobKey, LeaseId};

    /// What a test round does once its events are sent.
    #[derive(Clone, Copy, Debug)]
    enum Then {
        End,
        Hang,
    }

    /// Sends its events, then ends the round or waits to be cancelled. `alive` lives as long as
    /// the round does.
    #[derive(Debug)]
    struct Sends {
        events: Vec<TurnEvent>,
        then: Then,
        alive: mpsc::Sender<()>,
    }

    impl Engine for Sends {
        fn start(
            &self,
            _request: RunRequest,
            events: mpsc::Sender<TurnEvent>,
            _gate: SendGate,
        ) -> Round {
            let (script, then, alive) = (self.events.clone(), self.then, self.alive.clone());
            Box::pin(async move {
                let _alive = alive;
                for event in script {
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                if let Then::Hang = then {
                    std::future::pending::<()>().await;
                }
            })
        }
    }

    fn request() -> RunRequest {
        RunRequest::for_tests(RunId(7), ThreadId(2), "secret words")
    }

    fn gated() -> (
        Asks,
        mpsc::UnboundedSender<Input>,
        mpsc::UnboundedReceiver<Input>,
    ) {
        let (inputs, received) = mpsc::unbounded_channel();
        (Asks::default(), inputs, received)
    }

    fn lease() -> LeaseId {
        LeaseId {
            key: request().key,
            send: 1,
        }
    }

    #[tokio::test]
    async fn stop_between_grant_record_and_receipt_settles() {
        let (asks, inputs, mut received) = gated();
        let gate = SendGate::new(request().key, Arc::clone(&asks), inputs.clone());
        let round = tokio::spawn(async move { gate.ask().await });
        assert_eq!(
            received.recv().await,
            Some(Input::SendReady { key: lease().key })
        );
        round.abort();
        assert!(round.await.expect_err("stopped").is_cancelled());
        answer(
            &asks,
            lease().key,
            Answer::Grant(SendLease::new(lease(), inputs)),
        );
        assert_eq!(
            received.recv().await,
            Some(Input::SendSettled { lease: lease() })
        );
    }

    #[tokio::test]
    async fn a_dropped_grant_receiver_settles() {
        let (asks, inputs, mut received) = gated();
        let gate = SendGate::new(request().key, Arc::clone(&asks), inputs.clone());
        let round = tokio::spawn(async move { gate.ask().await });
        assert_eq!(
            received.recv().await,
            Some(Input::SendReady { key: lease().key })
        );
        answer(
            &asks,
            lease().key,
            Answer::Grant(SendLease::new(lease(), inputs)),
        );
        round.abort();
        assert!(
            round
                .await
                .expect_err("stopped before it took the grant")
                .is_cancelled()
        );
        assert_eq!(
            received.recv().await,
            Some(Input::SendSettled { lease: lease() })
        );
    }

    #[tokio::test]
    async fn an_ask_the_brain_cannot_hear_is_answered_rebuild() {
        let gate = SendGate::closed(request().key);
        assert!(matches!(gate.ask().await, Answer::Rebuild));
    }

    fn text(delta: &str) -> TurnEvent {
        TurnEvent::TextDelta(delta.to_owned())
    }

    /// What a title job's round of `events` reports.
    async fn job_title(events: Vec<TurnEvent>, then: Then) -> Option<String> {
        let (alive, _gone) = mpsc::channel(1);
        let engine = Sends {
            events,
            then,
            alive,
        };
        let key = JobKey {
            thread: ThreadId(2),
            serial: 3,
        };
        let (inputs, mut received) = mpsc::unbounded_channel();
        let gate = SendGate::closed(request().key);
        let _cancel = super::start_job(&engine, request(), key, inputs, gate);
        let Some(Input::JobDone { key: done, title }) = received.recv().await else {
            panic!("a job reports JobDone");
        };
        assert_eq!(done, key, "it reports its own key");
        title.map(|title| title.as_str().to_owned())
    }

    #[tokio::test]
    async fn a_title_job_takes_its_rounds_text_only_when_the_round_completes() {
        let completed = TurnEvent::Finished(Finish::Completed);
        let item = TurnEvent::OutputItem {
            index: 0,
            item: Opaque::new(b"{}".to_vec()).expect("small"),
        };
        assert_eq!(
            job_title(
                vec![
                    item,
                    text("  Lisbon"),
                    text(" trip\nmore"),
                    completed.clone()
                ],
                Then::Hang
            )
            .await,
            Some("Lisbon trip".to_owned())
        );
        let over = "x".repeat(crate::jobs::MAX_OUTPUT + 1);
        let call = TurnEvent::ToolCallStarted {
            call: CallId::new("call-1").expect("ascii"),
            name: ToolName::Draw,
        };
        for events in [
            vec![text("Lisbon"), TurnEvent::Finished(Finish::Truncated)],
            vec![text("Lisbon")],
            vec![text(&over), completed.clone()],
            vec![call, completed.clone()],
            vec![text("\u{7}"), completed],
        ] {
            assert_eq!(job_title(events, Then::End).await, None);
        }
    }

    /// Plays `events` through a pump into a queue of `depth`; the last receiver closes once the
    /// round's future is gone.
    fn play(
        events: Vec<TurnEvent>,
        then: Then,
        depth: usize,
    ) -> (Cancel, mpsc::Receiver<Input>, mpsc::Receiver<()>) {
        let (alive, gone) = mpsc::channel(1);
        let engine = Sends {
            events,
            then,
            alive,
        };
        let (inputs, received) = mpsc::channel(depth);
        let gate = SendGate::closed(request().key);
        (start(&engine, request(), inputs, gate), received, gone)
    }

    async fn drain(received: &mut mpsc::Receiver<Input>) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        while let Some(input) = received.recv().await {
            let Input::Engine { run, event } = input else {
                panic!("only engine events");
            };
            assert_eq!(run, RunId(7));
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn a_round_that_ends_with_no_events_is_truncated() {
        let (_cancel, mut received, _gone) = play(Vec::new(), Then::End, 4);
        assert_eq!(
            drain(&mut received).await,
            [TurnEvent::Finished(Finish::Truncated)]
        );
    }

    #[tokio::test]
    async fn a_round_that_ends_after_text_is_truncated_not_done() {
        let (_cancel, mut received, _gone) = play(vec![text("half an")], Then::End, 4);
        assert_eq!(
            drain(&mut received).await,
            [text("half an"), TurnEvent::Finished(Finish::Truncated)]
        );
    }

    #[tokio::test]
    async fn nothing_follows_the_first_finished() {
        let late = vec![
            TurnEvent::Finished(Finish::Completed),
            text("late"),
            TurnEvent::Finished(Finish::Truncated),
        ];
        let (_cancel, mut received, mut gone) = play(late, Then::Hang, 4);
        assert_eq!(
            drain(&mut received).await,
            [TurnEvent::Finished(Finish::Completed)]
        );
        assert_eq!(gone.recv().await, None, "the round was dropped");
    }

    #[tokio::test]
    async fn cancelling_aborts_a_round_blocked_on_a_full_queue() {
        let flood = vec![text("1"), text("2"), text("3"), text("4")];
        let (cancel, mut received, mut gone) = play(flood, Then::Hang, 1);
        while received.is_empty() {
            tokio::task::yield_now().await;
        }
        // The queue holds one event, so the pump waits to forward the next.
        tokio::task::yield_now().await;
        drop(cancel);
        assert_eq!(gone.recv().await, None, "the round was dropped");
        assert_eq!(drain(&mut received).await, [text("1")]);
    }

    #[tokio::test]
    async fn a_cancelled_round_is_never_reported_as_truncated() {
        let (cancel, mut received, mut gone) = play(vec![text("so far")], Then::Hang, 4);
        assert_eq!(
            received.recv().await,
            Some(Input::Engine {
                run: RunId(7),
                event: text("so far")
            })
        );
        drop(cancel);
        assert_eq!(gone.recv().await, None, "the round was dropped");
        assert_eq!(drain(&mut received).await, []);
    }

    #[test]
    fn call_ids_and_items_are_bounded_and_never_cut() {
        let longest = "a".repeat(CallId::MAX);
        assert_eq!(
            CallId::new(&longest).map(|id| id.0),
            Ok(longest.clone()),
            "64 ASCII bytes fit"
        );
        for bad in [String::new(), format!("{longest}b"), "caf\u{e9}".to_owned()] {
            assert_eq!(CallId::new(&bad), Err(Refused::CallId), "{bad:?}");
        }
        let boundary: Vec<u8> = (0..Opaque::MAX)
            .map(|i| u8::try_from(i % 251).expect("below 251"))
            .collect();
        assert_eq!(
            Opaque::new(boundary.clone()).map(|item| item.0),
            Ok(boundary.into_boxed_slice()),
            "256 KiB fits whole"
        );
        assert_eq!(
            Opaque::new(vec![0; Opaque::MAX + 1]),
            Err(Refused::OutputItem)
        );
    }

    #[test]
    fn debug_shows_sizes_never_contents() {
        let item = Opaque::new(b"hidden reasoning".to_vec()).expect("small");
        let call = CallId::new("call-1").expect("ascii");
        let printed = format!(
            "{:?} {:?} {:?} {:?}",
            request(),
            TurnEvent::TextDelta("secret words".to_owned()),
            TurnEvent::ToolCallReady {
                call: call.clone(),
                args: "secret words".to_owned()
            },
            TurnEvent::OutputItem { index: 3, item },
        );
        assert_eq!(
            printed,
            "RunRequest { key: Build(BuildKey { run: RunId(7), round: 1, attempt: 1 }), \
             run: RunId(7), thread: ThreadId(2), origin: Kumo, request: ModelRequest { \
             choice: None, instructions_bytes: 12, input: [], tools: 0 } } TextDelta(12) \
             ToolCallReady { call: CallId(\"call-1\"), args_bytes: 12 } OutputItem { index: 3, item: Opaque(16) }"
        );
        assert_eq!(
            format!(
                "{:?}",
                TurnEvent::ToolCallStarted {
                    call,
                    name: ToolName::Draw
                }
            ),
            "ToolCallStarted { call: CallId(\"call-1\"), name: Draw }"
        );
    }
}
