//! The brain's side of the inherited pair. The shell passes the control leg on descriptor 3 and the
//! data leg on 4; [`adopt`] checks both before the runtime exists, [`respond`] hands them to tokio
//! and runs the responder handshake over them, and the greetings are read only through the pair's
//! own openers, so a [`BoundHello`] never travels apart from the pair that authenticated it. See
//! `docs/development/ipc.md`.

use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdStream;

use kumochi_protocol::constants::{ProductIdentity, SESSION_READY};
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{ControlToBrain, ControlToShell, DataToBrain, Hello, IssueSession};
use kumochi_session::{
    BoundPair, BrainAwaitsInit, Channel, ENVELOPE_LEN, Outgoing, SECRET_LEN, Session, SessionError,
};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout_at};

use super::super::shell_process::Verdict;
use super::stream::{RecordReader, RecordWriter};
use crate::ipc;
use crate::platform::ShellProcess;
use crate::runtime::{Brain, Ended, Terminal};
use crate::startup::parent_bound::AdmissionPair;
use crate::tap::Tap;

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
    #[error("the parent was lost")]
    Lost,
    #[error("this edition has no session channel")]
    Edition,
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

impl InheritedPair {
    /// Hands both ends to tokio. Must run inside a runtime with I/O enabled.
    ///
    /// # Errors
    /// `Adopt` when tokio can't register an end.
    pub(crate) fn register(self) -> Result<RegisteredPair, PairError> {
        Ok(RegisteredPair {
            control: UnixStream::from_std(self.control).map_err(|_| PairError::Adopt)?,
            data: UnixStream::from_std(self.data).map_err(|_| PairError::Adopt)?,
        })
    }

    /// Registers both ends and keys the responder with `secret`, so the caller can wipe its own
    /// copy before READY. Must run inside a runtime with I/O enabled.
    ///
    /// # Errors
    /// As [`InheritedPair::register`], or the Noise library couldn't build the handshake.
    pub(crate) fn keyed(self, secret: &[u8; SECRET_LEN]) -> Result<KeyedPair, PairError> {
        let brain = BrainAwaitsInit::new(secret, channel(ProductIdentity::ACTIVE)?)?;
        Ok(KeyedPair {
            pair: self.register()?,
            brain,
        })
    }
}

impl AdmissionPair for InheritedPair {
    type Keyed = KeyedPair;
    type Greeted = Greeted;

    fn key(self, key: &[u8; SECRET_LEN]) -> Option<KeyedPair> {
        self.keyed(key).ok()
    }

    async fn admit(
        keyed: KeyedPair,
        shell: &ShellProcess,
        by: Instant,
        reserve: impl FnOnce() -> IssueSession,
    ) -> Option<Greeted> {
        admit(keyed, shell, by, reserve).await.ok()
    }

    fn cell(greeted: &Greeted) -> watch::Receiver<bool> {
        greeted.pair.pair.subscribe()
    }

    fn serve(
        greeted: &mut Greeted,
        brain: &mut Brain,
        terminal: Terminal,
        shell: &ShellProcess,
        by: Instant,
    ) -> impl Future<Output = Ended> {
        serve(greeted, brain, terminal, shell, by)
    }
}

/// The session channel of an edition the pair serves: an installed or a development build.
fn channel(identity: ProductIdentity) -> Result<Channel, PairError> {
    if identity == ProductIdentity::RELEASE {
        Ok(Channel::Release)
    } else if identity == ProductIdentity::DEV {
        Ok(Channel::Dev)
    } else {
        Err(PairError::Edition)
    }
}

/// Both inherited ends, registered with tokio.
#[derive(Debug)]
pub(crate) struct RegisteredPair {
    control: UnixStream,
    data: UnixStream,
}

impl RegisteredPair {
    /// Writes [`SESSION_READY`] on the control leg, once, by `by`. Each wait for room also
    /// watches the parent and the deadline, and both are checked again right before every write,
    /// so nothing is written once either is seen. The handshake starts only after it.
    ///
    /// # Errors
    /// `Lost`, `Timeout`, or `Transport` when the write fails.
    pub(crate) async fn ready(&self, shell: &ShellProcess, by: Instant) -> Result<(), PairError> {
        let mut loss = shell.loss();
        loop {
            tokio::select! {
                biased;
                _ = Verdict::lost(&mut loss) => return Err(PairError::Lost),
                () = sleep_until(by) => return Err(PairError::Timeout),
                room = self.control.writable() => room.map_err(|_| PairError::Transport)?,
            }
            shell.fence().map_err(|_| PairError::Lost)?;
            before(by)?;
            match self.control.try_write(&[SESSION_READY]) {
                Ok(1) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                _ => return Err(PairError::Transport),
            }
        }
    }
}

/// A registered pair and the responder keyed for it.
pub(crate) struct KeyedPair {
    pair: RegisteredPair,
    brain: BrainAwaitsInit,
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
/// await, so the caller can wipe its own copy of the secret first. Must run inside a tokio runtime
/// with I/O enabled.
///
/// # Errors
/// `Adopt` when tokio can't register an end, a session refusal, the deadline, or a transport
/// failure.
pub(crate) async fn respond(
    pair: RegisteredPair,
    brain: BrainAwaitsInit,
    by: Instant,
) -> Result<AuthenticatedPair, PairError> {
    let RegisteredPair {
        mut control,
        mut data,
    } = pair;
    let (session, proof) = handshake(&mut control, &mut data, brain, by).await?;
    AuthenticatedPair::new(session, &proof, control, data)
}

/// Runs `work` by `by` against the parent's loss, then fences and checks the clock again, so
/// work that finished after either doesn't count. On a loss or the deadline `work` is dropped.
async fn fenced<T>(
    shell: &ShellProcess,
    by: Instant,
    work: impl Future<Output = Result<T, PairError>>,
) -> Result<T, PairError> {
    let mut loss = shell.loss();
    let done = tokio::select! {
        biased;
        _ = Verdict::lost(&mut loss) => return Err(PairError::Lost),
        done = timeout_at(by, work) => done.map_err(|_| PairError::Timeout)??,
    };
    shell.fence().map_err(|_| PairError::Lost)?;
    before(by)?;
    Ok(done)
}

/// What admission hands on: the pair after both greetings, and what they chose.
pub(crate) struct Greeted {
    pair: AuthenticatedPair,
    admitted: ipc::Admitted<ShellProcess>,
}

/// Serves the admitted shell once the stages built `brain`: `ShellTuning` goes out by `by` while
/// the parent and the clock both pass, then the session and the shell are installed and the
/// runtime serves both legs, lent by `greeted`, with `terminal`. The greeting's text metrics are
/// handed on as a value, and any bytes already buffered after them stay in the data leg's reader.
async fn serve(
    greeted: &mut Greeted,
    brain: &mut Brain,
    terminal: Terminal,
    shell: &ShellProcess,
    by: Instant,
) -> Ended {
    let Greeted { pair, admitted } = greeted;
    let tuning = ControlToShell::ShellTuning {
        session: admitted.session,
        tuning: brain.shell_tuning(),
    };
    let Ok(tuning) = frame::encode(&tuning, Pipe::Control) else {
        return Ended::Broken("the shell tuning doesn't encode");
    };
    if tune(pair, shell, by, &tuning).await.is_err() {
        return Ended::Lost;
    }
    let Ok((control, data)) = pair.legs() else {
        return Ended::Broken("the greeted pair isn't greeted");
    };
    brain.install_issue_session(admitted.session);
    let _shell_identity = admitted.peer.register();
    brain.connected_shell(admitted.peer.clone());
    brain
        .serve_split(
            control,
            data,
            &mut Tap::default(),
            admitted.audio,
            admitted.text_metrics,
            admitted.start,
            Some(terminal),
        )
        .await
}

/// Writes `frame` on control only while the parent and the clock both pass right before every
/// poll of the writer, since the timer can fire after a writer woken past `by`; a loss or the
/// deadline wins over a writer that is ready at the same time, and both are checked again once
/// it's written.
async fn tune(
    pair: &mut AuthenticatedPair,
    shell: &ShellProcess,
    by: Instant,
    frame: &[u8],
) -> Result<(), PairError> {
    let mut loss = shell.loss();
    let mut write = std::pin::pin!(pair.write_control(frame));
    let checked = std::future::poll_fn(|cx| {
        shell.fence().map_err(|_| PairError::Lost)?;
        before(by)?;
        write.as_mut().poll(cx).map_err(|_| PairError::Transport)
    });
    tokio::select! {
        biased;
        _ = Verdict::lost(&mut loss) => return Err(PairError::Lost),
        () = sleep_until(by) => return Err(PairError::Timeout),
        written = checked => written?,
    }
    shell.fence().map_err(|_| PairError::Lost)?;
    before(by)
}

/// Admits the parent shell on `keyed` by `by`: READY, then the responder handshake and the
/// greeting, each raced against the parent's loss under its own phase cap. `reserve` reserves the
/// session Welcome carries. A refused greeting retires the pair, even one cancelled after the
/// data greeting's guard was let go.
///
/// # Errors
/// `Lost`, `Timeout`, the pair's own refusals, and `Greeting` for any refused greeting,
/// another build's included.
pub(crate) async fn admit(
    keyed: KeyedPair,
    shell: &ShellProcess,
    by: Instant,
    reserve: impl FnOnce() -> IssueSession,
) -> Result<Greeted, PairError> {
    let KeyedPair { pair, brain } = keyed;
    pair.ready(shell, by).await?;
    let phase = by.min(Instant::now() + ipc::HANDSHAKE_TIMEOUT);
    let mut pair = fenced(shell, phase, respond(pair, brain, phase)).await?;
    let phase = by.min(Instant::now() + ipc::HANDSHAKE_TIMEOUT);
    let greeted = fenced(shell, phase, async {
        let mut legs = legs::PairLegs::new(&mut pair, shell, phase);
        match ipc::greet(&mut legs, &mut Tap::default(), phase, reserve).await {
            Ok(Some(admitted)) => Ok(admitted),
            Err(ipc::SessionError::Timeout) => Err(PairError::Timeout),
            Err(ipc::SessionError::ClosedEarly) => Err(PairError::Lost),
            Ok(None) | Err(_) => Err(PairError::Greeting),
        }
    })
    .await;
    match greeted {
        Ok(admitted) => Ok(Greeted { pair, admitted }),
        Err(error) => {
            pair.retire();
            Err(error)
        }
    }
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
    /// Both greetings were read, so the legs can be served.
    Greeted,
    /// A read failed or came out of order.
    Spent,
}

/// One leg as the runtime serves it: frames read through its opener, written whole through its
/// sealer.
type Leg<'a> = (
    &'a mut RecordReader<OwnedReadHalf>,
    &'a mut RecordWriter<OwnedWriteHalf>,
);

/// A pair whose handshake finished with `BindAck`: both legs' record adapters over one pair cell.
pub(crate) struct AuthenticatedPair {
    control: RecordReader<OwnedReadHalf>,
    data: RecordReader<OwnedReadHalf>,
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
        self.stage = Stage::Greeted;
        Ok((BoundHello { greeting }, echo, len))
    }

    /// Writes one whole control frame, a greeting reply or Welcome, through this pair's control
    /// sealer. Only once the control greeting was read: Welcome goes before the data greeting and
    /// the tuning after it.
    ///
    /// # Errors
    /// `Order` before the control greeting or once a read failed, which refuses the whole pair;
    /// otherwise as [`RecordWriter::write_frame`].
    pub(crate) async fn write_control(&mut self, frame: &[u8]) -> io::Result<()> {
        if !matches!(self.stage, Stage::Data { .. } | Stage::Greeted) {
            self.pair.send_replace(true);
            return Err(io::Error::new(io::ErrorKind::InvalidData, PairError::Order));
        }
        self.writers[0].write_frame(frame).await
    }

    /// Refuses the whole pair from here on.
    pub(crate) fn retire(&self) {
        self.pair.send_replace(true);
    }

    /// Lends both legs to the runtime once both greetings were read; they close only when the
    /// pair drops. The legs keep sharing the pair's cell, which the runtime's terminal watches.
    ///
    /// # Errors
    /// `Order` unless both greetings were read.
    pub(crate) fn legs(&mut self) -> Result<(Leg<'_>, Leg<'_>), PairError> {
        if self.stage != Stage::Greeted {
            return Err(PairError::Order);
        }
        let [control_out, data_out] = &mut self.writers;
        Ok(((&mut self.control, control_out), (&mut self.data, data_out)))
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

#[path = "pair_legs.rs"]
pub(crate) mod legs;

#[cfg(test)]
#[path = "pair_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pair_admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "pair_admit_tests.rs"]
mod admit_tests;

#[cfg(test)]
#[path = "pair_serve_tests.rs"]
mod serve_tests;
