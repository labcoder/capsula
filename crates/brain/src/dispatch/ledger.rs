//! A run's ledger: one entry per logical call, so a retry is answered, joined or refused and
//! never runs twice.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;

use super::{Caller, LedgerKey, ReadCall, Route, Step, ToolError, ToolOutput, rehash};
use crate::engines::ToolName;
use crate::recall::control::CancelOnDrop;

/// The calls of one run, dropped with it. Its size is bounded by the run's call budget, which
/// the tool loop and the MCP listener charge before dispatch.
#[derive(Default)]
pub(crate) struct Ledger {
    entries: HashMap<(Route, Box<str>), (ToolName, Entry)>,
    /// Draw calls that stopped early, so every answer to one, replays included, is an error.
    stopped: HashSet<(Route, Box<str>)>,
}

enum Entry {
    Reading {
        call: Box<ReadCall>,
        waiting: Vec<Caller>,
        _cancel: CancelOnDrop,
    },
    /// The call was refused; the key stays refused.
    Refused(ToolOutput),
    Reserved {
        waiting: Vec<Caller>,
    },
    Streaming {
        waiting: Vec<Caller>,
    },
    Drawing {
        hash: [u8; 32],
        waiting: Vec<Caller>,
        terminal: bool,
    },
    /// The call's write is in the store; `waiting` are answered when it's acknowledged.
    Running {
        hash: [u8; 32],
        output: ToolOutput,
        waiting: Vec<Caller>,
        terminal: bool,
    },
    /// The call committed.
    Done {
        hash: [u8; 32],
        output: ToolOutput,
    },
}

pub(crate) enum Drawn {
    Refused(Vec<(Caller, ToolOutput)>),
    Commit([u8; 32]),
    Stale,
}

/// A draw call's result and whether it stopped early.
pub(crate) struct DrawEnd {
    pub(crate) output: ToolOutput,
    pub(crate) stopped: bool,
}

/// A refusal before anything was drawn: its output already says it's an error.
impl From<ToolOutput> for DrawEnd {
    fn from(output: ToolOutput) -> Self {
        Self {
            output,
            stopped: false,
        }
    }
}

impl Ledger {
    fn insert(&mut self, key: &LedgerKey, name: ToolName, entry: Entry) {
        self.entries
            .insert((key.route, key.call.clone()), (name, entry));
    }

    /// The answer to a key that already has an entry, or `None` for a key seen first.
    pub(super) fn retry(
        &mut self,
        key: &LedgerKey,
        caller: Caller,
        name: ToolName,
        args: &[u8],
    ) -> Option<Step> {
        let (owner, entry) = self.entries.get_mut(&(key.route, key.call.clone()))?;
        if matches!(key.route, Route::Mcp(_)) && *owner != name {
            return Some(Step::Answer(ToolOutput::Error(ToolError::Reused)));
        }
        if let Entry::Reading { call, .. } = entry
            && let Err(error) = call.control.check()
        {
            *entry = Entry::Refused(ToolOutput::Error(error.into()));
        }
        Some(match entry {
            Entry::Streaming { .. } => Step::Answer(ToolOutput::Error(ToolError::Reused)),
            Entry::Drawing {
                waiting,
                terminal: true,
                ..
            }
            | Entry::Running {
                waiting,
                terminal: true,
                ..
            } => {
                waiting.push(caller);
                Step::Join
            }
            Entry::Reading { call, .. } if rehash(key.route, name, args) != Some(call.hash) => {
                Step::Answer(ToolOutput::Error(ToolError::Reused))
            }
            Entry::Drawing { hash, .. }
            | Entry::Running { hash, .. }
            | Entry::Done { hash, .. }
                if rehash(key.route, name, args) != Some(*hash) =>
            {
                Step::Answer(ToolOutput::Error(ToolError::Reused))
            }
            Entry::Refused(output) | Entry::Done { output, .. } => Step::Answer(output.clone()),
            Entry::Reading { waiting, .. }
            | Entry::Reserved { waiting }
            | Entry::Drawing { waiting, .. }
            | Entry::Running { waiting, .. } => {
                waiting.push(caller);
                Step::Join
            }
        })
    }

    /// Whether `key` has an entry: a call the run has seen, whatever became of it.
    pub(crate) fn has(&self, key: &LedgerKey) -> bool {
        self.entries.contains_key(&(key.route, key.call.clone()))
    }

    /// Whether `key` is a draw the drawing may still be running.
    pub(crate) fn drawing(&self, key: &LedgerKey) -> bool {
        matches!(
            self.entries.get(&(key.route, key.call.clone())),
            Some((_, Entry::Reserved { .. } | Entry::Drawing { .. }))
        )
    }

    /// Whether `key` is a draw call that stopped early.
    pub(crate) fn stopped(&self, key: &LedgerKey) -> bool {
        self.stopped.contains(&(key.route, key.call.clone()))
    }

    /// Refuses `key` with a draw's own `output`, read as an error like a draw that stopped early.
    pub(crate) fn refuse_stopped(&mut self, key: &LedgerKey, output: ToolOutput) {
        let id = (key.route, key.call.clone());
        self.stopped.insert(id.clone());
        self.entries
            .insert(id, (ToolName::Draw, Entry::Refused(output)));
    }

    pub(crate) fn refuse(&mut self, key: &LedgerKey, name: ToolName, error: ToolError) {
        self.insert(key, name, Entry::Refused(ToolOutput::Error(error)));
    }

    pub(super) fn reserve(&mut self, key: &LedgerKey, caller: Caller) {
        self.insert(
            key,
            ToolName::Draw,
            Entry::Reserved {
                waiting: vec![caller],
            },
        );
    }

    pub(crate) fn bind_draw(&mut self, key: &LedgerKey, hash: [u8; 32]) {
        let id = (key.route, key.call.clone());
        if let Some((_, entry)) = self.entries.get_mut(&id)
            && let Entry::Reserved { waiting } = entry
        {
            *entry = Entry::Drawing {
                hash,
                waiting: mem::take(waiting),
                terminal: false,
            };
        }
    }

    pub(super) fn start_draw(&mut self, key: &LedgerKey, caller: Caller) -> bool {
        let id = (key.route, key.call.clone());
        if self.entries.contains_key(&id) {
            return false;
        }
        self.entries.insert(
            id,
            (
                ToolName::Draw,
                Entry::Streaming {
                    waiting: vec![caller],
                },
            ),
        );
        true
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the stream was canonical; a noncanonical binding is terminal"
    )]
    pub(crate) fn bind_stream(&mut self, key: &LedgerKey, hash: [u8; 32], canonical: bool) {
        let id = (key.route, key.call.clone());
        if let Some((_, entry)) = self.entries.get_mut(&id)
            && let Entry::Streaming { waiting } = entry
        {
            *entry = Entry::Drawing {
                hash,
                waiting: mem::take(waiting),
                terminal: !canonical,
            };
        }
    }

    pub(crate) fn drawn(&mut self, key: &LedgerKey, end: DrawEnd) -> Drawn {
        let DrawEnd { output, stopped } = end;
        let id = (key.route, key.call.clone());
        let Some((_, entry)) = self.entries.get_mut(&id) else {
            return Drawn::Stale;
        };
        if stopped && matches!(entry, Entry::Reserved { .. } | Entry::Drawing { .. }) {
            self.stopped.insert(id);
        }
        match entry {
            Entry::Reserved { waiting } => {
                let answers = mem::take(waiting)
                    .into_iter()
                    .map(|caller| (caller, output.clone()))
                    .collect();
                *entry = Entry::Refused(output);
                Drawn::Refused(answers)
            }
            Entry::Drawing {
                hash,
                waiting,
                terminal,
            } => {
                let hash = *hash;
                *entry = Entry::Running {
                    hash,
                    output,
                    waiting: mem::take(waiting),
                    terminal: *terminal,
                };
                Drawn::Commit(hash)
            }
            _ => Drawn::Stale,
        }
    }

    pub(super) fn reading(&mut self, name: ToolName, call: &ReadCall, caller: Caller) {
        self.insert(
            &call.key,
            name,
            Entry::Reading {
                call: Box::new(call.clone()),
                waiting: vec![caller],
                _cancel: CancelOnDrop(call.control.clone()),
            },
        );
    }

    pub(crate) fn read_call(&self, key: &LedgerKey) -> Option<&ReadCall> {
        match self.entries.get(&(key.route, key.call.clone()))? {
            (_, Entry::Reading { call, .. }) => Some(call),
            _ => None,
        }
    }

    pub(crate) fn read_finished(
        &mut self,
        key: &LedgerKey,
        output: ToolOutput,
    ) -> Option<super::CommitCall> {
        let id = (key.route, key.call.clone());
        let (_, entry) = self.entries.get_mut(&id)?;
        let Entry::Reading { call, waiting, .. } = entry else {
            return None;
        };
        let commit = super::CommitCall {
            key: key.clone(),
            thread: call.thread,
            tool: call.tool,
            hash: call.hash,
            write: super::LocalWrite::None,
            output,
            then: None,
        };
        *entry = Entry::Running {
            hash: commit.hash,
            output: commit.output.clone(),
            waiting: mem::take(waiting),
            terminal: false,
        };
        Some(commit)
    }

    pub(super) fn running(
        &mut self,
        key: &LedgerKey,
        name: ToolName,
        hash: [u8; 32],
        output: ToolOutput,
        first: Caller,
    ) {
        let waiting = vec![first];
        self.insert(
            key,
            name,
            Entry::Running {
                hash,
                output,
                waiting,
                terminal: false,
            },
        );
    }

    /// The store committed `key`'s write: every caller waiting on it, with the output. `None`
    /// unless the key was running, so a second acknowledgement does nothing.
    pub(crate) fn committed(&mut self, key: &LedgerKey) -> Option<Vec<(Caller, ToolOutput)>> {
        let id = (key.route, key.call.clone());
        let (name, entry) = self.entries.remove(&id)?;
        let Entry::Running {
            hash,
            output,
            waiting,
            terminal,
        } = entry
        else {
            self.entries.insert(id, (name, entry));
            return None;
        };
        let answers = waiting
            .into_iter()
            .map(|caller| (caller, output.clone()))
            .collect();
        let entry = if terminal {
            Entry::Refused(output)
        } else {
            Entry::Done { hash, output }
        };
        self.entries.insert(id, (name, entry));
        Some(answers)
    }

    /// The store refused `key`'s write on its own check: the key stays refused with `error`, and
    /// every caller waiting on it is returned to be answered with it.
    pub(crate) fn refused(&mut self, key: &LedgerKey, error: ToolError) -> Vec<Caller> {
        let id = (key.route, key.call.clone());
        let Some((_, entry)) = self.entries.get_mut(&id) else {
            return Vec::new();
        };
        let (Entry::Running { waiting, .. } | Entry::Reading { waiting, .. }) = entry else {
            return Vec::new();
        };
        let waiting = mem::take(waiting);
        *entry = Entry::Refused(ToolOutput::Error(error));
        waiting
    }

    /// The store refused `key`'s write: nobody is answered and the key stays refused.
    pub(crate) fn failed(&mut self, key: &LedgerKey) {
        let id = (key.route, key.call.clone());
        if let Some((
            _,
            entry @ (Entry::Reserved { .. }
            | Entry::Streaming { .. }
            | Entry::Drawing { .. }
            | Entry::Running { .. }),
        )) = self.entries.get_mut(&id)
        {
            *entry = Entry::Refused(ToolOutput::Error(ToolError::Ended));
        }
    }
}

impl fmt::Debug for Ledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ledger")
            .field("entries", &self.entries.len())
            .field("stopped", &self.stopped.len())
            .finish()
    }
}
