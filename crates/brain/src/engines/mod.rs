//! The seam between the coordinator and the engines that answer turns (architecture v1 §5.3).
//!
//! An engine plays one model round per [`Engine::start`]: it sends [`TurnEvent`]s and ends the
//! round with [`TurnEvent::Finished`]. The coordinator, not the engine, decides whether a round
//! ends its run. Each start gets its own pump and channel: the pump forwards the round's events
//! to the coordinator tagged with the run, closes after the first `Finished`, and reports a round
//! that ends without one, even after text, as `Finished(Truncated)`. An unattempted pictured
//! send orders its terminal outcome behind the lease receipt instead. Dropping the round's
//! [`Cancel`] aborts the pump and the engine's future with it, even while the pump waits on a
//! full coordinator queue, so a cancelled round sends nothing more and never reads as truncated.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "A3b activates the Anthropic mapper")
)]
pub(crate) mod anthropic;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the claude engine (slice 8's C5b) drives it")
)]
pub(crate) mod claude_code;
#[cfg(any(test, debug_assertions))]
pub(crate) mod fixture;
pub(crate) mod responses;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kumochi_protocol::message::{RunId, ThreadId, UserError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::context::compact::clean_summary;
use crate::context::{ModelRequest, Origin};
use crate::coordinator::{
    BuildKey, Input, JobKey, JobKind, JobOutput, LeaseId, PictureGrant, PictureWrite, SendKey,
};

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

/// The engine for a run its connection can't answer: its round fails at once with what it
/// carries, so another engine never gets a request for a connection it doesn't serve.
#[derive(Debug)]
pub(crate) struct Unserved(pub(crate) EngineFailure);

impl Unserved {
    /// For a connection that has no engine yet: `ModelUnavailable`.
    pub(crate) fn model_unavailable() -> Self {
        Self(EngineFailure::Access(Some(
            crate::coordinator::model_unavailable(),
        )))
    }
}

impl Engine for Unserved {
    fn start(
        &self,
        _request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        let failure = self.0.clone();
        Box::pin(async move {
            // Nobody listens once the round is dropped, so a failed send has nothing left to tell.
            events
                .send(TurnEvent::Finished(Finish::Failed(failure)))
                .await
                .ok();
        })
    }
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
        self.ask_for(None).await
    }

    /// As [`ask`](Self::ask), for a send whose lease is held for up to `hold` from now: one that
    /// can go out again until a process tree is confirmed ended. The coordinator caps it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "C5b-1b's question round is the first caller")
    )]
    pub(crate) async fn ask_held(&self, hold: Duration) -> Answer {
        self.ask_for(Some(hold)).await
    }

    async fn ask_for(&self, hold: Option<Duration>) -> Answer {
        let (answer, answered) = oneshot::channel();
        lock(&self.asks).push((self.key, answer));
        if self
            .inputs
            .send(Input::SendReady {
                key: self.key,
                hold,
            })
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

/// A granted send. Dropping it, wherever it is, reports `SendUnsent` on the store-reply channel,
/// which the brain always drains, so a lost grant settles too; `SendSettled` instead once the
/// transport has said it attempted a write of its request.
// shortcut: Claude Code text-only sends do not mark writes; only its pictured seed marks them.
#[derive(Debug)]
#[must_use = "dropping a SendLease settles its send"]
pub struct SendLease {
    lease: LeaseId,
    /// `None` once abandoned.
    inputs: Option<mpsc::UnboundedSender<Input>>,
    unsent: bool,
    /// The coordinator's revocable grant for the picture the request carries.
    picture: Option<PictureGrant>,
    picture_receipt: Option<oneshot::Sender<PictureReceipt>>,
}

/// Whether terminal delivery can use the pump or must follow the unsent receipt.
#[derive(Debug)]
enum PictureReceipt {
    Attempted,
    Unsent(mpsc::UnboundedSender<Input>),
}

impl SendLease {
    pub(crate) fn new(lease: LeaseId, inputs: mpsc::UnboundedSender<Input>) -> Self {
        Self {
            lease,
            inputs: Some(inputs),
            unsent: true,
            picture: None,
            picture_receipt: None,
        }
    }

    /// Lets the send write only while `picture`, the grant for the picture it carries, is open.
    pub(crate) fn with_picture(mut self, picture: Option<PictureGrant>) -> Self {
        self.picture = picture;
        self
    }

    /// Whether this send carries a Claude route's picture grant: true only for a lease the
    /// coordinator granted with one, open or since closed.
    pub(crate) fn carries_picture(&self) -> bool {
        self.picture.is_some()
    }

    fn track_picture_receipt(&mut self) -> oneshot::Receiver<PictureReceipt> {
        let (settled, wait) = oneshot::channel();
        self.picture_receipt = Some(settled);
        wait
    }

    /// Gives the lease up unsettled: its send may still be going out, so its sources stay held
    /// until its deadline fails it closed.
    pub(crate) fn abandon(mut self) {
        self.inputs = None;
    }
}

/// One writer poll a [`SendLease`] admitted, holding its picture grant's reservation if any.
#[derive(Debug)]
pub struct Admitted(
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "held for its drop, which ends the reservation")
    )]
    Option<PictureWrite>,
);

impl crate::connections::model::Lease for SendLease {
    type Write = Admitted;

    /// Every poll of a send without a picture grant; with one, a poll the grant reserves.
    fn admit(&self) -> Option<Admitted> {
        self.picture.as_ref().map_or(Some(Admitted(None)), |grant| {
            grant.reserve_write().map(|write| Admitted(Some(write)))
        })
    }

    fn wrote(&mut self) {
        self.unsent = false;
    }

    fn carries_picture(&self) -> bool {
        SendLease::carries_picture(self)
    }
}

impl Drop for SendLease {
    fn drop(&mut self) {
        let Some(inputs) = self.inputs.take() else {
            return;
        };
        let settled = if self.unsent {
            Input::SendUnsent { lease: self.lease }
        } else {
            Input::SendSettled { lease: self.lease }
        };
        if inputs.send(settled).is_err() {
            tracing::debug!("a send settled after the brain ended");
        } else if let Some(receipt) = self.picture_receipt.take() {
            receipt.send(PictureReceipt::Unsent(inputs)).ok();
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
    /// Argument bytes emitted only by the fixed draw timing replay.
    #[cfg(all(windows, debug_assertions))]
    ReplayDrawDelta {
        /// The replay's call.
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
    /// A call of the Anthropic key's completed reply, as its rounds store it.
    KeyCall {
        /// Its place in the reply.
        index: u32,
        /// The call.
        call: anthropic::encode::ToolUse,
    },
    /// The question's tokens, sent once before an agent engine's `Finished`.
    Usage(Usage),
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
            #[cfg(all(windows, debug_assertions))]
            Self::ReplayDrawDelta { delta, .. } => f
                .debug_struct("ReplayDrawDelta")
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
            Self::KeyCall { index, call } => f
                .debug_struct("KeyCall")
                .field("index", index)
                .field("call", call)
                .finish(),
            Self::Usage(usage) => f.debug_tuple("Usage").field(usage).finish(),
            Self::Finished(finish) => f.debug_tuple("Finished").field(finish).finish(),
        }
    }
}

/// How a round ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finish {
    /// The provider finished the round.
    Completed,
    /// The round's stream ended without saying how it finished.
    Truncated,
    /// The round's outcome is ordered by its send receipt, not by the pump: a pictured launch's
    /// terminal, or a Responses or Messages send its lease refused, which the coordinator builds
    /// again.
    ReceiptOrdered,
    /// The round failed.
    Failed(EngineFailure),
    /// The engine stopped the round itself. Only the coordinator interrupts runs, so this fails
    /// an active one.
    Interrupted,
}

/// Why a round failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineFailure {
    /// The engine's provider reported the round failed.
    Provider,
    /// The Responses stream failed, was refused, or broke its rules.
    Stream(crate::connections::model::error::Failure),
    /// The Anthropic stream failed, was refused, or broke its rules.
    Anthropic(crate::connections::anthropic::error::Failure),
    /// The account gave no bearer for the run's connection; nothing was sent. It carries the
    /// account's own reason when it gave one.
    Access(Option<UserError>),
    /// The question hit the native turn cap.
    TurnCap,
    /// Claude Code's stdout broke its rules.
    ClaudeStream(ClaudeStreamFailure),
    /// Claude Code refused or failed the question for a reason the shell names.
    Claude(UserError),
}

/// How Claude Code's stdout broke its rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeStreamFailure {
    /// A line over the line bound.
    LineTooLong,
    /// More stdout than the question's budget.
    Budget,
    /// A line that isn't JSON or isn't UTF-8.
    NotJson,
    /// A known message with the wrong shape.
    Mistyped,
    /// The child echoed the picture-bearing seed.
    PictureEcho,
}

/// Token counts as the provider reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Uncached input.
    pub input: u32,
    /// Output.
    pub output: u32,
    /// Input read from the prompt cache.
    pub cache_read: u32,
    /// Input written to the prompt cache.
    pub cache_write: u32,
}

/// One question's tokens: the whole question, and its first native request apart, for the
/// prompt-cache decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Every native request together.
    pub whole: Tokens,
    /// The first native request alone, when the stream showed it.
    pub first_request: Option<Tokens>,
}

/// A provider's id for one tool call: ASCII and at most [`CallId::MAX`] bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct CallId(String);

/// The id is the provider's text, so only its length prints.
impl fmt::Debug for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CallId(len={})", self.0.len())
    }
}

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
    /// Saves a local issue draft for explicit user review.
    DraftIssue,
    /// Draws on the overlay.
    Draw,
    /// Moves the user's typed input to a task, or back to Kumo.
    Focus,
    /// Creates an empty task.
    CreateTask,
    /// Search live conversation events.
    SearchHistory,
    /// Read a live task's event range.
    ReadHistory,
    /// List live task metadata.
    ListTasks,
    /// A name Kumochi doesn't offer.
    Unknown,
    /// The dispatcher's test tool that writes.
    #[cfg(test)]
    Probe,
    /// The dispatcher's test tool that returns history references.
    #[cfg(test)]
    Recall,
}

impl ToolName {
    pub(crate) fn from_wire(name: &str) -> Self {
        match name {
            DRAW => Self::Draw,
            _ => crate::dispatch::named(name).unwrap_or(Self::Unknown),
        }
    }
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
    let SendKey::Build(key) = request.key else {
        unreachable!("a run engine answers a build, not a background job");
    };
    let (events_tx, events) = mpsc::channel(ROUND_QUEUE);
    let round = engine.start(request, events_tx, gate);
    Cancel(tokio::spawn(pump(key, round, events, inputs)).abort_handle())
}

/// Starts a background job's one round behind its own task, which takes the round's text and
/// reports what it came to as `JobDone` on `inputs`. Dropping the `Cancel` drops the round.
pub(crate) fn start_job(
    engine: &dyn Engine,
    request: RunRequest,
    (key, kind): (JobKey, JobKind),
    inputs: mpsc::UnboundedSender<Input>,
    gate: SendGate,
) -> Cancel {
    let (events_tx, events) = mpsc::channel(ROUND_QUEUE);
    let round = engine.start(request, events_tx, gate);
    Cancel(tokio::spawn(collect(key, kind, round, events, inputs)).abort_handle())
}

/// Takes a job's text until its round completes, and cleans it into a title or summary as its
/// key's kind says. Anything but text, native items and usage, more than the kind's limit, or a
/// round that ends any other way fails it. It logs the outcome, never the text.
async fn collect(
    key: JobKey,
    kind: JobKind,
    mut round: Round,
    mut events: mpsc::Receiver<TurnEvent>,
    inputs: mpsc::UnboundedSender<Input>,
) {
    let limit = match kind {
        JobKind::Title => crate::jobs::MAX_OUTPUT,
        JobKind::Compact => crate::jobs::MAX_SUMMARY_OUTPUT,
    };
    let mut text = String::new();
    let mut playing = true;
    let output = loop {
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
            TurnEvent::TextDelta(delta) if text.len() + delta.len() <= limit => {
                text.push_str(&delta);
            }
            TurnEvent::OutputItem { .. } | TurnEvent::KeyCall { .. } | TurnEvent::Usage(_) => {}
            TurnEvent::Finished(Finish::Completed) => {
                break match kind {
                    JobKind::Title => crate::jobs::clean_title(&text).map(JobOutput::Title),
                    JobKind::Compact => clean_summary(&text, crate::context::compact::MAX_SUMMARY)
                        .map(JobOutput::Summary),
                };
            }
            _ => break None,
        }
    };
    tracing::info!(
        thread = key.thread.0,
        serial = key.serial,
        ?kind,
        produced = output.is_some(),
        "a job ended"
    );
    if inputs.send(Input::JobDone { key, output }).is_err() {
        tracing::debug!("a job ended after the brain");
    }
}

/// Forwards one round's events, waiting while `inputs` is full, until the first `Finished`.
async fn pump(
    key: BuildKey,
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
        if matches!(event, TurnEvent::Finished(Finish::ReceiptOrdered)) {
            return;
        }
        let finished = matches!(event, TurnEvent::Finished(_));
        if inputs.send(Input::Engine { key, event }).await.is_err() || finished {
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
        SendLease, Tokens, ToolName, TurnEvent, Usage, answer, start,
    };
    use crate::coordinator::{Input, JobKey, JobKind, JobOutput, LeaseId, SendKey};

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

    #[test]
    fn a_send_without_a_picture_grant_admits_every_writer_poll() {
        use crate::connections::model::Lease;
        let (_, inputs, _received) = gated();
        let lease = SendLease::new(lease(), inputs);
        let held = lease.admit().expect("admitted");
        assert!(held.0.is_none(), "no reservation to hold");
        assert!(lease.admit().is_some_and(|held| held.0.is_none()));
    }

    #[test]
    fn a_send_with_a_picture_grant_admits_one_poll_at_a_time_until_closed() {
        use crate::connections::model::Lease;
        let (_, inputs, _received) = gated();
        let grant = crate::coordinator::PictureGrant::open_for_tests();
        let lease = SendLease::new(lease(), inputs).with_picture(Some(grant.clone()));
        let held = lease.admit().expect("admitted");
        assert!(held.0.is_some());
        assert!(lease.admit().is_none(), "one poll at a time");
        drop(held);
        let held = lease.admit().expect("admitted again");
        grant.close();
        drop(held);
        assert!(lease.admit().is_none());
    }

    #[tokio::test]
    async fn stop_between_grant_record_and_receipt_settles() {
        let (asks, inputs, mut received) = gated();
        let gate = SendGate::new(request().key, Arc::clone(&asks), inputs.clone());
        let round = tokio::spawn(async move { gate.ask().await });
        assert_eq!(
            received.recv().await,
            Some(Input::SendReady {
                key: lease().key,
                hold: None,
            })
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
            Some(Input::SendUnsent { lease: lease() })
        );
    }

    #[tokio::test]
    async fn a_dropped_grant_receiver_settles() {
        let (asks, inputs, mut received) = gated();
        let gate = SendGate::new(request().key, Arc::clone(&asks), inputs.clone());
        let round = tokio::spawn(async move { gate.ask().await });
        assert_eq!(
            received.recv().await,
            Some(Input::SendReady {
                key: lease().key,
                hold: None,
            })
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
            Some(Input::SendUnsent { lease: lease() })
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
        job_output(events, then, JobKind::Title)
            .await
            .map(|output| match output {
                JobOutput::Title(title) => title.as_str().to_owned(),
                JobOutput::Summary(_) => panic!("a title job reports a title"),
            })
    }

    /// What a job of `kind` reports for a round of `events`.
    async fn job_output(events: Vec<TurnEvent>, then: Then, kind: JobKind) -> Option<JobOutput> {
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
        let _cancel = super::start_job(&engine, request(), (key, kind), inputs, gate);
        let Some(Input::JobDone { key: done, output }) = received.recv().await else {
            panic!("a job reports JobDone");
        };
        assert_eq!(done, key, "it reports its own key");
        output
    }

    #[tokio::test]
    async fn a_compaction_job_takes_up_to_16_kib_and_cuts_it_to_the_summary_bound() {
        let completed = TurnEvent::Finished(Finish::Completed);
        let limit = crate::jobs::MAX_SUMMARY_OUTPUT;
        assert_eq!(limit, 16 * 1024);
        assert_eq!(
            job_output(
                vec![text("  Earlier: "), text("Lisbon\n"), completed.clone()],
                Then::End,
                JobKind::Compact
            )
            .await,
            Some(JobOutput::Summary("Earlier: Lisbon".to_owned()))
        );
        assert_eq!(
            job_output(
                vec![text(&"x".repeat(limit)), completed.clone()],
                Then::End,
                JobKind::Compact
            )
            .await,
            Some(JobOutput::Summary("x".repeat(8 * 1024))),
            "exactly 16 KiB is taken"
        );
        assert_eq!(
            job_output(
                vec![text(&"x".repeat(limit - 1)), text("yy"), completed],
                Then::End,
                JobKind::Compact
            )
            .await,
            None,
            "one byte over across deltas fails"
        );
    }

    #[tokio::test]
    async fn a_jobs_usage_is_ignored_and_only_a_completed_round_titles_it() {
        let usage = TurnEvent::Usage(Usage {
            whole: Tokens::default(),
            first_request: None,
        });
        assert_eq!(
            job_title(
                vec![
                    text("Lisbon trip"),
                    usage.clone(),
                    TurnEvent::Finished(Finish::Completed)
                ],
                Then::End
            )
            .await,
            Some("Lisbon trip".to_owned())
        );
        assert_eq!(
            job_title(vec![text("Lisbon trip"), usage], Then::End).await,
            None,
            "a round that ends without completing titles nothing"
        );
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
        assert_eq!(
            job_title(vec![text(&"x".repeat(4096)), completed.clone()], Then::End).await,
            Some("x".repeat(80)),
            "exactly 4 KiB is taken"
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
            vec![text(&"x".repeat(4095)), text("yy"), completed.clone()],
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
            let Input::Engine { key, event } = input else {
                panic!("only engine events");
            };
            assert_eq!(SendKey::Build(key), request().key);
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
                key: match request().key {
                    SendKey::Build(key) => key,
                    SendKey::Job(_) => panic!("the fixture is a run request"),
                },
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
             ToolCallReady { call: CallId(len=6), args_bytes: 12 } OutputItem { index: 3, item: Opaque(16) }"
        );
        assert_eq!(
            format!(
                "{:?}",
                TurnEvent::ToolCallStarted {
                    call,
                    name: ToolName::Draw
                }
            ),
            "ToolCallStarted { call: CallId(len=6), name: Draw }"
        );
    }
}
