//! Closed model arguments and the read handed to the stamped reader.

#[cfg(test)]
mod tests;

use std::fmt;

use kumochi_protocol::message::ThreadId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LedgerKey, Prepared, Tool, ToolError, parameters, parse};
use crate::context::Origin;
use crate::coordinator::BuildKey;
use crate::engines::ToolName;
use crate::recall::control::Control;
use crate::recall::{Error, Query, Selection};

impl From<Error> for ToolError {
    fn from(error: Error) -> Self {
        match error {
            Error::Invalid => Self::Invalid,
            Error::NotHere => Self::NotHere,
            Error::NoTask => Self::NoTask,
            Error::Read | Error::Handler | Error::Ended => Self::Ended,
            Error::Timeout => Self::Timeout,
        }
    }
}

/// A read admitted from a run's build; equality excludes its execution capability.
#[derive(Clone)]
pub struct ReadCall {
    pub(crate) key: LedgerKey,
    pub(crate) build: BuildKey,
    pub(crate) thread: ThreadId,
    pub(crate) tool: &'static str,
    pub(crate) hash: [u8; 32],
    pub(crate) origin: Origin,
    pub(crate) query: Query,
    pub(crate) control: Control,
    /// The established history cutoff this read must find unchanged and keep to; unset until
    /// reconnect admission sets it.
    pub(crate) require_recovery: Option<crate::store::Boundary>,
}

impl fmt::Debug for ReadCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadCall")
            .field("key", &self.key)
            .field("build", &self.build)
            .field("query", &self.query)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ReadCall {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.build == other.build
            && self.thread == other.thread
            && self.tool == other.tool
            && self.hash == other.hash
            && self.origin == other.origin
            && self.query == other.query
            && self.require_recovery == other.require_recovery
    }
}
impl Eq for ReadCall {}

/// A read's selected references and source identities, never its rendered content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFinished {
    pub(crate) key: LedgerKey,
    pub(crate) build: BuildKey,
    pub(crate) result: Result<(Selection, Vec<ThreadId>), Error>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[schemars(range(min = 1, max = 9_223_372_036_854_775_807u64))]
    task: Option<u64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[schemars(range(min = 1, max = 9_223_372_036_854_775_807u64))]
    task: u64,
    #[schemars(range(min = 1, max = 9_223_372_036_854_775_807u64))]
    from: u64,
    #[schemars(range(min = 1, max = 9_223_372_036_854_775_807u64))]
    to: u64,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TasksArgs {}

fn search(args: &[u8]) -> Result<Prepared, ToolError> {
    let (args, hash) = parse::<SearchArgs>("search_history", args)?;
    Ok(Prepared::Read {
        hash,
        query: Query::Search {
            query: args.query,
            task: args.task,
        },
    })
}
fn read(args: &[u8]) -> Result<Prepared, ToolError> {
    let (args, hash) = parse::<ReadArgs>("read_history", args)?;
    Ok(Prepared::Read {
        hash,
        query: Query::Read {
            task: args.task,
            from: args.from,
            to: args.to,
        },
    })
}
fn tasks(args: &[u8]) -> Result<Prepared, ToolError> {
    let (_, hash) = parse::<TasksArgs>("list_tasks", args)?;
    Ok(Prepared::Read {
        hash,
        query: Query::Tasks,
    })
}

pub(super) const SEARCH: Tool = Tool {
    name: ToolName::SearchHistory,
    wire: "search_history",
    description: "Search conversation events by words (OR). At most ten matched excerpts. A task may search only itself.",
    kumo_only: false,
    mcp: false,
    parameters: parameters::<SearchArgs>,
    prepare: search,
};
pub(super) const READ: Tool = Tool {
    name: ToolName::ReadHistory,
    wire: "read_history",
    description: "Read an inclusive task event range, at most twenty events and 32 KiB. A cut event's tail cannot be paged; next advances beyond it.",
    kumo_only: false,
    mcp: false,
    parameters: parameters::<ReadArgs>,
    prepare: read,
};
pub(super) const TASKS: Tool = Tool {
    name: ToolName::ListTasks,
    wire: "list_tasks",
    description: "List live task metadata, at most one hundred tasks. A truncated list cannot prove title uniqueness; ask for clarification.",
    kumo_only: true,
    mcp: false,
    parameters: parameters::<TasksArgs>,
    prepare: tasks,
};
