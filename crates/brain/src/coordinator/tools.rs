//! The tool loop: a run's calls and native items, one model round at a time. A round that made
//! calls is stored with a result for each, and the run's next round is built from the store; a
//! round that made none is stored and ends the run. Budgets are charged at admission (D13-M2):
//! a call before anything of it is kept, a round before its build.

use std::fmt;

use kumochi_protocol::message::{Content, RunId, ThreadId};

use super::{Coordinator, Effect, Notify, Stale, StoreOp, unstored};
use crate::context::{BOUND, ConnectionTag};
use crate::engines::{CallId, Opaque};
use crate::store::{Failure, RunEnd};

/// The most model rounds one run may have [guess, from §13b].
pub const MAX_ROUNDS: u32 = 8;
/// The most tool calls one run may make, across its rounds [guess, from §13b].
pub const MAX_CALLS: u32 = 16;
/// What every call gets until a tool answers it (PR 14a). It doesn't repeat the call's name, so
/// nothing the provider sent comes back.
pub(crate) const UNKNOWN_TOOL: &str = r#"{"error":"unknown tool"}"#;

/// One round's write, in one transaction: the text streamed since the last write, the round's
/// native items in output order, then a result for each call in call order. `RoundStored`
/// answers it.
#[derive(Clone, Debug, PartialEq)]
pub struct RoundItems {
    pub(crate) thread: ThreadId,
    pub(crate) run: RunId,
    pub(crate) round: u32,
    /// The run's connection, which replays the items.
    pub(crate) connection: ConnectionTag,
    pub(crate) text: Option<Content>,
    pub(crate) items: Vec<Opaque>,
    pub(crate) calls: Vec<CallId>,
}

/// A run's tool loop.
#[derive(Default)]
pub(super) struct Loop {
    /// Calls accepted across the run's rounds.
    calls: u32,
    /// This round's calls nothing answers, in call order, and whether each is ready.
    open: Vec<(CallId, bool)>,
    /// This round's native items, with their output index.
    items: Vec<(u32, Opaque)>,
    item_bytes: usize,
    /// The round being stored, and whether it's the run's last.
    writing: Option<(u32, bool)>,
}

impl fmt::Debug for Loop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loop")
            .field("calls", &self.calls)
            .field("open", &self.open.len())
            .field("items", &self.items.len())
            .field("item_bytes", &self.item_bytes)
            .field("writing", &self.writing)
            .finish()
    }
}

impl Loop {
    /// Charges one call; false when the run has made all it may.
    pub(super) fn charge(&mut self) -> bool {
        if self.calls >= MAX_CALLS {
            return false;
        }
        self.calls += 1;
        true
    }

    /// A call nothing answers; false when the round already has it.
    pub(super) fn open(&mut self, call: CallId) -> bool {
        if self.open.iter().any(|(open, _)| *open == call) {
            return false;
        }
        self.open.push((call, false));
        true
    }

    /// The call is open and not ready yet.
    pub(super) fn takes(&self, call: &CallId) -> bool {
        self.open.iter().any(|(open, ready)| open == call && !ready)
    }

    pub(super) fn ready(&mut self, call: &CallId) {
        if let Some((_, ready)) = self.open.iter_mut().find(|(open, _)| open == call) {
            *ready = true;
        }
    }

    pub(super) fn all_ready(&self) -> bool {
        self.open.iter().all(|(_, ready)| *ready)
    }

    /// The round made calls the loop still has to answer.
    pub(super) fn calling(&self) -> bool {
        !self.open.is_empty()
    }

    /// Keeps a native item; false when the round's items are over a request's bound.
    pub(super) fn keep(&mut self, index: u32, item: Opaque) -> bool {
        if self.item_bytes + item.size() > BOUND {
            return false;
        }
        self.item_bytes += item.size();
        self.items.push((index, item));
        true
    }

    pub(super) fn writing(&self) -> bool {
        self.writing.is_some()
    }

    /// The pending round write failed; true when there was one.
    pub(super) fn write_failed(&mut self) -> bool {
        self.writing.take().is_some()
    }
}

impl Coordinator {
    /// The current run's round completed and its steps are stored: a round with calls is stored
    /// with their results unless it was the run's last allowed round, a round with only native
    /// items is stored before the run ends, and a round with neither ends it.
    pub(super) fn round_end(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let current = self.current(run)?;
        let round = current.build.as_ref().map_or(1, |build| build.key().round);
        let calling = current.tools.calling();
        if calling && round >= MAX_ROUNDS {
            return self.fail(run, Failure::Budget, effects);
        }
        if !calling && current.tools.items.is_empty() {
            let done = self.take(run)?;
            self.end(done, RunEnd::Done, None, Notify::Shell, effects);
            return Ok(());
        }
        let Some(connection) = current.connection else {
            return self.fail(run, Failure::Engine, effects);
        };
        current.finishing = !calling;
        let mut items = std::mem::take(&mut current.tools.items);
        items.sort_by_key(|(index, _)| *index);
        current.tools.item_bytes = 0;
        let calls = std::mem::take(&mut current.tools.open)
            .into_iter()
            .map(|(call, _)| call)
            .collect();
        current.tools.writing = Some((round, !calling));
        effects.push(Effect::Store(StoreOp::RoundItems(RoundItems {
            thread: current.thread,
            run,
            round,
            connection,
            text: unstored(current),
            items: items.into_iter().map(|(_, item)| item).collect(),
            calls,
        })));
        Ok(())
    }

    /// A round is stored. A late answer for a run that ended only lets the settle pass after
    /// every input write its outcome; otherwise the run's next round is built, or the run ends
    /// after its last.
    pub(super) fn round_stored(
        &mut self,
        run: RunId,
        round: u32,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let found = self.find(run)?;
        let Some((_, last)) = found.tools.writing.filter(|(writing, _)| *writing == round) else {
            return Err(Stale);
        };
        found.tools.writing = None;
        if found.ending.is_some() {
            return Ok(());
        }
        if last {
            let done = self.take(run)?;
            self.end(done, RunEnd::Done, None, Notify::Shell, effects);
        } else {
            self.build(run, round + 1, effects);
        }
        Ok(())
    }
}
