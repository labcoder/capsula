//! The brain's side of the inherited pair. The shell passes the control leg on descriptor 3 and the
//! data leg on 4; [`adopt`] checks both before the runtime exists, [`respond`] hands them to tokio
//! and runs the responder handshake over them, and the greetings are read only through the pair's
//! own openers, so a [`BoundHello`] never travels apart from the pair that authenticated it. See
//! `docs/development/ipc.md`.

use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdStream;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{ControlToBrain, DataToBrain, Hello, IssueSession};
use kumochi_session::{BoundPair, BrainAwaitsInit, ENVELOPE_LEN, Outgoing, Session, SessionError};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::{Instant, timeout_at};

use super::stream::{RecordReader, RecordWriter};
use crate::ipc;

/// Why the pair was refused. Every refusal ends the attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PairError {
    #[error("an inherited descriptor isn't a connected Unix stream")]
    NotAStream,
    #[error("an inherited descriptor doesn't suppress SIGPIPE")]
    SigPipe,
    #[error("an inherited descriptor has no stable identity")]
    Unstable,
    #[error("both inherited descriptors are one socket")]
    Aliased,
    #[error("an inherited descriptor couldn't be adopted")]
    Adopt,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("the pair's deadline passed")]
    Timeout,
    #[error("the pair's transport failed")]
    Transport,
    #[error("the proof isn't for this pair")]
    Unbound,
    #[error("a greeting was read out of order or twice")]
    Order,
    #[error("the first frame on a leg isn't its greeting")]
    Greeting,
}

/// A socket's identity from `fstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    dev: i64,
    ino: u64,
}

/// Two samples of one descriptor. Darwin numbers a socket's inode lazily from zero, so a zero or
/// a sample that changed is inconclusive, and inconclusive refuses.
fn stable(first: Identity, second: Identity) -> Result<Identity, PairError> {
    if first.ino == 0 || first != second {
        return Err(PairError::Unstable);
    }
    Ok(first)
}

fn identity(fd: BorrowedFd<'_>) -> Result<Identity, PairError> {
    let stat = rustix::fs::fstat(fd).map_err(|_| PairError::NotAStream)?;
    Ok(Identity {
        dev: i64::from(stat.st_dev),
        ino: stat.st_ino,
    })
}

fn socket_option(fd: BorrowedFd<'_>, name: libc::c_int) -> Result<libc::c_int, PairError> {
    let mut value: libc::c_int = 0;
    let mut len = libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("an int fits");
    // SAFETY: `value` and `len` are valid for writes, `len` is `value`'s size, and `fd` is
    // open for the whole call.
    let read = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if read != 0 {
        return Err(PairError::NotAStream);
    }
    Ok(value)
}

/// One inherited end: a connected Unix stream that suppresses SIGPIPE, with a stable identity.
fn checked(fd: OwnedFd) -> Result<(StdStream, Identity), PairError> {
    if socket_option(fd.as_fd(), libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Err(PairError::NotAStream);
    }
    let stream = StdStream::from(fd);
    // std refuses a peer whose family isn't AF_UNIX. Darwin doesn't answer SO_ACCEPTCONN for a
    // Unix socket, so a listener is caught here too: it has no peer.
    stream.peer_addr().map_err(|_| PairError::NotAStream)?;
    if socket_option(stream.as_fd(), libc::SO_NOSIGPIPE)? == 0 {
        return Err(PairError::SigPipe);
    }
    let identity = stable(identity(stream.as_fd())?, identity(stream.as_fd())?)?;
    Ok((stream, identity))
}

fn prepared(stream: &StdStream) -> Result<(), PairError> {
    rustix::io::fcntl_setfd(stream, rustix::io::FdFlags::CLOEXEC).map_err(|_| PairError::Adopt)?;
    stream.set_nonblocking(true).map_err(|_| PairError::Adopt)
}

/// Both inherited ends, checked, close-on-exec and non-blocking. Only [`adopt`] makes one.
#[derive(Debug)]
pub(crate) struct InheritedPair {
    control: StdStream,
    data: StdStream,
}

/// Checks both inherited ends. Needs no runtime, so it can run before one exists. Proves a
/// connected, distinct pair of Unix streams, not who holds the other ends: that's the
/// handshake's job.
///
/// # Errors
/// The first check either end fails, or `Aliased` when both are one socket.
pub(crate) fn adopt(control: OwnedFd, data: OwnedFd) -> Result<InheritedPair, PairError> {
    let (control, control_identity) = checked(control)?;
    let (data, data_identity) = checked(data)?;
    if control_identity == data_identity {
        return Err(PairError::Aliased);
    }
    prepared(&control)?;
    prepared(&data)?;
    Ok(InheritedPair { control, data })
}

/// A deadline is final: work that finishes after it doesn't count, even when its I/O was ready.
fn before(by: Instant) -> Result<(), PairError> {
    if Instant::now() >= by {
        return Err(PairError::Timeout);
    }
    Ok(())
}

async fn exact(io: &mut UnixStream, bytes: &mut [u8], by: Instant) -> Result<(), PairError> {
    before(by)?;
    timeout_at(by, io.read_exact(bytes))
        .await
        .map_err(|_| PairError::Timeout)?
        .map_err(|_| PairError::Transport)?;
    before(by)
}

/// One handshake message, its length taken from the envelope by the state that expects it.
async fn message(
    io: &mut UnixStream,
    by: Instant,
    len: impl FnOnce([u8; ENVELOPE_LEN]) -> Result<usize, SessionError>,
) -> Result<Vec<u8>, PairError> {
    let mut envelope = [0; ENVELOPE_LEN];
    exact(io, &mut envelope, by).await?;
    let mut message = vec![0; len(envelope)?];
    exact(io, &mut message, by).await?;
    Ok(message)
}

async fn send(io: &mut UnixStream, out: &Outgoing, by: Instant) -> Result<(), PairError> {
    before(by)?;
    timeout_at(by, io.write_all(&out.bytes))
        .await
        .map_err(|_| PairError::Timeout)?
        .map_err(|_| PairError::Transport)?;
    before(by)
}

/// The responder's side, in the order the session crate enforces: control, data, then Bind.
async fn handshake(
    control: &mut UnixStream,
    data: &mut UnixStream,
    mut brain: BrainAwaitsInit,
    by: Instant,
) -> Result<(Session, BoundPair), PairError> {
    let init = message(control, by, |envelope| brain.record_len(envelope)).await?;
    let (mut brain, reply) = brain.receive(&init)?;
    send(control, &reply, by).await?;
    let finished = message(control, by, |envelope| brain.record_len(envelope)).await?;
    let brain = brain.receive(&finished)?;
    let init = message(data, by, |envelope| brain.record_len(envelope)).await?;
    let (brain, reply) = brain.receive(&init)?;
    send(data, &reply, by).await?;
    let finished = message(data, by, |envelope| brain.record_len(envelope)).await?;
    let brain = brain.receive(&finished)?;
    let bind = message(control, by, |envelope| brain.record_len(envelope)).await?;
    let (session, proof, ack) = brain.receive(&bind)?;
    send(control, &ack, by).await?;
    Ok((session, proof))
}

/// Hands an adopted pair to the brain's runtime and runs the responder handshake on it, all of it
/// by `by`, returning the pair bound once `BindAck` is written. `brain` is made before the first
/// await, so the caller can wipe its own copy of the secret first.
///
/// # Errors
/// `Adopt` outside a tokio runtime, a session refusal, the deadline, or a transport failure.
pub(crate) async fn respond(
    pair: InheritedPair,
    brain: BrainAwaitsInit,
    by: Instant,
) -> Result<AuthenticatedPair, PairError> {
    let mut control = UnixStream::from_std(pair.control).map_err(|_| PairError::Adopt)?;
    let mut data = UnixStream::from_std(pair.data).map_err(|_| PairError::Adopt)?;
    let (session, proof) = handshake(&mut control, &mut data, brain, by).await?;
    AuthenticatedPair::new(session, &proof, control, data)
}

/// A greeting read through its own authenticated pair. Only [`AuthenticatedPair`]'s greeting
/// reads make one, from a frame they opened themselves, and `check_hello` consumes it.
pub(crate) struct BoundHello {
    greeting: Hello,
}

impl BoundHello {
    pub(crate) fn into_greeting(self) -> Hello {
        self.greeting
    }
}

/// The control greeting's outer variant, for the caller to check as the pipes do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opening {
    Legacy,
    Queued,
    /// A queued reconnect, naming the session it follows.
    Reconnect(IssueSession),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Control,
    /// The control greeting was read; whether it chose the queued mode.
    Data {
        queued: bool,
    },
    /// Both greetings were read, or a read failed.
    Spent,
}

/// A pair whose handshake finished with `BindAck`: both legs' record adapters over one pair cell.
pub(crate) struct AuthenticatedPair {
    control: RecordReader<OwnedReadHalf>,
    data: RecordReader<OwnedReadHalf>,
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "held so the write directions stay open; the ParentBound entry writes through them"
        )
    )]
    writers: [RecordWriter<OwnedWriteHalf>; 2],
    pair: watch::Sender<bool>,
    stage: Stage,
}

impl AuthenticatedPair {
    /// Refuses unless `proof` names both of `session`'s openers.
    fn new(
        session: Session,
        proof: &BoundPair,
        control: UnixStream,
        data: UnixStream,
    ) -> Result<Self, PairError> {
        if !proof.is_for(&session.control.opener) || !proof.is_for(&session.data.opener) {
            return Err(PairError::Unbound);
        }
        let (pair, _) = watch::channel(false);
        let (control_in, control_out) = control.into_split();
        let (data_in, data_out) = data.into_split();
        Ok(Self {
            control: RecordReader::new(control_in, session.control.opener, pair.clone()),
            data: RecordReader::new(data_in, session.data.opener, pair.clone()),
            writers: [
                RecordWriter::new(
                    control_out,
                    session.control.sealer,
                    Pipe::Control,
                    pair.clone(),
                ),
                RecordWriter::new(data_out, session.data.sealer, Pipe::Data, pair.clone()),
            ],
            pair,
            stage: Stage::Control,
        })
    }

    /// Reads the control greeting, one whole frame through this pair's control opener. Succeeds
    /// once, and first.
    ///
    /// # Errors
    /// `Order` on a second call, `Greeting` when the frame isn't a greeting, or the deadline. Any
    /// failure refuses the whole pair.
    pub(crate) async fn control_greeting(
        &mut self,
        by: Instant,
    ) -> Result<(BoundHello, Opening, usize), PairError> {
        let refusing = Refusing(&self.pair);
        if std::mem::replace(&mut self.stage, Stage::Spent) != Stage::Control {
            return Err(PairError::Order);
        }
        let (first, len) = greeting(&mut self.control, Pipe::Control, by).await?;
        let (greeting, opening) = match first {
            ControlToBrain::Hello(hello) => (hello, Opening::Legacy),
            ControlToBrain::QueuedHello(hello) => (hello, Opening::Queued),
            ControlToBrain::QueuedAudioReconnect(hello, previous) => {
                (*hello, Opening::Reconnect(previous))
            }
            _ => return Err(PairError::Greeting),
        };
        std::mem::forget(refusing);
        self.stage = Stage::Data {
            queued: opening != Opening::Legacy,
        };
        Ok((BoundHello { greeting }, opening, len))
    }

    /// Reads the data greeting in the mode the control greeting chose, with the queued echo.
    /// Succeeds once, and only after the control greeting.
    ///
    /// # Errors
    /// `Order` before the control greeting or on a second call, `Greeting` when the frame isn't
    /// the chosen mode's greeting, or the deadline. Any failure refuses the whole pair.
    pub(crate) async fn data_greeting(
        &mut self,
        by: Instant,
    ) -> Result<(BoundHello, Option<IssueSession>, usize), PairError> {
        let refusing = Refusing(&self.pair);
        let Stage::Data { queued } = std::mem::replace(&mut self.stage, Stage::Spent) else {
            return Err(PairError::Order);
        };
        let (first, len) = greeting(&mut self.data, Pipe::Data, by).await?;
        let (greeting, echo) = match (queued, first) {
            (false, DataToBrain::Hello(hello)) => (hello, None),
            (true, DataToBrain::QueuedHello(hello, echo)) => (hello, Some(echo)),
            _ => return Err(PairError::Greeting),
        };
        std::mem::forget(refusing);
        Ok((BoundHello { greeting }, echo, len))
    }
}

/// Sets the pair cell when dropped: on every early return, and when a pending greeting read is
/// dropped. Forgotten only on success.
struct Refusing<'a>(&'a watch::Sender<bool>);

impl Drop for Refusing<'_> {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

async fn greeting<T: DeserializeOwned>(
    reader: &mut RecordReader<OwnedReadHalf>,
    pipe: Pipe,
    by: Instant,
) -> Result<(T, usize), PairError> {
    before(by)?;
    let read = match timeout_at(by, ipc::read_sized::<T>(reader, pipe)).await {
        Err(_) => return Err(PairError::Timeout),
        Ok(Ok(Some(read))) => read,
        Ok(Ok(None) | Err(_)) => return Err(PairError::Greeting),
    };
    before(by)?;
    Ok(read)
}

#[cfg(test)]
#[path = "pair_tests.rs"]
mod tests;
