//! One link's output owner: each session's output and every output generation's tail debt
//! (docs/development/speaker.md).
//! Each section under its lock is short and never blocks: opens, joins, pipe aborts and last
//! drops of a speaker happen after it.

use std::fmt;
use std::sync::atomic::{Ordering, fence};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_channel::{Receiver, Sender};
use kumochi_protocol::message::{OutputGeneration, RunId};

use crate::speaker::{Command, FAULT, RELEASED, Speaker, Tail};

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;

/// Output generations tracked until their tails settle; `LIVE` keeps one speaker thread at a
/// time, so this is a ceiling, not the safety argument.
const LEDGER: usize = 4;

/// Aborts one session's pipes.
pub type Abort = Arc<dyn Fn() + Send + Sync>;

/// The owner of a link's outputs.
pub struct OutputOwner {
    state: Mutex<State>,
    changed: Sender<()>,
    changes: Receiver<()>,
}

impl Default for OutputOwner {
    fn default() -> Self {
        let (sender, changes) = async_channel::bounded(1);
        Self {
            state: Mutex::default(),
            changed: sender,
            changes,
        }
    }
}

impl fmt::Debug for OutputOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputOwner").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    session: Option<u64>,
    output: Output,
    commands: Option<SyncSender<Command>>,
    abort: Option<Abort>,
    ledger: [Option<Entry>; LEDGER],
    off: bool,
    generations: u32,
    epochs: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Output {
    #[default]
    Closed,
    Quiet {
        epoch: u64,
        why: Why,
    },
    Opening(OutputGeneration),
    Live(OutputGeneration),
    Lost(OutputGeneration),
}

/// Why a session has no output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// No opener: every normal launch.
    NoOpener,
    /// Talk-back is off.
    Off,
    /// The open was refused, or no generation or ledger slot was left.
    Refused,
    /// Its output was withdrawn or failed, and its tail settled clean.
    Retired,
}

struct Entry {
    generation: OutputGeneration,
    tail: Arc<Tail>,
    terminal: bool,
}

/// An output the owner named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKey {
    /// A live output generation.
    Generation(OutputGeneration),
    /// A session's no-output epoch.
    NoOutput(u64),
}

/// What the UI may show about outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputView {
    /// The current output's key.
    pub key: Option<OutputKey>,
    /// Why the session has no output.
    pub why: Option<Why>,
    /// An earlier generation is still settling.
    pub retiring: bool,
    /// A tail couldn't be shown played.
    pub terminal: bool,
}

enum Verdict {
    Pending,
    Clean,
    Terminal,
}

/// A generation's verdict from its own tail. Only the ledger holding it means no worker, engine
/// or thread can write it again.
fn verdict(tail: &Arc<Tail>) -> Verdict {
    let gone = Arc::strong_count(tail) == 1;
    fence(Ordering::Acquire);
    let Some(now) = tail.read() else {
        return if gone {
            Verdict::Terminal
        } else {
            Verdict::Pending
        };
    };
    let owed = now.owed();
    if gone || now.flags & RELEASED != 0 {
        if owed {
            Verdict::Terminal
        } else {
            Verdict::Clean
        }
    } else if now.flags & FAULT != 0 && owed {
        Verdict::Terminal
    } else {
        Verdict::Pending
    }
}

impl State {
    fn settle(&mut self) {
        let Self {
            ledger,
            output,
            epochs,
            ..
        } = self;
        for slot in ledger.iter_mut() {
            let Some(entry) = slot.as_mut().filter(|entry| !entry.terminal) else {
                continue;
            };
            let live = *output == Output::Live(entry.generation);
            match verdict(&entry.tail) {
                Verdict::Pending => {}
                Verdict::Clean => {
                    *slot = None;
                    if live {
                        *epochs += 1;
                        *output = Output::Quiet {
                            epoch: *epochs,
                            why: Why::Retired,
                        };
                    }
                }
                Verdict::Terminal => {
                    entry.terminal = true;
                    if live {
                        *output = Output::Lost(entry.generation);
                    }
                }
            }
        }
    }

    fn quiet(&mut self, why: Why) {
        self.epochs += 1;
        self.output = Output::Quiet {
            epoch: self.epochs,
            why,
        };
    }

    /// Queues a command on the speaker. A failed enqueue drops the output and hands back the
    /// session's abort.
    fn command(&mut self, command: Command) -> Option<Abort> {
        if self.commands.as_ref()?.try_send(command).is_ok() {
            return None;
        }
        self.commands = None;
        self.output = match self.output {
            Output::Opening(generation) | Output::Live(generation) | Output::Lost(generation) => {
                Output::Lost(generation)
            }
            _ => Output::Closed,
        };
        self.abort.clone()
    }

    fn live(&self) -> Option<OutputGeneration> {
        match self.output {
            Output::Live(generation) => Some(generation),
            _ => None,
        }
    }
}

impl OutputOwner {
    fn lock(&self) -> MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.settle();
        state
    }

    fn wake(&self) {
        // A full channel already holds a wake.
        let _woken = self.changed.try_send(());
    }

    /// Ends a session whose speaker couldn't take a command, then wakes waiting claims.
    fn after(&self, abort: Option<Abort>) {
        if let Some(abort) = abort {
            abort();
        }
        self.wake();
    }

    /// Wakes once per change to an output or a tail; read [`OutputOwner::view`] after each.
    pub fn changes(&self) -> Receiver<()> {
        self.changes.clone()
    }

    /// For a speaker's worker, which wakes the owner without locking it.
    pub(super) fn waker(&self) -> Sender<()> {
        self.changed.clone()
    }

    /// The link thread starts `session`, before it says Connected. With an opener (`abort`
    /// ends the session's pipes) and talk-back on, it reserves a ledger slot and a generation
    /// before the opener runs and returns them; without a slot or a generation it refuses.
    pub(super) fn begin(
        &self,
        session: u64,
        abort: Option<Abort>,
    ) -> Option<(OutputGeneration, Arc<Tail>)> {
        let mut state = self.lock();
        state.session = Some(session);
        let opener = abort.is_some();
        state.abort = abort;
        let free = state.ledger.iter().position(Option::is_none);
        let next = state.generations.checked_add(1);
        let begun = match (opener, state.off, free, next) {
            (false, ..) => {
                state.quiet(Why::NoOpener);
                None
            }
            (true, true, ..) => {
                state.quiet(Why::Off);
                None
            }
            (true, false, Some(free), Some(next)) => {
                let generation = OutputGeneration(next);
                let tail = Arc::<Tail>::default();
                state.generations = next;
                state.ledger[free] = Some(Entry {
                    generation,
                    tail: Arc::clone(&tail),
                    terminal: false,
                });
                state.output = Output::Opening(generation);
                Some((generation, tail))
            }
            _ => {
                state.quiet(Why::Refused);
                None
            }
        };
        drop(state);
        self.wake();
        begun
    }

    /// The link thread's open of `generation` returned, before its reader starts. A kept speaker
    /// is fenced first: withdrawn when talk-back went off. An `Err` speaker couldn't be fenced:
    /// the link drops it and ends the session.
    ///
    /// # Errors
    /// The speaker, when it couldn't be fenced.
    pub(super) fn opened(
        &self,
        generation: OutputGeneration,
        speaker: Option<Speaker>,
    ) -> Result<Option<Speaker>, Speaker> {
        let mut state = self.lock();
        let opening = state.output == Output::Opening(generation);
        let opened = match speaker {
            None => {
                if opening {
                    state.quiet(Why::Refused);
                }
                Ok(None)
            }
            Some(speaker) => {
                let fence = state.off.then_some(Command::Withdraw);
                if opening && fence.is_none_or(|fence| speaker.command(fence)) {
                    state.commands = speaker.commands();
                    state.output = Output::Live(generation);
                    Ok(Some(speaker))
                } else {
                    state.output = Output::Lost(generation);
                    Err(speaker)
                }
            }
        };
        drop(state);
        self.wake();
        opened
    }

    /// The link thread ends `session`, first in teardown: the speaker's queue and the abort are
    /// let go here. Pending tails stay in the ledger.
    pub(super) fn end(&self, session: u64) {
        let mut state = self.lock();
        if state.session != Some(session) {
            return;
        }
        state.session = None;
        state.output = Output::Closed;
        let released = (state.commands.take(), state.abort.take());
        drop(state);
        drop(released);
        self.wake();
    }

    /// Queues the speaker's `Stop` of `run` on `session` only, after the brain accepted its
    /// own stop. False when `session` isn't current, or the speaker couldn't take it, which ends
    /// the session.
    pub fn stop(&self, session: u64, run: RunId) -> bool {
        let mut state = self.lock();
        if state.session != Some(session) {
            return false;
        }
        let abort = state.command(Command::Stop(run));
        drop(state);
        let stopped = abort.is_none();
        self.after(abort);
        stopped
    }

    /// Talk-back off: a live speaker is withdrawn, an opening one when it opens, and later
    /// sessions get none.
    pub fn withdraw(&self) {
        let mut state = self.lock();
        if std::mem::replace(&mut state.off, true) {
            return;
        }
        let abort = state.command(Command::Withdraw);
        drop(state);
        self.after(abort);
    }

    /// The current output, settled.
    pub fn view(&self) -> OutputView {
        let state = self.lock();
        let (key, why) = match state.output {
            Output::Live(generation) => (Some(OutputKey::Generation(generation)), None),
            Output::Quiet { epoch, why } => (Some(OutputKey::NoOutput(epoch)), Some(why)),
            _ => (None, None),
        };
        let mut entries = state.ledger.iter().flatten();
        OutputView {
            key,
            why,
            retiring: entries
                .clone()
                .any(|entry| !entry.terminal && Some(entry.generation) != state.live()),
            terminal: entries.any(|entry| entry.terminal),
        }
    }
}
