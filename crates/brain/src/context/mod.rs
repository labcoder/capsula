//! Builds each model request from the store (architecture v1 §6.5; plans/slice-3.md PR 12).
//!
//! A request is built from one read transaction: the run's thread, its newest summary and raw
//! runs, and every source its history results name, re-read through references so a deleted
//! task's text is never copied into another task. The run's origin decides what a reference may
//! reach. Nothing here sends anything, and nothing calls it yet; PR 12b wires it into the
//! coordinator.

pub(crate) mod bound;
pub(crate) mod build;
#[cfg(test)]
mod tests;

use std::fmt;

use kumochi_protocol::message::{
    AssistantName, Effort, EventId, ModelChoice, ModelSlug, RunId, ThreadId, UserName,
};

use crate::engines::{CallId, Opaque};
use crate::store::StoreError;

/// The request's hard size bound [guess].
pub(crate) const BOUND: usize = 512 * 1024;
/// The newest runs kept raw, the current one included, when they fit [guess].
pub(crate) const RAW_RUNS: usize = 8;
/// The most one reference re-reads [guess].
pub(crate) const REREAD_CAP: usize = 32 * 1024;
/// What a reference to a deleted task renders as.
pub(crate) const REMOVED: &str = "[removed with a deleted task]";
/// What a reference the run's origin may not reach renders as.
pub(crate) const UNAVAILABLE: &str = "[not available here]";

/// Where a run was started, which decides what it may reach. It comes from the run's thread,
/// never from anything a model produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Kumo's own conversation, which may reach every live task.
    Kumo,
    /// A task, which reaches only itself.
    Task(ThreadId),
}

/// Which connection produced a reasoning item. PR 13b records it with the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionTag(pub(crate) u64);

/// What a run reads once when it starts and reuses for every round, so a rename applies from the
/// next run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunSnapshot {
    pub(crate) origin: Origin,
    pub(crate) assistant: AssistantName,
    pub(crate) user: Option<UserName>,
    pub(crate) choice: ModelChoice,
    pub(crate) connection: ConnectionTag,
}

/// A range of a task's events that a history result showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRef {
    pub(crate) thread: ThreadId,
    pub(crate) first: EventId,
    pub(crate) last: EventId,
}

/// A summary of a thread's events up to and including `last`. PR 17 writes them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) first: EventId,
    pub(crate) last: EventId,
    pub(crate) text: String,
}

impl fmt::Debug for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Summary")
            .field("first", &self.first)
            .field("last", &self.last)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

/// One item of a run, ordered by its event id.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Body {
    User(String),
    Assistant(String),
    /// A tool call; its result is the next [`Body::History`] with the same id.
    Call {
        call: CallId,
        name: String,
        arguments: String,
    },
    /// A history tool's result, stored as references only (PR 14a).
    History {
        call: CallId,
        refs: Vec<HistoryRef>,
    },
    /// A native reasoning item (PR 13b), replayed only on the connection that made it.
    Reasoning {
        connection: ConnectionTag,
        item: Opaque,
    },
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(text) => f.debug_tuple("User").field(&text.len()).finish(),
            Self::Assistant(text) => f.debug_tuple("Assistant").field(&text.len()).finish(),
            Self::Call { call, .. } => f.debug_struct("Call").field("call", call).finish(),
            Self::History { call, refs } => f
                .debug_struct("History")
                .field("call", call)
                .field("refs", refs)
                .finish(),
            Self::Reasoning { connection, item } => f
                .debug_struct("Reasoning")
                .field("connection", connection)
                .field("item", item)
                .finish(),
        }
    }
}

/// A thread item: an event the store read, or one another part supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) id: EventId,
    pub(crate) run: RunId,
    pub(crate) body: Body,
}

/// What the parts that come later supply (PRs 13b, 14a and 17); until then, nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Supplied {
    pub(crate) summary: Option<Summary>,
    pub(crate) items: Vec<Item>,
}

/// Who a message is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Developer,
    User,
    Assistant,
}

/// One input item of the request.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum InputItem {
    Message {
        role: Role,
        text: String,
    },
    FunctionCall {
        call: CallId,
        name: String,
        arguments: String,
    },
    /// A tool result. A history result's output is quoted data, never instructions.
    FunctionOutput {
        call: CallId,
        output: String,
    },
    Reasoning(Opaque),
}

impl fmt::Debug for InputItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message { role, text } => f
                .debug_struct("Message")
                .field("role", role)
                .field("text_bytes", &text.len())
                .finish(),
            Self::FunctionCall { call, .. } => {
                f.debug_struct("FunctionCall").field("call", call).finish()
            }
            Self::FunctionOutput { call, output } => f
                .debug_struct("FunctionOutput")
                .field("call", call)
                .field("output_bytes", &output.len())
                .finish(),
            Self::Reasoning(item) => f.debug_tuple("Reasoning").field(item).finish(),
        }
    }
}

/// A tool the request offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolSchema {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: serde_json::Value,
}

/// The tools a run with `origin` is offered.
/// shortcut: none yet; 13a adds the draw tool's schema and 14a the history tools for Kumo.
pub(crate) fn tools(origin: Origin) -> Vec<ToolSchema> {
    match origin {
        Origin::Kumo | Origin::Task(_) => Vec::new(),
    }
}

/// A built request, as data: PR 13a serialises it, with `store:false` and `stream:true`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ModelRequest {
    pub(crate) model: ModelSlug,
    pub(crate) effort: Effort,
    pub(crate) instructions: String,
    pub(crate) input: Vec<InputItem>,
    pub(crate) tools: Vec<ToolSchema>,
}

impl fmt::Debug for ModelRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRequest")
            .field("model", &self.model.as_str())
            .field("effort", &self.effort)
            .field("instructions_bytes", &self.instructions.len())
            .field("input", &self.input)
            .field("tools", &self.tools.len())
            .finish()
    }
}

/// A request and every thread whose raw text it carries, for the coordinator's delete check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) request: ModelRequest,
    /// Sorted, each once.
    pub(crate) sources: Vec<ThreadId>,
}

/// Why no request was built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildError {
    /// The thread is being deleted or is gone, so it sends nothing.
    #[error("the thread is being deleted or is gone")]
    Deleting,
    /// The run's origin doesn't match the thread it runs in.
    #[error("the run's origin doesn't match its thread")]
    Origin,
    /// The run's turn isn't in the store.
    #[error("the run's turn isn't stored")]
    NoTurn,
    /// A tool call and its result are apart, or an item is repeated.
    #[error("a tool call is split from its result, or an item repeats")]
    InvalidCut,
    /// Even the current run and the summary are over the bound.
    #[error("the request is over its size bound")]
    TooLarge,
    /// The read failed.
    #[error("can't read the context")]
    Store(#[from] StoreError),
}
