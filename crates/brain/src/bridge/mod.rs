//! `kumochid --mcp-bridge <launch>`: the stdio MCP server Claude Code starts from Kumochi's
//! `--mcp-config`. It checks each JSON-RPC message's envelope and id, relays requests over the
//! launch's named pipe to the brain, and writes the brain's answers back with the original id. It
//! decides nothing about tools and logs no message body.
//!
//! The pipe runs on a current-thread tokio runtime because synchronous Windows pipe I/O serializes
//! reads and writes on one file object; stdin and stdout stay on their own std threads.

mod id;
mod stdio;
#[cfg(test)]
mod tests;
mod wire;

use std::io::{self, Read as _, Write as _};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use kumochi_protocol::constants::{BRIDGE_LOG, BRIDGE_PIPE, OBJECT_PREFIX};
use kumochi_protocol::frame::{self, FrameError, Pipe};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::mpsc;
use tokio::time::Instant;

use self::id::JsonRpcId;
use self::stdio::{Body, Incoming, Line, LineReader, RpcError, classify, reply};
use self::wire::{CallKey, Method, Reassembly, ToBrain, ToBridge, chunks};

/// The longest stdin line, newline excluded [guess: call arguments].
pub(crate) const MAX_INCOMING: usize = 512 * 1024;
/// The largest reply [guess: two 1 MiB base64 images plus the envelope, C4c].
pub(crate) const MAX_REPLY: usize = 3 * 1024 * 1024;

/// Why the bridge stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// The launch value isn't 32 lowercase hex digits.
    #[error("bad launch value")]
    Launch,
    /// The pipe couldn't be opened or its server checked.
    #[error("can't connect to the brain")]
    Connect(#[source] io::Error),
    /// The pipe's server isn't this executable.
    #[error("the pipe's server isn't Kumochi's brain")]
    NotOurBrain,
    /// Stdout closed.
    #[error("stdout closed")]
    Stdout,
    /// The launch's deadline passed.
    #[error("the launch's deadline passed")]
    Deadline,
}

/// The mode's body: connects to the launch's pipe, then relays until stdin ends, the pipe ends
/// or the deadline passes.
///
/// # Errors
/// [`BridgeError`]; a lost pipe isn't one, since the launch ends with it.
pub fn serve(launch: &str) -> Result<(), BridgeError> {
    // A bridge that failed to start would make the audit refuse every launch, so logging is optional.
    if crate::logging::init(BRIDGE_LOG).is_err() {
        crate::logging::install_panic_hook();
    }
    if launch.len() != 32
        || !launch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BridgeError::Launch);
    }
    let session = crate::platform::session_id().map_err(BridgeError::Connect)?;
    let name = format!(r"\\.\pipe\{OBJECT_PREFIX}-{session}-{BRIDGE_PIPE}-{launch}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(BridgeError::Connect)?;
    let (lines, input) = mpsc::channel(16);
    std::thread::spawn(move || read_stdin(&lines));
    let (out, written) = std_mpsc::channel::<Vec<u8>>();
    let writer = std::thread::spawn(move || -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        for line in written {
            stdout.write_all(&line)?;
            stdout.flush()?;
        }
        Ok(())
    });
    let relayed = runtime.block_on(async move {
        let own = std::env::current_exe().map_err(BridgeError::Connect)?;
        let pipe = connect(&name, &own)?;
        relay(pipe, input, out).await
    });
    let flushed = writer.join().is_ok_and(|written| written.is_ok());
    relayed?;
    if flushed {
        Ok(())
    } else {
        Err(BridgeError::Stdout)
    }
}

/// Opens `name` and checks its server runs `expected`, so a process that took the name first
/// gets nothing.
fn connect(name: &str, expected: &std::path::Path) -> Result<NamedPipeClient, BridgeError> {
    let pipe = ClientOptions::new()
        .open(name)
        .map_err(BridgeError::Connect)?;
    let server = crate::platform::server_image(&pipe).map_err(BridgeError::Connect)?;
    if !server
        .as_os_str()
        .eq_ignore_ascii_case(expected.as_os_str())
    {
        return Err(BridgeError::NotOurBrain);
    }
    Ok(pipe)
}

/// One stdin event.
#[derive(Debug)]
pub(crate) enum Input {
    Line(Line),
    End,
}

fn read_stdin(lines: &mpsc::Sender<Input>) {
    let mut reader = LineReader::default();
    let mut stdin = io::stdin().lock();
    let mut chunk = vec![0; 64 * 1024];
    let mut ready = Vec::new();
    loop {
        let read = match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        reader.push(&chunk[..read], &mut ready);
        for line in ready.drain(..) {
            if lines.blocking_send(Input::Line(line)).is_err() {
                return;
            }
        }
    }
    if let Some(line) = reader.finish()
        && lines.blocking_send(Input::Line(line)).is_err()
    {
        return;
    }
    lines.blocking_send(Input::End).unwrap_or_default();
}

/// Relays stdin lines to the brain over `pipe` and its answers to `out`, one request at a time.
pub(crate) async fn relay<P>(
    pipe: P,
    mut input: mpsc::Receiver<Input>,
    out: std_mpsc::Sender<Vec<u8>>,
) -> Result<(), BridgeError>
where
    P: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (answers, mut brain) = mpsc::channel::<ToBridge>(4);
    let reading = tokio::spawn(async move {
        while let Ok(Some(message)) = crate::ipc::read(&mut reader, Pipe::Data).await {
            if answers.send(message).await.is_err() {
                break;
            }
        }
    });
    let emit = |line: Vec<u8>| out.send(line).map_err(|_| BridgeError::Stdout);
    let mut in_flight: Option<(JsonRpcId, CallKey, Reassembly)> = None;
    let mut deadline: Option<Instant> = None;
    let result = 'relay: loop {
        let expiry = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            line = input.recv() => match line {
                None | Some(Input::End) => break Ok(()),
                Some(Input::Line(Line::TooLong)) => {
                    emit(reply(None, Body::Local(RpcError::InvalidRequest)))?;
                }
                Some(Input::Line(Line::Text(text))) => match classify(&text) {
                    Incoming::Blank | Incoming::Notification { initialized: false } => {}
                    Incoming::Notification { initialized: true } => {
                        if send(&mut writer, &ToBrain::Initialized).await.is_err() {
                            break Ok(());
                        }
                    }
                    Incoming::Invalid { id, error } => emit(reply(id.as_ref(), Body::Local(error)))?,
                    Incoming::Request { id, method: Method::Ping, .. } => {
                        emit(reply(Some(&id), Body::Result(b"{}")))?;
                    }
                    Incoming::Request { id, .. } if in_flight.is_some() => {
                        emit(reply(Some(&id), Body::Local(RpcError::InvalidRequest)))?;
                    }
                    Incoming::Request { id, method, params } => {
                        let key = id.key();
                        for (chunk, last) in chunks(&params) {
                            let request = ToBrain::Request { key: key.clone(), method, chunk: chunk.to_vec(), last };
                            if send(&mut writer, &request).await.is_err() {
                                break 'relay Ok(());
                            }
                        }
                        in_flight = Some((id, key, Reassembly::new(MAX_REPLY)));
                    }
                },
            },
            message = brain.recv() => match message {
                None => break Ok(()),
                Some(ToBridge::Hello { remaining_ms }) => {
                    deadline = Some(Instant::now() + Duration::from_millis(remaining_ms));
                }
                Some(ToBridge::Reply { key, chunk, last, error }) => {
                    let Some((id, expected, reassembly)) = &mut in_flight else { continue };
                    if *expected != key {
                        continue;
                    }
                    reassembly.push(&chunk);
                    if !last {
                        continue;
                    }
                    let whole = reassembly.finish();
                    let line = match &whole {
                        Ok(body) if error => reply(Some(id), Body::Error(body)),
                        Ok(body) => reply(Some(id), Body::Result(body)),
                        Err(_) => reply(Some(id), Body::Local(RpcError::Internal)),
                    };
                    in_flight = None;
                    emit(line)?;
                }
            },
            () = expiry => {
                if let Some((id, ..)) = in_flight.take() {
                    emit(reply(Some(&id), Body::Local(RpcError::Internal)))?;
                }
                break Err(BridgeError::Deadline);
            }
        }
    };
    reading.abort();
    result
}

async fn send(pipe: &mut (impl AsyncWrite + Unpin), message: &ToBrain) -> Result<(), FrameError> {
    pipe.write_all(&frame::encode(message, Pipe::Data)?).await?;
    Ok(())
}
