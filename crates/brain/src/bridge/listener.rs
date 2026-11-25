//! The brain's end of a launch's bridge pipe: one session per launch, the checks on its one
//! client, and the admission of each `tools/call` against the launch's audit gate and bound run.
//!
//! The checks refuse stray and unrelated connections. They aren't a defence against code running
//! as the user, which the pipe's DACL admits anyway: the pipe reports its opener's pid, and that
//! number can name another process once the opener exits.

use std::future::{pending, poll_fn};
use std::pin::{Pin, pin};
use std::sync::{Arc, OnceLock};
use std::task::{Poll, ready};
use std::{io, path::Path};

use kumochi_protocol::constants::MCP_SERVER_NAME;
use kumochi_protocol::frame::{self, Pipe};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::MAX_INCOMING;
use super::id::{LaunchId, is_call_key};
use super::stdio::RpcError;
use super::wire::{CallKey, Method, Reassembly, ToBrain, ToBridge, chunks};
use crate::context::ToolSchema;
use crate::coordinator::BuildKey;
use crate::engines::ToolName;
use crate::engines::claude_code::audit::AuditGate;
use crate::platform::{self, ClientRefused, Instance, Job, PipeServer};

/// The MCP versions this server speaks; a client asking for another gets the first.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];
/// What the model reads when a call isn't admitted.
const NOT_ADMITTED: &str = "Kumochi isn't taking tool calls from this session now.";
/// What the model reads for an admitted call when no brain takes the launch's calls.
const UNAVAILABLE: &str = "This tool isn't available over MCP yet.";
/// What the model reads when its call ended before the brain answered it.
const ENDED: &str = "This call ended before it finished.";

/// The launch's one pipe instance, created before its name is published.
pub(crate) struct Listener {
    launch: LaunchId,
    #[cfg(all(test, windows))]
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
            #[cfg(all(test, windows))]
            name,
            server,
        })
    }

    #[cfg(all(test, windows))]
    fn name(&self) -> &str {
        &self.name
    }

    /// Accepts the first client, checks it, sends `Hello`, then serves until the session ends.
    /// The listener is used up: there's never a second accept, refusals included.
    pub(crate) async fn serve(
        mut self,
        job: &Job,
        brain_exe: &Path,
        launch: LaunchContext<'_>,
    ) -> SessionEnd {
        if !matches!(
            tokio::time::timeout_at(launch.deadline, self.server.connect()).await,
            Ok(Ok(()))
        ) {
            return SessionEnd::NoClient;
        }
        let end = match platform::bridge_peer(&self.server, job, brain_exe) {
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
    /// The launch's deadline.
    pub(crate) deadline: Instant,
    /// The tools frozen with the launch.
    pub(crate) tools: &'a [ToolSchema],
    pub(crate) gate: AuditGate,
    pub(crate) binding: Binding,
    /// The brain's mailbox for admitted calls.
    pub(crate) calls: Option<mpsc::Sender<McpCall>>,
}

impl LaunchContext<'_> {
    /// Admits a call, or refuses it, with the instant its content may leave by: the bound
    /// effective deadline. The gate's load is the instant of admission: a failure before it
    /// refuses the call, and one after it doesn't undo the admission.
    fn admit(&self, launch: LaunchId, key: &CallKey) -> Result<(McpOrigin, Instant), NotAdmitted> {
        let (build, until) = self.binding.0.get().copied().flatten().ok_or(NotAdmitted)?;
        let until = until.min(self.deadline);
        if !self.releases(until) {
            return Err(NotAdmitted);
        }
        let origin = McpOrigin {
            build,
            launch,
            key: key.clone(),
        };
        Ok((origin, until))
    }

    /// Whether a tool's content may still leave, checked again before each frame of it.
    fn releases(&self, until: Instant) -> bool {
        Instant::now() < until && self.gate.may_call()
    }
}

/// The build a launch's calls belong to and its effective deadline, the earliest of the launch's,
/// its question's and its build's: set once, at bind, and never rebound. A launch bound to `None` takes no
/// call.
#[derive(Clone, Debug, Default)]
pub(crate) struct Binding(Arc<OnceLock<Option<(BuildKey, Instant)>>>);

impl Binding {
    /// # Errors
    /// [`AlreadyBound`] when it's bound; the first bind stays.
    pub(crate) fn bind(&self, bound: Option<(BuildKey, Instant)>) -> Result<(), AlreadyBound> {
        self.0.set(bound).map_err(|_| AlreadyBound)
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
    pub(crate) build: BuildKey,
    pub(crate) launch: LaunchId,
    pub(crate) key: CallKey,
}

/// An admitted call on its way to the brain, with the one reply it waits for.
pub(crate) struct McpCall {
    pub(crate) origin: McpOrigin,
    pub(crate) tool: ToolName,
    /// The `arguments` object as the client wrote it, or `None` when it's over
    /// [`MAX_ARGS`](crate::dispatch::MAX_ARGS): kept out of the mailbox, so the dispatcher refuses
    /// it by size.
    pub(crate) args: Option<Vec<u8>>,
    pub(crate) reply: oneshot::Sender<McpReply>,
}

/// The brain's answer to a call: the tool's text, and whether it reads as an error.
pub(crate) struct McpReply {
    pub(crate) text: String,
    pub(crate) error: bool,
}

// Kinds and lengths only: arguments and answers are the model's and the tool's text.
impl std::fmt::Debug for McpCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCall")
            .field("origin", &self.origin)
            .field("tool", &self.tool)
            .field("args", &self.args.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for McpReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpReply")
            .field("text", &self.text.len())
            .field("error", &self.error)
            .finish()
    }
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
    /// A tool's reply could no longer leave before all of it was written, so none of the rest is.
    Withheld,
}

/// The one request being reassembled; its first chunk fixes its key and method.
struct Assembly {
    key: CallKey,
    method: Method,
    body: Reassembly,
}

/// Sends `Hello`, then answers one request at a time until the authenticated client is gone.
pub(crate) async fn session<P>(pipe: P, launch: LaunchId, context: &LaunchContext<'_>) -> SessionEnd
where
    P: AsyncRead + AsyncWrite + Unpin,
{
    let end = connected(pipe, launch, context).await;
    context.gate.fail();
    end
}

async fn connected<P>(pipe: P, launch: LaunchId, context: &LaunchContext<'_>) -> SessionEnd
where
    P: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let left = context.deadline.saturating_duration_since(Instant::now());
    let hello = ToBridge::Hello {
        remaining_ms: u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
    };
    if send(&mut writer, &hello, context.deadline, None)
        .await
        .is_err()
    {
        return SessionEnd::NoClient;
    }
    let mut assembly: Option<Assembly> = None;
    let mut read = receive(&mut reader, context.deadline).await;
    loop {
        let message = match read {
            Err(end) => return end,
            Ok(message) => message,
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
            (Some(ToBrain::Initialized), false) => {
                read = receive(&mut reader, context.deadline).await;
                continue;
            }
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
            read = receive(&mut reader, context.deadline).await;
            continue;
        }
        let mut next = pin!(async {
            loop {
                let frame = receive(&mut reader, context.deadline).await?;
                if !matches!(frame, Some(ToBrain::Initialized)) {
                    return Ok(frame);
                }
            }
        });
        tokio::select! {
            biased;
            early = next.as_mut() => return match early {
                Err(end) => end,
                Ok(None) => SessionEnd::Closed,
                Ok(Some(_)) => SessionEnd::Broken,
            },
            sent = respond(&mut writer, &mut current, launch, context) => {
                if let Err(end) = sent {
                    return end;
                }
            }
        }
        read = next.await;
    }
}

async fn receive(
    pipe: &mut (impl AsyncRead + Unpin),
    deadline: Instant,
) -> Result<Option<ToBrain>, SessionEnd> {
    tokio::time::timeout_at(deadline, crate::ipc::read(pipe, Pipe::Data))
        .await
        .map_err(|_| SessionEnd::Deadline)?
        .map_err(|_| SessionEnd::Broken)
}

async fn respond(
    pipe: &mut (impl AsyncWrite + Unpin),
    request: &mut Assembly,
    launch: LaunchId,
    context: &LaunchContext<'_>,
) -> Result<(), SessionEnd> {
    let (body, error, release) = match request.body.finish() {
        Ok(params) => answer(request.method, &params, launch, &request.key, context).await,
        Err(_) => (RpcError::Internal.json().as_bytes().to_vec(), true, None),
    };
    let gate = release.map(|_| &context.gate);
    let deadline = release.unwrap_or(context.deadline);
    for (chunk, last) in chunks(&body) {
        let reply = ToBridge::Reply {
            key: request.key.clone(),
            chunk: chunk.to_vec(),
            last,
            error,
        };
        send(pipe, &reply, deadline, gate).await?;
    }
    Ok(())
}

/// One request's answer: the body, whether it's a JSON-RPC error, and for a tool's content the
/// effective deadline its frames must leave by.
async fn answer(
    method: Method,
    params: &[u8],
    launch: LaunchId,
    key: &CallKey,
    context: &LaunchContext<'_>,
) -> (Vec<u8>, bool, Option<Instant>) {
    let mut release = None;
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
            let Some((tool, args)) = offered(params, context.tools) else {
                return (
                    RpcError::InvalidParams.json().as_bytes().to_vec(),
                    true,
                    None,
                );
            };
            let (text, error) = match (context.admit(launch, key), &context.calls) {
                (Err(NotAdmitted), _) => (NOT_ADMITTED.to_owned(), true),
                (Ok(_), None) => (UNAVAILABLE.to_owned(), true),
                (Ok((origin, until)), Some(calls)) => {
                    let (reply, answer) = oneshot::channel();
                    let call = McpCall {
                        origin,
                        tool,
                        args,
                        reply,
                    };
                    match relay(calls, call, answer, context, until).await {
                        Some(reply) => {
                            release = Some(until);
                            (reply.text, reply.error)
                        }
                        None => (ENDED.to_owned(), true),
                    }
                }
            };
            json!({"content": [{"type": "text", "text": text}], "isError": error})
        }
    };
    (body.to_string().into_bytes(), false, release)
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

/// Hands an admitted call to the brain and waits for its answer, both before the effective
/// deadline `until` and the launch's revocation, which come first at every poll, and releases
/// the answer only while the launch would still admit the call; `None` when the call ended
/// unanswered.
async fn relay(
    calls: &mpsc::Sender<McpCall>,
    call: McpCall,
    answer: oneshot::Receiver<McpReply>,
    context: &LaunchContext<'_>,
    until: Instant,
) -> Option<McpReply> {
    let work = async {
        calls.send(call).await.ok()?;
        answer.await.ok()
    };
    let reply = tokio::select! {
        biased;
        () = tokio::time::sleep_until(until) => None,
        () = context.gate.failed() => None,
        reply = work => reply,
    }?;
    context.releases(until).then_some(reply)
}

/// `tools/call`'s params: named, each field once, and nothing else. The arguments stay as the
/// client wrote them, so the dispatcher weighs and checks the original bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallParams<'a> {
    name: String,
    #[serde(borrow, default, deserialize_with = "present")]
    arguments: Option<&'a RawValue>,
    #[serde(rename = "_meta", default)]
    _meta: Option<Map<String, Value>>,
}

/// A present `arguments`, `null` included: only an omitted one means none.
fn present<'de, D: serde::Deserializer<'de>>(
    arguments: D,
) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(arguments).map(Some)
}

/// The launch's tool `params` names and its arguments: an object, or `{}` when omitted.
fn offered(params: &[u8], tools: &[ToolSchema]) -> Option<(ToolName, Option<Vec<u8>>)> {
    // An array would deserialize into the struct by position; MCP's params are named.
    if params.trim_ascii_start().first() != Some(&b'{') {
        return None;
    }
    let call: CallParams<'_> = serde_json::from_slice(params).ok()?;
    let args = call
        .arguments
        .map_or(&b"{}"[..], |raw| raw.get().as_bytes());
    if args.first() != Some(&b'{') {
        return None;
    }
    let tool = tools.iter().find(|tool| tool.name == call.name)?;
    let args = (args.len() <= crate::dispatch::MAX_ARGS).then(|| args.to_vec());
    Some((crate::dispatch::named(tool.name)?, args))
}

/// Writes one frame before the deadline and, for a tool's content, while `gate` holds. Each
/// write to the pipe checks the deadline, then the gate, first, so a pipe that turns writable as
/// either ends takes nothing more and the frame stays unfinished; bytes it already took are sent.
async fn send<P: AsyncWrite + Unpin>(
    pipe: &mut P,
    message: &ToBridge,
    deadline: Instant,
    gate: Option<&AuditGate>,
) -> Result<(), SessionEnd> {
    let frame = frame::encode(message, Pipe::Data).map_err(|_| SessionEnd::Broken)?;
    let mut expiry = pin!(tokio::time::sleep_until(deadline));
    let mut revoked = pin!(async {
        match gate {
            Some(gate) => gate.failed().await,
            None => pending().await,
        }
    });
    let mut written = 0;
    poll_fn(|cx| {
        loop {
            if written == frame.len() {
                return Poll::Ready(Ok(()));
            }
            let expired = expiry.as_mut().poll(cx).is_ready();
            if expired || Instant::now() >= deadline {
                return Poll::Ready(Err(SessionEnd::Deadline));
            }
            let failed = revoked.as_mut().poll(cx).is_ready();
            if failed || gate.is_some_and(|gate| !gate.may_call()) {
                return Poll::Ready(Err(SessionEnd::Withheld));
            }
            match ready!(Pin::new(&mut *pipe).poll_write(cx, &frame[written..])) {
                Ok(0) | Err(_) => return Poll::Ready(Err(SessionEnd::Closed)),
                Ok(wrote) => written += wrote,
            }
        }
    })
    .await
}

#[cfg(test)]
pub(crate) mod tests;
