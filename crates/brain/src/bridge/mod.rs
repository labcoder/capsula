//! `kumochid --mcp-bridge <launch>`: the stdio MCP server Claude Code starts from Kumochi's
//! `--mcp-config`. It checks each JSON-RPC message's envelope and id, relays requests over the
//! launch's named pipe to the brain, and writes the brain's answers back with the original id. It
//! decides nothing about tools and logs no message body.
//!
//! The pipe runs on a current-thread tokio runtime because synchronous Windows pipe I/O serializes
//! reads and writes on one file object; stdin and stdout stay on their own std threads.

mod id;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "C5b binds a launch's run and C4b-2 dispatches its calls"
    )
)]
pub(crate) mod listener;
mod stdio;
#[cfg(test)]
mod tests;
mod wire;

use std::future::Future;
use std::io::{self, Read, Write};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use kumochi_protocol::constants::{BRIDGE_LOG, BRIDGE_PIPE, OBJECT_PREFIX};
use kumochi_protocol::frame::{self, Pipe};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

pub(crate) use self::id::LaunchId;
pub(crate) use self::wire::{CallKey, Method, ToBrain, ToBridge};

use self::id::JsonRpcId;
use self::stdio::{Body, Incoming, Line, LineReader, RpcError, classify, reply};
use self::wire::{Reassembly, chunks};
use crate::platform::{self, PipeClient};

/// The longest stdin line, newline excluded [guess: call arguments].
pub(crate) const MAX_INCOMING: usize = 512 * 1024;
/// The largest reply [guess: two 1 MiB base64 images plus the envelope, C4c].
pub(crate) const MAX_REPLY: usize = 3 * 1024 * 1024;
/// How long the brain has to send `Hello` once the bridge connects [guess].
pub(crate) const HELLO_WAIT: Duration = Duration::from_secs(5);
/// How long the bridge waits at the end for stdout to take its last lines [guess].
const FLUSH_WAIT: Duration = Duration::from_secs(1);
/// Lines waiting for the stdout thread; a reader that stops reading stalls the relay, never memory.
const STDOUT_QUEUE: usize = 4;

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
    /// A stdin or stdout thread couldn't start.
    #[error("can't start the bridge's threads")]
    Start(#[source] io::Error),
    /// The brain sent no `Hello` within [`HELLO_WAIT`], or sent something else first.
    #[error("the brain sent no hello")]
    NoHello,
    /// Stdout closed, or didn't take the last lines within [`FLUSH_WAIT`].
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
    let session = platform::session_id().map_err(BridgeError::Connect)?;
    let name = pipe_name(session, launch);
    let own = std::env::current_exe().map_err(BridgeError::Connect)?;
    run(|| connect(&name, &own), io::stdin(), io::stdout())
}

/// The launch's pipe, for one Windows session and the launch's 32 hex digits.
fn pipe_name(session: u32, launch: &str) -> String {
    format!(r"\\.\pipe\{OBJECT_PREFIX}-{session}-{BRIDGE_PIPE}-{launch}")
}

/// Runs the relay between `stdin`, `stdout` and the pipe `open` gives. Nothing here waits on
/// stdout without a bound: a reader that stops reading costs at most [`FLUSH_WAIT`] at the end.
pub(crate) fn run<P, R, W>(
    open: impl FnOnce() -> Result<P, BridgeError>,
    stdin: R,
    stdout: W,
) -> Result<(), BridgeError>
where
    P: AsyncRead + AsyncWrite + Send + 'static,
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(BridgeError::Connect)?;
    let (out, written) = mpsc::channel(STDOUT_QUEUE);
    let (flushed, done) = std_mpsc::sync_channel(1);
    let writer = std::thread::Builder::new()
        .name("bridge-stdout".to_owned())
        .spawn(move || {
            let wrote = write_stdout(stdout, written).is_ok();
            flushed.send(wrote).unwrap_or_default();
        })
        .map_err(BridgeError::Start)?;
    let (lines, input) = mpsc::channel(16);
    let (ended, stdin_end) = watch::channel(false);
    let reader = std::thread::Builder::new()
        .name("bridge-stdin".to_owned())
        .spawn(move || read_stdin(stdin, &lines, &ended));
    if let Err(error) = reader {
        // The writer has nothing queued, so it ends as soon as its sender is gone.
        drop(out);
        writer.join().unwrap_or_default();
        return Err(BridgeError::Start(error));
    }
    let relayed = runtime.block_on(async move { relay(open()?, input, stdin_end, out).await });
    drop(runtime);
    let flushed = done.recv_timeout(FLUSH_WAIT).unwrap_or(false);
    relayed?;
    if flushed {
        Ok(())
    } else {
        Err(BridgeError::Stdout)
    }
}

/// Opens `name` and checks its server runs `expected`, so a process that took the name first
/// gets nothing.
fn connect(name: &str, expected: &std::path::Path) -> Result<PipeClient, BridgeError> {
    let pipe = platform::open_pipe(name).map_err(BridgeError::Connect)?;
    let server = platform::server_image(&pipe).map_err(BridgeError::Connect)?;
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

#[cfg(test)]
impl Input {
    pub(crate) fn from_test_line(line: &[u8]) -> Self {
        Self::Line(Line::Text(line.to_vec()))
    }

    pub(crate) fn from_test_json(value: &serde_json::Value) -> Self {
        Self::Line(Line::Text(value.to_string().into_bytes()))
    }
}

/// Reads stdin into `lines`. `ended` is set the moment a read sees the end, before the last
/// sends, which can block on a full queue: the relay learns of the end even then. A reader
/// blocked on a full queue before its end never sees it; the deadline ends that case.
fn read_stdin(mut stdin: impl Read, lines: &mpsc::Sender<Input>, ended: &watch::Sender<bool>) {
    let mut reader = LineReader::default();
    let mut chunk = vec![0; 64 * 1024];
    let mut ready = Vec::new();
    loop {
        let read = match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        // A full line queue blocks here, before the next read can see the end.
        reader.push(&chunk[..read], &mut ready);
        for line in ready.drain(..) {
            if lines.blocking_send(Input::Line(line)).is_err() {
                return;
            }
        }
    }
    ended.send_replace(true);
    if let Some(line) = reader.finish()
        && lines.blocking_send(Input::Line(line)).is_err()
    {
        return;
    }
    lines.blocking_send(Input::End).unwrap_or_default();
}

fn write_stdout(mut stdout: impl Write, mut lines: mpsc::Receiver<Vec<u8>>) -> io::Result<()> {
    while let Some(line) = lines.blocking_recv() {
        stdout.write_all(&line)?;
        stdout.flush()?;
    }
    Ok(())
}

/// Why the relay stopped.
enum Stop {
    /// stdin ended, or the pipe did.
    Ended,
    Deadline,
    Stdout,
}

/// What ends every wait: the deadline, the pipe's end and stdin's.
struct Ends {
    deadline: Instant,
    /// Resolves when the pipe's reader stops.
    lost: oneshot::Receiver<()>,
    /// Set once stdin's reader has seen its end.
    stdin: watch::Receiver<bool>,
}

/// Waits for `work` unless an end comes first. A known end wins over work that's also ready.
async fn within<T>(work: impl Future<Output = T>, ends: &mut Ends) -> Result<T, Stop> {
    tokio::select! {
        biased;
        () = tokio::time::sleep_until(ends.deadline) => Err(Stop::Deadline),
        () = stdin_ended(&mut ends.stdin) => Err(Stop::Ended),
        _ = &mut ends.lost => Err(Stop::Ended),
        done = work => Ok(done),
    }
}

/// Resolves once stdin's reader has seen its end; never if it stopped without one.
async fn stdin_ended(stdin: &mut watch::Receiver<bool>) {
    if stdin.wait_for(|ended| *ended).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The relay's state for one connection.
struct Relay<W> {
    pipe: W,
    out: mpsc::Sender<Vec<u8>>,
    ends: Ends,
    in_flight: Option<(JsonRpcId, CallKey, Reassembly)>,
}

impl<W: AsyncWrite + Unpin> Relay<W> {
    /// Writes one frame; a write the deadline or the pipe's end cut short ends the connection.
    async fn send(&mut self, message: &ToBrain) -> Result<(), Stop> {
        let frame = frame::encode(message, Pipe::Data).map_err(|_| Stop::Ended)?;
        within(self.pipe.write_all(&frame), &mut self.ends)
            .await?
            .map_err(|_| Stop::Ended)
    }

    async fn emit(&mut self, line: Vec<u8>) -> Result<(), Stop> {
        within(self.out.send(line), &mut self.ends)
            .await?
            .map_err(|_| Stop::Stdout)
    }

    async fn line(&mut self, line: Line) -> Result<(), Stop> {
        let text = match line {
            Line::TooLong => {
                return self
                    .emit(reply(None, Body::Local(RpcError::InvalidRequest)))
                    .await;
            }
            Line::Text(text) => text,
        };
        match classify(&text) {
            Incoming::Blank | Incoming::Notification { initialized: false } => Ok(()),
            Incoming::Notification { initialized: true } => self.send(&ToBrain::Initialized).await,
            Incoming::Invalid { id, error } => {
                self.emit(reply(id.as_ref(), Body::Local(error))).await
            }
            Incoming::Request {
                id,
                method: Method::Ping,
                ..
            } => self.emit(reply(Some(&id), Body::Result(b"{}"))).await,
            Incoming::Request { id, .. } if self.in_flight.is_some() => {
                self.emit(reply(Some(&id), Body::Local(RpcError::InvalidRequest)))
                    .await
            }
            Incoming::Request { id, method, params } => {
                let key = id.key();
                self.in_flight = Some((id, key.clone(), Reassembly::new(MAX_REPLY)));
                for (chunk, last) in chunks(params) {
                    let request = ToBrain::Request {
                        key: key.clone(),
                        method,
                        chunk: chunk.to_vec(),
                        last,
                    };
                    self.send(&request).await?;
                }
                Ok(())
            }
        }
    }

    async fn brain(&mut self, message: ToBridge) -> Result<(), Stop> {
        let ToBridge::Reply {
            key,
            chunk,
            last,
            error,
        } = message
        else {
            return Ok(());
        };
        let Some((id, expected, reassembly)) = &mut self.in_flight else {
            return Ok(());
        };
        if *expected != key {
            return Ok(());
        }
        reassembly.push(&chunk);
        if !last {
            return Ok(());
        }
        let line = match reassembly.finish() {
            Ok(body) if error => reply(Some(id), Body::Error(&body)),
            Ok(body) => reply(Some(id), Body::Result(&body)),
            Err(_) => reply(Some(id), Body::Local(RpcError::Internal)),
        };
        self.in_flight = None;
        self.emit(line).await
    }
}

/// Relays stdin lines to the brain over `pipe` and its answers to `out`, one request at a time,
/// once the brain's `Hello` has come. Every wait, pipe writes included, ends at the deadline,
/// and at stdin's end once its reader has seen it (`stdin_end`).
pub(crate) async fn relay<P>(
    pipe: P,
    mut input: mpsc::Receiver<Input>,
    mut stdin_end: watch::Receiver<bool>,
    out: mpsc::Sender<Vec<u8>>,
) -> Result<(), BridgeError>
where
    P: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, writer) = tokio::io::split(pipe);
    let hello = tokio::select! {
        biased;
        () = stdin_ended(&mut stdin_end) => return Ok(()),
        hello = tokio::time::timeout(HELLO_WAIT, crate::ipc::read(&mut reader, Pipe::Data)) => hello,
    };
    let Ok(Ok(Some(ToBridge::Hello { remaining_ms }))) = hello else {
        return Err(BridgeError::NoHello);
    };
    let (ended, lost) = oneshot::channel::<()>();
    let (answers, mut brain) = mpsc::channel::<ToBridge>(4);
    let reading = tokio::spawn(async move {
        let _ended = ended;
        while let Ok(Some(message)) = crate::ipc::read(&mut reader, Pipe::Data).await {
            if answers.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut relay = Relay {
        pipe: writer,
        out,
        ends: Ends {
            deadline: Instant::now() + Duration::from_millis(remaining_ms),
            lost,
            stdin: stdin_end,
        },
        in_flight: None,
    };
    let stop = loop {
        let step = tokio::select! {
            line = input.recv() => match line {
                None | Some(Input::End) => Err(Stop::Ended),
                Some(Input::Line(line)) => relay.line(line).await,
            },
            message = brain.recv() => match message {
                None => Err(Stop::Ended),
                Some(message) => relay.brain(message).await,
            },
            () = tokio::time::sleep_until(relay.ends.deadline) => Err(Stop::Deadline),
        };
        if let Err(stop) = step {
            break stop;
        }
    };
    reading.abort();
    match stop {
        Stop::Ended => Ok(()),
        Stop::Stdout => Err(BridgeError::Stdout),
        Stop::Deadline => {
            if let Some((id, ..)) = relay.in_flight.take() {
                // Never waits: a full queue means stdout isn't being read anyway.
                relay
                    .out
                    .try_send(reply(Some(&id), Body::Local(RpcError::Internal)))
                    .unwrap_or_default();
            }
            Err(BridgeError::Deadline)
        }
    }
}
