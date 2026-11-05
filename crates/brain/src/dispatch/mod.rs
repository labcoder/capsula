//! The dispatcher: runs each tool call once, from what its caller may reach
//! (docs/development/tools.md; architecture v1 §5.4).
//!
//! The coordinator resolves a call's run first, so a queued call can't borrow another run's
//! reach or ledger. Everything here is pure: the store applies a call's one write, and the
//! coordinator answers it on the acknowledgement.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "slice 3's 13b-2 tool loop and slice 8's C4b-2 hand calls to Coordinator::tool_call"
    )
)]

mod ledger;
#[cfg(test)]
mod tests;

use std::fmt;

use kumochi_protocol::message::{RunId, ThreadId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::bridge::{CallKey, LaunchId};
use crate::context::{HistoryRef, Origin, ToolSchema};
use crate::engines::{CallId, ToolName};

pub(crate) use ledger::Ledger;

/// The most bytes a call's complete arguments may hold [guess].
pub(crate) const MAX_ARGS: usize = 16 * 1024;
/// The most bytes a call's stored output may hold [guess].
pub(crate) const MAX_OUTPUT: usize = 64 * 1024;

/// Who made a call, frozen when it arrived.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    /// The model's function call in a run's round.
    Model { run: RunId, call: CallId },
    /// An MCP `tools/call` from the launch bound to the run.
    Mcp {
        run: RunId,
        launch: LaunchId,
        key: CallKey,
    },
}

impl Caller {
    pub(crate) fn run(&self) -> RunId {
        match self {
            Self::Model { run, .. } | Self::Mcp { run, .. } => *run,
        }
    }

    /// The call's identity in its run's ledger.
    pub(crate) fn key(&self) -> LedgerKey {
        match self {
            Self::Model { run, call } => LedgerKey {
                run: *run,
                route: Route::Model,
                call: call.as_str().into(),
            },
            Self::Mcp { run, launch, key } => LedgerKey {
                run: *run,
                route: Route::Mcp(*launch),
                call: key.0.as_str().into(),
            },
        }
    }
}

/// The kind and run only: call ids, launches and keys aren't for diagnostics.
impl fmt::Debug for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, run) = match self {
            Self::Model { run, .. } => ("Model", run),
            Self::Mcp { run, .. } => ("Mcp", run),
        };
        f.debug_struct(kind)
            .field("run", run)
            .finish_non_exhaustive()
    }
}

/// Which way a call came, so the same id on two routes, or two launches, is two calls.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Route {
    Model,
    Mcp(LaunchId),
}

impl Route {
    /// The ledger table's `route` column.
    pub(crate) fn column(self) -> String {
        match self {
            Self::Model => "model".to_owned(),
            Self::Mcp(launch) => format!("mcp:{}", launch.hex()),
        }
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Model => "Model",
            Self::Mcp(_) => "Mcp",
        })
    }
}

/// One logical call: its run, its route and the caller's id for it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LedgerKey {
    pub(crate) run: RunId,
    pub(crate) route: Route,
    pub(crate) call: Box<str>,
}

impl fmt::Debug for LedgerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerKey")
            .field("run", &self.run)
            .field("route", &self.route)
            .field("call_bytes", &self.call.len())
            .finish()
    }
}

/// Why a call ran nothing. Each is final for its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolError {
    /// No tool has that name.
    Unknown,
    /// The tool isn't offered to this caller, route or reach.
    NotHere,
    /// The arguments or the output are over their cap.
    TooLarge,
    /// The arguments don't parse into the tool's fields.
    Invalid,
    /// The key was used for another call.
    Reused,
    /// The run ended, or isn't the one the call came from.
    Ended,
}

impl ToolError {
    /// What the model reads.
    fn text(self) -> &'static str {
        match self {
            Self::Unknown => "unknown tool",
            Self::NotHere => "not available here",
            Self::TooLarge => "too large",
            Self::Invalid => "invalid arguments",
            Self::Reused => "call id already used",
            Self::Ended => "the run ended",
        }
    }

    fn from_text(text: &str) -> Option<Self> {
        [
            Self::Unknown,
            Self::NotHere,
            Self::TooLarge,
            Self::Invalid,
            Self::Reused,
            Self::Ended,
        ]
        .into_iter()
        .find(|error| error.text() == text)
    }
}

/// A call's result.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ToolOutput {
    /// Text for the model.
    Text(String),
    /// A history tool's result: references, re-read at every build, never the text.
    History(Vec<HistoryRef>),
    /// The call ran nothing.
    Error(ToolError),
}

impl ToolOutput {
    /// The tagged JSON the ledger and `tool_result` events store.
    pub(crate) fn stored(&self) -> String {
        match self {
            Self::Text(text) => json!({ "text": text }),
            Self::History(refs) => json!({
                "history": refs
                    .iter()
                    .map(|r| json!({ "thread": r.thread.0, "first": r.first.0, "last": r.last.0 }))
                    .collect::<Vec<_>>()
            }),
            Self::Error(error) => json!({ "error": error.text() }),
        }
        .to_string()
    }

    /// Reads [`ToolOutput::stored`]'s JSON back; `None` for anything else.
    pub(crate) fn from_stored(stored: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(stored).ok()?;
        let object = value.as_object().filter(|object| object.len() == 1)?;
        let (tag, body) = object.iter().next()?;
        match tag.as_str() {
            "text" => Some(Self::Text(body.as_str()?.to_owned())),
            "error" => ToolError::from_text(body.as_str()?).map(Self::Error),
            "history" => body
                .as_array()?
                .iter()
                .map(|r| {
                    let id = |field: &str| r.get(field).and_then(Value::as_u64);
                    let (thread, first, last) = (id("thread")?, id("first")?, id("last")?);
                    (r.as_object()?.len() == 3 && first <= last).then_some(HistoryRef {
                        thread: ThreadId(thread),
                        first: kumochi_protocol::message::EventId(first),
                        last: kumochi_protocol::message::EventId(last),
                    })
                })
                .collect::<Option<_>>()
                .map(Self::History),
            _ => None,
        }
    }
}

impl fmt::Debug for ToolOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f.debug_struct("Text").field("bytes", &text.len()).finish(),
            Self::History(refs) => f
                .debug_struct("History")
                .field("refs", &refs.len())
                .finish(),
            Self::Error(error) => f.debug_tuple("Error").field(error).finish(),
        }
    }
}

/// The one local write a call makes, applied in the transaction that records it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum LocalWrite {
    /// A read-only call: only its ledger row is written.
    None,
    /// Renames the run's thread; only the test tool writes it.
    #[cfg(test)]
    Title(String),
}

impl fmt::Debug for LocalWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            #[cfg(test)]
            Self::Title(title) => f
                .debug_struct("Title")
                .field("bytes", &title.len())
                .finish(),
        }
    }
}

/// A first successful call, for the store to apply and record in one transaction.
#[derive(Clone, PartialEq, Eq)]
pub struct CommitCall {
    pub(crate) key: LedgerKey,
    pub(crate) thread: ThreadId,
    /// The tool's wire name.
    pub(crate) tool: &'static str,
    pub(crate) hash: [u8; 32],
    pub(crate) write: LocalWrite,
    pub(crate) output: ToolOutput,
}

impl fmt::Debug for CommitCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitCall")
            .field("key", &self.key)
            .field("thread", &self.thread)
            .field("write", &self.write)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

/// What became of a call the coordinator handed over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// A refusal, or a call already committed under this key.
    Answered(ToolOutput),
    /// The same call is being committed; this caller is answered with it.
    Joined,
    /// The call's write went to the store; it's answered on the acknowledgement.
    Committing,
}

/// What the dispatcher decided; the coordinator sends `Commit` to the store.
#[derive(Debug)]
pub(crate) enum Step {
    Answer(ToolOutput),
    Join,
    Commit(CommitCall),
}

/// The run a call resolved to, borrowed from the coordinator's current run.
#[derive(Debug)]
pub(crate) struct RunCtx<'a> {
    pub(crate) thread: ThreadId,
    pub(crate) reach: Origin,
    /// The tools the admitted request offered the model.
    pub(crate) offered: &'a [ToolName],
    /// The launch authorized for the run now, if any.
    pub(crate) mcp: Option<LaunchId>,
    pub(crate) ledger: &'a mut Ledger,
}

/// A tool's arguments, hashed and turned into its write and output.
struct Prepared {
    hash: [u8; 32],
    write: LocalWrite,
    output: ToolOutput,
}

/// One tool.
struct Tool {
    name: ToolName,
    wire: &'static str,
    description: &'static str,
    /// Only Kumo's runs may call it.
    kumo_only: bool,
    /// MCP may call it; history tools stay model-only until C4b-2 fences its replies.
    mcp: bool,
    parameters: fn() -> Value,
    prepare: fn(&[u8]) -> Result<Prepared, ToolError>,
}

/// Every tool. shortcut: none in production yet; 14b, 15 and B10 add theirs.
const TOOLS: &[Tool] = &[
    #[cfg(test)]
    tests::PROBE,
    #[cfg(test)]
    tests::RECALL,
];

fn tool(name: ToolName) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

/// The tool a request offers under `wire`.
pub(crate) fn named(wire: &str) -> Option<ToolName> {
    TOOLS
        .iter()
        .find(|tool| tool.wire == wire)
        .map(|tool| tool.name)
}

/// The schemas a run with `reach` is offered on `route`.
pub(crate) fn schemas(reach: Origin, route: Route) -> Vec<ToolSchema> {
    TOOLS
        .iter()
        .filter(|tool| {
            (!tool.kumo_only || reach == Origin::Kumo) && (tool.mcp || route == Route::Model)
        })
        .map(|tool| ToolSchema {
            name: tool.wire,
            description: tool.description,
            parameters: (tool.parameters)(),
        })
        .collect()
}

/// The JSON Schema of a tool's arguments, from the struct that parses them.
fn parameters<A: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(A)).expect("a generated schema is JSON")
}

/// Parses `args` into the tool's closed struct and hashes the tool and the parsed fields, so
/// whitespace and key order don't change the hash.
fn parse<A: DeserializeOwned + Serialize>(
    wire: &str,
    args: &[u8],
) -> Result<(A, [u8; 32]), ToolError> {
    let parsed: A = serde_json::from_slice(args).map_err(|_| ToolError::Invalid)?;
    let fields = serde_json::to_vec(&parsed).map_err(|_| ToolError::Invalid)?;
    let mut hash = Sha256::new();
    hash.update(wire.as_bytes());
    hash.update([0]);
    hash.update(&fields);
    Ok((parsed, hash.finalize().into()))
}

/// Dispatches `caller`'s call in the run it resolved to (B4). An existing entry for its key
/// always decides, so a retry never changes a refused, running or finished call.
pub(crate) fn call(run: &mut RunCtx<'_>, caller: Caller, name: ToolName, args: &[u8]) -> Step {
    if let Caller::Mcp { launch, .. } = &caller
        && run.mcp != Some(*launch)
    {
        return Step::Answer(ToolOutput::Error(ToolError::NotHere));
    }
    let key = caller.key();
    if let Some(step) = run.ledger.retry(&key, caller.clone(), name, args) {
        return step;
    }
    match first(run, &caller, name, args) {
        Err(error) => {
            run.ledger.refuse(&key, error);
            Step::Answer(ToolOutput::Error(error))
        }
        Ok((tool, prepared)) => {
            run.ledger
                .running(&key, prepared.hash, prepared.output.clone(), caller);
            Step::Commit(CommitCall {
                key,
                thread: run.thread,
                tool: tool.wire,
                hash: prepared.hash,
                write: prepared.write,
                output: prepared.output,
            })
        }
    }
}

/// A key's first call: every check before its effect.
fn first(
    run: &RunCtx<'_>,
    caller: &Caller,
    name: ToolName,
    args: &[u8],
) -> Result<(&'static Tool, Prepared), ToolError> {
    let tool = tool(name).ok_or(ToolError::Unknown)?;
    let offered = match caller {
        Caller::Model { .. } => run.offered.contains(&name),
        Caller::Mcp { .. } => tool.mcp,
    };
    if !offered || (tool.kumo_only && run.reach != Origin::Kumo) {
        return Err(ToolError::NotHere);
    }
    if args.len() > MAX_ARGS {
        return Err(ToolError::TooLarge);
    }
    let prepared = (tool.prepare)(args)?;
    if prepared.output.stored().len() > MAX_OUTPUT {
        return Err(ToolError::TooLarge);
    }
    Ok((tool, prepared))
}

/// The hash a retry of an existing entry is compared with; a retry that doesn't parse can't
/// be the same call.
fn rehash(name: ToolName, args: &[u8]) -> Option<[u8; 32]> {
    let tool = tool(name)?;
    (args.len() <= MAX_ARGS)
        .then(|| (tool.prepare)(args).ok())
        .flatten()
        .map(|prepared| prepared.hash)
}
