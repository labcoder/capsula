//! The brain's systems around the coordinator: the store thread, the engine, timers,
//! and one shell connection's readers and writers.
//!
//! Each pipe has its own reader and writer, polled together in one task, so a reader never
//! waits behind a write (CAPS-8 finding 1). Esc reaches the coordinator even while the data
//! writer is blocked on a shell that stopped reading.

pub(crate) mod account;
pub(crate) mod claude;
mod connections;
mod controls;
#[cfg(any(test, all(debug_assertions, feature = "test-support")))]
pub(crate) mod credit_fixture;
mod data;
#[cfg(test)]
mod draw_stream_tests;
mod hold;
mod issues;
mod recovery;
mod screen_gate;
mod settings;
mod store_thread;
mod terminal;
#[cfg(test)]
mod terminal_tests;
pub(crate) use issues::IssueEpochs;
#[cfg(test)]
pub(crate) use store_thread::REFUSE_SPAWN;
#[cfg(test)]
mod tests;
mod voice;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::DataToBrain::{Hello, QueuedHello, TextMetrics};
use kumochi_protocol::message::{
    AudioSession, ConnectionKind, ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain,
    DataToShell, Deleted, MAX_PENDING_TURNS, MAX_VISIBLE_SCENES, RunId, SceneDescriptor, SceneId,
    SegmentStart, SpeechPcm, ThreadId, Transcript,
};
use kumochi_protocol::tap::{Direction, Tapped as _};
use kumochi_scene::display::TextMetricsV1;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::config::Config;
use crate::connections::account::Input as AccountInput;
use crate::context::{ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::{Availability, Ready};
use crate::coordinator::{
    BuildContext, BuildKey, Coordinator, Effect, Input, JobKey, JobKind, LeaseId, SendKey, StoreOp,
    Synthesized,
};
use crate::engines::responses::AccessAsk;
use crate::engines::{self, Answer, Asks, Cancel, Engine, SendGate, SendLease};
use crate::ipc;
use crate::store::{SceneState, Store};
use crate::tap::Tap;

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
pub(crate) mod audio_fixture;
use crate::voice::pump::Pump;
use crate::voice::tts::{TtsError, TtsEvent};
use account::Accounts;
use controls::{Registration, read_control};
use data::{DataQueue, Owner};
use recovery::Recovery;
#[cfg(test)]
pub(crate) use store_thread::StoreThread;
#[cfg(not(test))]
use store_thread::StoreThread;
use terminal::Fence;
pub(crate) use terminal::{Fatal, Terminal};

macro_rules! select_connection {
    ($watch_fatal:expr, [$($fatal_arm:tt)*], $($arms:tt)*) => {
        if $watch_fatal {
            tokio::select! { biased; $($fatal_arm)* $($arms)* }
        } else {
            tokio::select! { biased; $($arms)* }
        }
    };
}

/// Control frames queued for the shell (architecture v1 §3.2) [guess]. A full queue ends the
/// connection: a deliberate overload policy, not proof that the shell hung.
/// shortcut: no slots reserved for urgent frames and no coalescing of `RunState`, so a burst a
/// slow but healthy shell would survive can still end it; add both with the first urgent
/// brain-to-shell message.
const CONTROL_QUEUE: usize = 256;
/// The watchdog's ping interval while a run is active [guess].
const PING_EVERY: Duration = Duration::from_millis(500);
/// How long a ping may go unanswered [guess].
const PONG_WITHIN: Duration = Duration::from_secs(2);
/// Ended runs waiting on the store before new turns wait too [guess].
const MAX_ENDING: usize = 16;
/// Scenes with a clear or a kept selection owed before new turns wait too [guess]. A
/// watermark, not a cap: an admitted run's scene and a Clear of the visible set can still add
/// debt, so it stays below this plus `MAX_VISIBLE_SCENES` plus `MAX_ENDING + 1`.
const DEBT_WATERMARK: usize = MAX_VISIBLE_SCENES + 4;
/// Data queued for the shell before new turns and the engine wait, one credit window [guess].
const DATA_CAP: usize = 512 * 1024;
/// Engine events queued before the engine waits [guess].
const ENGINE_QUEUE: usize = 16;
/// How long closing waits for ended runs to reach the store, then for the store to close, and
/// how long a whole Quit may take from the moment it's accepted [guess].
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How long closing waits for a refresh already out, and its save, before the account's session
/// lock is released; past it the token stays marked as sent, so the next start never sends it
/// again [guess].
const REFRESH_WAIT: Duration = Duration::from_secs(5);
/// How long a new connection may wait for the visible set to settle before `Scenes` [guess].
const SCENES_WAIT: Duration = Duration::from_secs(2);
/// How long the control pipe is still read after another pipe ended, so a `Quitting` already
/// in it is accepted [guess].
const QUIT_GRACE: Duration = Duration::from_millis(250);

/// [`CLOSE_WAIT`]; tests choose their own, so a slow runner never races it.
#[cfg(not(test))]
fn close_wait() -> Duration {
    CLOSE_WAIT
}

/// Closes accounts no Brain took, with the waits `Brain::close` gives its own.
pub(crate) async fn close_unbuilt(mut accounts: Accounts) -> Stage {
    let deadline = Instant::now() + close_wait();
    accounts
        .close(deadline, Instant::now() + REFRESH_WAIT)
        .await
}

/// [`SCENES_WAIT`]; tests choose their own, so a slow runner never races it.
#[cfg(not(test))]
fn scenes_wait() -> Duration {
    SCENES_WAIT
}

#[cfg(test)]
use tests::{close_wait, scenes_wait};

/// How a shell connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The shell sent `Quitting`; shutting down must finish by `deadline`.
    Quit {
        /// [`CLOSE_WAIT`] after the Quit was accepted.
        deadline: Instant,
    },
    /// The shell went away or stopped answering.
    Lost,
    /// The connection cannot accept any more input.
    Fatal(Fatal),
    /// The shell broke the protocol after the handshake.
    Broken(&'static str),
    /// The visible set didn't settle in time, so no `Scenes` was sent; the shell reconnects.
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairStart {
    Ordinary,
    AudioRepair,
}

/// Whether a Quit's last clear committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quit {
    /// Every visible scene's clear is durable.
    Complete,
    /// The deadline passed or a write failed; the scenes come back at the next start.
    Incomplete,
}

/// What one shutdown stage was seen to do, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    /// An explicit success, observed before the stage's deadline.
    Confirmed,
    /// An explicit success, observed at or after it.
    Late,
    /// Ended with nothing to observe, as a raw store's drop: never Confirmed.
    Unconfirmed,
    /// An error, a refusal, a panic or a closed sender.
    Failed,
    /// The deadline passed and the stage wasn't seen to end.
    Outstanding,
}

impl Stage {
    /// A success observed at `at`: `timeout_at` returns a ready result even past its deadline,
    /// so only the clock says whether it was in time.
    fn seen(at: Instant, deadline: Instant) -> Self {
        if at < deadline {
            Self::Confirmed
        } else {
            Self::Late
        }
    }
}

/// What each stage of the Brain's shutdown was seen to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shutdown {
    /// Some from `quit` only.
    pub(crate) quit: Option<Quit>,
    pub(crate) store: Stage,
    /// Every settings write owed from the start of shutdown.
    pub(crate) settings: Stage,
    pub(crate) claude: Stage,
    pub(crate) account: Stage,
}

/// The engine runs use, picked once at start (docs/development/engines.md, "Selection").
#[derive(Debug)]
pub(crate) enum Selected {
    /// The dev fixture answers every run, with no account; a release build can't pick it.
    #[cfg(debug_assertions)]
    DevFixture(Box<dyn Engine>),
    #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
    Fixture(Box<dyn Engine>),
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    DrawReplay(Box<dyn Engine>),
    /// The account's engine answers, so runs need a usable account. Its rounds ask for the
    /// bearer on `asks`.
    Account {
        engine: Box<dyn Engine>,
        asks: mpsc::UnboundedReceiver<AccessAsk>,
    },
}

/// The coordinator and the systems that outlive one shell connection.
#[derive(Debug)]
pub(crate) struct Brain {
    pub(crate) crashes: crate::relaunch::Crashes,
    /// Boxed, so a future that owns the brain stays under clippy's `large_futures` limit.
    coordinator: Box<Coordinator>,
    store: Option<StoreThread>,
    engine: Box<dyn Engine>,
    /// The dev fixture was picked, so runs never wait on the account.
    #[cfg(any(test, debug_assertions, all(windows, feature = "gate-draw-replay")))]
    fixture: Option<Ready>,
    #[cfg(debug_assertions)]
    fixture_engine: bool,
    /// Store replies, never paused.
    inputs_tx: mpsc::UnboundedSender<Input>,
    inputs: mpsc::UnboundedReceiver<Input>,
    /// Engine events, taken only while the data queue has room.
    engine_tx: mpsc::Sender<Input>,
    engine_events: mpsc::Receiver<Input>,
    /// The current run's round, aborted when its `Cancel` drops.
    round: Option<(RunId, Cancel)>,
    key_round: Option<(BuildKey, Arc<engines::anthropic::live::Live>)>,
    /// The key attempts the newest run may still start, kept across its rebuilds.
    key_allowance: engines::anthropic::live::Allowance,
    /// The background job's request, aborted the same way.
    job: Option<(JobKey, Cancel)>,
    /// The uncovered backlog, in estimated tokens, at which a thread compacts after a run.
    pub(crate) compact_at: u64,
    /// Rounds waiting for leave to send.
    asks: Asks,
    /// Rounds asking for the bearer.
    access: mpsc::UnboundedReceiver<AccessAsk>,
    /// When each granted send's deadline passes, in no order.
    send_deadlines: Vec<(LeaseId, Instant)>,
    deadline: Option<(RunId, Instant)>,
    /// The one reader of TTS; none when TTS couldn't start.
    pump: Option<Pump>,
    /// The voice parts; none when there's no data folder for them.
    voice: Option<voice::Voice>,
    holds: hold::Holds,
    /// The current run's drain stall timer: its run, epoch and deadline.
    drain: Option<(RunId, u32, Instant)>,
    /// When each revoked run's wait for the shell to go quiet ends.
    quiet: Vec<(RunId, Instant)>,
    /// When owed writes are tried again.
    retry_at: Option<Instant>,
    /// When a draw call waiting for the shell is checked again.
    ready_at: Option<Instant>,
    /// Page completions belong only to the connection that requested them.
    epoch: u64,
    drawing_reaccount: Option<recovery::Reaccount>,
    /// What a recovery chunk packs up to; tests lower it to force several chunks.
    chunk_target: usize,
    /// The settings, owned here: they never touch runs or the store.
    config: Config,
    /// The profile's writer lock; each settings write keeps a clone until it stops.
    profile: Arc<crate::platform::SessionLock>,
    /// What the settings writes owed since shutdown began did; none before it.
    settings_owed: Option<settings::Owed>,
    /// Settings changes waiting for the one write in flight, answered in order.
    settings_ops: VecDeque<settings::Queued>,
    /// The settings write in flight, off the core loop: a durable replace can take any time.
    config_write: Option<settings::Write>,
    /// The account system; its credential work runs on its own thread.
    account: Accounts,
    issue_epochs: IssueEpochs,
    filing: issues::Filing,
    /// Claude Code's connection; its calls run on its own lane.
    claude: claude::Claude,
    /// Holds the next settings write until it sends or drops.
    #[cfg(test)]
    config_gate: Option<std::sync::mpsc::Receiver<settings::Gate>>,
    /// Hears what tests wait on instead of sleeping.
    #[cfg(test)]
    observer: mpsc::UnboundedSender<Seen>,
    #[cfg(test)]
    seen: Option<mpsc::UnboundedReceiver<Seen>>,
    #[cfg(test)]
    gates: std::sync::mpsc::Sender<store_thread::Gate>,
}

/// What a test can wait for instead of sleeping.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Seen {
    /// The core queued this op for the store thread.
    StoreOpQueued(crate::coordinator::StoreOp),
    /// The store thread, or the page reader, began this op; a write may then wait on a lock.
    StoreOpStarted(crate::coordinator::StoreOp),
    /// The page reader holds a read at its gate with a snapshot open.
    ReadHeld,
    /// A clean copy waits for a side connection in use before it's swapped in.
    SwapWaiting,
    /// The page reader waits for an op queued before its read.
    ReadWaiting,
    /// The store thread asked the checkpoint helper for a checkpoint after an op.
    CheckpointOwed,
    /// The brain handled a control input and applied its effects.
    ControlApplied,
    FirstPcmExpired,
    HoldPcmBuffered,
    /// A page reply landed while the data queue was full and waits for room.
    PageHeld,
    /// Another pipe ended first, and the control reader's grace began.
    Grace,
    /// A control the reader forwarded as its pipe ended is applied after the core has gone.
    ControlDrained,
    /// The store answered a delete, and the brain applied the answer.
    DeleteAnswered,
    /// A screen hold's words came in and wait for its picture.
    WordsWaiting,
    /// A screen hold's picture was sealed and taken into its pending part.
    PictureSealed,
    /// A quit ended the current run and now waits for the store.
    QuitDraining,
    /// A settings write is on disk and became the settings.
    SettingsCommitted,
    /// A served pair's link measures text with this profile.
    TextMetrics(Option<TextMetricsV1>),
    /// A hold control was handled, and this is what it left behind.
    HoldsLeft {
        pending: bool,
        debt: bool,
        capturing: bool,
    },
}

impl Brain {
    pub(crate) fn shell_tuning(&self) -> kumochi_protocol::message::ShellTuning {
        self.config.shell_tuning()
    }

    /// Starts the store thread, which keeps `profile` until it ends. The store must already be recovered, `main_thread` is its main
    /// thread, and `high_water` is its highest thread and run ids.
    ///
    /// # Errors
    /// Fails when the store's thread can't be started.
    #[expect(
        clippy::too_many_arguments,
        reason = "the recovered store, its writer lock and what the store's scan read"
    )]
    #[expect(clippy::too_many_lines, reason = "one field per part of the brain")]
    pub(crate) fn new(
        store: Store,
        profile: Arc<crate::platform::SessionLock>,
        main_thread: ThreadId,
        high_water: (ThreadId, RunId),
        scenes: Vec<SceneState>,
        engine: Selected,
        config: Config,
        mut account: Accounts,
        issue_epochs: IssueEpochs,
    ) -> Result<Self, (std::io::Error, Box<Accounts>)> {
        account.github.restore_default(config.github_default());
        account.github.tuning = config.github_tuning();
        let connections = &mut account.connections;
        connections.configure_transport(config.model_tuning());
        #[cfg(debug_assertions)]
        let mut fixture_engine = false;
        #[cfg(any(test, debug_assertions, all(windows, feature = "gate-draw-replay")))]
        let (engine, access, fixture) = match engine {
            #[cfg(debug_assertions)]
            Selected::DevFixture(engine) => {
                fixture_engine = true;
                (engine, mpsc::unbounded_channel().1, Some(Ready::fixture()))
            }
            #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
            Selected::Fixture(engine) => {
                (engine, mpsc::unbounded_channel().1, Some(Ready::fixture()))
            }
            #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
            Selected::DrawReplay(engine) => (
                engine,
                mpsc::unbounded_channel().1,
                Some(Ready::draw_replay()),
            ),
            Selected::Account { engine, asks } => (engine, asks, None),
        };
        #[cfg(not(any(test, debug_assertions, all(windows, feature = "gate-draw-replay"))))]
        let Selected::Account {
            engine,
            asks: access,
        } = engine;
        let (last_thread, last_run) = high_water;
        let (inputs_tx, inputs) = mpsc::unbounded_channel();
        #[cfg(test)]
        let (observer, seen) = mpsc::unbounded_channel();
        #[cfg(test)]
        let (gates, gates_heard) = std::sync::mpsc::channel();
        let schedule = store_thread::Schedule {
            context: config.context_tuning(),
            #[cfg(test)]
            observed: Some(observer.clone()),
            #[cfg(test)]
            gates: Some(gates_heard),
            ..store_thread::Schedule::default()
        };
        let drawing_scenes = store.drawing_scenes();
        let mut coordinator = Coordinator::new(main_thread, last_thread, last_run, scenes);
        coordinator.tuning = config.run_tuning();
        coordinator.configure_drawing(config.drawing_tuning());
        let recovery_clears = coordinator.restore_drawings(drawing_scenes);
        // The accounts go back with the error, so their owner can still close them.
        let store =
            match StoreThread::spawn(store, Arc::clone(&profile), inputs_tx.clone(), schedule) {
                Ok(store) => store,
                Err(error) => return Err((error, Box::new(account))),
            };
        account.bind_history(&store.history_link());
        let (engine_tx, engine_events) = mpsc::channel(ENGINE_QUEUE);
        let mut brain = Self {
            crashes: crate::relaunch::Crashes::default(),
            coordinator: Box::new(coordinator),
            store: Some(store),
            engine,
            #[cfg(any(test, debug_assertions, all(windows, feature = "gate-draw-replay")))]
            fixture,
            #[cfg(debug_assertions)]
            fixture_engine,
            inputs_tx,
            inputs,
            engine_tx,
            engine_events,
            round: None,
            key_round: None,
            key_allowance: engines::anthropic::live::Allowance::default(),
            job: None,
            compact_at: crate::jobs::compact_at(config.context_tuning().compact_at),
            asks: Asks::default(),
            access,
            send_deadlines: Vec::new(),
            deadline: None,
            pump: None,
            voice: None,
            holds: hold::Holds::default(),
            drain: None,
            quiet: Vec::new(),
            retry_at: None,
            ready_at: None,
            epoch: 0,
            drawing_reaccount: None,
            chunk_target: crate::drawing::CHUNK_TARGET,
            config,
            profile,
            settings_owed: None,
            settings_ops: VecDeque::new(),
            config_write: None,
            account,
            issue_epochs,
            filing: issues::Filing::new(),
            claude: claude::Claude::default(),
            #[cfg(test)]
            config_gate: None,
            #[cfg(test)]
            observer,
            #[cfg(test)]
            seen: Some(seen),
            #[cfg(test)]
            gates,
        };
        brain
            .apply(recovery_clears, None)
            .expect("offline effects cannot fill a shell queue");
        if let Some(generation) = brain.account.sendable() {
            brain.offline(Input::SignedIn { generation });
        }
        brain
            .availability_changed(None)
            .expect("offline effects cannot fill a shell queue");
        Ok(brain)
    }

    /// Gives Claude Code's connection its lane, or its off switch.
    /// Reported at once: lib.rs attaches before any shell connects, so no turn meets the
    /// coordinator's start placeholder (OSC-753).
    #[cfg(any(test, windows))]
    pub(crate) fn attach_claude(&mut self, mut claude: claude::Claude) {
        claude.launch_lifetime = self.config.run_tuning().claude_launch;
        self.claude = claude;
        let reported = self.claude_reported(None);
        debug_assert!(reported.is_ok(), "reporting without a shell can't fail");
    }

    /// Tells the coordinator whether runs can be answered now; the same answer again changes
    /// nothing. `ChatGPT` is ready only once its sign-in is proved: an ordinary one plainly, a
    /// recovered one with its cutoff. Until then a turn that arrives reads no history for it.
    fn availability_changed(&mut self, link: Option<&mut Link<'_, '_>>) -> Result<(), Ended> {
        #[cfg(any(test, debug_assertions, all(windows, feature = "gate-draw-replay")))]
        let fixture = self.fixture.clone();
        #[cfg(not(any(test, debug_assertions, all(windows, feature = "gate-draw-replay"))))]
        let fixture: Option<Ready> = None;
        let chatgpt = kumochi_protocol::message::ConnectionKind::ChatGpt;
        let input = if let Some(ready) = fixture {
            Input::Engines(chatgpt, Availability::Ready(ready))
        } else {
            match (self.account.availability(), self.account.recovered()) {
                (Availability::Ready(ready), Some(boundary)) => {
                    Input::RecoveredChatGpt { ready, boundary }
                }
                (now, _) => Input::Engines(chatgpt, now),
            }
        };
        self.hold_fence(&input);
        let effects = self
            .coordinator
            .handle(input)
            .expect("availability is never stale");
        self.fence_chatgpt_reads();
        self.apply(effects, link)
    }

    /// Tells the coordinator whether the account's sign-in keeps `ChatGPT`'s history from a turn
    /// that arrives now. The dev fixture answers without the account.
    fn fence_chatgpt_reads(&mut self) {
        #[cfg(any(test, debug_assertions, all(windows, feature = "gate-draw-replay")))]
        let fixture = self.fixture.is_some();
        #[cfg(not(any(test, debug_assertions, all(windows, feature = "gate-draw-replay"))))]
        let fixture = false;
        if !fixture && self.account.reads_fenced() {
            self.coordinator.fence_chatgpt_reads();
        } else {
            self.coordinator.lift_chatgpt_fence();
        }
    }

    /// Holds the next store op `matches` accepts before it runs, until the returned sender
    /// sends or drops.
    #[cfg(test)]
    pub(crate) fn hold_store(
        &self,
        matches: fn(&crate::coordinator::StoreOp) -> bool,
    ) -> std::sync::mpsc::Sender<()> {
        let (release, held) = std::sync::mpsc::channel();
        let gate = store_thread::Gate {
            matches,
            release: held,
        };
        self.gates.send(gate).expect("the store thread runs");
        release
    }

    /// What the brain reports to a test, from here on.
    #[cfg(test)]
    pub(crate) fn observe(&mut self) -> mpsc::UnboundedReceiver<Seen> {
        self.seen.take().expect("observed once")
    }

    /// Reports a control input, or a delete's answer, as it's taken. Tests run on one thread, so they hear it only
    /// once its effects are applied and the brain waits again.
    #[cfg(test)]
    fn control_taken(&self, input: &Input) {
        if matches!(
            input,
            Input::Stop { .. } | Input::Clear { .. } | Input::Delete { .. } | Input::ThreadOp(_)
        ) {
            // A test that stopped listening doesn't matter.
            self.observer.send(Seen::ControlApplied).ok();
        }
        if matches!(input, Input::Deleted { .. }) {
            self.observer.send(Seen::DeleteAnswered).ok();
        }
    }

    #[cfg(all(test, windows))]
    pub(crate) fn take_store_close_receipt(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        self.store
            .as_mut()
            .expect("store started")
            .take_close_receipt()
    }

    /// Shuts down after an accepted Quit, all by one absolute `deadline`: ends the current run
    /// as shell loss would, drains every accepted store write and delete, clears the visible set
    /// and what's still clearing in one write, waits for its commit, then closes the store. No
    /// stage starts a budget of its own. Only a committed clear with every delete done is a
    /// complete quit; otherwise the scenes, or a thread whose delete didn't commit, come back at
    /// the next start, as after a crash. Reports what each later stage was seen to do.
    pub(crate) async fn quit(mut self, deadline: Instant) -> Shutdown {
        self.settings_owed = Some(settings::Owed::default());
        self.claude.stop();
        self.offline(Input::ShellLost);
        self.voice_quit();
        self.pump_settings_offline();
        let filing_in_time = self.finish_filing(deadline).await;
        #[cfg(test)]
        self.observer.send(Seen::QuitDraining).ok();
        let mut undeleted = self.coordinator.refused_delete();
        let cleared = tokio::time::timeout_at(deadline, async {
            while self.coordinator.ending_runs() > 0
                || self.coordinator.clears_in_flight()
                || self.coordinator.deletes_pending()
                || self.coordinator.drawing_in_flight()
            {
                let Some(input) = self.inputs.recv().await else {
                    return false;
                };
                // Latched before the answer settles its delete, so an emptied list can't pass
                // for success.
                undeleted |= matches!(
                    input,
                    Input::Deleted {
                        outcome: Deleted::NotHidden,
                        ..
                    }
                );
                self.offline(input);
            }
            self.offline(Input::ClearVisible);
            while self.coordinator.clears_in_flight() || self.coordinator.drawing_in_flight() {
                let Some(input) = self.inputs.recv().await else {
                    return false;
                };
                let failed = matches!(input, Input::ScenesCleared { ok: false, .. });
                self.offline(input);
                if failed {
                    return false;
                }
            }
            self.coordinator.settled()
                && !self.coordinator.deletes_pending()
                && self.coordinator.drawing_available()
        })
        .await
        .unwrap_or(false);
        // The vault's link holds the store thread's queue open, so it goes first.
        self.account.history.release();
        let store = self.close_store(deadline).await;
        let settings = self.drain_settings_by(deadline).await;
        let claude = self.close_claude(deadline).await;
        let account = self
            .account
            .close(deadline, Instant::now() + REFRESH_WAIT)
            .await;
        let quit = if cleared && !undeleted && filing_in_time {
            Quit::Complete
        } else {
            tracing::warn!(
                undeleted,
                "the quit was incomplete; what it didn't clear or delete comes back at the next start"
            );
            Quit::Incomplete
        };
        Shutdown {
            quit: Some(quit),
            store,
            settings,
            claude,
            account,
        }
    }

    /// Lets runs that ended reach the store, then lets its queued writes finish. Each wait is
    /// bounded except for an unabortable issue phase, whose owner must settle before the writer
    /// closes. At the next start, runs still active in the store become `failed(crash)`;
    /// committed outcomes stay, and uncommitted work may be lost. Reports what each later stage
    /// was seen to do.
    pub(crate) async fn close(mut self) -> Shutdown {
        self.settings_owed = Some(settings::Owed::default());
        let deadline = Instant::now() + close_wait();
        self.voice_quit();
        self.pump_settings_offline();
        self.finish_filing(deadline).await;
        let settled = tokio::time::timeout(close_wait(), async {
            while self.coordinator.ending_runs() > 0 {
                let Some(input) = self.inputs.recv().await else {
                    break;
                };
                self.offline(input);
            }
        })
        .await;
        if settled.is_err() {
            let ending = self.coordinator.ending_runs();
            tracing::warn!(ending, "closing before the store confirmed every run");
        }
        // The vault's link holds the store thread's queue open, so it goes first.
        self.account.history.release();
        // Its own wait from here, unlike Quit's one deadline.
        let store = self.close_store(Instant::now() + close_wait()).await;
        let settings = self.drain_settings_by(deadline).await;
        let claude = self.close_claude(deadline).await;
        let account = self
            .account
            .close(deadline, Instant::now() + REFRESH_WAIT)
            .await;
        Shutdown {
            quit: None,
            store,
            settings,
            claude,
            account,
        }
    }

    /// Closes the store thread by `deadline`. Its receipt proves SQLite closed, not that its
    /// helpers joined or that it let the profile's lock go.
    async fn close_store(&mut self, deadline: Instant) -> Stage {
        // No store thread left means no receipt to confirm.
        let Some(store) = self.store.take() else {
            return Stage::Failed;
        };
        match tokio::time::timeout_at(deadline, store.close()).await {
            Ok(Ok(())) => Stage::seen(Instant::now(), deadline),
            Ok(Err(_)) => {
                tracing::error!("the store thread panicked");
                Stage::Failed
            }
            Err(_) => {
                tracing::warn!("the store thread didn't finish in time");
                Stage::Outstanding
            }
        }
    }

    /// Before the vault: no other brain may take Claude Code over while a call still runs.
    async fn close_claude(&mut self, deadline: Instant) -> Stage {
        if self.claude.close(deadline).await {
            Stage::seen(Instant::now(), deadline)
        } else {
            tracing::warn!("a claude call may still be running; Claude Code's lock is kept");
            Stage::Outstanding
        }
    }

    /// Keeps runs that ended moving through the store, and settings changes being written,
    /// while no shell is connected. Never returns; drop it when a shell connects.
    pub(crate) async fn idle(&mut self) {
        self.pump_settings_offline();
        loop {
            let send_due = self.next_send_deadline();
            let input = tokio::select! {
                event = self.filing.next() => {
                    let effects = self.issue_event(event);
                    self.apply(effects, None).expect("offline issue effects cannot fill a shell queue");
                    continue;
                }
                Some(input) = self.inputs.recv() => input,
                _ = recovery::reaccounted(&mut self.drawing_reaccount) => continue,
                // Engines run only for a current run, and nothing is current without a shell.
                Some(input) = self.engine_events.recv() => input,
                () = sleep_until(send_due) => self.send_deadline_passed(),
                done = settings::written(&mut self.config_write) => {
                    let settled = self.settings_done(done, None);
                    debug_assert!(settled.is_ok(), "settling without a shell can't fail");
                    continue;
                }
                input = self.account.next() => {
                    let applied = self.account_input(input, None);
                    debug_assert!(applied.is_ok(), "applying without a shell can't fail");
                    continue;
                }
                done = self.claude.next() => {
                    let applied = self.claude_done(done, None);
                    debug_assert!(applied.is_ok(), "applying without a shell can't fail");
                    continue;
                }
                Some(ask) = self.access.recv() => {
                    let answered = self.access_asked(ask, None);
                    debug_assert!(answered.is_ok(), "answering without a shell can't fail");
                    continue;
                }
                event = voice::next(&mut self.voice) => {
                    let applied = self.voice_event(event, None);
                    debug_assert!(applied.is_ok(), "applying without a shell can't fail");
                    self.pump_settings_offline();
                    continue;
                }
            };
            self.offline(input);
        }
    }

    pub(crate) async fn clear_before_relaunch(&mut self, scenes: &[SceneId]) -> bool {
        if self.store.is_none() || !self.coordinator.drawing_available() {
            return false;
        }
        let mut pending = scenes.to_vec();
        let effects = self.coordinator.failed_drawings(pending.clone());
        self.apply(effects, None)
            .expect("offline effects cannot fill a shell queue");
        let effects = self.coordinator.retry_clears();
        self.apply(effects, None)
            .expect("offline effects cannot fill a shell queue");
        tokio::time::timeout(scenes_wait(), async {
            while !pending.is_empty() || !self.coordinator.settled() {
                let Some(input) = self.inputs.recv().await else {
                    return false;
                };
                let mut failed = false;
                if let Input::ScenesCleared { scenes, ok } = &input {
                    failed = !ok;
                    if *ok {
                        pending.retain(|scene| !scenes.contains(scene));
                    }
                }
                self.offline(input);
                if failed {
                    return false;
                }
            }
            self.coordinator.drawing_available()
        })
        .await
        .unwrap_or(false)
    }

    fn handle(&mut self, input: Input) -> Option<Vec<Effect>> {
        self.hold_fence(&input);
        let input = match input {
            Input::IssueWritten { serial, reply } => {
                return Some(self.issue_written(serial, reply));
            }
            Input::Engine { key, event } => Input::Engine {
                key,
                event: self.claude.history_event(key, event),
            },
            Input::ScreenGrantStored { lease, result } => {
                let before_deadline = self
                    .send_deadlines
                    .iter()
                    .position(|(id, _)| *id == lease)
                    .is_some_and(|at| Instant::now() < self.send_deadlines.swap_remove(at).1);
                let stale_claude_launch = matches!(lease.key, SendKey::Build(key)
                    if self.coordinator.route_kind(key.run) == Some(ConnectionKind::ClaudeCode)
                        && !self.claude.picture_launch_current(lease.key));
                Input::ScreenGrantStored {
                    lease,
                    result: result.and_then(|()| {
                        use crate::coordinator::ScreenGrantRefusal;
                        if !before_deadline || !engines::waiting(&self.asks, lease.key) {
                            Err(ScreenGrantRefusal::Expired)
                        } else if stale_claude_launch {
                            Err(ScreenGrantRefusal::LaunchChanged)
                        } else {
                            Ok(())
                        }
                    }),
                }
            }
            other => other,
        };
        if matches!(input, Input::ShellLost) {
            self.filing.cancel();
        }
        if let Input::ScenesCleared { scenes, ok: true } = &input {
            self.crashes.cleared(scenes);
        }
        let effects = self.coordinator.handle(input).ok();
        self.settle_mcp();
        effects
    }

    /// Acts on one input with no shell to tell: only the store and the engine act.
    fn offline(&mut self, input: Input) {
        if let Some(effects) = self.handle(input) {
            let applied = self.apply(effects, None);
            debug_assert!(applied.is_ok(), "applying without a shell can't fail");
        }
    }

    /// Serves one authenticated shell until it quits, leaves or stops answering. Whatever run
    /// was active ends as `interrupted(shell_lost)`.
    ///
    /// A `Quitting` the control reader takes latches the quit, so whichever task ends first the
    /// connection ends as [`Ended::Quit`]. When another pipe ends first, the control reader's
    /// own read goes on for [`QUIT_GRACE`], so a `Quitting` already in the pipe, even one
    /// partly read, is still accepted; the controls it forwards meanwhile keep being applied, so
    /// its channel never fills. Controls forwarded before the Quit apply before `serve` returns.
    #[cfg(test)]
    pub(crate) async fn serve<C, D>(&mut self, control: C, data: D, tap: &mut Tap) -> Ended
    where
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
    {
        self.serve_with_audio(control, data, tap, AudioSession::Legacy)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn serve_with_audio<C, D>(
        &mut self,
        control: C,
        data: D,
        tap: &mut Tap,
        audio: AudioSession,
    ) -> Ended
    where
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
    {
        self.serve_with_profile(control, data, tap, audio, None, PairStart::Ordinary, None)
            .await
    }

    fn begin_pair(&mut self) {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("connection epochs never wrap");
        self.coordinator.begin_session();
        self.holds.opened();
        self.account.connections.stage_session(Some(self.epoch));
        self.account.github_session(Some(self.epoch));
        self.crashes.begin_session();
        #[cfg(debug_assertions)]
        self.filing.report_live_block();
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one connection carries two pipe legs, its profile and terminal source"
    )]
    pub(crate) async fn serve_with_profile<C, D>(
        &mut self,
        control: C,
        data: D,
        tap: &mut Tap,
        audio: AudioSession,
        text_metrics: Option<TextMetricsV1>,
        start: PairStart,
        terminal: Option<Terminal>,
    ) -> Ended
    where
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
    {
        self.serve_split(
            tokio::io::split(control),
            tokio::io::split(data),
            tap,
            audio,
            text_metrics,
            start,
            terminal,
        )
        .await
    }

    /// Serves one connection over legs already split into a reader and a whole-frame writer each,
    /// as a transport whose frames can't be resumed hands them over.
    #[expect(
        clippy::too_many_arguments,
        reason = "one connection carries two legs, its profile and terminal source"
    )]
    pub(crate) async fn serve_split<CR, CW, DR, DW>(
        &mut self,
        control: (CR, CW),
        data: (DR, DW),
        tap: &mut Tap,
        audio: AudioSession,
        text_metrics: Option<TextMetricsV1>,
        start: PairStart,
        terminal: Option<Terminal>,
    ) -> Ended
    where
        CR: AsyncRead + Unpin,
        CW: FrameWrite,
        DR: AsyncRead + Unpin,
        DW: FrameWrite,
    {
        if terminal.is_some() {
            self.serve_pair::<true, _, _, _, _>(
                control,
                data,
                tap,
                audio,
                text_metrics,
                start,
                terminal,
            )
            .await
        } else {
            self.serve_pair::<false, _, _, _, _>(
                control,
                data,
                tap,
                audio,
                text_metrics,
                start,
                None,
            )
            .await
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "keeps connection cleanup and disposition in order"
    )]
    #[expect(
        clippy::too_many_arguments,
        reason = "connection specialization preserves the profile and terminal source"
    )]
    async fn serve_pair<const WATCH_FATAL: bool, CR, CW, DR, DW>(
        &mut self,
        control: (CR, CW),
        data: (DR, DW),
        tap: &mut Tap,
        audio: AudioSession,
        text_metrics: Option<TextMetricsV1>,
        start: PairStart,
        terminal: Option<Terminal>,
    ) -> Ended
    where
        CR: AsyncRead + Unpin,
        CW: FrameWrite,
        DR: AsyncRead + Unpin,
        DW: FrameWrite,
    {
        let mut wake = pin!(if WATCH_FATAL {
            terminal.clone().map(Terminal::wake_on_fatal)
        } else {
            None
        });
        let fence = WATCH_FATAL.then(|| Fence::new(terminal));
        let fence = fence.as_ref();
        if WATCH_FATAL && let Some(cause) = fence.and_then(Fence::check) {
            return Ended::Fatal(cause);
        }
        self.begin_pair();
        let (mut control_in, mut control_out) = control;
        let (mut data_in, mut data_out) = data;
        let (controls_tx, mut controls) = mpsc::channel(64);
        let (turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
        let (pages_tx, mut pages) = mpsc::channel(kumochi_protocol::message::MAX_PENDING_PAGES);
        let (out_tx, mut out) = mpsc::channel::<Vec<u8>>(CONTROL_QUEUE);
        let queue = RefCell::new(DataQueue::new(window()));
        let ready = Notify::new();
        let room = Notify::new();
        let quit = Cell::new(None);
        let pcm = self.holds.ingress();
        let registration =
            (audio == AudioSession::QueuedRequired).then_some(Cell::new(Registration::Awaiting));
        let registration = registration.as_ref();

        let control_reader =
            read_control::<WATCH_FATAL>(&mut control_in, &controls_tx, &quit, fence);
        tokio::pin!(control_reader);
        // The writers outlive the select, unpolled, until the disposition is settled: dropping a
        // frame part way refuses a pair whose frames can't resume, turning ordinary loss fatal.
        let control_writer = write_control(&mut control_out, &mut out);
        tokio::pin!(control_writer);
        let data_writer = write_data(&mut data_out, &queue, &ready, &room);
        tokio::pin!(data_writer);
        let mut reading = true;
        let mut ended = {
            let data_reader = read_data::<WATCH_FATAL>(
                &mut data_in,
                &turns_tx,
                &pages_tx,
                &queue,
                &ready,
                self.epoch,
                Some(&pcm),
                registration,
                fence,
            );
            let mut link = Link::new(&out_tx, &queue, &ready, &mut *tap);
            link.registration = registration;
            link.text_metrics = text_metrics;
            #[cfg(test)]
            self.observer
                .send(Seen::TextMetrics(link.text_metrics))
                .ok();
            link.start = start;
            let core = self.core::<WATCH_FATAL>(
                &mut controls,
                &mut turns,
                &mut pages,
                &room,
                link,
                fence,
                wake.as_mut().as_pin_mut(),
            );
            tokio::select! {
                ended = &mut control_reader => {
                    reading = false;
                    ended
                }
                ended = data_reader => ended,
                ended = &mut control_writer => ended,
                ended = &mut data_writer => ended,
                ended = core => ended,
            }
        };
        if WATCH_FATAL && let Some(cause) = fence.and_then(Fence::check) {
            ended = Ended::Fatal(cause);
        }
        if let Some(reaccount) = &self.drawing_reaccount {
            reaccount.cancel();
        }
        let quitting = quit.get().is_some();
        if quitting {
            self.claude.stop();
        }
        self.holds.lost();
        self.filing.cancel();
        queue.borrow_mut().end_reads();
        if reading && !quitting && (!WATCH_FATAL || !matches!(ended, Ended::Fatal(_))) {
            #[cfg(test)]
            self.observer.send(Seen::Grace).ok();
            let grace = tokio::time::sleep(QUIT_GRACE);
            tokio::pin!(grace);
            loop {
                select_connection!(WATCH_FATAL, [
                    cause = poll_fn(|cx| fence.expect("active pair has a fence").poll_fatal(
                        wake.as_mut().as_pin_mut().expect("enabled fatal arm has a wake"),
                        cx,
                    )) => {
                        ended = Ended::Fatal(cause);
                        break;
                    }
                ],
                    Some((message, len)) = controls.recv() => {
                        self.control_after_loss(message, len, tap, audio);
                    }
                    _ = &mut control_reader => break,
                    () = &mut grace => break,
                );
            }
            if WATCH_FATAL && let Some(cause) = fence.and_then(Fence::check) {
                ended = Ended::Fatal(cause);
            }
        }
        if !quitting && quit.get().is_some() {
            self.claude.stop();
        }
        // The reader may have forwarded controls in the same poll that saw the pipe end.
        if !WATCH_FATAL || !matches!(ended, Ended::Fatal(_)) {
            while let Ok((message, len)) = controls.try_recv() {
                if WATCH_FATAL && let Some(cause) = fence.and_then(Fence::check) {
                    ended = Ended::Fatal(cause);
                    break;
                }
                #[cfg(test)]
                self.observer.send(Seen::ControlDrained).ok();
                self.control_after_loss(message, len, tap, audio);
            }
        }
        let ended = match quit.get() {
            Some(deadline) => Ended::Quit { deadline },
            None => ended,
        };
        self.offline(Input::ShellLost);
        self.account.connections.stage_session(None);
        self.account.github_session(None);
        self.deadline = None;
        self.drain = None;
        self.quiet.clear();
        self.filing.session = None;
        ended
    }

    /// The core loop: frames from the shell, completions from the systems, the deadline and
    /// the watchdog, each turned into coordinator inputs and their effects applied. Nothing in
    /// it awaits a pipe, the store or an engine. Control and store replies are always taken.
    /// A page read from an older connection is dropped, and one that arrives while the data
    /// queue is full is held until it has room; any other input passes.
    fn admit_page(&mut self, mut input: Input, link: &mut Link<'_, '_>) -> Option<Input> {
        if let Input::CardsRead { cards, reply } = &mut input {
            if cards.key.epoch != self.epoch || !link.queue.borrow().accepts_cards(cards) {
                return None;
            }
            if let Ok(rows) = reply {
                rows.retain(crate::coordinator::CardRead::live);
            }
            if link.queue.borrow().bytes() >= DATA_CAP {
                link.held_pages.push_back(input);
                return None;
            }
        }
        if let Input::PageRead { key, .. } = &input {
            if key.epoch != self.epoch || !link.queue.borrow().accepts_page(key) {
                return None;
            }
            if link.queue.borrow().bytes() >= DATA_CAP {
                link.held_pages.push_back(input);
                #[cfg(test)]
                self.observer.send(Seen::PageHeld).ok();
                return None;
            }
        }
        Some(input)
    }

    /// New turns wait while too many runs are ending or the data queue is full, and engine
    /// events while the data queue is full, so memory stays bounded however slow the store or
    /// the shell is. A turn is the only way to add a run, and so a `RunEnd`, so the frames
    /// queued past the cap are bounded too. `room` wakes the loop when the queue drains.
    ///
    /// Each connection starts awaiting `Scenes`: Stop, Clear and Pong are served, but no turn is
    /// taken until the visible set has settled and `Scenes` has named it. If required state
    /// hasn't settled within [`SCENES_WAIT`], the connection ends as unavailable. Optional
    /// re-accounting uses the same deadline but doesn't hold up a settled scene past it.
    /// After `Scenes`, snapshots go out a chunk at a time, ahead of engine events.
    #[expect(clippy::too_many_lines, reason = "one arm per source the loop serves")]
    #[expect(
        clippy::too_many_arguments,
        reason = "one connection owns the ingress and terminal state"
    )]
    async fn core<const WATCH_FATAL: bool>(
        &mut self,
        controls: &mut mpsc::Receiver<(ControlToBrain, usize)>,
        turns: &mut mpsc::Receiver<Turn>,
        pages: &mut mpsc::Receiver<Input>,
        room: &Notify,
        mut link: Link<'_, '_>,
        fence: Option<&Fence>,
        mut wake: Option<Pin<&mut impl Future<Output = ()>>>,
    ) -> Ended {
        let mut ticks = tokio::time::interval(PING_EVERY);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ping_sent: Option<Instant> = None;
        let mut barrier = recovery::Barrier {
            deadline: Instant::now() + scenes_wait(),
            settled: false,
        };
        if tokio::time::timeout_at(barrier.deadline, self.settle_deferred_talk_back())
            .await
            .is_err()
        {
            return Ended::Unavailable;
        }
        if let Err(ended) = self.open(&mut link) {
            return ended;
        }
        loop {
            if !turns.is_empty() {
                self.holds
                    .invalidate(kumochi_protocol::message::HoldStatus::Cancelled);
            }
            if self.hold_voice_changed(&mut link).is_err() {
                return Ended::Lost;
            }
            if let Err(ended) = self.advance_scene_barrier(&mut link, &mut barrier) {
                return ended;
            }
            let described = link.recovery.is_some();
            let running = self.coordinator.current_run();
            let deadline = self.deadline;
            let first_pcm = self.pump.as_ref().and_then(Pump::first_pcm_deadline);
            let first_pcm_due = first_pcm.is_some_and(|due| due.at <= Instant::now());
            let send_due = self.next_send_deadline();
            let take_engine = link.queue.borrow().bytes() < DATA_CAP;
            let take_turns = self.admits_turn(&link);
            let feed = take_engine && link.recovery.as_ref().is_some_and(Recovery::pending);
            let input = select_connection!(WATCH_FATAL, [
                cause = poll_fn(|cx| fence.expect("active pair has a fence").poll_fatal(
                    wake.as_mut().map(Pin::as_mut).expect("enabled fatal arm has a wake"),
                    cx,
                )) => return Ended::Fatal(cause),
            ],
                () = tokio::time::sleep_until(barrier.deadline), if !described => continue,
                Some((message, len)) = controls.recv() => {
                    match self.control_frame(message, len, &mut link, &mut ping_sent) {
                        Ok(Some(input)) => input,
                        Ok(None) => continue,
                        Err(ended) => return ended,
                    }
                }
                () = std::future::ready(()), if take_engine && !first_pcm_due && !link.held_pages.is_empty() => {
                    link.held_pages.pop_front().expect("a held page is ready")
                }
                () = std::future::ready(()), if feed && !first_pcm_due => {
                    if self.queue_recovery(&mut link).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                written = settings::written(&mut self.config_write) => {
                    if self.settings_done(written, Some(&mut link)).is_err()
                        || self.screen_holds_changed(&mut link).is_err()
                    {
                        return Ended::Lost;
                    }
                    continue;
                }
                input = self.account.next() => {
                    if self.account_input(input, Some(&mut link)).is_err()
                        || self.screen_holds_changed(&mut link).is_err()
                    {
                        return Ended::Lost;
                    }
                    continue;
                }
                event = self.filing.next() => {
                    let effects = self.issue_event(event);
                    if let Err(ended) = self.apply(effects, Some(&mut link)) {
                        return ended;
                    }
                    continue;
                }
                done = self.claude.next() => {
                    if self.claude_done(done, Some(&mut link)).is_err()
                        || self.screen_holds_changed(&mut link).is_err()
                    {
                        return Ended::Lost;
                    }
                    continue;
                }
                Some(ask) = self.access.recv() => {
                    if self.access_asked(ask, Some(&mut link)).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                Some(input) = self.inputs.recv() => input,
                receipt = recovery::reaccounted(&mut self.drawing_reaccount) => {
                    if !described && let Some(receipt) = receipt {
                        self.receive_reaccount(barrier.deadline, receipt);
                    }
                    continue;
                }
                Some(input) = pages.recv(), if !first_pcm_due => input,
                Some(input) = turns.recv(), if take_turns => {
                    if self.hold_end(kumochi_protocol::message::HoldStatus::Cancelled, &mut link).is_err() {
                        return Ended::Lost;
                    }
                    input
                },
                event = hold::next(&mut self.holds) => {
                    if !turns.is_empty() && self.hold_end(kumochi_protocol::message::HoldStatus::Cancelled, &mut link).is_err() {
                        return Ended::Lost;
                    }
                    if self.hold_event(event, &mut link).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                () = sleep_until(first_pcm.map(|due| due.at)) => {
                    match self.pump.as_mut().and_then(|pump| pump.expire_first_pcm(first_pcm?)) {
                        Some((run, segment)) => {
                            #[cfg(test)]
                            self.observer.send(Seen::FirstPcmExpired).ok();
                            Input::Tts { run, segment, made: Synthesized::Failed { run: true } }
                        }
                        None => continue,
                    }
                }
                Some(input) = self.engine_events.recv(), if take_engine => input,
                () = room.notified(), if !take_engine => continue,
                () = sleep_until(self.retry_at) => {
                    self.retry_at = None;
                    Input::Retry
                }
                () = sleep_until(self.ready_at) => {
                    self.ready_at = None;
                    Input::ReadyWait
                }
                () = sleep_until(send_due) => self.send_deadline_passed(),
                (run, event) = next_tts(&mut self.pump), if take_engine => tts_input(run, event),
                event = voice::next(&mut self.voice) => {
                    if self.voice_event(event, Some(&mut link)).is_err()
                        || self.pump_settings(Some(&mut link)).is_err()
                    {
                        return Ended::Lost;
                    }
                    continue;
                }
                () = sleep_until(self.drain.map(|(_, _, at)| at)) => {
                    let (run, epoch, _) = self.drain.take().expect("the drain timer fired");
                    Input::DrainTimeout { run, epoch }
                }
                () = sleep_until(self.quiet.iter().map(|(_, at)| *at).min()) => {
                    let first = self.quiet.iter().enumerate().min_by_key(|(_, (_, at))| *at);
                    let at = first.map(|(at, _)| at).expect("a quiet timer fired");
                    Input::QuietTimeout { run: self.quiet.swap_remove(at).0 }
                }
                () = sleep_until(deadline.map(|(_, at)| at)) => {
                    self.deadline = None;
                    match deadline {
                        Some((run, _)) => Input::Deadline { run },
                        None => continue,
                    }
                }
                // The tick also runs, with no watchdog, while an unanswered stop receipt waits.
                _ = ticks.tick(), if running.is_some() || self.owes_stop() => {
                    if self.retry_stop(&mut link).is_err() {
                        return Ended::Lost;
                    }
                    if running.is_none() {
                        continue;
                    }
                    match watchdog(&mut ping_sent, running, &mut link) {
                        Ok(Some(input)) => input,
                        Ok(None) => continue,
                        Err(ended) => return ended,
                    }
                }
            );
            if let Input::HoldPrepared { key, result } = input {
                if self.hold_prepared(key, result, &mut link).is_err() {
                    return Ended::Lost;
                }
                continue;
            }
            let Some(input) = self.admit_page(input, &mut link) else {
                continue;
            };
            #[cfg(test)]
            self.control_taken(&input);
            let Some(effects) = self.handle(input) else {
                continue;
            };
            if self.coordinator.current_run().is_none() {
                ping_sent = None;
                self.deadline = None;
            }
            if let Err(ended) = self.apply(effects, Some(&mut link)) {
                tracing::warn!(?ended, "ending the connection");
                return ended;
            }
        }
    }

    /// A connection's first frames: the settings and the account, then anything owed from while
    /// no shell was connected.
    fn open(&mut self, link: &mut Link<'_, '_>) -> Result<(), Ended> {
        if self
            .voice
            .as_mut()
            .is_some_and(|voice| std::mem::take(&mut voice.deferred_talk_back))
        {
            self.voice_policy();
        }
        link.queued_talk_back = link.registration.map(|_| self.config.view().talk_back);
        self.settings_now(link).map_err(|Full| Ended::Lost)?;
        self.pump_settings(Some(link)).map_err(|Full| Ended::Lost)?;
        let connected = match link.start {
            PairStart::Ordinary => AccountInput::Connected.into(),
            PairStart::AudioRepair => account::Event::AudioRepaired,
        };
        self.account_input(connected, Some(link))
            .map_err(|Full| Ended::Lost)?;
        for frame in self.account.github_opened() {
            link.control(&frame).map_err(|Full| Ended::Lost)?;
        }
        let connected = self.account.connections.connected();
        link.control(&self.composed(&connected))
            .map_err(|Full| Ended::Lost)?;
        self.voice_opened(link).map_err(|Full| Ended::Lost)?;
        self.screen_holds_changed(link)
            .map_err(|Full| Ended::Lost)?;
        #[cfg(debug_assertions)]
        if self.fixture_engine {
            link.control(&ControlToShell::FixtureEngineActive)
                .map_err(|Full| Ended::Lost)?;
        }
        if let Some(models) = self.account.connections.models_unsent() {
            link.data(&models).map_err(|Full| Ended::Lost)?;
        }
        self.claude.connected();
        self.claude_models(link).map_err(|Full| Ended::Lost)?;
        // A clear whose write failed while no shell was connected is retried now at the latest.
        self.retry_at = None;
        let retried = self.coordinator.retry_clears();
        self.apply(retried, Some(link)).map_err(|_| Ended::Lost)
    }

    /// New turns wait while too many runs are ending or too many writes are owed.
    fn room_for_turns(&self) -> bool {
        self.coordinator.ending_runs() < MAX_ENDING && self.coordinator.debt() < DEBT_WATERMARK
    }

    fn admits_turn(&self, link: &Link<'_, '_>) -> bool {
        Registration::admits(link.registration)
            && link.recovery.is_some()
            && link.queue.borrow().bytes() < DATA_CAP
            && self.room_for_turns()
    }

    fn advance_scene_barrier(
        &mut self,
        link: &mut Link<'_, '_>,
        barrier: &mut recovery::Barrier,
    ) -> Result<(), Ended> {
        if link.recovery.is_some() {
            return Ok(());
        }
        let settled = self.coordinator.settled();
        if Instant::now() >= barrier.deadline {
            if !barrier.settled || !settled {
                tracing::warn!("the visible scenes didn't settle in time; ending the connection");
                return Err(Ended::Unavailable);
            }
            if let Some(replay) = &self.drawing_reaccount {
                replay.cancel();
            }
            self.coordinator.use_uncounted_measure(link.text_metrics);
        } else {
            barrier.settled = settled;
            if !settled || self.drawing_reaccount.is_some() {
                return Ok(());
            }
            if let Some(job) = self
                .coordinator
                .reaccount_drawings(self.epoch, link.text_metrics)
            {
                self.drawing_reaccount = Some(recovery::Reaccount::start(job));
                return Ok(());
            }
        }
        link.recovery = self.describe(link).map_err(|Full| Ended::Lost)?;
        Ok(())
    }

    fn receive_reaccount(&mut self, deadline: Instant, receipt: recovery::ReaccountReceipt) {
        if Instant::now() < deadline {
            self.coordinator
                .install_reaccounted(self.epoch, &receipt.source, receipt.result);
        }
    }

    /// Sends `Scenes` for the settled visible set, and the recovery that sends their snapshots.
    fn describe(&mut self, link: &mut Link<'_, '_>) -> Result<Option<Recovery>, Full> {
        let drawings = self.coordinator.drawing_snapshot().map_err(|_| Full)?;
        if !drawings.skipped.is_empty() {
            let effects = self.coordinator.failed_drawings(drawings.skipped);
            self.apply(effects, None)
                .expect("offline effects cannot fill a shell queue");
            return Ok(None);
        }
        if !drawings.scenes.is_empty() {
            let ids = drawings.scenes.iter().map(|scene| scene.scene).collect();
            link.control(&ControlToShell::ScenesV1 {
                scenes: drawings.scenes,
            })?;
            self.crashes.described(ids, std::time::Instant::now());
            self.coordinator.drawing_described();
            return Ok(Some(Recovery::drawings(drawings.chunks)));
        }
        let visible = self.coordinator.visible();
        let scenes = visible.iter().map(descriptor).collect();
        link.control(&ControlToShell::Scenes { scenes })?;
        let recovery = Recovery::new(visible, self.chunk_target);
        self.coordinator.drawing_described();
        Ok(Some(recovery))
    }

    fn queue_recovery(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        if let Some(chunk) = link.recovery.as_mut().and_then(Recovery::next_chunk) {
            link.data(&chunk)?;
        }
        if !link.recovery.as_ref().is_some_and(Recovery::pending) {
            self.crashes.queued(std::time::Instant::now());
        }
        Ok(())
    }

    /// When the soonest granted send's deadline passes.
    fn next_send_deadline(&self) -> Option<Instant> {
        self.send_deadlines.iter().map(|&(_, at)| at).min()
    }

    /// The soonest send deadline passed: it's reported once, and it's stale if its lease settled.
    fn send_deadline_passed(&mut self) -> Input {
        let at = (0..self.send_deadlines.len())
            .min_by_key(|&at| self.send_deadlines[at].1)
            .expect("a deadline passed");
        let (lease, _) = self.send_deadlines.swap_remove(at);
        Input::SendDeadline { lease }
    }

    /// A run's first build takes the names and default model as they are now, so a rename
    /// applies from the next run.
    fn seed(&self, build: &mut BuildContext) {
        let origin = build.origin;
        build.snapshot.get_or_insert_with(|| RunSnapshot {
            // The build reads the run's choice and sets the tag its kind arrived with.
            connection: ConnectionTag(0),
            ..self.snapshot(origin)
        });
    }

    /// Seeds a build, or a job's read with the names and default model now and the compaction
    /// threshold.
    fn seed_op(&self, op: &mut StoreOp) {
        match op {
            StoreOp::BuildContext(build) => self.seed(build),
            StoreOp::ReadJob(read) => {
                read.snapshot = Some(self.snapshot(read.origin));
                read.compact_at = self.compact_at;
            }
            _ => {}
        }
    }

    /// Starts `job`'s request on the engine, replacing any job still running.
    fn start_job(&mut self, job: JobKey, kind: JobKind, request: engines::RunRequest) {
        let gate = SendGate::new(request.key, Arc::clone(&self.asks), self.inputs_tx.clone());
        let owned = self.owned_engine(&request);
        let cancel = engines::start_job(
            owned.as_deref().unwrap_or(&*self.engine),
            request,
            (job, kind),
            self.inputs_tx.clone(),
            gate,
        );
        self.job = Some((job, cancel));
    }

    /// An engine of its own for `request`'s choice, for a run's round and a job alike: a Claude
    /// Code question, or [`Unserved`] with Claude Code's refusal or for a connection with no
    /// engine yet. `None` for `ChatGPT`'s choice and for none, which `self.engine` answers.
    ///
    /// [`Unserved`]: engines::Unserved
    fn owned_engine(&mut self, request: &engines::RunRequest) -> Option<Box<dyn Engine>> {
        match request
            .request
            .choice
            .as_ref()
            .map(|choice| choice.connection)
        {
            None | Some(ConnectionKind::ChatGpt) => None,
            Some(ConnectionKind::ClaudeCode) => Some(match self.claude.question(request) {
                Ok(question) => Box::new(question),
                Err(refused) => Box::new(engines::Unserved(self.claude.refused(refused))),
            }),
            Some(ConnectionKind::AnthropicKey) => Some(
                self.key_engine(request)
                    .unwrap_or_else(|| Box::new(engines::Unserved::model_unavailable())),
            ),
        }
    }

    /// What a run's first build, or a job's read, takes: the names and default model now.
    fn snapshot(&self, origin: Origin) -> RunSnapshot {
        let view = self.config.view();
        // An invalid default runs nothing: with no choice, a run's model check refuses it.
        let choice = match self.config.default_choice() {
            crate::store::choice::StoredChoice::Absent
            | crate::store::choice::StoredChoice::Invalid => None,
            crate::store::choice::StoredChoice::Valid {
                connection,
                model,
                effort,
            } => Some(kumochi_protocol::message::ModelChoice {
                connection,
                model,
                effort: effort.unwrap_or(kumochi_protocol::message::Effort::Low),
            }),
        };
        RunSnapshot {
            origin,
            assistant: view.assistant_name.clone(),
            user: view.user_name.clone(),
            choice,
            // shortcut: a job records no connection yet; 13b tags it with the one it used.
            connection: ConnectionTag(0),
        }
    }

    /// Applies effects in order. Frames go to `link`, or nowhere without a shell. Once the
    /// shell's control queue is full nothing more goes to the shell, but every store, engine and
    /// timer effect still applies, since the coordinator counts them as done; then it fails, as
    /// it does when the shell broke the protocol.
    #[expect(clippy::too_many_lines, reason = "one short arm per runtime effect")]
    fn apply(
        &mut self,
        effects: Vec<Effect>,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Ended> {
        let shell = link.is_some();
        let mut full = false;
        let mut broken = None;
        for effect in effects {
            match effect {
                Effect::DrawingReplyText { .. } => {
                    unreachable!("drawing reply text is released by the coordinator")
                }
                Effect::Store(mut op) => {
                    self.claude.deleting(&op);
                    self.seed_op(&mut op);
                    if let StoreOp::CommitCall(call) = &mut op
                        && let crate::dispatch::LocalWrite::DraftIssue { repo, account, .. } =
                            &mut call.write
                    {
                        match self
                            .account
                            .github
                            .draft_binding(repo.as_ref(), self.config.github_default())
                        {
                            Ok(binding) => (*repo, *account) = binding,
                            Err(error) => {
                                let sent = self
                                    .inputs_tx
                                    .send(Input::CallCommitted {
                                        key: call.key.clone(),
                                        outcome: crate::dispatch::Committed::Refused(error),
                                        then: call.then,
                                    })
                                    .is_ok();
                                debug_assert!(sent, "the brain holds its own receiver");
                                continue;
                            }
                        }
                    }
                    #[cfg(test)]
                    self.observer.send(Seen::StoreOpQueued(op.clone())).ok();
                    let failure = op.failed();
                    if !self.store.as_ref().is_some_and(|store| store.send(op)) {
                        tracing::error!("the store thread is gone");
                        let sent = failure.is_none_or(|input| self.inputs_tx.send(input).is_ok());
                        debug_assert!(sent, "the brain holds its own receiver");
                    }
                }
                Effect::Control(message) => {
                    if let Some(shell) = link.as_deref_mut()
                        && shell.control(&message).is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::HoldStored(key) => {
                    if key.epoch == self.epoch
                        && let Some(shell) = link.as_deref_mut()
                        && shell
                            .data(&DataToShell::HoldStored {
                                gesture: key.gesture,
                            })
                            .is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::EventsChanged { hold, thread } => {
                    if hold.epoch == self.epoch
                        && let Some(shell) = link.as_deref_mut()
                        && shell.data(&DataToShell::EventsChanged { thread }).is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::HoldScreen { hold, outcome } => {
                    if hold.epoch == self.epoch
                        && let Some(shell) = link.as_deref_mut()
                        && shell
                            .control(&ControlToShell::HoldScreen {
                                gesture: hold.gesture,
                                outcome,
                            })
                            .is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::Page { key, reply } => {
                    if let Some(shell) = link.as_deref_mut() {
                        let frame = frame::encode(&reply, Pipe::Data).map_err(|_| Full);
                        match frame {
                            Ok(frame) => {
                                shell.queue.borrow_mut().push_page(&key, frame);
                                shell.ready.notify_one();
                            }
                            Err(_) => full = true,
                        }
                    }
                }
                Effect::Cards { cards, reply } => {
                    if let Some(shell) = link.as_deref_mut() {
                        if !shell.queue.borrow_mut().push_cards(&cards, reply) {
                            full = true;
                        }
                        shell.ready.notify_one();
                    }
                }
                Effect::Data(message) => {
                    if link.as_ref().is_some_and(|shell| shell.recovery.is_none())
                        && matches!(
                            message,
                            DataToShell::SceneBegin { .. }
                                | DataToShell::SceneStepCommit { .. }
                                | DataToShell::SceneCutApplied { .. }
                                | DataToShell::SceneEnd { .. }
                        )
                    {
                        continue;
                    }
                    if let Some(shell) = link.as_deref_mut()
                        && shell.data(&message).is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::StartEngine { request } => {
                    let run = request.run;
                    let by = self
                        .deadline
                        .filter(|(owner, _)| *owner == run)
                        .map_or_else(Instant::now, |(_, at)| at);
                    let gate =
                        SendGate::new(request.key, Arc::clone(&self.asks), self.inputs_tx.clone())
                            .with_run_deadline(by);
                    let owned = self.owned_engine(&request);
                    let engine = owned.as_deref().unwrap_or(&*self.engine);
                    let cancel = engines::start(engine, request, self.engine_tx.clone(), gate);
                    self.round = Some((run, cancel));
                }
                Effect::StartJob { job, kind, request } => self.start_job(job, kind, request),
                Effect::CancelJob(job) => drop(self.job.take_if(|(held, _)| *held == job)),
                Effect::Grant(lease) => {
                    let picture = self.coordinator.picture_grant(lease);
                    if let Some(grant) = &picture {
                        let kind = match lease.key {
                            SendKey::Build(build) => self.coordinator.route_kind(build.run),
                            SendKey::Job(_) => None,
                        };
                        self.claude.picture_granted(lease.key, kind, grant.clone());
                    }
                    let granted =
                        SendLease::new(lease, self.inputs_tx.clone()).with_picture(picture);
                    engines::answer(&self.asks, lease.key, Answer::Grant(granted));
                }
                Effect::Rebuild(key) => {
                    if let SendKey::Build(build) = key
                        && self
                            .key_round
                            .as_ref()
                            .is_some_and(|(owner, _)| *owner == build)
                    {
                        self.retire_key_run(build.run);
                    }
                    engines::answer(&self.asks, key, Answer::Rebuild);
                }
                Effect::ArmSendDeadline { lease, after } => {
                    self.send_deadlines.push((lease, Instant::now() + after));
                }
                Effect::ArmSendDeadlineAt { lease, at } => self.send_deadlines.push((lease, at)),
                Effect::Revoke { run } => {
                    if let Some(link) = link.as_deref_mut() {
                        link.queue.borrow_mut().revoke(run);
                    }
                }
                Effect::PrepareDrawing(job) => {
                    let unprepared = Input::Drawing(job.unprepared());
                    let inputs = self.inputs_tx.clone();
                    let spawned = std::thread::Builder::new()
                        .name("drawing-prepare".to_owned())
                        .spawn(move || inputs.send(Input::Drawing(job.run())).unwrap_or_default());
                    if spawned.is_err() {
                        tracing::warn!("the drawing preparation thread didn't start");
                        let sent = self.inputs_tx.send(unprepared).is_ok();
                        debug_assert!(sent, "the brain holds its own receiver");
                    }
                }
                Effect::RevokeScenes { scenes } => {
                    if let Some(shell) = link.as_deref_mut() {
                        if let Some(recovery) = &mut shell.recovery {
                            recovery.drop_scenes(&scenes);
                        }
                        shell.queue.borrow_mut().revoke_scenes(&scenes);
                    }
                }
                Effect::RetireScenes { scenes, successor } => {
                    if let Some(shell) = link.as_deref_mut()
                        && shell.retire_scenes(scenes, successor).is_err()
                    {
                        full = true;
                        link = None;
                    }
                }
                Effect::CancelEngine { run } => {
                    for grant in self.coordinator.code_picture_grants(run) {
                        grant.close();
                    }
                    self.retire_key_run(run);
                    if self.round.as_ref().is_some_and(|(round, _)| *round == run) {
                        self.round = None;
                    }
                }
                Effect::ArmRetry { after } => {
                    self.retry_at = Some(Instant::now() + after);
                }
                Effect::CancelRetry => self.retry_at = None,
                Effect::ArmReadyWait => {
                    self.ready_at = Some(Instant::now() + self.config.drawing_tuning().ready_wait);
                }
                Effect::CancelReadyWait => self.ready_at = None,
                Effect::ArmDeadline { run, after } => {
                    self.deadline = Some((run, Instant::now() + after));
                }
                Effect::ArmDeadlineAt { run, at } => self.deadline = Some((run, at)),
                Effect::Broken(reason) => broken = Some(reason),
                speech @ (Effect::Speak { .. }
                | Effect::Credit { .. }
                | Effect::CancelSpeech { .. }
                | Effect::RevokeSpeech { .. }
                | Effect::ArmDrain { .. }
                | Effect::ArmQuiet { .. }) => self.speech(speech),
            }
        }
        if let Some(reason) = broken.filter(|_| shell) {
            return Err(Ended::Broken(reason));
        }
        if full { Err(Ended::Lost) } else { Ok(()) }
    }

    /// A run's speech effect: the pump's work and its timers. With no pump, a Speak fails as
    /// if TTS weren't loaded.
    fn speech(&mut self, effect: Effect) {
        match effect {
            Effect::Speak {
                run,
                segment,
                rate,
                text,
            } => {
                if let Some(pump) = &mut self.pump {
                    pump.set_output(self.coordinator.output_mode());
                    pump.speak(run, segment, rate, text.into_string());
                } else {
                    let made = Synthesized::Failed { run: true };
                    let failed = self.inputs_tx.send(Input::Tts { run, segment, made });
                    debug_assert!(failed.is_ok(), "the brain holds its own receiver");
                }
            }
            Effect::Credit { run, samples } => {
                if let Some(pump) = &mut self.pump {
                    pump.credit(run, samples);
                }
            }
            Effect::CancelSpeech { run } => {
                if let Some(pump) = &mut self.pump {
                    pump.cancel(run);
                }
            }
            Effect::RevokeSpeech { run } => {
                if let Some(pump) = &mut self.pump {
                    pump.revoke(run);
                }
            }
            Effect::ArmDrain { run, epoch, after } => {
                self.drain = Some((run, epoch, Instant::now() + after));
            }
            Effect::ArmQuiet { run, after } => self.quiet.push((run, Instant::now() + after)),
            _ => unreachable!("apply carries out every other effect"),
        }
    }
}

/// One watchdog tick while `running`: pings the shell, ends the connection when a ping has gone
/// unanswered too long, and stalls the run when the shell stopped taking data.
fn watchdog(
    ping_sent: &mut Option<Instant>,
    running: Option<RunId>,
    link: &mut Link<'_, '_>,
) -> Result<Option<Input>, Ended> {
    let now = Instant::now();
    if ping_sent.is_some_and(|sent| now.duration_since(sent) > PONG_WITHIN) {
        tracing::warn!("the shell stopped answering pings");
        return Err(Ended::Lost);
    }
    if ping_sent.is_none() {
        *ping_sent = Some(now);
        if link.control(&ControlToShell::Ping).is_err() {
            return Err(Ended::Lost);
        }
    }
    Ok(running
        .filter(|_| link.queue.borrow().stalled(now.into_std()))
        .map(|run| Input::Stalled { run }))
}

/// A visible scene as `Scenes` names it.
fn descriptor(state: &SceneState) -> SceneDescriptor {
    SceneDescriptor {
        scene: state.scene,
        revision: state.revision,
        committed: u32::try_from(state.kept.len()).expect("at most MAX_STEPS kept"),
    }
}

/// A typed turn or an ask about a part, in the order the shell sent them: each is numbered and
/// answered once.
type Turn = Input;

/// Queues a turn without waiting for the core; a shell that outruns the queue is broken.
fn take(turns: &mpsc::Sender<Turn>, turn: Turn) -> Option<Ended> {
    match turns.try_send(turn) {
        Ok(()) => None,
        Err(TrySendError::Full(_)) => Some(Ended::Broken("turns sent faster than taken")),
        Err(TrySendError::Closed(_)) => Some(Ended::Lost),
    }
}

/// Reads the data pipe. Credit is applied here, so it never waits behind turns the core
/// isn't taking yet.
#[expect(
    clippy::too_many_lines,
    reason = "one flat arm per frame with one acceptance check"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the reader borrows per-connection ingress"
)]
async fn read_data<const WATCH_FATAL: bool>(
    pipe: &mut (impl AsyncRead + Unpin),
    turns: &mpsc::Sender<Turn>,
    pages: &mpsc::Sender<Input>,
    queue: &RefCell<DataQueue>,
    ready: &Notify,
    epoch: u64,
    pcm: Option<&hold::Ingress>,
    registration: Option<&Cell<Registration>>,
    fence: Option<&Fence>,
) -> Ended {
    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    let probe = audio_fixture::probe();
    loop {
        let message = ipc::read::<DataToBrain>(pipe, Pipe::Data).await;
        let fatal = if WATCH_FATAL {
            fence.and_then(Fence::check)
        } else {
            None
        };
        if WATCH_FATAL
            && let Some(cause) = fatal
            && !matches!(message, Ok(Some(DataToBrain::MicPcm(_))))
        {
            return Ended::Fatal(cause);
        }
        match message {
            Ok(Some(Hello(_) | QueuedHello(..) | TextMetrics(_))) => {
                return Ended::Broken("startup frame on live data pipe");
            }
            Ok(Some(DataToBrain::MicPcm(mut chunk))) => {
                if let Some(pcm) = pcm.filter(|_| {
                    (!WATCH_FATAL || fatal.is_none()) && Registration::admits(registration)
                }) {
                    pcm.push(chunk);
                } else {
                    zeroize::Zeroize::zeroize(&mut chunk.samples);
                }
                if WATCH_FATAL && let Some(cause) = fatal {
                    return Ended::Fatal(cause);
                }
            }
            Ok(Some(message @ DataToBrain::IssueEdit { .. })) => {
                tracing::debug!(op = message.op(), "issue data ignored");
            }
            Ok(Some(DataToBrain::Credit { bytes })) => {
                #[cfg(all(windows, debug_assertions, feature = "test-support"))]
                if let Some(probe) = &probe {
                    probe.credit(queue, ready, bytes);
                    continue;
                }
                queue.borrow_mut().grant(bytes);
                ready.notify_one();
            }
            Ok(Some(DataToBrain::IssueCards { .. })) => {
                return Ended::Broken("uncorrelated card request");
            }
            Ok(Some(DataToBrain::IssueCardsRequest { request, drafts })) => {
                let Some(cards) = queue.borrow_mut().admit_cards(epoch, request, &drafts) else {
                    return Ended::Broken("too many unanswered read requests");
                };
                ready.notify_one();
                match pages.try_send(Input::ReadCards { cards }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        return Ended::Broken("too many unanswered read requests");
                    }
                    Err(TrySendError::Closed(_)) => return Ended::Lost,
                }
            }
            Ok(Some(DataToBrain::PageRequest { request, page })) => {
                let Some(key) = queue.borrow_mut().admit_page(epoch, request) else {
                    return Ended::Broken("too many unanswered page requests");
                };
                ready.notify_one();
                match pages.try_send(Input::ReadPage { key, page }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        return Ended::Broken("too many unanswered page requests");
                    }
                    Err(TrySendError::Closed(_)) => return Ended::Lost,
                }
            }
            // Never waits for the core, so credit behind a turn is always read.
            Ok(Some(DataToBrain::TextSubmit { .. } | DataToBrain::AskAbout { .. }))
                if !Registration::admits(registration) =>
            {
                return Ended::Broken("turn before queued audio registration");
            }
            Ok(Some(
                DataToBrain::AskAbout { .. }
                | DataToBrain::TextSubmit {
                    screen: kumochi_protocol::message::ScreenChoice::Include { .. },
                    ..
                },
            )) if registration.is_some() => return Ended::Broken("non-Plain queued turn"),
            Ok(Some(DataToBrain::TextSubmit {
                turn, thread, text, ..
            })) => {
                if let Some(ended) = take(turns, Input::Turn { turn, thread, text }) {
                    return ended;
                }
            }
            Ok(Some(DataToBrain::AskAbout {
                turn,
                scene,
                element,
            })) => {
                if let Some(ended) = take(
                    turns,
                    Input::Ask {
                        turn,
                        scene,
                        element,
                    },
                ) {
                    return ended;
                }
            }
            Ok(None) => return Ended::Lost,
            Err(error) => {
                tracing::warn!(%error, "data pipe failed");
                return Ended::Lost;
            }
        }
    }
}

/// One leg's outgoing side, one whole encoded frame per call. A pipe writes the bytes, as it always
/// has; a transport whose frames can't be resumed refuses its pair when a pending frame is dropped.
pub(crate) trait FrameWrite {
    async fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()>;
}

impl<T: AsyncWrite + Unpin> FrameWrite for T {
    async fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.write_all(frame).await
    }
}

async fn write_control(pipe: &mut impl FrameWrite, out: &mut mpsc::Receiver<Vec<u8>>) -> Ended {
    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    let probe = audio_fixture::probe();
    while let Some(frame) = out.recv().await {
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        if let Some(probe) = &probe {
            probe.before_control(&frame).await;
        }
        if let Err(error) = pipe.write_frame(&frame).await {
            tracing::warn!(%error, "control pipe failed");
            return Ended::Lost;
        }
    }
    Ended::Lost
}

/// Writes queued data as credit allows, waking the core through `room` as the queue drains.
async fn write_data(
    pipe: &mut impl FrameWrite,
    queue: &RefCell<DataQueue>,
    ready: &Notify,
    room: &Notify,
) -> Ended {
    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    let probe = audio_fixture::probe();
    let mut ticks = tokio::time::interval(crate::recall::control::POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if queue.borrow().reads_ended() {
            return Ended::Lost;
        }
        let before = queue.borrow().bytes();
        let next = queue.borrow_mut().pop(Instant::now().into_std());
        if next.is_none() && queue.borrow().bytes() < before {
            room.notify_one();
        }
        let Some(frame) = next else {
            #[cfg(all(windows, debug_assertions, feature = "test-support"))]
            if let Some(probe) = &probe
                && queue.borrow().credit_blocked()
            {
                probe
                    .data_blocked
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            tokio::select! {
                () = ready.notified() => {},
                _ = ticks.tick(), if queue.borrow().has_reads() => {},
            }
            continue;
        };
        room.notify_one();
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        if let Some(probe) = &probe {
            probe.writing_scene_prefix(queue, &frame);
            probe.sent.fetch_add(
                u64::try_from(frame.len()).expect("encoded test data length fits"),
                std::sync::atomic::Ordering::Release,
            );
        }
        let writing = pipe.write_frame(&frame);
        tokio::pin!(writing);
        loop {
            tokio::select! {
                biased;
                _ = ticks.tick(), if queue.borrow().has_reads() => {
                    if queue.borrow().reads_ended() {
                        return Ended::Lost;
                    }
                }
                result = std::future::poll_fn(|cx| {
                    if queue.borrow().reads_ended() {
                        return std::task::Poll::Ready(None);
                    }
                    std::future::Future::poll(writing.as_mut(), cx).map(Some)
                }) => {
                    let Some(result) = result else {
                        return Ended::Lost;
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "data pipe failed");
                        return Ended::Lost;
                    }
                    if queue.borrow().reads_ended() {
                        return Ended::Lost;
                    }
                    break;
                }
                () = ready.notified() => {},
            }
        }
        queue.borrow_mut().wrote();
    }
}

/// A shell frame couldn't be encoded or queued.
#[derive(Debug)]
struct Full;

/// One connection's outgoing side.
struct Link<'a, 't> {
    out: &'a mpsc::Sender<Vec<u8>>,
    queue: &'a RefCell<DataQueue>,
    ready: &'a Notify,
    tap: &'t mut Tap,
    registration: Option<&'a Cell<Registration>>,
    start: PairStart,
    /// The pair's opening Talk-back choice, until its first change in either direction.
    queued_talk_back: Option<bool>,
    text_metrics: Option<TextMetricsV1>,
    /// The snapshots still to send, once `Scenes` has gone.
    recovery: Option<Recovery>,
    /// Page replies waiting for room in the data queue.
    held_pages: VecDeque<Input>,
}

impl<'a, 't> Link<'a, 't> {
    fn new(
        out: &'a mpsc::Sender<Vec<u8>>,
        queue: &'a RefCell<DataQueue>,
        ready: &'a Notify,
        tap: &'t mut Tap,
    ) -> Self {
        Self {
            out,
            queue,
            ready,
            tap,
            registration: None,
            start: PairStart::Ordinary,
            queued_talk_back: None,
            text_metrics: None,
            recovery: None,
            held_pages: VecDeque::new(),
        }
    }

    fn control(&mut self, message: &ControlToShell) -> Result<(), Full> {
        match frame::encode(message, Pipe::Control) {
            Ok(frame) => {
                let len = frame.len() - frame::HEADER_LEN;
                self.tap
                    .record(Pipe::Control, Direction::ToShell, message, len);
                self.out.try_send(frame).map_err(|_| Full)
            }
            Err(error) => {
                tracing::error!(%error, "a control message didn't encode");
                if matches!(message, ControlToShell::QueuedAudioRegistered(_)) {
                    Err(Full)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn retire_scenes(
        &mut self,
        scenes: Vec<SceneId>,
        successor: Option<SceneId>,
    ) -> Result<(), Full> {
        let message = match successor {
            Some(successor) => DataToShell::ScenesReplaced { scenes, successor },
            None => DataToShell::ScenesRemoved { scenes },
        };
        let frame = frame::encode(&message, Pipe::Data).map_err(|_| Full)?;
        let (DataToShell::ScenesRemoved { scenes } | DataToShell::ScenesReplaced { scenes, .. }) =
            message
        else {
            unreachable!("the retirement marker was constructed above");
        };
        if let Some(recovery) = &mut self.recovery {
            recovery.drop_scenes(&scenes);
        }
        self.queue.borrow_mut().retire_scenes(&scenes, frame);
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        if let Some(probe) = audio_fixture::probe() {
            if successor.is_some() {
                probe
                    .replacement_queued
                    .store(true, std::sync::atomic::Ordering::Release);
            } else {
                probe.applied_scene_revoke(&scenes);
            }
        }
        self.ready.notify_one();
        Ok(())
    }

    /// Data frames carry content, so the tap never records them.
    fn data(&mut self, message: &DataToShell) -> Result<(), Full> {
        #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
        let sample = credit_fixture::expand(message);
        #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
        let message = sample.as_ref().unwrap_or(message);
        let queued = self.registration.is_some();
        let owner = match message {
            DataToShell::SceneStepCommit {
                scene,
                speech: Some(_),
                ..
            } if queued => Owner::SpeechPromise(*scene),
            DataToShell::SegmentQueued {
                run,
                start: SegmentStart::Speech,
                step: Some(step),
                ..
            } if queued => Owner::SpeechAnnouncement(*run, step.scene),
            DataToShell::AssistantText { run, .. }
            | DataToShell::StepCommit { run, .. }
            | DataToShell::RunEnd { run }
            | DataToShell::SegmentQueued { run, .. }
            | DataToShell::Transcript(Transcript { run, .. })
            | DataToShell::SpeechPcm(SpeechPcm { run, .. }) => Owner::Run(*run),
            DataToShell::ThreadList { .. }
            | DataToShell::Page { .. }
            | DataToShell::PageFailed { .. }
            | DataToShell::IssueCardRead { .. }
            | DataToShell::IssueCardsDone { .. } => return Err(Full),
            DataToShell::ScenesRemoved { .. } | DataToShell::ScenesReplaced { .. } => {
                Owner::Presentation
            }
            DataToShell::SceneCutApplied { cut, .. } if cut.activated == 0 => Owner::Marker,
            DataToShell::Models { .. }
            | DataToShell::IssueCard(_)
            | DataToShell::IssueReview { .. }
            | DataToShell::HoldWords { .. }
            | DataToShell::HoldStored { .. }
            | DataToShell::EventsChanged { .. } => Owner::Marker,
            DataToShell::SceneSnapshot { scene, .. }
            | DataToShell::SceneBegin { scene, .. }
            | DataToShell::SceneStepCommit { scene, .. }
            | DataToShell::SceneCutApplied { scene, .. }
            | DataToShell::SceneEnd { scene, .. } => Owner::Scene(*scene),
            DataToShell::SceneSnapshotV1 { chunk } => Owner::Scene(chunk.scene),
        };
        let Ok(frame) = frame::encode(message, Pipe::Data) else {
            tracing::error!("a data message didn't encode");
            return Err(Full);
        };
        if matches!(message, DataToShell::IssueReview { .. }) {
            self.queue.borrow_mut().push_review(frame)?;
        } else {
            self.queue.borrow_mut().push(owner, frame);
        }
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        if let Some(probe) = audio_fixture::probe() {
            probe.queued_scene_data(message);
        }
        #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
        if let DataToShell::SceneStepCommit {
            run,
            scene,
            index,
            step,
            ..
        } = message
            && let Some(observation) =
                crate::connections::model::text_observation::Observation::drawing()
        {
            observation.mark_draw(
                *run,
                crate::platform::qpc_now(),
                crate::connections::model::text_observation::DrawStage::Pipe {
                    scene: *scene,
                    index: *index,
                    step: step.id.as_str(),
                },
            );
        }
        self.ready.notify_one();
        Ok(())
    }
}

fn window() -> usize {
    usize::try_from(DATA_WINDOW).expect("the window fits in usize")
}

/// The pump's next event, or never without a pump.
async fn next_tts(pump: &mut Option<Pump>) -> (RunId, TtsEvent) {
    match pump {
        Some(pump) => pump.next().await,
        None => std::future::pending().await,
    }
}

/// A TTS event as the coordinator takes it. A model that isn't loaded, or failed to load, can't
/// speak the rest of the run either.
fn tts_input(run: RunId, event: TtsEvent) -> Input {
    let (segment, made) = match event {
        TtsEvent::Pcm { segment, samples } => (segment, Synthesized::Audio(samples)),
        TtsEvent::Done { segment, .. } => (segment, Synthesized::Done),
        TtsEvent::Failed { segment, error } => {
            let run = matches!(error, TtsError::NotLoaded | TtsError::Load);
            (segment, Synthesized::Failed { run })
        }
    };
    Input::Tts {
        run,
        segment: kumochi_protocol::message::SegmentId(segment),
        made,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
