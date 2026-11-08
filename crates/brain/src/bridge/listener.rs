//! The brain's end of a launch's bridge pipe: one session per launch, the checks on its one
//! client, and the admission of each `tools/call` against the launch's audit gate and bound run.
//!
//! The checks refuse stray and unrelated connections. They aren't a defence against code running
//! as the user, which the pipe's DACL admits anyway: the pipe reports its opener's pid, and that
//! number can name another process once the opener exits.

use std::io;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use kumochi_protocol::constants::MCP_SERVER_NAME;
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::RunId;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::time::Instant;

use super::MAX_INCOMING;
use super::id::{LaunchId, is_call_key};
use super::stdio::RpcError;
use super::wire::{CallKey, Method, Reassembly, ToBrain, ToBridge, chunks};
use crate::context::ToolSchema;
use crate::engines::claude_code::audit::AuditGate;
pub(crate) use crate::platform::BridgePeerError as ClientRefused;
use crate::platform::{self, Instance, Job, PipeServer};

/// The MCP versions this server speaks; a client asking for another gets the first.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];
/// What the model reads when a call isn't admitted.
const NOT_ADMITTED: &str = "Kumochi isn't taking tool calls from this session now.";
/// What the model reads for an admitted call until tools run over MCP.
const UNAVAILABLE: &str = "This tool isn't available over MCP yet.";

/// The launch's one pipe instance, created before its name is published.
pub(crate) struct Listener {
    launch: LaunchId,
    name: String,
    server: PipeServer,
}

impl Listener {
    /// Creates the pipe as the only instance of its name, owner-only and local.
    ///
    /// # Errors
    /// Fails when the name is taken, so a squatter fails the launch, or the OS refuses the pipe.
    pub(crate) fn create(launch: LaunchId) -> io::Result<Self> {
        let name = super::pipe_name(platform::session_id()?, &launch.hex());
        let server = platform::create_owner_only(&name, Instance::Only)?;
        Ok(Self {
            launch,
            name,
            server,
        })
    }

    /// The pipe's name, for `mcp.json` and the bridge's argv.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Accepts the first client, checks it, sends `Hello`, then serves until the session ends.
    /// The listener is used up: there's never a second accept, refusals included.
    pub(crate) async fn serve(mut self, launch: LaunchContext<'_>) -> SessionEnd {
        if !matches!(
            tokio::time::timeout_at(launch.deadline, self.server.connect()).await,
            Ok(Ok(()))
        ) {
            return SessionEnd::NoClient;
        }
        let client: Result<platform::BridgePeer, ClientRefused> =
            platform::admit_bridge_client(&self.server, launch.job, launch.brain_exe);
        let end = match client {
            Ok(_client) => session(&mut self.server, self.launch, &launch).await,
            Err(refused) => SessionEnd::Refused(refused),
        };
        // Dropping alone can leave the client waiting: the runtime may hold the handle for a
        // read it started. Disconnecting ends the client's end now; an error means it's gone.
        self.server.disconnect().unwrap_or_default();
        end
    }
}

/// What the listener needs from the launch.
pub(crate) struct LaunchContext<'a> {
    /// This launch's own run job, never the brain's.
    pub(crate) job: &'a Job,
    /// The brain's executable, which the bridge runs.
    pub(crate) brain_exe: &'a Path,
    /// The launch's deadline.
    pub(crate) deadline: Instant,
    /// The tools frozen with the launch.
    pub(crate) tools: &'a [ToolSchema],
    pub(crate) gate: AuditGate,
    pub(crate) binding: Binding,
}

impl LaunchContext<'_> {
    /// Admits a call, or refuses it. The gate's load is the instant of admission: a failure
    /// before it refuses the call, and one after it doesn't undo the admission.
    fn admit(&self, launch: LaunchId, key: &CallKey) -> Result<McpOrigin, NotAdmitted> {
        let run = self.binding.0.get().copied().ok_or(NotAdmitted)?;
        if Instant::now() >= self.deadline || !self.gate.may_call() {
            return Err(NotAdmitted);
        }
        Ok(McpOrigin {
            run,
            launch,
            key: key.clone(),
        })
    }
}

/// The run a launch serves: set once, at bind, and never rebound.
#[derive(Clone, Debug, Default)]
pub(crate) struct Binding(Arc<OnceLock<RunId>>);

impl Binding {
    /// # Errors
    /// [`AlreadyBound`] when a run is set; the first one stays.
    pub(crate) fn bind(&self, run: RunId) -> Result<(), AlreadyBound> {
        self.0.set(run).map_err(|_| AlreadyBound)
    }
}

/// A second bind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AlreadyBound;

/// A call that wasn't admitted: unbound, not passed, failed, or past the deadline.
#[derive(Debug, PartialEq, Eq)]
struct NotAdmitted;

/// A call's origin, frozen at admission, before any queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpOrigin {
    pub(crate) run: RunId,
    pub(crate) launch: LaunchId,
    pub(crate) key: CallKey,
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The client failed a check: no `Hello` was sent and nothing was read.
    Refused(ClientRefused),
    /// No client, or no `Hello` written, before the deadline.
    NoClient,
    /// The bridge closed its end between requests.
    Closed,
    /// The deadline passed mid-session.
    Deadline,
    /// A frame was malformed, a chunk didn't continue its request, or the pipe ended inside one.
    Broken,
}

/// The one request being reassembled; its first chunk fixes its key and method.
struct Assembly {
    key: CallKey,
    method: Method,
    body: Reassembly,
}

/// Sends `Hello`, then answers one request at a time until the session ends.
async fn session<P>(mut pipe: P, launch: LaunchId, context: &LaunchContext<'_>) -> SessionEnd
where
    P: AsyncRead + AsyncWrite + Unpin,
{
    let left = context.deadline.saturating_duration_since(Instant::now());
    let hello = ToBridge::Hello {
        remaining_ms: u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
    };
    if send(&mut pipe, &hello, context.deadline).await.is_err() {
        return SessionEnd::NoClient;
    }
    let mut assembly: Option<Assembly> = None;
    loop {
        let read = tokio::time::timeout_at(
            context.deadline,
            crate::ipc::read::<ToBrain>(&mut pipe, Pipe::Data),
        )
        .await;
        let message = match read {
            Err(_) => return SessionEnd::Deadline,
            Ok(Err(_)) => return SessionEnd::Broken,
            Ok(Ok(message)) => message,
        };
        let (key, method, chunk, last) = match (message, assembly.is_some()) {
            (
                Some(ToBrain::Request {
                    key,
                    method,
                    chunk,
                    last,
                }),
                _,
            ) => (key, method, chunk, last),
            // An end or `Initialized` inside a request: the bridge never interleaves.
            (_, true) => return SessionEnd::Broken,
            (None, false) => return SessionEnd::Closed,
            (Some(ToBrain::Initialized), false) => continue,
        };
        let mut current = match assembly.take() {
            None if is_call_key(&key) => Assembly {
                key,
                method,
                body: Reassembly::new(MAX_INCOMING),
            },
            Some(current) if current.key == key && current.method == method => current,
            _ => return SessionEnd::Broken,
        };
        current.body.push(&chunk);
        if !last {
            assembly = Some(current);
            continue;
        }
        let (body, error) = match current.body.finish() {
            Ok(params) => answer(current.method, &params, launch, &current.key, context),
            Err(_) => (RpcError::Internal.json().as_bytes().to_vec(), true),
        };
        for (chunk, last) in chunks(&body) {
            let reply = ToBridge::Reply {
                key: current.key.clone(),
                chunk: chunk.to_vec(),
                last,
                error,
            };
            if let Err(end) = send(&mut pipe, &reply, context.deadline).await {
                return end;
            }
        }
    }
}

/// One request's answer: the body and whether it's a JSON-RPC error.
fn answer(
    method: Method,
    params: &[u8],
    launch: LaunchId,
    key: &CallKey,
    context: &LaunchContext<'_>,
) -> (Vec<u8>, bool) {
    let body = match method {
        Method::Initialize => initialize(params),
        Method::ToolsList => json!({
            "tools": context.tools.iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.parameters,
            })).collect::<Vec<_>>(),
        }),
        Method::Ping => json!({}),
        Method::ToolsCall => {
            if !is_offered(params, context.tools) {
                return (RpcError::InvalidParams.json().as_bytes().to_vec(), true);
            }
            let text = match context.admit(launch, key) {
                // shortcut: an admitted call isn't run; C4b-2 hands its origin to 14a's dispatcher.
                Ok(_origin) => UNAVAILABLE,
                Err(NotAdmitted) => NOT_ADMITTED,
            };
            json!({"content": [{"type": "text", "text": text}], "isError": true})
        }
    };
    (body.to_string().into_bytes(), false)
}

fn initialize(params: &[u8]) -> Value {
    let asked = serde_json::from_slice::<Value>(params).ok();
    let asked = asked
        .as_ref()
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .into_iter()
        .find(|version| Some(*version) == asked)
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
    })
}

/// `tools/call`'s params: named, and nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallParams {
    name: String,
    #[cfg_attr(
        test,
        expect(dead_code, reason = "C4b-2 hands the arguments to the dispatcher")
    )]
    #[serde(default)]
    arguments: Option<Map<String, Value>>,
    #[serde(rename = "_meta", default)]
    _meta: Option<Map<String, Value>>,
}

/// Whether `params` is an object naming one of the launch's tools, with object arguments.
fn is_offered(params: &[u8], tools: &[ToolSchema]) -> bool {
    // An array would deserialize into the struct by position; MCP's params are named.
    let Ok(Value::Object(params)) = serde_json::from_slice::<Value>(params) else {
        return false;
    };
    serde_json::from_value::<CallParams>(Value::Object(params))
        .is_ok_and(|call| tools.iter().any(|tool| tool.name == call.name))
}

/// Writes one frame before the deadline.
async fn send<P: AsyncWrite + Unpin>(
    pipe: &mut P,
    message: &ToBridge,
    deadline: Instant,
) -> Result<(), SessionEnd> {
    let frame = frame::encode(message, Pipe::Data).map_err(|_| SessionEnd::Broken)?;
    match tokio::time::timeout_at(deadline, pipe.write_all(&frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SessionEnd::Closed),
        Err(_) => Err(SessionEnd::Deadline),
    }
}

#[cfg(test)]
mod tests;
