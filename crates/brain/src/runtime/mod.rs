//! The brain's systems around the coordinator: the store thread, the engine, timers,
//! and one shell connection's readers and writers.
//!
//! Each pipe has its own reader and writer, polled together in one task, so a reader never
//! waits behind a write (CAPS-8 finding 1). Esc reaches the coordinator even while the data
//! writer is blocked on a shell that stopped reading.

pub(crate) mod account;
mod controls;
mod data;
mod recovery;
mod settings;
mod store_thread;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain, DataToShell, Deleted,
    MAX_PENDING_TURNS, MAX_VISIBLE_SCENES, RunId, SceneDescriptor, SceneId, ThreadId,
};
use kumochi_protocol::tap::Direction;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::config::Config;
use crate::connections::account::Input as AccountInput;
use crate::context::{ConnectionTag, Origin, RunSnapshot};
#[cfg(debug_assertions)]
use crate::coordinator::{Availability, Ready};
use crate::coordinator::{
    BuildContext, Coordinator, Effect, Input, JobKey, JobKind, LeaseId, StoreOp,
};
use crate::engines::{self, Answer, Asks, Cancel, Engine, SendGate, SendLease};
use crate::ipc;
use crate::store::{SceneState, Store};
use crate::tap::Tap;
use account::Accounts;
use controls::read_control;
use data::{DataQueue, Owner};
use recovery::Recovery;
use store_thread::StoreThread;

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
    /// The shell broke the protocol after the handshake.
    Broken(&'static str),
    /// The visible set didn't settle in time, so no `Scenes` was sent; the shell reconnects.
    Unavailable,
}

/// Whether a Quit's last clear committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quit {
    /// Every visible scene's clear is durable.
    Complete,
    /// The deadline passed or a write failed; the scenes come back at the next start.
    Incomplete,
}

/// The engine runs use, picked once at start (docs/development/engines.md, "Selection").
#[derive(Debug)]
pub(crate) enum Selected {
    /// The dev fixture answers every run, with no account; a release build can't pick it.
    #[cfg(debug_assertions)]
    Fixture(Box<dyn Engine>),
    /// The account's engine answers, so runs need a usable account.
    Account(Box<dyn Engine>),
}

/// The coordinator and the systems that outlive one shell connection.
#[derive(Debug)]
pub(crate) struct Brain {
    pub(crate) crashes: crate::relaunch::Crashes,
    coordinator: Coordinator,
    store: Option<StoreThread>,
    engine: Box<dyn Engine>,
    /// The dev fixture was picked, so runs never wait on the account.
    #[cfg(debug_assertions)]
    fixture: bool,
    /// Store replies, never paused.
    inputs_tx: mpsc::UnboundedSender<Input>,
    inputs: mpsc::UnboundedReceiver<Input>,
    /// Engine events, taken only while the data queue has room.
    engine_tx: mpsc::Sender<Input>,
    engine_events: mpsc::Receiver<Input>,
    /// The current run's round, aborted when its `Cancel` drops.
    round: Option<(RunId, Cancel)>,
    /// The background job's request, aborted the same way.
    job: Option<(JobKey, Cancel)>,
    /// The uncovered backlog, in estimated tokens, at which a thread compacts after a run.
    pub(crate) compact_at: u64,
    /// Rounds waiting for leave to send.
    asks: Asks,
    /// When each granted send's deadline passes, in no order.
    send_deadlines: Vec<(LeaseId, Instant)>,
    deadline: Option<(RunId, Instant)>,
    /// When owed writes are tried again.
    retry_at: Option<Instant>,
    /// Page completions belong only to the connection that requested them.
    epoch: u64,
    /// What a recovery chunk packs up to; tests lower it to force several chunks.
    chunk_target: usize,
    /// The settings, owned here: they never touch runs or the store.
    config: Config,
    /// Settings changes waiting for the one write in flight, answered in order.
    settings_ops: VecDeque<settings::Queued>,
    /// The settings write in flight, off the core loop: a durable replace can take any time.
    config_write: Option<settings::Write>,
    /// The account system; its credential work runs on its own thread.
    account: Accounts,
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
    /// A page reply landed while the data queue was full and waits for room.
    PageHeld,
    /// Another pipe ended first, and the control reader's grace began.
    Grace,
    /// A control the reader forwarded as its pipe ended is applied after the core has gone.
    ControlDrained,
    /// The store answered a delete, and the brain applied the answer.
    DeleteAnswered,
    /// A quit ended the current run and now waits for the store.
    QuitDraining,
    /// A settings write is on disk and became the settings.
    SettingsCommitted,
}

impl Brain {
    /// Starts the store thread. The store must already be recovered, `main_thread` is its main
    /// thread, and `high_water` is its highest thread and run ids.
    ///
    /// # Errors
    /// Fails when the store's thread can't be started.
    pub(crate) fn new(
        store: Store,
        main_thread: ThreadId,
        high_water: (ThreadId, RunId),
        scenes: Vec<SceneState>,
        engine: Selected,
        config: Config,
        account: Accounts,
    ) -> std::io::Result<Self> {
        #[cfg(debug_assertions)]
        let (engine, fixture) = match engine {
            Selected::Fixture(engine) => (engine, true),
            Selected::Account(engine) => (engine, false),
        };
        #[cfg(not(debug_assertions))]
        let Selected::Account(engine) = engine;
        let (last_thread, last_run) = high_water;
        let (inputs_tx, inputs) = mpsc::unbounded_channel();
        #[cfg(test)]
        let (observer, seen) = mpsc::unbounded_channel();
        #[cfg(test)]
        let (gates, gates_heard) = std::sync::mpsc::channel();
        let schedule = store_thread::Schedule {
            #[cfg(test)]
            observed: Some(observer.clone()),
            #[cfg(test)]
            gates: Some(gates_heard),
            ..store_thread::Schedule::default()
        };
        let drawing_scenes = store.drawing_scenes();
        let mut coordinator = Coordinator::new(main_thread, last_thread, last_run, scenes);
        let recovery_clears = coordinator.restore_drawings(drawing_scenes);
        let store = StoreThread::spawn(store, inputs_tx.clone(), schedule)?;
        let (engine_tx, engine_events) = mpsc::channel(ENGINE_QUEUE);
        let mut brain = Self {
            crashes: crate::relaunch::Crashes::default(),
            coordinator,
            store: Some(store),
            engine,
            #[cfg(debug_assertions)]
            fixture,
            inputs_tx,
            inputs,
            engine_tx,
            engine_events,
            round: None,
            job: None,
            compact_at: crate::jobs::compact_at(),
            asks: Asks::default(),
            send_deadlines: Vec::new(),
            deadline: None,
            retry_at: None,
            epoch: 0,
            chunk_target: crate::drawing::CHUNK_TARGET,
            config,
            settings_ops: VecDeque::new(),
            config_write: None,
            account,
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

    /// Tells the coordinator whether runs can be answered now; the same answer again changes
    /// nothing.
    fn availability_changed(&mut self, link: Option<&mut Link<'_, '_>>) -> Result<(), Ended> {
        #[cfg(debug_assertions)]
        let now = if self.fixture {
            Availability::Ready(Ready::fixture())
        } else {
            self.account.availability()
        };
        #[cfg(not(debug_assertions))]
        let now = self.account.availability();
        let effects = self
            .coordinator
            .handle(Input::Engines(now))
            .expect("availability is never stale");
        self.apply(effects, link)
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

    /// Shuts down after an accepted Quit, all by one absolute `deadline`: ends the current run
    /// as shell loss would, drains every accepted store write and delete, clears the visible set
    /// and what's still clearing in one write, waits for its commit, then closes the store. No
    /// stage starts a budget of its own. Only a committed clear with every delete done is a
    /// complete quit; otherwise the scenes, or a thread whose delete didn't commit, come back at
    /// the next start, as after a crash.
    pub(crate) async fn quit(mut self, deadline: Instant) -> Quit {
        self.offline(Input::ShellLost);
        self.pump_settings_offline();
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
        if let Some(store) = self.store.take() {
            match tokio::time::timeout_at(deadline, store.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!("the store thread panicked"),
                Err(_) => tracing::warn!("the store thread didn't finish in time"),
            }
        }
        self.drain_settings_by(deadline).await;
        self.account
            .close(deadline, Instant::now() + REFRESH_WAIT)
            .await;
        if cleared && !undeleted {
            Quit::Complete
        } else {
            tracing::warn!(
                undeleted,
                "the quit was incomplete; what it didn't clear or delete comes back at the next start"
            );
            Quit::Incomplete
        }
    }

    /// Lets runs that ended reach the store, then lets its queued writes finish. Each wait is
    /// bounded, so a hung store can't hold the brain open. At the next start, runs still active in
    /// the store become `failed(crash)`; committed outcomes stay, and uncommitted work may be lost.
    pub(crate) async fn close(mut self) {
        let deadline = Instant::now() + close_wait();
        self.pump_settings_offline();
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
        if let Some(store) = self.store.take() {
            match tokio::time::timeout(close_wait(), store.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!("the store thread panicked"),
                Err(_) => tracing::warn!("the store thread didn't finish in time"),
            }
        }
        self.drain_settings_by(deadline).await;
        self.account
            .close(deadline, Instant::now() + REFRESH_WAIT)
            .await;
    }

    /// Keeps runs that ended moving through the store, and settings changes being written,
    /// while no shell is connected. Never returns; drop it when a shell connects.
    pub(crate) async fn idle(&mut self) {
        self.pump_settings_offline();
        loop {
            let send_due = self.next_send_deadline();
            let input = tokio::select! {
                Some(input) = self.inputs.recv() => input,
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
        if let Input::ScenesCleared { scenes, ok: true } = &input {
            self.crashes.cleared(scenes);
        }
        self.coordinator.handle(input).ok()
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
    pub(crate) async fn serve<C, D>(&mut self, control: C, data: D, tap: &mut Tap) -> Ended
    where
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
    {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("connection epochs never wrap");
        self.coordinator.begin_session();
        self.crashes.begin_session();
        let (mut control_in, mut control_out) = tokio::io::split(control);
        let (mut data_in, mut data_out) = tokio::io::split(data);
        let (controls_tx, mut controls) = mpsc::channel(64);
        let (turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
        let (pages_tx, mut pages) = mpsc::channel(kumochi_protocol::message::MAX_PENDING_PAGES);
        let (out_tx, mut out) = mpsc::channel::<Vec<u8>>(CONTROL_QUEUE);
        let queue = RefCell::new(DataQueue::new(window()));
        let ready = Notify::new();
        let room = Notify::new();
        let quit = Cell::new(None);

        let control_reader = read_control(&mut control_in, &controls_tx, &quit);
        tokio::pin!(control_reader);
        let mut reading = true;
        let ended = {
            let data_reader = read_data(
                &mut data_in,
                &turns_tx,
                &pages_tx,
                &queue,
                &ready,
                self.epoch,
            );
            let control_writer = write_control(&mut control_out, &mut out);
            let data_writer = write_data(&mut data_out, &queue, &ready, &room);
            let link = Link::new(&out_tx, &queue, &ready, &mut *tap);
            let core = self.core(&mut controls, &mut turns, &mut pages, &room, link);
            tokio::select! {
                ended = &mut control_reader => {
                    reading = false;
                    ended
                }
                ended = data_reader => ended,
                ended = control_writer => ended,
                ended = data_writer => ended,
                ended = core => ended,
            }
        };
        if reading && quit.get().is_none() {
            #[cfg(test)]
            self.observer.send(Seen::Grace).ok();
            let grace = tokio::time::sleep(QUIT_GRACE);
            tokio::pin!(grace);
            loop {
                tokio::select! {
                    biased;
                    Some((message, len)) = controls.recv() => {
                        self.control_offline(message, len, tap);
                    }
                    _ = &mut control_reader => break,
                    () = &mut grace => break,
                }
            }
        }
        // The reader may have forwarded controls in the same poll that saw the pipe end.
        while let Ok((message, len)) = controls.try_recv() {
            #[cfg(test)]
            self.observer.send(Seen::ControlDrained).ok();
            self.control_offline(message, len, tap);
        }
        let ended = match quit.get() {
            Some(deadline) => Ended::Quit { deadline },
            None => ended,
        };
        self.offline(Input::ShellLost);
        self.deadline = None;
        ended
    }

    /// The core loop: frames from the shell, completions from the systems, the deadline and
    /// the watchdog, each turned into coordinator inputs and their effects applied. Nothing in
    /// it awaits a pipe, the store or an engine. Control and store replies are always taken.
    /// A page read from an older connection is dropped, and one that arrives while the data
    /// queue is full is held until it has room; any other input passes.
    fn admit_page(&mut self, input: Input, link: &mut Link<'_, '_>) -> Option<Input> {
        if let Input::PageRead { epoch, .. } = &input {
            if *epoch != self.epoch {
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
    /// taken until the visible set has settled and `Scenes` has named it, so the list is never
    /// one the store has already moved past. If it hasn't settled within [`SCENES_WAIT`], the
    /// connection ends as unavailable instead. After `Scenes`, the snapshots go out a chunk at
    /// a time whenever the data queue has room, ahead of engine events.
    async fn core(
        &mut self,
        controls: &mut mpsc::Receiver<(ControlToBrain, usize)>,
        turns: &mut mpsc::Receiver<Turn>,
        pages: &mut mpsc::Receiver<Input>,
        room: &Notify,
        mut link: Link<'_, '_>,
    ) -> Ended {
        let mut ticks = tokio::time::interval(PING_EVERY);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ping_sent: Option<Instant> = None;
        let barrier = Instant::now() + scenes_wait();
        if let Err(ended) = self.open(&mut link) {
            return ended;
        }
        loop {
            if link.recovery.is_none() && self.coordinator.settled() {
                let Ok(described) = self.describe(&mut link) else {
                    return Ended::Lost;
                };
                link.recovery = described;
            }
            let described = link.recovery.is_some();
            let running = self.coordinator.current_run();
            let deadline = self.deadline;
            let send_due = self.next_send_deadline();
            let take_engine = link.queue.borrow().bytes() < DATA_CAP;
            let take_turns = described && take_engine && self.room_for_turns();
            let feed = take_engine && link.recovery.as_ref().is_some_and(Recovery::pending);
            let input = tokio::select! {
                biased;
                Some((message, len)) = controls.recv() => {
                    match self.control_frame(message, len, &mut link, &mut ping_sent) {
                        Ok(Some(input)) => input,
                        Ok(None) => continue,
                        Err(ended) => return ended,
                    }
                }
                () = std::future::ready(()), if take_engine && !link.held_pages.is_empty() => {
                    link.held_pages.pop_front().expect("a held page is ready")
                }
                () = std::future::ready(()), if feed => {
                    if self.queue_recovery(&mut link).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                written = settings::written(&mut self.config_write) => {
                    if self.settings_done(written, Some(&mut link)).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                input = self.account.next() => {
                    if self.account_input(input, Some(&mut link)).is_err() {
                        return Ended::Lost;
                    }
                    continue;
                }
                Some(input) = self.inputs.recv() => input,
                Some(input) = pages.recv() => input,
                Some((thread, text)) = turns.recv(), if take_turns => Input::Turn { thread, text },
                Some(input) = self.engine_events.recv(), if take_engine => input,
                () = room.notified(), if !take_engine => continue,
                () = sleep_until(self.retry_at) => {
                    self.retry_at = None;
                    Input::Retry
                }
                () = sleep_until(send_due) => self.send_deadline_passed(),
                () = sleep_until(deadline.map(|(_, at)| at)) => {
                    self.deadline = None;
                    match deadline {
                        Some((run, _)) => Input::Deadline { run },
                        None => continue,
                    }
                }
                () = tokio::time::sleep_until(barrier), if !described => {
                    tracing::warn!("the visible scenes didn't settle in time; ending the connection");
                    return Ended::Unavailable;
                }
                _ = ticks.tick(), if running.is_some() => {
                    match watchdog(&mut ping_sent, running, &mut link) {
                        Ok(Some(input)) => input,
                        Ok(None) => continue,
                        Err(ended) => return ended,
                    }
                }
            };
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
        self.settings_now(link).map_err(|Full| Ended::Lost)?;
        self.pump_settings(Some(link)).map_err(|Full| Ended::Lost)?;
        self.account_input(AccountInput::Connected, Some(link))
            .map_err(|Full| Ended::Lost)?;
        // A clear whose write failed while no shell was connected is retried now at the latest.
        self.retry_at = None;
        let retried = self.coordinator.retry_clears();
        self.apply(retried, Some(link)).map_err(|_| Ended::Lost)
    }

    /// New turns wait while too many runs are ending or too many writes are owed.
    fn room_for_turns(&self) -> bool {
        self.coordinator.ending_runs() < MAX_ENDING && self.coordinator.debt() < DEBT_WATERMARK
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
        let (origin, run) = (build.origin, build.key.run);
        build.snapshot.get_or_insert_with(|| RunSnapshot {
            // A build for a run that already ended is stale, so its tag is never read.
            connection: self.coordinator.bound(run).unwrap_or(ConnectionTag(0)),
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
        let cancel = engines::start_job(
            &*self.engine,
            request,
            (job, kind),
            self.inputs_tx.clone(),
            gate,
        );
        self.job = Some((job, cancel));
    }

    /// What a run's first build, or a job's read, takes: the names and default model now.
    fn snapshot(&self, origin: Origin) -> RunSnapshot {
        let view = self.config.view();
        RunSnapshot {
            origin,
            assistant: view.assistant_name.clone(),
            user: view.user_name.clone(),
            choice: view.default_model.clone(),
            // shortcut: a job records no connection yet; 13b tags it with the one it used.
            connection: ConnectionTag(0),
        }
    }

    /// Applies effects in order. Frames go to `link`, or nowhere without a shell. Once the
    /// shell's control queue is full nothing more goes to the shell, but every store, engine and
    /// timer effect still applies, since the coordinator counts them as done; then it fails, as
    /// it does when the shell broke the protocol.
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
                Effect::Store(mut op) => {
                    self.seed_op(&mut op);
                    #[cfg(test)]
                    self.observer.send(Seen::StoreOpQueued(op.clone())).ok();
                    let failure = op.failed();
                    if !self.store.as_ref().is_some_and(|store| store.send(op)) {
                        tracing::error!("the store thread is gone");
                        if let Some(failure) = failure {
                            let failed = self.inputs_tx.send(failure);
                            debug_assert!(failed.is_ok(), "the brain holds its own receiver");
                        }
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
                    let gate =
                        SendGate::new(request.key, Arc::clone(&self.asks), self.inputs_tx.clone());
                    let cancel =
                        engines::start(&*self.engine, request, self.engine_tx.clone(), gate);
                    self.round = Some((run, cancel));
                }
                Effect::StartJob { job, kind, request } => self.start_job(job, kind, request),
                Effect::CancelJob(job) => drop(self.job.take_if(|(held, _)| *held == job)),
                Effect::Grant(lease) => {
                    let granted = SendLease::new(lease, self.inputs_tx.clone());
                    engines::answer(&self.asks, lease.key, Answer::Grant(granted));
                }
                Effect::Rebuild(key) => engines::answer(&self.asks, key, Answer::Rebuild),
                Effect::ArmSendDeadline { lease, after } => {
                    self.send_deadlines.push((lease, Instant::now() + after));
                }
                Effect::Revoke { run } => {
                    if let Some(link) = link.as_deref_mut() {
                        link.queue.borrow_mut().revoke(run);
                    }
                }
                Effect::RevokeScenes { scenes } => {
                    if let Some(link) = link.as_deref_mut() {
                        if let Some(recovery) = &mut link.recovery {
                            recovery.drop_scenes(&scenes);
                        }
                        link.queue.borrow_mut().revoke_scenes(&scenes);
                    }
                }
                Effect::CancelEngine { run } => {
                    if self.round.as_ref().is_some_and(|(round, _)| *round == run) {
                        self.round = None;
                    }
                }
                Effect::ArmRetry { after } => {
                    self.retry_at = Some(Instant::now() + after);
                }
                Effect::CancelRetry => self.retry_at = None,
                Effect::ArmDeadline { run, after } => {
                    self.deadline = Some((run, Instant::now() + after));
                }
                Effect::Broken(reason) => broken = Some(reason),
            }
        }
        if let Some(reason) = broken.filter(|_| shell) {
            return Err(Ended::Broken(reason));
        }
        if full { Err(Ended::Lost) } else { Ok(()) }
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

/// A typed turn: the thread to continue, or `None` for a new one, and the text.
type Turn = (Option<ThreadId>, String);

/// Reads the data pipe. Credit is applied here, so it never waits behind turns the core
/// isn't taking yet.
async fn read_data(
    pipe: &mut (impl AsyncRead + Unpin),
    turns: &mpsc::Sender<Turn>,
    pages: &mpsc::Sender<Input>,
    queue: &RefCell<DataQueue>,
    ready: &Notify,
    epoch: u64,
) -> Ended {
    loop {
        match ipc::read::<DataToBrain>(pipe, Pipe::Data).await {
            Ok(Some(DataToBrain::Hello(_))) => return Ended::Broken("Hello sent twice"),
            Ok(Some(DataToBrain::Credit { bytes })) => {
                queue.borrow_mut().grant(bytes);
                ready.notify_one();
            }
            Ok(Some(DataToBrain::PageRequest { request, page })) => {
                if !queue.borrow_mut().admit_page() {
                    return Ended::Broken("too many unanswered page requests");
                }
                match pages.try_send(Input::ReadPage {
                    epoch,
                    request,
                    page,
                }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        return Ended::Broken("too many unanswered page requests");
                    }
                    Err(TrySendError::Closed(_)) => return Ended::Lost,
                }
            }
            // Never waits for the core, so credit behind a turn is always read.
            Ok(Some(DataToBrain::TextSubmit { thread, text })) => match turns
                .try_send((thread, text))
            {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return Ended::Broken("turns sent faster than taken"),
                Err(TrySendError::Closed(_)) => return Ended::Lost,
            },
            Ok(None) => return Ended::Lost,
            Err(error) => {
                tracing::warn!(%error, "data pipe failed");
                return Ended::Lost;
            }
        }
    }
}

async fn write_control(
    pipe: &mut (impl AsyncWrite + Unpin),
    out: &mut mpsc::Receiver<Vec<u8>>,
) -> Ended {
    while let Some(frame) = out.recv().await {
        if let Err(error) = pipe.write_all(&frame).await {
            tracing::warn!(%error, "control pipe failed");
            return Ended::Lost;
        }
    }
    Ended::Lost
}

/// Writes queued data as credit allows, waking the core through `room` as the queue drains.
async fn write_data(
    pipe: &mut (impl AsyncWrite + Unpin),
    queue: &RefCell<DataQueue>,
    ready: &Notify,
    room: &Notify,
) -> Ended {
    loop {
        let next = queue.borrow_mut().pop(Instant::now().into_std());
        let Some(frame) = next else {
            ready.notified().await;
            continue;
        };
        room.notify_one();
        if let Err(error) = pipe.write_all(&frame).await {
            tracing::warn!(%error, "data pipe failed");
            return Ended::Lost;
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
                Ok(())
            }
        }
    }

    /// Data frames carry content, so the tap never records them.
    fn data(&mut self, message: &DataToShell) -> Result<(), Full> {
        let owner = match message {
            DataToShell::AssistantText { run, .. }
            | DataToShell::StepCommit { run, .. }
            | DataToShell::RunEnd { run } => Owner::Run(*run),
            DataToShell::ThreadList { .. }
            | DataToShell::Page { .. }
            | DataToShell::PageFailed { .. } => Owner::Page,
            DataToShell::ScenesRemoved { .. } | DataToShell::Models { .. } => Owner::Marker,
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
        self.queue.borrow_mut().push(owner, frame);
        self.ready.notify_one();
        Ok(())
    }
}

fn window() -> usize {
    usize::try_from(DATA_WINDOW).expect("the window fits in usize")
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
