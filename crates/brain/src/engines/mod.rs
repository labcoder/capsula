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

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use kumochi_protocol::message::{RunId, ThreadId};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::coordinator::Input;

/// Events a round queues ahead of its pump; the coordinator's own queue does the buffering.
const ROUND_QUEUE: usize = 1;

/// What an engine is asked to answer.
#[derive(Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The run the round belongs to; every event it produces is tagged with it.
    pub run: RunId,
    /// The thread the turn was typed in.
    pub thread: ThreadId,
    /// What the user typed.
    pub turn: String,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunRequest")
            .field("run", &self.run)
            .field("thread", &self.thread)
            .field("turn_bytes", &self.turn.len())
            .finish()
    }
}

/// One model round: it ends when the round does, and dropping it cancels the round.
pub type Round = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Something that answers turns. A direct-provider engine plays one round per start; the
/// coordinator starts the next round itself.
pub trait Engine: Send + Sync + fmt::Debug {
    /// Starts one model round for `request`, which sends its events to `events`.
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>) -> Round;
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
    /// A native output item, such as reasoning, kept in order for replay.
    OutputItem(Opaque),
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
            Self::OutputItem(item) => f.debug_tuple("OutputItem").field(item).finish(),
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
pub fn start(engine: &dyn Engine, request: RunRequest, inputs: mpsc::Sender<Input>) -> Cancel {
    let run = request.run;
    let (events_tx, events) = mpsc::channel(ROUND_QUEUE);
    let round = engine.start(request, events_tx);
    Cancel(tokio::spawn(pump(run, round, events, inputs)).abort_handle())
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
    use kumochi_protocol::message::{RunId, ThreadId};
    use tokio::sync::mpsc;

    use super::{
        CallId, Cancel, Engine, Finish, Opaque, Refused, Round, RunRequest, ToolName, TurnEvent,
        start,
    };
    use crate::coordinator::Input;

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
        fn start(&self, _request: RunRequest, events: mpsc::Sender<TurnEvent>) -> Round {
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
        RunRequest {
            run: RunId(7),
            thread: ThreadId(2),
            turn: "secret words".to_owned(),
        }
    }

    fn text(delta: &str) -> TurnEvent {
        TurnEvent::TextDelta(delta.to_owned())
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
        (start(&engine, request(), inputs), received, gone)
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
            TurnEvent::OutputItem(item),
        );
        assert_eq!(
            printed,
            "RunRequest { run: RunId(7), thread: ThreadId(2), turn_bytes: 12 } TextDelta(12) \
             ToolCallReady { call: CallId(\"call-1\"), args_bytes: 12 } OutputItem(Opaque(16))"
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
