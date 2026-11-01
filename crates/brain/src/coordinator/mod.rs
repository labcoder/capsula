//! The coordinator: the brain's core loop, sans I/O (architecture v1 §5.1).
//!
//! It takes inputs (a typed turn, engine events, `Stop`, store acknowledgements, shell loss,
//! stalls, deadlines) and returns effects (store operations, frames for the shell, engine start
//! and cancel). It owns run ids, the one current run and its budgets, and nothing else; the
//! systems around it do the I/O and report back. Every input that names a run other than the
//! current one is [`Stale`] and changes nothing, which is how a late completion after `Stop`
//! is refused.

#[cfg(test)]
mod tests;

use std::time::Duration;

use capsula_protocol::message::{
    ControlToShell, DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, Step, ThreadId,
};

use crate::store::{EventKind, Failure, Interruption, RunEnd};

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
    /// A store operation for `run` failed.
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
    /// Write to the store; acknowledged with `TurnStored`, `StepStored` or `StoreFailed`.
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

/// A store write.
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
    /// An event of the run.
    Append {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// What it records.
        kind: EventKind,
        /// Its searchable text.
        text: Option<String>,
        /// Its JSON body.
        body: Option<String>,
    },
    /// One step, acknowledged with `StepStored`.
    CommitStep {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
    },
    /// The run's outcome.
    EndRun {
        /// The run.
        run: RunId,
        /// How it ended.
        end: RunEnd,
    },
}

/// The input named a run that isn't current, so it changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

/// Whether ending a run tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notify {
    Shell,
    Nobody,
}

/// The run in flight.
#[derive(Debug)]
struct Run {
    id: RunId,
    thread: ThreadId,
    /// The user's text, until the turn is stored and the engine starts with it; `None` once
    /// the engine has started.
    turn: Option<String>,
    /// Set when the store refused the turn, so nothing more is written for it.
    store_failed: bool,
    answering: bool,
    /// Streamed text not yet stored; it's stored before each step and at the end.
    unstored: String,
    text_bytes: usize,
    steps: usize,
    /// Steps sent to the store and not yet acknowledged.
    pending: usize,
    /// The engine said `Done` while steps were pending; the run ends with the last one.
    finishing: bool,
}

/// The core loop's state.
#[derive(Debug)]
pub struct Coordinator {
    last_thread: ThreadId,
    last_run: RunId,
    current: Option<Run>,
}

impl Coordinator {
    /// Starts from the store's high-water marks, so ids keep growing across restarts.
    pub fn new(last_thread: ThreadId, last_run: RunId) -> Self {
        Self {
            last_thread,
            last_run,
            current: None,
        }
    }

    /// Acts on one input.
    ///
    /// # Errors
    /// [`Stale`] when the input names a run that isn't current; nothing changed.
    pub fn handle(&mut self, input: Input) -> Result<Vec<Effect>, Stale> {
        let mut effects = Vec::new();
        match input {
            Input::Turn { thread, text } => self.turn(thread, text, &mut effects),
            Input::Stop { run, revealed } => {
                let current = self.take(run)?;
                let mut revealed: Vec<u32> = revealed
                    .into_iter()
                    .filter(|&index| usize::try_from(index).is_ok_and(|i| i < current.steps))
                    .collect();
                revealed.sort_unstable();
                revealed.dedup();
                end(
                    current,
                    RunEnd::Interrupted(Interruption::Stop),
                    Some(&revealed),
                    Notify::Shell,
                    &mut effects,
                );
            }
            Input::Engine { run, event } => self.engine(run, event, &mut effects)?,
            Input::TurnStored { run } => {
                let current = self.current(run)?;
                let text = current.turn.take().ok_or(Stale)?;
                effects.push(Effect::StartEngine { run, text });
            }
            Input::StepStored { run, index, step } => {
                let current = self.current(run)?;
                current.pending = current.pending.saturating_sub(1);
                let finished = current.finishing && current.pending == 0;
                effects.push(Effect::Data(DataToShell::StepCommit { run, index, step }));
                if finished {
                    let done = self.take(run)?;
                    end(done, RunEnd::Done, None, Notify::Shell, &mut effects);
                }
            }
            Input::StoreFailed { run } => {
                let mut current = self.take(run)?;
                // Only a turn that never committed has no run row to end.
                current.store_failed = current.turn.is_some();
                end(
                    current,
                    RunEnd::Failed(Failure::Store),
                    None,
                    Notify::Shell,
                    &mut effects,
                );
            }
            Input::ShellLost => {
                if let Some(current) = self.current.take() {
                    let lost = RunEnd::Interrupted(Interruption::ShellLost);
                    end(current, lost, None, Notify::Nobody, &mut effects);
                }
            }
            Input::Stalled { run } => {
                let current = self.take(run)?;
                let stalled = RunEnd::Interrupted(Interruption::ViewStalled);
                end(current, stalled, None, Notify::Shell, &mut effects);
            }
            Input::Deadline { run } => {
                let current = self.take(run)?;
                end(
                    current,
                    RunEnd::Failed(Failure::Budget),
                    None,
                    Notify::Shell,
                    &mut effects,
                );
            }
        }
        Ok(effects)
    }

    fn turn(&mut self, thread: Option<ThreadId>, text: String, effects: &mut Vec<Effect>) {
        if let Some(current) = self.current.take() {
            let replaced = RunEnd::Interrupted(Interruption::Replaced);
            end(current, replaced, None, Notify::Shell, effects);
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
            store_failed: false,
            answering: false,
            unstored: String::new(),
            text_bytes: 0,
            steps: 0,
            pending: 0,
            finishing: false,
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
                    end(
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
                    effects.push(Effect::Data(DataToShell::AssistantText {
                        run,
                        delta: piece.to_owned(),
                    }));
                    rest = tail;
                }
            }
            EngineEvent::Step(step) => {
                let failure = if step.label.len() > MAX_STEP_LABEL {
                    Some(Failure::Engine)
                } else if current.steps >= MAX_STEPS {
                    Some(Failure::Budget)
                } else {
                    None
                };
                if let Some(failure) = failure {
                    let failed = self.take(run)?;
                    end(
                        failed,
                        RunEnd::Failed(failure),
                        None,
                        Notify::Shell,
                        effects,
                    );
                    return Ok(());
                }
                store_text(current, effects);
                answering(current, effects);
                effects.push(Effect::Store(StoreOp::CommitStep {
                    thread: current.thread,
                    run,
                    index: u32::try_from(current.steps).expect("steps stay under MAX_STEPS"),
                    step,
                }));
                current.steps += 1;
                current.pending += 1;
            }
            EngineEvent::Done if current.pending > 0 => current.finishing = true,
            EngineEvent::Done => {
                let done = self.take(run)?;
                end(done, RunEnd::Done, None, Notify::Shell, effects);
            }
            EngineEvent::Failed => {
                let failed = self.take(run)?;
                end(
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
}

/// Ends a run: the engine is cancelled first, so it produces nothing more, then the text it
/// streamed and the revealed set are stored, then the outcome, then the shell hears of it.
fn end(
    run: Run,
    outcome: RunEnd,
    revealed: Option<&[u32]>,
    notify: Notify,
    effects: &mut Vec<Effect>,
) {
    let mut run = run;
    if run.turn.is_none() && outcome != RunEnd::Done {
        effects.push(Effect::CancelEngine { run: run.id });
    }
    // The store runs its operations in order, so this lands after the turn even when the turn
    // isn't acknowledged yet.
    if !run.store_failed {
        store_text(&mut run, effects);
        if let Some(revealed) = revealed {
            let list: Vec<String> = revealed.iter().map(u32::to_string).collect();
            effects.push(Effect::Store(StoreOp::Append {
                thread: run.thread,
                run: run.id,
                kind: EventKind::Revealed,
                text: None,
                body: Some(format!("[{}]", list.join(","))),
            }));
        }
        effects.push(Effect::Store(StoreOp::EndRun {
            run: run.id,
            end: outcome,
        }));
    }
    if notify == Notify::Shell {
        let phase = match outcome {
            RunEnd::Done => Phase::Done,
            RunEnd::Interrupted(_) => Phase::Interrupted,
            RunEnd::Failed(_) => Phase::Failed,
        };
        effects.push(Effect::Control(ControlToShell::RunState {
            run: run.id,
            thread: run.thread,
            phase,
        }));
    }
}

/// Stores the text streamed since the last store, as one event.
fn store_text(run: &mut Run, effects: &mut Vec<Effect>) {
    if run.unstored.is_empty() {
        return;
    }
    effects.push(Effect::Store(StoreOp::Append {
        thread: run.thread,
        run: run.id,
        kind: EventKind::AssistantText,
        text: Some(std::mem::take(&mut run.unstored)),
        body: None,
    }));
}

fn answering(run: &mut Run, effects: &mut Vec<Effect>) {
    if !run.answering {
        run.answering = true;
        effects.push(Effect::Control(ControlToShell::RunState {
            run: run.id,
            thread: run.thread,
            phase: Phase::Answering,
        }));
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
