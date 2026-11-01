//! The coordinator: the brain's core loop, sans I/O (architecture v1 §5.1).
//!
//! It takes inputs (a typed turn, engine events, `Stop`, store acknowledgements, shell loss,
//! stalls, deadlines) and returns effects (store operations, frames for the shell, engine start
//! and cancel). It owns run ids, the one current run and its budgets, and the runs still ending,
//! and nothing else; the systems around it do the I/O and report back. An engine input or a
//! timer that names a run other than the current one is [`Stale`] and changes nothing, which is
//! how a late completion after `Stop` is refused. A run that ended stays accountable to the
//! store until its outcome is written, so a failed write is still reported.

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::time::Duration;

use capsula_protocol::message::{
    ControlToShell, DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, Step, ThreadId,
};

use crate::store::{Failure, Interruption, RunEnd};

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
    /// Write to the store. The store runs operations in order, each in one transaction, and
    /// answers every one: `TurnStored`, `StepStored` or `RunStored`, or `StoreFailed`.
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
    /// Report `Deadline` for `run` after `after`.
    ArmDeadline {
        /// The run.
        run: RunId,
        /// How long from now.
        after: Duration,
    },
}

/// A store write, one transaction each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOp {
    /// The turn, its run, and the thread when `title` names a new one.
    BeginTurn {
        /// The thread.
        thread: ThreadId,
        /// The new thread's title, or `None` for an existing thread.
        title: Option<String>,
        /// The run.
        run: RunId,
        /// The user's text.
        text: String,
    },
    /// The text streamed since the last write, then one step.
    CommitStep {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed before the step, if any.
        text: Option<String>,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
    },
    /// The text streamed since the last write, the revealed set, then the run's outcome.
    EndRun {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed since the last step, if any.
        text: Option<String>,
        /// The steps that stayed on screen, when the user stopped the run.
        revealed: Option<Vec<u32>>,
        /// How it ended.
        end: RunEnd,
    },
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

/// Data for the shell, held in the order the engine produced it.
#[derive(Debug)]
enum Held {
    /// A step the store hasn't acknowledged; it's sent from the acknowledgement.
    Step,
    /// A frame produced after a pending step.
    Data(DataToShell),
}

/// The run in flight, or one that ended and is waiting on the store.
#[derive(Debug)]
struct Run {
    id: RunId,
    thread: ThreadId,
    /// The user's text, until the turn is stored; `None` once it is.
    turn: Option<String>,
    answering: bool,
    /// Streamed text not yet stored; it's stored with the next step or the outcome.
    unstored: String,
    text_bytes: usize,
    /// Steps sent to the store, and how many of them it acknowledged, in order.
    steps: u32,
    acked: u32,
    /// Data waiting behind the oldest pending step.
    held: VecDeque<Held>,
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
}

impl Run {
    fn outstanding(&self) -> bool {
        self.turn.is_some() || self.acked < self.steps
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
}

impl Coordinator {
    /// Starts from the store's high-water marks, so ids keep growing across restarts.
    pub fn new(last_thread: ThreadId, last_run: RunId) -> Self {
        Self {
            last_thread,
            last_run,
            current: None,
            ending: Vec::new(),
        }
    }

    /// Acts on one input.
    ///
    /// # Errors
    /// [`Stale`] when the input names a run that isn't current or answers nothing that is
    /// pending; nothing changed.
    pub fn handle(&mut self, input: Input) -> Result<Vec<Effect>, Stale> {
        let mut effects = Vec::new();
        match input {
            Input::Turn { thread, text } => {
                if !text.trim().is_empty() {
                    self.turn(thread, text, &mut effects);
                }
            }
            Input::Stop { run, revealed } => {
                let current = self.take(run)?;
                let mut revealed: Vec<u32> = revealed
                    .into_iter()
                    .filter(|&index| index < current.steps)
                    .collect();
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
                let text = found.turn.take().ok_or(Stale)?;
                if found.ending.is_some() {
                    self.settle(run, &mut effects);
                } else {
                    effects.push(Effect::StartEngine { run, text });
                }
            }
            Input::StepStored { run, index, step } => {
                self.step_stored(run, index, step, &mut effects)?;
            }
            Input::RunStored { run } => {
                let at = self.ending_at(run)?;
                let ending = self.ending[at].ending.as_ref().ok_or(Stale)?;
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
                    effects.push(run_state(&done, Phase::Done));
                }
            }
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
                let current = self.take(run)?;
                let over = RunEnd::Failed(Failure::Budget);
                self.end(current, over, None, Notify::Shell, &mut effects);
            }
        }
        Ok(effects)
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
        effects.push(Effect::Store(StoreOp::BeginTurn {
            thread,
            title,
            run,
            text: text.clone(),
        }));
        effects.push(Effect::Control(ControlToShell::RunState {
            run,
            thread,
            phase: Phase::Thinking,
        }));
        effects.push(Effect::ArmDeadline {
            run,
            after: WALL_CLOCK,
        });
        self.current = Some(Run {
            id: run,
            thread,
            turn: Some(text),
            answering: false,
            unstored: String::new(),
            text_bytes: 0,
            steps: 0,
            acked: 0,
            held: VecDeque::new(),
            finishing: false,
            ending: None,
        });
    }

    fn engine(
        &mut self,
        run: RunId,
        event: EngineEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let current = self.current(run)?;
        // An engine that said Done has nothing more to say.
        if current.finishing {
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
                    let frame = DataToShell::AssistantText {
                        run,
                        delta: piece.to_owned(),
                    };
                    // Text after a pending step waits for it, so the shell sees the engine's order.
                    if current.held.is_empty() {
                        effects.push(Effect::Data(frame));
                    } else {
                        current.held.push_back(Held::Data(frame));
                    }
                    rest = tail;
                }
            }
            EngineEvent::Step(step) => {
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
                effects.push(Effect::Store(StoreOp::CommitStep {
                    thread: current.thread,
                    run,
                    text: unstored(current),
                    index: current.steps,
                    step,
                }));
                current.steps += 1;
                current.held.push_back(Held::Step);
            }
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

    /// The store acknowledges steps in order, so anything but the next pending index is stale.
    fn step_stored(
        &mut self,
        run: RunId,
        index: u32,
        step: Step,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let found = self.find(run)?;
        if found.turn.is_some() || index != found.acked || index >= found.steps {
            return Err(Stale);
        }
        found.acked += 1;
        if found.ending.is_some() {
            self.settle(run, effects);
            return Ok(());
        }
        found.held.pop_front();
        effects.push(Effect::Data(DataToShell::StepCommit { run, index, step }));
        while let Some(Held::Data(_)) = found.held.front() {
            if let Some(Held::Data(frame)) = found.held.pop_front() {
                effects.push(Effect::Data(frame));
            }
        }
        if found.finishing && found.acked == found.steps {
            let done = self.take(run)?;
            self.end(done, RunEnd::Done, None, Notify::Shell, effects);
        }
        Ok(())
    }

    /// The oldest unanswered operation of `run` failed: the turn, a step, or the outcome.
    fn store_failed(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        if let Ok(current) = self.current(run) {
            if !current.outstanding() {
                return Err(Stale);
            }
            let current = self.take(run)?;
            let failed = RunEnd::Failed(Failure::Store);
            self.end(current, failed, None, Notify::Shell, effects);
        } else {
            let ended = &self.ending[self.ending_at(run)?];
            let ending = ended.ending.as_ref().ok_or(Stale)?;
            // A failed outcome was already told; any other ending now turns out failed.
            if ending.notify == Notify::Shell
                && !ending.failed
                && !matches!(ending.outcome, RunEnd::Failed(_))
            {
                effects.push(run_state(ended, Phase::Failed));
            }
        }
        let at = self.ending_at(run)?;
        let ended = &mut self.ending[at];
        let ending = ended.ending.as_mut().ok_or(Stale)?;
        let fallback_failed = ending.written && ending.failed;
        if !ending.written && ended.turn.is_some() || fallback_failed {
            // The turn never stored, so there's no row, or the failed(store) write failed too;
            // a run left active is marked failed(crash) at the next start.
            self.ending.swap_remove(at);
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
        run.held.clear();
        if notify == Notify::Shell && outcome != RunEnd::Done {
            let phase = match outcome {
                RunEnd::Interrupted(_) => Phase::Interrupted,
                RunEnd::Done | RunEnd::Failed(_) => Phase::Failed,
            };
            effects.push(run_state(&run, phase));
        }
        run.ending = Some(Ending {
            outcome,
            revealed,
            notify,
            failed: outcome == RunEnd::Failed(Failure::Store),
            written: false,
        });
        let id = run.id;
        self.ending.push(run);
        self.settle(id, effects);
    }

    /// Writes an ended run's outcome once nothing earlier of it is pending.
    fn settle(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        let Some(ended) = self.ending.iter_mut().find(|ended| ended.id == run) else {
            return;
        };
        if ended.outstanding() {
            return;
        }
        let (thread, text) = (ended.thread, unstored(ended));
        let Some(ending) = ended.ending.as_mut().filter(|ending| !ending.written) else {
            return;
        };
        ending.written = true;
        let op = if ending.failed {
            StoreOp::EndRun {
                thread,
                run,
                text: None,
                revealed: None,
                end: RunEnd::Failed(Failure::Store),
            }
        } else {
            StoreOp::EndRun {
                thread,
                run,
                text,
                revealed: ending.revealed.take(),
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
