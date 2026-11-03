//! The coordinator: the brain's core loop, sans I/O (architecture v1 §5.1).
//!
//! It takes inputs (a typed turn, engine events, `Stop`, store acknowledgements, shell loss,
//! stalls, deadlines) and returns effects (store operations, frames for the shell, engine start
//! and cancel). It owns run ids, the one current run and its budgets, and the runs still ending,
//! and nothing else; the systems around it do the I/O and report back. An engine input or a
//! timer that names a run other than the current one is [`Stale`] and changes nothing, which is
//! how a late completion after `Stop` is refused. A run that ended stays accountable to the
//! store until its outcome is written, so a failed write is still reported. The streaming half,
//! which turns an engine round's events into text frames, steps and the run's end, is in `run`.

mod delete;
mod run;
mod scenes;
mod store_op;
#[cfg(test)]
mod tests;
mod thread_op;

use std::collections::VecDeque;
use std::time::Duration;

use kumochi_protocol::message::{
    ControlToShell, DataToShell, Deleted, PageOf, Phase, RunId, SceneId, Step, ThreadId, ThreadOp,
};

use crate::engines::{RunRequest, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd, SceneState};

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
        event: TurnEvent,
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
    /// The shell renamed a task or set a thread's model.
    ThreadOp(ThreadOp),
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
    /// Start the engine's round for a stored turn.
    StartEngine {
        /// What the engine answers.
        request: RunRequest,
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
    /// What the user typed, kept for the run's life.
    turn: String,
    turn_stored: bool,
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
    /// The draw call the engine's steps stream in, once it starts.
    draw: Option<run::DrawArgs>,
    /// The engine's round completed while steps were pending; the run ends with the last one.
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
        !self.turn_stored || self.acked < self.sent()
    }
}

/// The core loop's state.
#[derive(Debug)]
pub struct Coordinator {
    /// Never deleted or renamed.
    main_thread: ThreadId,
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
    /// Starts from the store's main thread and high-water marks, so ids keep growing across
    /// restarts, and from the visible set the store recovered.
    pub fn new(
        main_thread: ThreadId,
        last_thread: ThreadId,
        last_run: RunId,
        scenes: Vec<SceneState>,
    ) -> Self {
        Self {
            main_thread,
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
            Input::TurnStored { run } => self.turn_stored(run, &mut effects)?,
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
            Input::ThreadOp(op) => self.thread_op(op, &mut effects),
        }
        self.begin_turns(&mut effects);
        self.retry_with(&mut effects);
        self.arm_retry(&mut effects);
        Ok(effects)
    }

    /// A run's turn is stored: its engine starts, unless it already ended. The run keeps its
    /// request; the engine gets a copy.
    fn turn_stored(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let found = self.find(run)?;
        if found.begin.is_some() || found.turn_stored {
            return Err(Stale);
        }
        found.turn_stored = true;
        if found.ending.is_some() {
            self.settle(run, effects);
            return Ok(());
        }
        let request = RunRequest {
            run,
            thread: found.thread,
            turn: found.turn.clone(),
        };
        effects.push(Effect::StartEngine { request });
        Ok(())
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
            turn: text,
            turn_stored: false,
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
            draw: None,
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
        if !ended.turn_stored || ending.written && ending.failed {
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
        if run.turn_stored && outcome != RunEnd::Done {
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
