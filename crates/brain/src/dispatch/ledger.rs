//! A run's ledger: one entry per logical call, so a retry is answered, joined or refused and
//! never runs twice.

use std::collections::HashMap;
use std::fmt;

use super::{Caller, LedgerKey, ReadCall, Route, Step, ToolError, ToolOutput, rehash};
use crate::engines::ToolName;
use crate::recall::control::CancelOnDrop;

/// The calls of one run, dropped with it. Its size is bounded by the run's call budget, which
/// the tool loop and the MCP listener charge before dispatch.
#[derive(Default)]
pub(crate) struct Ledger {
    entries: HashMap<(Route, Box<str>), Entry>,
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
    Drawing {
        hash: [u8; 32],
        waiting: Vec<Caller>,
    },
    /// The call's write is in the store; `waiting` are answered when it's acknowledged.
    Running {
        hash: [u8; 32],
        output: ToolOutput,
        waiting: Vec<Caller>,
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

impl Ledger {
    /// The answer to a key that already has an entry, or `None` for a key seen first.
    pub(super) fn retry(
        &mut self,
        key: &LedgerKey,
        caller: Caller,
        name: ToolName,
        args: &[u8],
    ) -> Option<Step> {
        let entry = self.entries.get_mut(&(key.route, key.call.clone()))?;
        if let Entry::Reading { call, .. } = entry
            && let Err(error) = call.control.check()
        {
            *entry = Entry::Refused(ToolOutput::Error(error.into()));
        }
        Some(match entry {
            Entry::Reading { call, .. } if rehash(name, args) != Some(call.hash) => {
                Step::Answer(ToolOutput::Error(ToolError::Reused))
            }
            Entry::Drawing { hash, .. }
            | Entry::Running { hash, .. }
            | Entry::Done { hash, .. }
                if rehash(name, args) != Some(*hash) =>
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

    pub(super) fn refuse(&mut self, key: &LedgerKey, error: ToolError) {
        self.entries.insert(
            (key.route, key.call.clone()),
            Entry::Refused(ToolOutput::Error(error)),
        );
    }

    pub(super) fn reserve(&mut self, key: &LedgerKey, caller: Caller) {
        self.entries.insert(
            (key.route, key.call.clone()),
            Entry::Reserved {
                waiting: vec![caller],
            },
        );
    }

    pub(super) fn bind_draw(&mut self, key: &LedgerKey, hash: [u8; 32]) {
        let id = (key.route, key.call.clone());
        match self.entries.remove(&id) {
            Some(Entry::Reserved { waiting }) => {
                self.entries.insert(id, Entry::Drawing { hash, waiting });
            }
            Some(other) => {
                self.entries.insert(id, other);
            }
            None => {}
        }
    }

    pub(crate) fn drawn(&mut self, key: &LedgerKey, output: ToolOutput) -> Drawn {
        let id = (key.route, key.call.clone());
        match self.entries.remove(&id) {
            Some(Entry::Reserved { waiting }) => {
                let answers = waiting
                    .into_iter()
                    .map(|caller| (caller, output.clone()))
                    .collect();
                self.entries.insert(id, Entry::Refused(output));
                Drawn::Refused(answers)
            }
            Some(Entry::Drawing { hash, waiting }) => {
                self.entries.insert(
                    id,
                    Entry::Running {
                        hash,
                        output,
                        waiting,
                    },
                );
                Drawn::Commit(hash)
            }
            Some(other) => {
                self.entries.insert(id, other);
                Drawn::Stale
            }
            None => Drawn::Stale,
        }
    }

    pub(super) fn reading(&mut self, call: &ReadCall, caller: Caller) {
        self.entries.insert(
            (call.key.route, call.key.call.clone()),
            Entry::Reading {
                call: Box::new(call.clone()),
                waiting: vec![caller],
                _cancel: CancelOnDrop(call.control.clone()),
            },
        );
    }

    pub(crate) fn read_call(&self, key: &LedgerKey) -> Option<&ReadCall> {
        match self.entries.get(&(key.route, key.call.clone()))? {
            Entry::Reading { call, .. } => Some(call),
            _ => None,
        }
    }

    pub(crate) fn read_finished(
        &mut self,
        key: &LedgerKey,
        output: ToolOutput,
    ) -> Option<super::CommitCall> {
        let id = (key.route, key.call.clone());
        if !matches!(self.entries.get(&id), Some(Entry::Reading { .. })) {
            return None;
        }
        let Entry::Reading { call, waiting, .. } = self.entries.remove(&id)? else {
            unreachable!()
        };
        self.entries.insert(
            id,
            Entry::Running {
                hash: call.hash,
                output: output.clone(),
                waiting,
            },
        );
        Some(super::CommitCall {
            key: key.clone(),
            thread: call.thread,
            tool: call.tool,
            hash: call.hash,
            write: super::LocalWrite::None,
            output,
            then: None,
        })
    }

    pub(super) fn running(
        &mut self,
        key: &LedgerKey,
        hash: [u8; 32],
        output: ToolOutput,
        first: Caller,
    ) {
        let waiting = vec![first];
        self.entries.insert(
            (key.route, key.call.clone()),
            Entry::Running {
                hash,
                output,
                waiting,
            },
        );
    }

    /// The store committed `key`'s write: every caller waiting on it, with the output. `None`
    /// unless the key was running, so a second acknowledgement does nothing.
    pub(crate) fn committed(&mut self, key: &LedgerKey) -> Option<Vec<(Caller, ToolOutput)>> {
        let id = (key.route, key.call.clone());
        match self.entries.remove(&id) {
            Some(Entry::Running {
                hash,
                output,
                waiting,
            }) => {
                let answers = waiting
                    .into_iter()
                    .map(|caller| (caller, output.clone()))
                    .collect();
                self.entries.insert(id, Entry::Done { hash, output });
                Some(answers)
            }
            Some(other) => {
                self.entries.insert(id, other);
                None
            }
            None => None,
        }
    }

    /// The store refused `key`'s write on its own check: the key stays refused with `error`, and
    /// every caller waiting on it is returned to be answered with it.
    pub(crate) fn refused(&mut self, key: &LedgerKey, error: ToolError) -> Vec<Caller> {
        let id = (key.route, key.call.clone());
        match self.entries.remove(&id) {
            Some(Entry::Running { waiting, .. } | Entry::Reading { waiting, .. }) => {
                self.entries
                    .insert(id, Entry::Refused(ToolOutput::Error(error)));
                waiting
            }
            Some(other) => {
                self.entries.insert(id, other);
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// The store refused `key`'s write: nobody is answered and the key stays refused.
    pub(crate) fn failed(&mut self, key: &LedgerKey) {
        let id = (key.route, key.call.clone());
        if let Some(
            entry @ (Entry::Reserved { .. } | Entry::Drawing { .. } | Entry::Running { .. }),
        ) = self.entries.get_mut(&id)
        {
            *entry = Entry::Refused(ToolOutput::Error(ToolError::Ended));
        }
    }
}

impl fmt::Debug for Ledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ledger")
            .field("entries", &self.entries.len())
            .finish()
    }
}
