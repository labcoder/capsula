//! The coordinator: the brain's core loop, sans I/O (architecture v1 §5.1).
//!
//! It takes inputs (a typed turn, engine events, `Stop`, store acknowledgements, shell loss,
//! stalls, deadlines) and returns effects (store operations, frames for the shell, engine start
//! and cancel). It owns run ids, the one current run and its budgets, and the runs still ending,
//! and nothing else; the systems around it do the I/O and report back. An engine input or a
//! timer that names a run other than the current one is [`Stale`] and changes nothing, which is
//! how a late completion after `Stop` is refused. A run that ended stays accountable to the
//! store until its outcome is written, so a failed write is still reported.

mod delete;
mod scenes;
mod store_op;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::time::Duration;

use capsula_protocol::message::{
    ControlToShell, DataToShell, Deleted, MAX_STEP_LABEL, MAX_STEPS, PageOf, Phase, RunId, SceneId,
    Step, ThreadId,
};

use crate::store::{Failure, Interruption, RunEnd, SceneState, SceneTarget};

pub use store_op::StoreOp;

/// A run's whole wall-clock budget [S3's total cap].
pub const WALL_CLOCK: Duration = Duration::from_secs(120);
/// The most visible text one run may stream [S3].
pub const MAX_TEXT: usize = 1024 * 1024;
/// The largest piece of text one `AssistantText` frame carries, well under the data pipe's cap.
pub const MAX_DELTA: usize = 64 * 1024;
/// How many characters of a new thread's first turn become its title [guess].
const TITLE_CHARS: usize = 80;

/// Something that happened, for the coordinator to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Read a page for one shell connection, without changing any run.
    ReadPage {
        /// The connection that requested the page.
        epoch: u64,
        /// The shell's request counter.
        request: u32,
        /// The page to read.
        page: PageOf,
    },
    /// A page read finished.
    PageRead {
        /// The connection that requested the page.
        epoch: u64,
        /// The shell's request counter.
        request: u32,
        /// The page, or `PageFailed` when the read failed.
        reply: DataToShell,
    },
    /// The user typed a turn, in `thread` or in a new thread.
    Turn {
        /// The thread to continue, or `None` for a new one.
        thread: Option<ThreadId>,
        /// What the user typed.
        text: String,
    },
    /// The shell stopped `run` locally; `revealed` are the steps that stayed on screen.
    Stop {
        /// The stopped run.
        run: RunId,
        /// Indices of the steps whose entrance had finished.
        revealed: Vec<u32>,
    },
    /// The engine running `run` produced something.
    Engine {
        /// The run it belongs to.
        run: RunId,
        /// What it produced.
        event: EngineEvent,
    },
    /// The store committed a turn.
    TurnStored {
        /// The run the turn started.
        run: RunId,
    },
    /// The store committed a step.
    StepStored {
        /// The run it belongs to.
        run: RunId,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
        /// Its scene; a run's first step learns its new scene's id here.
        scene: SceneId,
    },
    /// The store committed a run's outcome.
    RunStored {
        /// The run.
        run: RunId,
    },
    /// The oldest unanswered store operation for `run` failed.
    StoreFailed {
        /// The run it belonged to.
        run: RunId,
    },
    /// The shell went away.
    ShellLost,
    /// The shell took no data for too long while `run` streamed.
    Stalled {
        /// The streaming run.
        run: RunId,
    },
    /// `run`'s wall-clock budget ran out.
    Deadline {
        /// The run.
        run: RunId,
    },
    /// The shell cleared these scenes locally.
    Clear {
        /// The scenes, at most four.
        scenes: Vec<SceneId>,
    },
    /// A `ClearScenes` write finished.
    ScenesCleared {
        /// The scenes it named.
        scenes: Vec<SceneId>,
        /// Whether it committed.
        ok: bool,
    },
    /// The shell quit: every visible scene is cleared, so only a crash brings drawings back.
    ClearVisible,
    /// A `KeepScene` write finished.
    SceneKept {
        /// The scene it named.
        scene: SceneId,
        /// Whether it committed.
        ok: bool,
    },
    /// The timer for owed clears and selections fired.
    Retry,
    /// The user confirmed deleting `thread`.
    Delete {
        /// The thread.
        thread: ThreadId,
        /// The submits the shell sent on this connection before it.
        after_turns: u32,
        /// A resend of a delete the shell sent on an earlier connection.
        resend: bool,
    },
    /// A `Delete` op finished.
    Deleted {
        /// The thread.
        thread: ThreadId,
        /// What it came to.
        outcome: Deleted,
        /// The thread's newest stored run, read before its rows went.
        last_run: Option<RunId>,
    },
}

/// What an engine produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEvent {
    /// More of the answer.
    Text(String),
    /// One drawing step, not yet committed.
    Step(Step),
    /// The answer is complete.
    Done,
    /// The engine failed.
    Failed,
}

/// Something for the systems around the coordinator to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Run a store operation in order, with one acknowledgement for every read or write.
    Store(StoreOp),
    /// Send on the control pipe.
    Control(ControlToShell),
    /// Send on the data pipe.
    Data(DataToShell),
    /// Start the engine for a stored turn.
    StartEngine {
        /// The run.
        run: RunId,
        /// The user's text.
        text: String,
    },
    /// Stop the engine for `run` and drop whatever it still produces.
    CancelEngine {
        /// The run.
        run: RunId,
    },
    /// Drop `run`'s data not yet sent to the shell, which no longer shows it.
    Revoke {
        /// The run.
        run: RunId,
    },
    /// Drop the snapshots of `scenes` not yet sent to the shell, which cleared them.
    RevokeScenes {
        /// The scenes.
        scenes: Vec<SceneId>,
    },
    /// Report `Retry` after `after`, replacing any earlier `ArmRetry`.
    ArmRetry {
        /// How long from now.
        after: Duration,
    },
    /// Forget the armed `ArmRetry`: nothing owed is left unwritten.
    CancelRetry,
    /// Report `Deadline` for `run` after `after`.
    ArmDeadline {
        /// The run.
        run: RunId,
        /// How long from now.
        after: Duration,
    },
    /// The shell broke the protocol: end its connection.
    Broken(&'static str),
}

/// The input named a run that isn't current, or an acknowledgement nothing is waiting for, so
/// it changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

/// Whether ending a run tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notify {
    Shell,
    Nobody,
}

/// The run in flight, or one that ended and is waiting on the store.
#[derive(Debug)]
struct Run {
    id: RunId,
    thread: ThreadId,
    /// The user's text, until the turn is stored; `None` once it is.
    turn: Option<String>,
    /// The turn's write, held until every older run's outcome is queued.
    begin: Option<StoreOp>,
    answering: bool,
    /// Streamed text not yet stored; it's stored with the next step or the outcome.
    unstored: String,
    text_bytes: usize,
    /// Steps the run produced, and how many the store acknowledged, in order.
    steps: u32,
    acked: u32,
    /// The run's scene, once its first step is committed.
    scene: Option<SceneId>,
    /// The scene the first step's write evicts, until it's acknowledged.
    evict: Option<SceneId>,
    /// Steps after the first, with the text streamed before each, until the scene has its id.
    waiting: VecDeque<(Option<String>, u32, Step)>,
    /// One entry per pending step: the frames the engine produced after it, sent once it's
    /// acknowledged, so the shell sees the engine's order.
    held: VecDeque<Vec<DataToShell>>,
    /// The engine said `Done` while steps were pending; the run ends with the last one.
    finishing: bool,
    ending: Option<Ending>,
}

/// How an ended run is being written.
#[derive(Debug)]
struct Ending {
    outcome: RunEnd,
    revealed: Option<Vec<u32>>,
    notify: Notify,
    /// A store operation of the run failed, so only `failed(store)` is written.
    failed: bool,
    /// The outcome was sent to the store.
    written: bool,
    /// The scene a Stop kept nothing of, cleared in the outcome's transaction.
    clear: Option<SceneId>,
    /// The Stop's selection was applied to the visible set; it stays for every outcome write.
    trimmed: bool,
}

impl Run {
    /// Steps sent to the store; the ones waiting for the scene's id aren't.
    fn sent(&self) -> u32 {
        self.steps - u32::try_from(self.waiting.len()).expect("at most MAX_STEPS wait")
    }

    fn outstanding(&self) -> bool {
        self.turn.is_some() || self.acked < self.sent()
    }
}

/// The core loop's state.
#[derive(Debug)]
pub struct Coordinator {
    last_thread: ThreadId,
    last_run: RunId,
    current: Option<Run>,
    /// Runs that ended, until the store answers for their outcome.
    ending: Vec<Run>,
    /// The visible set, oldest first, at most four.
    scenes: Vec<SceneState>,
    /// Cleared scenes whose `scene_cleared` isn't committed yet, and whether a write for each
    /// is in the store; a failed one is retried with the next store operation.
    clearing: Vec<(SceneId, bool)>,
    /// Visible scenes whose kept selection no outcome recorded, the same way.
    keeping: Vec<(SceneId, Vec<u32>, bool)>,
    /// The first run the connection being served could start; older runs were drawn by a
    /// shell that's gone, so this one's Stop can't name them.
    session_start: RunId,
    /// The wait before owed writes are tried again, doubling while they keep failing.
    retry_after: Duration,
    /// The wait of the `ArmRetry` that's out and hasn't come back as `Retry`.
    retry_armed: Option<Duration>,
    /// Deletes sent to the store and not answered yet, from any connection.
    deleting: Vec<delete::Deleting>,
    /// Threads hidden or removed on this connection while a submit sent before their delete is
    /// still unread, with how many submits came before it.
    fenced: Vec<(ThreadId, u32)>,
    /// Threads whose delete, sent on an earlier connection, was answered on this one before the
    /// shell's resend of it was read; that resend needs no second op.
    crossed: Vec<ThreadId>,
    /// The turns taken on this connection.
    turns_read: u32,
    /// A delete on this connection was refused for capacity, so its thread wasn't hidden.
    refused_delete: bool,
}

impl Coordinator {
    /// Starts from the store's high-water marks, so ids keep growing across restarts, and from
    /// the visible set the store recovered.
    pub fn new(last_thread: ThreadId, last_run: RunId, scenes: Vec<SceneState>) -> Self {
        Self {
            last_thread,
            last_run,
            current: None,
            ending: Vec::new(),
            scenes,
            clearing: Vec::new(),
            keeping: Vec::new(),
            session_start: RunId(last_run.0 + 1),
            retry_after: scenes::FIRST_RETRY,
            retry_armed: None,
            deleting: Vec::new(),
            fenced: Vec::new(),
            crossed: Vec::new(),
            turns_read: 0,
            refused_delete: false,
        }
    }

    /// The run in flight, if any.
    pub fn current_run(&self) -> Option<RunId> {
        self.current.as_ref().map(|run| run.id)
    }

    /// How many ended runs still wait on the store.
    pub fn ending_runs(&self) -> usize {
        self.ending.len()
    }

    /// Acts on one input.
    ///
    /// # Errors
    /// [`Stale`] when the input names a run that isn't current or answers nothing that is
    /// pending; nothing changed.
    ///
    /// # Panics
    /// Only if a run in the ending list has no ending record, which `end` never allows.
    pub fn handle(&mut self, input: Input) -> Result<Vec<Effect>, Stale> {
        let mut effects = Vec::new();
        match input {
            Input::ReadPage {
                epoch,
                request,
                page,
            } => {
                let op = StoreOp::ReadPage {
                    epoch,
                    request,
                    page,
                };
                return Ok(vec![Effect::Store(op)]);
            }
            Input::PageRead { reply, .. } => return Ok(vec![Effect::Data(reply)]),
            Input::Turn { thread, text } => self.typed(thread, text, &mut effects),
            Input::Stop { run, .. } if run < self.session_start => return Err(Stale),
            Input::Stop { run, revealed } if self.current_run() != Some(run) => {
                self.late_stop(run, revealed, &mut effects)?;
            }
            Input::Stop { run, mut revealed } => {
                let current = self.take(run)?;
                revealed.retain(|&index| index < current.acked);
                revealed.sort_unstable();
                revealed.dedup();
                let stopped = RunEnd::Interrupted(Interruption::Stop);
                self.end(
                    current,
                    stopped,
                    Some(revealed),
                    Notify::Shell,
                    &mut effects,
                );
            }
            Input::Engine { run, event } => self.engine(run, event, &mut effects)?,
            Input::TurnStored { run } => {
                let found = self.find(run)?;
                if found.begin.is_some() {
                    return Err(Stale);
                }
                let text = found.turn.take().ok_or(Stale)?;
                if found.ending.is_some() {
                    self.settle(run, &mut effects);
                } else {
                    effects.push(Effect::StartEngine { run, text });
                }
            }
            Input::StepStored {
                run,
                index,
                step,
                scene,
            } => {
                self.step_stored(run, index, step, scene, &mut effects)?;
            }
            Input::RunStored { run } => self.run_stored(run, &mut effects)?,
            Input::StoreFailed { run } => self.store_failed(run, &mut effects)?,
            Input::ShellLost => {
                for run in &mut self.ending {
                    if let Some(ending) = &mut run.ending {
                        ending.notify = Notify::Nobody;
                    }
                }
                if let Some(current) = self.current.take() {
                    let lost = RunEnd::Interrupted(Interruption::ShellLost);
                    self.end(current, lost, None, Notify::Nobody, &mut effects);
                }
            }
            Input::Stalled { run } => {
                let current = self.take(run)?;
                let stalled = RunEnd::Interrupted(Interruption::ViewStalled);
                self.end(current, stalled, None, Notify::Shell, &mut effects);
            }
            Input::Deadline { run } => {
                // The engine finished within its budget; only its last step is being stored.
                if self.current(run)?.finishing {
                    return Err(Stale);
                }
                let current = self.take(run)?;
                let over = RunEnd::Failed(Failure::Budget);
                self.end(current, over, None, Notify::Shell, &mut effects);
            }
            Input::Clear { scenes } => self.clear(&scenes, &mut effects),
            Input::ScenesCleared { scenes, ok: true } => self.cleared(&scenes),
            Input::ScenesCleared { scenes, ok: false } => self.clear_failed(&scenes),
            Input::ClearVisible => self.clear_visible(&mut effects),
            Input::SceneKept { scene, ok: true } => self.kept(scene),
            Input::SceneKept { scene, ok: false } => self.keep_failed(scene),
            Input::Retry => self.retry_now(&mut effects),
            input @ (Input::Delete { .. } | Input::Deleted { .. }) => {
                self.deletion(&input, &mut effects)?;
            }
        }
        self.begin_turns(&mut effects);
        self.retry_with(&mut effects);
        self.arm_retry(&mut effects);
        Ok(effects)
    }

    /// A run's outcome is stored: it leaves the ending list, and the shell hears Done.
    fn run_stored(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let at = self.ending_at(run)?;
        let ending = self.ending[at]
            .ending
            .as_ref()
            .expect("an ending run has an Ending");
        if !ending.written {
            return Err(Stale);
        }
        let done = self.ending.swap_remove(at);
        if let Some(Ending {
            outcome: RunEnd::Done,
            notify: Notify::Shell,
            failed: false,
            ..
        }) = done.ending
        {
            told(&done, Phase::Done, effects);
        }
        Ok(())
    }

    fn turn(&mut self, thread: Option<ThreadId>, text: String, effects: &mut Vec<Effect>) {
        if let Some(current) = self.current.take() {
            let replaced = RunEnd::Interrupted(Interruption::Replaced);
            self.end(current, replaced, None, Notify::Shell, effects);
        }
        self.last_run = RunId(self.last_run.0 + 1);
        let run = self.last_run;
        let (thread, title) = if let Some(thread) = thread {
            (thread, None)
        } else {
            self.last_thread = ThreadId(self.last_thread.0 + 1);
            (self.last_thread, Some(title(&text)))
        };
        let begin = StoreOp::BeginTurn {
            thread,
            title,
            run,
            text: text.clone(),
        };
        self.current = Some(Run {
            id: run,
            thread,
            turn: Some(text),
            begin: Some(begin),
            answering: false,
            unstored: String::new(),
            text_bytes: 0,
            steps: 0,
            acked: 0,
            scene: None,
            evict: None,
            waiting: VecDeque::new(),
            held: VecDeque::new(),
            finishing: false,
            ending: None,
        });
        self.begin_turns(effects);
        effects.push(Effect::Control(ControlToShell::RunState {
            run,
            thread,
            phase: Phase::Thinking,
        }));
        effects.push(Effect::ArmDeadline {
            run,
            after: WALL_CLOCK,
        });
    }

    /// Queues held turns whose older runs all have their outcomes queued, so the store keeps a
    /// thread's events in run order and a replacement's engine starts only after its predecessor
    /// is stored.
    fn begin_turns(&mut self, effects: &mut Vec<Effect>) {
        let oldest_unwritten = self
            .ending
            .iter()
            .filter(|ended| ended.ending.as_ref().is_some_and(|ending| !ending.written))
            .map(|ended| ended.id)
            .min();
        for run in self.ending.iter_mut().chain(self.current.as_mut()) {
            if oldest_unwritten.is_none_or(|oldest| run.id <= oldest)
                && let Some(begin) = run.begin.take()
            {
                effects.push(Effect::Store(begin));
            }
        }
    }

    fn engine(
        &mut self,
        run: RunId,
        event: EngineEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let current = self.current(run)?;
        // The engine hasn't started before its turn is stored, and has nothing more after Done.
        if current.turn.is_some() || current.finishing {
            return Err(Stale);
        }
        match event {
            EngineEvent::Text(delta) => {
                if current.text_bytes + delta.len() > MAX_TEXT {
                    let over = self.take(run)?;
                    self.end(
                        over,
                        RunEnd::Failed(Failure::Budget),
                        None,
                        Notify::Shell,
                        effects,
                    );
                    return Ok(());
                }
                current.text_bytes += delta.len();
                current.unstored.push_str(&delta);
                answering(current, effects);
                let mut rest = delta.as_str();
                while !rest.is_empty() {
                    let mut cut = rest.len().min(MAX_DELTA);
                    while !rest.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    let (piece, tail) = rest.split_at(cut);
                    let frame = || DataToShell::AssistantText {
                        run,
                        delta: piece.to_owned(),
                    };
                    // Held text joins the frame before it, so small deltas don't each cost an entry.
                    match current.held.back_mut() {
                        Some(behind) => match behind.last_mut() {
                            Some(DataToShell::AssistantText { delta, .. })
                                if delta.len() + piece.len() <= MAX_DELTA =>
                            {
                                delta.push_str(piece);
                            }
                            _ => behind.push(frame()),
                        },
                        None => effects.push(Effect::Data(frame())),
                    }
                    rest = tail;
                }
            }
            EngineEvent::Step(step) => self.step(run, step, effects)?,
            EngineEvent::Done if current.acked < current.steps => current.finishing = true,
            EngineEvent::Done => {
                let done = self.take(run)?;
                self.end(done, RunEnd::Done, None, Notify::Shell, effects);
            }
            EngineEvent::Failed => {
                let failed = self.take(run)?;
                self.end(
                    failed,
                    RunEnd::Failed(Failure::Engine),
                    None,
                    Notify::Shell,
                    effects,
                );
            }
        }
        Ok(())
    }

    /// An engine step: committed in order, or failing the run when it's over a limit. A run's
    /// first step begins its scene; the steps after it wait for the scene's id.
    fn step(&mut self, run: RunId, step: Step, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let current = self.current(run)?;
        let failure = if step.label.len() > MAX_STEP_LABEL {
            Some(Failure::Engine)
        } else if usize::try_from(current.steps).is_ok_and(|steps| steps >= MAX_STEPS) {
            Some(Failure::Budget)
        } else {
            None
        };
        if let Some(failure) = failure {
            let failed = self.take(run)?;
            self.end(
                failed,
                RunEnd::Failed(failure),
                None,
                Notify::Shell,
                effects,
            );
            return Ok(());
        }
        answering(current, effects);
        let evict = if current.steps == 0 {
            self.evict()
        } else {
            None
        };
        let current = self.current(run)?;
        let (text, index) = (unstored(current), current.steps);
        current.steps += 1;
        current.held.push_back(Vec::new());
        let scene = match current.scene {
            Some(scene) => SceneTarget::Existing(scene),
            None if index == 0 => {
                current.evict = evict;
                SceneTarget::New { evict }
            }
            // The scene's id comes with the first step's ack; the core loop never waits.
            None => {
                current.waiting.push_back((text, index, step));
                return Ok(());
            }
        };
        effects.push(Effect::Store(StoreOp::CommitStep {
            thread: current.thread,
            run,
            text,
            scene,
            index,
            step,
        }));
        Ok(())
    }

    /// The store acknowledges steps in order, so anything but the next pending index is stale.
    /// The first ack gives the run its scene and sends the steps that waited for it.
    fn step_stored(
        &mut self,
        run: RunId,
        index: u32,
        step: Step,
        scene: SceneId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let found = self.find(run)?;
        if found.turn.is_some() || index != found.acked || index >= found.sent() {
            return Err(Stale);
        }
        found.acked += 1;
        let first = found.scene.is_none();
        let scene = *found.scene.get_or_insert(scene);
        let (thread, evict) = (found.thread, found.evict.take());
        // Late acks after a Stop count in the revision but never come back into the kept set.
        let stopped = found
            .ending
            .as_ref()
            .is_some_and(|ending| ending.outcome == RunEnd::Interrupted(Interruption::Stop));
        let ending = found.ending.is_some();
        let waiting = std::mem::take(&mut found.waiting);
        if first {
            self.begin_scene(scene, (thread, run), evict);
        }
        self.scene_stored(scene, (!stopped).then_some(&step));
        if ending {
            self.settle(run, effects);
            return Ok(());
        }
        effects.push(Effect::Data(DataToShell::StepCommit {
            run,
            scene,
            index,
            step,
        }));
        let found = self.find(run)?;
        effects.extend(
            found
                .held
                .pop_front()
                .into_iter()
                .flatten()
                .map(Effect::Data),
        );
        for (text, index, step) in waiting {
            effects.push(Effect::Store(StoreOp::CommitStep {
                thread,
                run,
                text,
                scene: SceneTarget::Existing(scene),
                index,
                step,
            }));
        }
        if found.finishing && found.acked == found.steps {
            let done = self.take(run)?;
            self.end(done, RunEnd::Done, None, Notify::Shell, effects);
        }
        Ok(())
    }

    /// The oldest unanswered operation of `run` failed: the turn, a step, or the outcome.
    fn store_failed(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        if self.find(run)?.begin.is_some() {
            return Err(Stale);
        }
        if let Ok(current) = self.current(run) {
            if !current.outstanding() {
                return Err(Stale);
            }
            let current = self.take(run)?;
            let failed = RunEnd::Failed(Failure::Store);
            self.end(current, failed, None, Notify::Shell, effects);
        }
        let at = self.ending_at(run)?;
        let ended = &mut self.ending[at];
        let (id, thread) = (ended.id, ended.thread);
        let ending = ended.ending.as_mut().expect("an ending run has an Ending");
        // A failed outcome was already told; any other ending now turns out failed.
        if ending.notify == Notify::Shell
            && !ending.failed
            && !matches!(ending.outcome, RunEnd::Failed(_))
        {
            effects.push(Effect::Control(ControlToShell::RunState {
                run: id,
                thread,
                phase: Phase::Failed,
            }));
            // Every ending but Done was already told, with its data's end.
            if ending.outcome == RunEnd::Done {
                effects.push(Effect::Data(DataToShell::RunEnd { run: id }));
            }
        }
        if ended.turn.is_some() || ending.written && ending.failed {
            // The turn never stored, so there's no row, or the failed(store) write failed too;
            // a run left active is marked failed(crash) at the next start. Its scene's clear or
            // kept selection is still owed, so it's retried on its own.
            let kept = ended
                .scene
                .zip(ending.revealed.take().filter(|_| ending.trimmed));
            let clear = ending.clear;
            self.ending.swap_remove(at);
            self.owe(clear, kept);
            return Ok(());
        }
        if ending.written {
            ending.written = false;
        } else {
            ended.acked += 1;
        }
        ending.failed = true;
        self.settle(run, effects);
        Ok(())
    }

    /// Ends a run: the engine is cancelled first, so it produces nothing more, and data it
    /// still held is dropped. Every ending but `Done` tells the shell at once; `Done` waits
    /// until the store has it. The outcome is written once the run's earlier writes are answered.
    fn end(
        &mut self,
        mut run: Run,
        outcome: RunEnd,
        revealed: Option<Vec<u32>>,
        notify: Notify,
        effects: &mut Vec<Effect>,
    ) {
        if run.turn.is_none() && outcome != RunEnd::Done {
            effects.push(Effect::CancelEngine { run: run.id });
        }
        // Steps waiting for the scene's id were never sent, so no ack is owed for them.
        run.steps = run.sent();
        run.waiting.clear();
        if matches!(
            outcome,
            RunEnd::Interrupted(
                Interruption::Stop | Interruption::Replaced | Interruption::Deleted
            )
        ) {
            effects.push(Effect::Revoke { run: run.id });
        }
        run.held.clear();
        if notify == Notify::Shell && outcome != RunEnd::Done {
            let phase = match outcome {
                RunEnd::Interrupted(_) => Phase::Interrupted,
                RunEnd::Done | RunEnd::Failed(_) => Phase::Failed,
            };
            told(&run, phase, effects);
        }
        run.ending = Some(Ending {
            outcome,
            revealed,
            notify,
            failed: outcome == RunEnd::Failed(Failure::Store),
            written: false,
            clear: None,
            trimmed: false,
        });
        let id = run.id;
        self.ending.push(run);
        self.settle(id, effects);
    }

    /// Writes an ended run's outcome once nothing earlier of it is pending. A Stop's revealed
    /// set becomes its scene's kept set here, once every step write has settled; a scene left
    /// with nothing kept is cleared in the same transaction.
    fn settle(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        let Some(ended) = self.ending.iter_mut().find(|ended| ended.id == run) else {
            return;
        };
        if ended.outstanding() {
            return;
        }
        let (thread, text, scene) = (ended.thread, unstored(ended), ended.scene);
        let Some(ending) = ended.ending.as_mut().filter(|ending| !ending.written) else {
            return;
        };
        ending.written = true;
        if let (Some(revealed), Some(scene)) = (&ending.revealed, scene)
            && !ending.trimmed
        {
            ending.trimmed = true;
            ending.clear = scenes::keep(&mut self.scenes, scene, revealed);
        }
        let op = if ending.failed {
            StoreOp::EndRun {
                thread,
                run,
                text: None,
                revealed: ending.revealed.clone(),
                clear: ending.clear,
                end: RunEnd::Failed(Failure::Store),
            }
        } else {
            StoreOp::EndRun {
                thread,
                run,
                text,
                revealed: ending.revealed.clone(),
                clear: ending.clear,
                end: ending.outcome,
            }
        };
        effects.push(Effect::Store(op));
    }

    fn current(&mut self, run: RunId) -> Result<&mut Run, Stale> {
        self.current
            .as_mut()
            .filter(|current| current.id == run)
            .ok_or(Stale)
    }

    fn take(&mut self, run: RunId) -> Result<Run, Stale> {
        self.current(run)?;
        self.current.take().ok_or(Stale)
    }

    fn ending_at(&self, run: RunId) -> Result<usize, Stale> {
        self.ending
            .iter()
            .position(|ended| ended.id == run)
            .ok_or(Stale)
    }

    /// The current run or one still ending.
    fn find(&mut self, run: RunId) -> Result<&mut Run, Stale> {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.id == run)
        {
            return self.current(run);
        }
        let at = self.ending_at(run)?;
        Ok(&mut self.ending[at])
    }
}

fn run_state(run: &Run, phase: Phase) -> Effect {
    Effect::Control(ControlToShell::RunState {
        run: run.id,
        thread: run.thread,
        phase,
    })
}

/// Tells the shell a run ended: its terminal phase on the control pipe, and the end of its data on
/// the data pipe, after every frame it was sent.
fn told(run: &Run, phase: Phase, effects: &mut Vec<Effect>) {
    effects.push(run_state(run, phase));
    effects.push(Effect::Data(DataToShell::RunEnd { run: run.id }));
}

/// The text streamed since the last write, taken for the next one.
fn unstored(run: &mut Run) -> Option<String> {
    (!run.unstored.is_empty()).then(|| std::mem::take(&mut run.unstored))
}

fn answering(run: &mut Run, effects: &mut Vec<Effect>) {
    if !run.answering {
        run.answering = true;
        effects.push(run_state(run, Phase::Answering));
    }
}

/// A new thread's title: the first line of its first turn, cut to [`TITLE_CHARS`].
fn title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(TITLE_CHARS)
        .collect()
}
