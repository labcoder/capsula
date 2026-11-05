//! A run's ledger: one entry per logical call, so a retry is answered, joined or refused and
//! never runs twice.

use std::collections::HashMap;
use std::fmt;

use super::{Caller, LedgerKey, Route, Step, ToolError, ToolOutput, rehash};
use crate::engines::ToolName;

/// The calls of one run, dropped with it. Its size is bounded by the run's call budget, which
/// the tool loop and the MCP listener charge before dispatch.
#[derive(Default)]
pub(crate) struct Ledger {
    entries: HashMap<(Route, Box<str>), Entry>,
}

enum Entry {
    /// The call was refused; the key stays refused.
    Refused(ToolError),
    /// The call's write is in the store; `waiting` are answered when it's acknowledged.
    Running {
        hash: [u8; 32],
        output: ToolOutput,
        waiting: Vec<Caller>,
    },
    /// The call committed.
    Done { hash: [u8; 32], output: ToolOutput },
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
        Some(match entry {
            Entry::Refused(error) => Step::Answer(ToolOutput::Error(*error)),
            Entry::Running { hash, .. } | Entry::Done { hash, .. }
                if rehash(name, args) != Some(*hash) =>
            {
                Step::Answer(ToolOutput::Error(ToolError::Reused))
            }
            Entry::Done { output, .. } => Step::Answer(output.clone()),
            Entry::Running { waiting, .. } => {
                waiting.push(caller);
                Step::Join
            }
        })
    }

    pub(super) fn refuse(&mut self, key: &LedgerKey, error: ToolError) {
        self.entries
            .insert((key.route, key.call.clone()), Entry::Refused(error));
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

    /// The store answered `key`'s write: every caller waiting on it with the output, or nobody
    /// when the write failed and the key stays refused.
    pub(crate) fn committed(&mut self, key: &LedgerKey, ok: bool) -> Vec<(Caller, ToolOutput)> {
        let id = (key.route, key.call.clone());
        match self.entries.remove(&id) {
            Some(Entry::Running {
                hash,
                output,
                waiting,
            }) if ok => {
                let answers = waiting
                    .into_iter()
                    .map(|caller| (caller, output.clone()))
                    .collect();
                self.entries.insert(id, Entry::Done { hash, output });
                answers
            }
            Some(Entry::Running { .. }) => {
                self.entries.insert(id, Entry::Refused(ToolError::Ended));
                Vec::new()
            }
            Some(other) => {
                self.entries.insert(id, other);
                Vec::new()
            }
            None => Vec::new(),
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
