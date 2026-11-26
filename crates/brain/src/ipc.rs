//! The brain's end of the two pipes: who may connect, the handshake, and the connection loop.
//!
//! One shell at a time. A client must run the shell executable from the brain's own folder and
//! present the instance capability before the brain decodes anything else from it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::frame::{self, FrameError, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    AudioSession, CAPABILITY_LEN, Capability, ControlToBrain, ControlToShell, DataToBrain, Hello,
    Incarnation, IssueSession, TEXT_METRICS_MAX_BYTES,
};
use kumochi_protocol::tap::Direction;
use kumochi_scene::display::TextMetricsV1;
use serde::de::DeserializeOwned;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

use crate::platform::{self, Instance, PipeServer};
use crate::runtime::{Brain, Ended, PairStart};
use crate::tap::Tap;

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
#[path = "ipc/audio_fixture.rs"]
mod audio_fixture;
#[cfg(all(windows, debug_assertions, feature = "test-support"))]
pub use crate::runtime::audio_fixture::AudioProbe;
#[cfg(all(windows, debug_assertions, feature = "test-support"))]
pub use audio_fixture::{
    offline_audio_peer, offline_authored_then_narrated_peer, offline_hold_peer,
    offline_narrated_peer, offline_narrated_replacement_chain_peer,
    offline_narrated_replacement_peer, offline_narrated_stopped_replacement_peer,
    offline_plain_peer, offline_speaking_hold_peer, offline_text_peer,
};

/// How long a connected client has to complete the entire handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the brain waits with no shell before exiting [guess]. A shell that gave up on it,
/// or quit before it was up, leaves nobody to tell it to stop; the next shell starts another.
const NO_SHELL_TIMEOUT: Duration = Duration::from_secs(30);

/// What the brain checks a shell against.
#[derive(Debug)]
pub struct Identity {
    /// This start's capability, also in Credential Manager.
    pub capability: Capability,
    /// This start's incarnation.
    pub incarnation: Incarnation,
    /// The only executable allowed to connect: the shell beside the brain.
    pub shell_image: PathBuf,
}

/// The pipes couldn't be created.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Creating a pipe failed, usually because another brain already holds it.
    #[error("can't create the {pipe:?} pipe (is another brain running?)")]
    Create {
        /// Which pipe.
        pipe: Pipe,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// Waiting for a client failed.
    #[error("the pipe failed while waiting for a shell")]
    Accept(#[from] io::Error),
}

/// How one shell connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Session {
    /// The shell sent `Quitting`; shutting down must finish by `deadline`.
    Quit {
        /// One absolute deadline for every stage of the shutdown.
        deadline: tokio::time::Instant,
    },
    /// A shell from another build connected; it starts its own brain.
    BuildMismatch,
    /// The shell went away without `Quitting`: it crashed or was killed.
    ShellLost,
    /// No shell connected for [`NO_SHELL_TIMEOUT`].
    NoShell,
}

/// Why a client was turned away or a session broke.
#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("client is not the shell beside the brain")]
    NotOurShell,
    #[error("client didn't finish the handshake in time")]
    Timeout,
    #[error("client closed before the handshake")]
    ClosedEarly,
    #[error("first message wasn't Hello")]
    NotHello,
    #[error("wrong capability")]
    WrongCapability,
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    #[error("the visible scenes didn't settle in time")]
    Unavailable,
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Authorization selected by the local transport, not by the greeting's contents.
pub(crate) enum PeerAuth<'a> {
    Capability {
        hello: Hello,
        expected: &'a Capability,
    },
    #[cfg_attr(
        not(all(test, target_os = "macos")),
        expect(
            dead_code,
            reason = "only an authenticated pair reader can produce this greeting"
        )
    )]
    SessionBound(platform::BoundHello),
}

/// Why a greeting was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    WrongCapability,
    BuildMismatch,
}

/// Checks authorization before disclosing a build mismatch. Mode and epoch are checked separately.
pub(crate) fn check_hello(auth: PeerAuth<'_>) -> Result<(), Refusal> {
    let hello = match auth {
        PeerAuth::Capability { hello, expected } => {
            if !bool::from(hello.capability.expose().ct_eq(expected.expose())) {
                return Err(Refusal::WrongCapability);
            }
            hello
        }
        PeerAuth::SessionBound(bound) => {
            let hello = bound.into_greeting();
            require_session_placeholder(&hello.capability)?;
            hello
        }
    };
    if hello.build_id != BUILD_ID {
        return Err(Refusal::BuildMismatch);
    }
    Ok(())
}

fn require_session_placeholder(field: &Capability) -> Result<(), Refusal> {
    if field.expose() != &[0; CAPABILITY_LEN] {
        return Err(Refusal::WrongCapability);
    }
    Ok(())
}

fn startup_hello(
    first: ControlToBrain,
    last_queued: Option<IssueSession>,
) -> Result<(Hello, AudioSession, PairStart), SessionError> {
    Ok(match first {
        ControlToBrain::Hello(hello) => (hello, AudioSession::Legacy, PairStart::Ordinary),
        ControlToBrain::QueuedHello(hello) => {
            (hello, AudioSession::QueuedRequired, PairStart::Ordinary)
        }
        ControlToBrain::QueuedAudioReconnect(hello, previous) => (
            *hello,
            AudioSession::QueuedRequired,
            if last_queued == Some(previous) {
                PairStart::AudioRepair
            } else {
                PairStart::Ordinary
            },
        ),
        _ => return Err(SessionError::NotHello),
    })
}

/// The control and data pipes.
#[derive(Debug)]
pub struct Pipes {
    session: u32,
    control: PipeServer,
    data: PipeServer,
    last_queued: Option<IssueSession>,
}

#[cfg(all(test, windows))]
impl Pipes {
    pub(crate) fn create_for_startup_test(slot: u32) -> io::Result<Self> {
        let name = format!(
            r"\\.\pipe\{}-fixture-startup-unwind-{}-{slot}",
            kumochi_protocol::constants::OBJECT_PREFIX,
            std::process::id()
        );
        Ok(Self {
            session: 0,
            control: platform::create_owner_only(&format!("{name}-control"), Instance::First)?,
            data: platform::create_owner_only(&format!("{name}-data"), Instance::First)?,
            last_queued: None,
        })
    }
}

impl Pipes {
    /// Creates both pipes for `session`, owner-only and single-instance.
    ///
    /// # Errors
    /// Fails when either name is taken or the OS refuses.
    pub fn create(session: u32) -> Result<Self, IpcError> {
        Ok(Self {
            session,
            control: create(session, Pipe::Control, Instance::First)?,
            data: create(session, Pipe::Data, Instance::First)?,
            last_queued: None,
        })
    }

    /// Swaps in fresh instances for the next shell. A used instance isn't reused, because its
    /// end of stream outlives `DisconnectNamedPipe` in tokio's readiness state.
    fn renew(&mut self) -> Result<(), IpcError> {
        self.control = create(self.session, Pipe::Control, Instance::Next)?;
        self.data = create(self.session, Pipe::Data, Instance::Next)?;
        Ok(())
    }

    /// Waits for a shell, serves it, and returns how its session ended. Clients that fail the
    /// handshake are logged and skipped. A relaunched shell that ends before a session starts
    /// (`launched` fires) counts as lost.
    ///
    /// # Errors
    /// Fails only when a pipe can no longer accept clients.
    pub(crate) async fn next_session(
        &mut self,
        identity: &Identity,
        brain: &mut Brain,
        tap: &mut Tap,
        launched: &mut Option<oneshot::Receiver<()>>,
    ) -> Result<Session, IpcError> {
        loop {
            // A shell the brain relaunched tells it to stop once it connects, so only an
            // idle brain gives up.
            let idle = launched.is_none();
            tokio::select! {
                accepted = self.control.connect() => accepted?,
                () = brain.idle() => {}
                () = exited(launched) => return Ok(Session::ShellLost),
                () = tokio::time::sleep(NO_SHELL_TIMEOUT), if idle => return Ok(Session::NoShell),
            }
            let session = self
                .session(identity, brain, tap, Instant::now() + HANDSHAKE_TIMEOUT)
                .await;
            self.renew()?;
            match session {
                Ok(session) => return Ok(session),
                Err(error) => tracing::warn!(%error, "connection refused"),
            }
        }
    }

    async fn session(
        &mut self,
        identity: &Identity,
        brain: &mut Brain,
        tap: &mut Tap,
        deadline: Instant,
    ) -> Result<Session, SessionError> {
        check_client(&self.control, &identity.shell_image)?;
        let mut legs = PipeLegs {
            pipes: self,
            identity,
        };
        let greeted = greet(&mut legs, tap, deadline, || {
            brain.reserve_issue_session(identity.incarnation)
        })
        .await?;
        let Some(Admitted {
            peer: process,
            audio,
            start,
            session,
            text_metrics,
        }) = greeted
        else {
            return Ok(Session::BuildMismatch);
        };
        let tuning = ControlToShell::ShellTuning {
            session,
            tuning: brain.shell_tuning(),
        };
        if Instant::now() >= deadline {
            return Err(SessionError::Timeout);
        }
        timeout_at(deadline, send(&mut legs, &tuning, tap))
            .await
            .map_err(|_| SessionError::Timeout)??;
        if Instant::now() >= deadline {
            return Err(SessionError::Timeout);
        }
        brain.install_issue_session(session);
        self.last_queued = (audio == AudioSession::QueuedRequired).then_some(session);
        let _shell_identity = process.register();
        brain.connected_shell(process);
        tracing::info!("shell connected");
        Ok(
            match brain
                .serve_with_profile(
                    &mut self.control,
                    &mut self.data,
                    tap,
                    audio,
                    text_metrics,
                    start,
                    None,
                )
                .await
            {
                Ended::Quit { deadline } => Session::Quit { deadline },
                Ended::Lost => Session::ShellLost,
                Ended::Broken(reason) => return Err(SessionError::Protocol(reason)),
                Ended::Fatal(_) => return Err(SessionError::Protocol("connection refused")),
                // The shell is still there and reconnects, so it isn't relaunched.
                Ended::Unavailable => return Err(SessionError::Unavailable),
            },
        )
    }
}

/// One transport's part of the greeting: acquiring, reading, writing and binding its two legs.
/// Negotiation and authorization stay in [`greet`], so every transport runs the same checks in the
/// same order. A greeting comes from the transport's own reader; shared code never assembles one.
trait Legs {
    /// The one process both legs were bound to.
    type Peer;
    /// The data leg, read after its greeting.
    type Data: AsyncRead + Unpin;

    /// Reads the control greeting, recording it on the tap, with the mode its variant selects.
    /// `None` means the client closed first.
    async fn control_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, AudioSession, PairStart)>, SessionError>;
    /// Writes one whole control frame.
    async fn write_control(&mut self, frame: &[u8]) -> io::Result<()>;
    /// Acquires the data leg and checks who opened it.
    async fn open_data(&mut self) -> Result<(), SessionError>;
    /// Reads the data greeting, recording it on the tap, with its Queued echo.
    /// `None` means the client closed first.
    async fn data_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, Option<IssueSession>)>, SessionError>;
    /// Binds both legs to one process.
    fn bind(&mut self) -> Result<Self::Peer, SessionError>;
    fn data(&mut self) -> &mut Self::Data;
}

/// The pipes: the data instance is accepted only after Welcome, and the OS names each client's
/// executable and process.
struct PipeLegs<'a> {
    pipes: &'a mut Pipes,
    identity: &'a Identity,
}

impl Legs for PipeLegs<'_> {
    type Peer = platform::ShellProcess;
    type Data = PipeServer;

    async fn control_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, AudioSession, PairStart)>, SessionError> {
        let Some((first, len)) =
            read_sized::<ControlToBrain>(&mut self.pipes.control, Pipe::Control).await?
        else {
            return Ok(None);
        };
        tap.record(Pipe::Control, Direction::ToBrain, &first, len);
        let (hello, audio, start) = startup_hello(first, self.pipes.last_queued)?;
        let auth = PeerAuth::Capability {
            hello,
            expected: &self.identity.capability,
        };
        Ok(Some((auth, audio, start)))
    }

    async fn write_control(&mut self, frame: &[u8]) -> io::Result<()> {
        self.pipes.control.write_all(frame).await
    }

    async fn open_data(&mut self) -> Result<(), SessionError> {
        self.pipes.data.connect().await?;
        check_client(&self.pipes.data, &self.identity.shell_image)
    }

    async fn data_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, Option<IssueSession>)>, SessionError> {
        let Some((first, len)) =
            read_sized::<DataToBrain>(&mut self.pipes.data, Pipe::Data).await?
        else {
            return Ok(None);
        };
        tap.record(Pipe::Data, Direction::ToBrain, &first, len);
        let (hello, session) = match first {
            DataToBrain::Hello(hello) => (hello, None),
            DataToBrain::QueuedHello(hello, session) => (hello, Some(session)),
            _ => return Err(SessionError::ClosedEarly),
        };
        let auth = PeerAuth::Capability {
            hello,
            expected: &self.identity.capability,
        };
        Ok(Some((auth, session)))
    }

    fn bind(&mut self) -> Result<platform::ShellProcess, SessionError> {
        platform::shell_connection(
            &self.pipes.control,
            &self.pipes.data,
            &self.identity.shell_image,
        )
        .map_err(SessionError::from)
    }

    fn data(&mut self) -> &mut PipeServer {
        &mut self.pipes.data
    }
}

/// A greeting both legs passed, ready to serve.
struct Admitted<P> {
    peer: P,
    audio: AudioSession,
    start: PairStart,
    /// The reserved session Welcome carried.
    session: IssueSession,
    text_metrics: Option<TextMetricsV1>,
}

/// Greets a shell over `legs` by the caller's `deadline`: the control greeting is authorized
/// before a build mismatch is disclosed or a session reserved, and nothing on the data leg is
/// touched before Welcome. `reserve` reserves the issue session Welcome carries; installing it is
/// the caller's. `None`: a shell from another build was told so.
async fn greet<L: Legs>(
    legs: &mut L,
    tap: &mut Tap,
    deadline: Instant,
    reserve: impl FnOnce() -> IssueSession,
) -> Result<Option<Admitted<L::Peer>>, SessionError> {
    let first = timeout_at(deadline, legs.control_greeting(tap))
        .await
        .map_err(|_| SessionError::Timeout)??;
    let Some((greeting, audio, start)) = first else {
        return Err(SessionError::ClosedEarly);
    };
    match check_hello(greeting) {
        Err(Refusal::WrongCapability) => return Err(SessionError::WrongCapability),
        Err(Refusal::BuildMismatch) => {
            tracing::warn!("build mismatch");
            let reply = ControlToShell::BuildMismatch {
                brain_build: BUILD_ID.to_owned(),
            };
            timeout_at(deadline, send(legs, &reply, tap))
                .await
                .map_err(|_| SessionError::Timeout)??;
            return Ok(None);
        }
        Ok(()) => {}
    }
    let session = reserve();
    let welcome = ControlToShell::Welcome {
        incarnation: session.incarnation,
        issue_epoch: session.epoch,
    };
    timeout_at(deadline, send(legs, &welcome, tap))
        .await
        .map_err(|_| SessionError::Timeout)??;

    let (peer, text_metrics) = read_startup_data(
        legs,
        (audio == AudioSession::QueuedRequired).then_some(session),
        tap,
        deadline,
    )
    .await?;
    Ok(Some(Admitted {
        peer,
        audio,
        start,
        session,
        text_metrics,
    }))
}

/// The data leg after Welcome: acquiring it, its greeting and the drawing-font frame keep the
/// startup `deadline`, and the greeting is authorized before its echo is compared or the legs are
/// bound.
async fn read_startup_data<L: Legs>(
    legs: &mut L,
    expected: Option<IssueSession>,
    tap: &mut Tap,
    deadline: Instant,
) -> Result<(L::Peer, Option<TextMetricsV1>), SessionError> {
    timeout_at(deadline, legs.open_data())
        .await
        .map_err(|_| SessionError::Timeout)??;
    let first = timeout_at(deadline, legs.data_greeting(tap))
        .await
        .map_err(|_| SessionError::Timeout)??;
    let Some((greeting, paired)) = first else {
        return Err(SessionError::ClosedEarly);
    };
    check_hello(greeting).map_err(|_| SessionError::WrongCapability)?;
    if paired != expected {
        return Err(SessionError::Protocol("data hello does not match control"));
    }
    let peer = legs.bind()?;
    let text_metrics = read_text_metrics(legs.data(), deadline).await?;
    Ok((peer, text_metrics))
}

/// Resolves when the relaunched shell has ended, or when the relaunch dropped its sender
/// because no shell started; never when there's none to wait for.
async fn exited(launched: &mut Option<oneshot::Receiver<()>>) {
    let Some(exit) = launched.as_mut() else {
        return std::future::pending().await;
    };
    // Either way no shell is coming, so both count as a lost shell.
    if exit.await.is_err() {
        tracing::warn!("the shell wasn't relaunched");
    }
    *launched = None;
}

fn create(session: u32, pipe: Pipe, instance: Instance) -> Result<PipeServer, IpcError> {
    platform::create_owner_only(&pipe.name(session), instance)
        .map_err(|source| IpcError::Create { pipe, source })
}

fn check_client(pipe: &PipeServer, shell_image: &Path) -> Result<(), SessionError> {
    check_client_image(&platform::client_image(pipe)?, shell_image)
}

fn check_client_image(image: &Path, shell_image: &Path) -> Result<(), SessionError> {
    if image
        .as_os_str()
        .eq_ignore_ascii_case(shell_image.as_os_str())
    {
        Ok(())
    } else {
        Err(SessionError::NotOurShell)
    }
}

async fn send(
    legs: &mut impl Legs,
    message: &ControlToShell,
    tap: &mut Tap,
) -> Result<(), SessionError> {
    let frame = frame::encode(message, Pipe::Control)?;
    tap.record(
        Pipe::Control,
        Direction::ToShell,
        message,
        frame.len() - HEADER_LEN,
    );
    legs.write_control(&frame).await?;
    Ok(())
}

/// Reads one frame. `None` means the client closed between frames, which on a named pipe
/// arrives as a broken-pipe error rather than a zero-length read.
pub(crate) async fn read<T: DeserializeOwned>(
    pipe: &mut (impl AsyncRead + Unpin),
    kind: Pipe,
) -> Result<Option<T>, FrameError> {
    Ok(read_sized(pipe, kind).await?.map(|(message, _)| message))
}

/// Reads one frame, with its payload's length for the tap.
pub(crate) async fn read_sized<T: DeserializeOwned>(
    pipe: &mut (impl AsyncRead + Unpin),
    kind: Pipe,
) -> Result<Option<(T, usize)>, FrameError> {
    read_capped(pipe, kind.max_payload()).await
}

async fn read_text_metrics(
    pipe: &mut (impl AsyncRead + Unpin),
    deadline: Instant,
) -> Result<Option<TextMetricsV1>, SessionError> {
    if Instant::now() >= deadline {
        return Err(SessionError::Timeout);
    }
    let metrics = match timeout_at(
        deadline,
        read_capped::<DataToBrain>(pipe, TEXT_METRICS_MAX_BYTES),
    )
    .await
    .map_err(|_| SessionError::Timeout)??
    {
        Some((DataToBrain::TextMetrics(profile), _)) => profile.filter(TextMetricsV1::valid),
        Some(_) => return Err(SessionError::Protocol("expected startup TextMetrics")),
        None => return Err(SessionError::ClosedEarly),
    };
    if Instant::now() >= deadline {
        return Err(SessionError::Timeout);
    }
    Ok(metrics)
}

async fn read_capped<T: DeserializeOwned>(
    pipe: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> Result<Option<(T, usize)>, FrameError> {
    let closed = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
        )
    };
    let mut header = [0; HEADER_LEN];
    let first = match pipe.read(&mut header).await {
        Ok(0) => return Ok(None),
        Ok(n) => n,
        Err(e) if closed(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let truncated = |e: io::Error| {
        if closed(&e) {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    };
    pipe.read_exact(&mut header[first..])
        .await
        .map_err(truncated)?;
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut payload = vec![0; len];
    pipe.read_exact(&mut payload).await.map_err(truncated)?;
    frame::decode(&payload).map(|message| Some((message, len)))
}

#[cfg(test)]
#[path = "ipc/greeting_tests.rs"]
mod greeting_tests;

#[cfg(test)]
#[path = "ipc/text_metrics_tests.rs"]
mod text_metrics_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kumochi_protocol::frame::{self, FrameError, Pipe};
    use kumochi_protocol::message::{
        AudioSession, CAPABILITY_LEN, ControlToBrain, Hello, Incarnation, IssueSession,
    };
    #[cfg(windows)]
    use kumochi_protocol::message::{
        ControlToShell, DataToBrain, DataToShell, Narration, ShellTuning,
    };
    use kumochi_protocol::{BUILD_ID, Secret};

    use tokio::io::AsyncWriteExt as _;
    #[cfg(windows)]
    use tokio::net::windows::named_pipe::ClientOptions;
    #[cfg(windows)]
    use tokio::sync::oneshot;
    #[cfg(windows)]
    use tokio::time::{Instant, timeout};

    #[cfg(windows)]
    use super::{HANDSHAKE_TIMEOUT, Pipes, Session, SessionError};
    use super::{
        Identity, PeerAuth, Refusal, check_hello, require_session_placeholder, startup_hello,
    };
    use crate::runtime::PairStart;
    #[cfg(windows)]
    use crate::tap::Tap;
    #[cfg(windows)]
    use crate::test_support::{self, Scratch};

    fn identity() -> Identity {
        Identity {
            capability: Secret::new([1; CAPABILITY_LEN]),
            incarnation: Incarnation(1),
            shell_image: PathBuf::from(r"C:\app.exe"),
        }
    }

    #[cfg(windows)]
    async fn expect_startup(
        control: &mut (impl tokio::io::AsyncRead + Unpin),
        session: IssueSession,
        tuning: ShellTuning,
    ) {
        assert_eq!(
            super::read(control, Pipe::Control).await.unwrap(),
            Some(ControlToShell::ShellTuning { session, tuning })
        );
        assert!(matches!(
            super::read::<ControlToShell>(control, Pipe::Control)
                .await
                .unwrap(),
            Some(ControlToShell::Settings { .. })
        ));
    }

    fn hello(byte: u8, build: &str) -> Hello {
        Hello {
            build_id: build.to_owned(),
            capability: Secret::new([byte; CAPABILITY_LEN]),
        }
    }

    #[test]
    fn re_pair_names_the_last_authenticated_queued_pair() {
        let previous = IssueSession {
            incarnation: Incarnation(1),
            epoch: 7,
        };
        let hello = hello(1, BUILD_ID);
        let repair = ControlToBrain::QueuedAudioReconnect(Box::new(hello.clone()), previous);
        for (greeting, last_queued, start) in [
            (
                ControlToBrain::QueuedHello(hello.clone()),
                Some(previous),
                PairStart::Ordinary,
            ),
            (repair.clone(), Some(previous), PairStart::AudioRepair),
            (repair.clone(), None, PairStart::Ordinary),
            (
                repair.clone(),
                Some(IssueSession {
                    epoch: 8,
                    ..previous
                }),
                PairStart::Ordinary,
            ),
            (
                repair,
                Some(IssueSession {
                    incarnation: Incarnation(2),
                    ..previous
                }),
                PairStart::Ordinary,
            ),
        ] {
            assert_eq!(
                startup_hello(greeting, last_queued).unwrap(),
                (hello.clone(), AudioSession::QueuedRequired, start)
            );
        }
    }

    #[cfg(windows)]
    async fn request_audio(
        control: &mut crate::platform::PipeClient,
        mode: AudioSession,
    ) -> IssueSession {
        let hello = hello(1, BUILD_ID);
        let message = match mode {
            AudioSession::Legacy => ControlToBrain::Hello(hello),
            AudioSession::QueuedRequired => ControlToBrain::QueuedHello(hello),
        };
        control
            .write_all(&frame::encode(&message, Pipe::Control).unwrap())
            .await
            .unwrap();
        let Some(ControlToShell::Welcome {
            incarnation,
            issue_epoch,
        }) = super::read(control, Pipe::Control).await.unwrap()
        else {
            panic!("expected Welcome before the data hello");
        };
        IssueSession {
            incarnation,
            epoch: issue_epoch,
        }
    }

    #[cfg(windows)]
    fn cancel_review(session: IssueSession) -> ControlToBrain {
        use kumochi_protocol::message::{DraftId, IssueOp};

        ControlToBrain::Issue {
            session,
            request: 2,
            op: Box::new(IssueOp::CancelReview {
                draft: DraftId(1),
                review: 1,
            }),
        }
    }

    #[cfg(windows)]
    async fn register_queued_output(
        control: &mut crate::platform::PipeClient,
        session: IssueSession,
    ) {
        use kumochi_protocol::message::OutputGeneration;
        let output = OutputGeneration(1);
        for message in [
            cancel_review(session),
            ControlToBrain::QueuedAudioOutput(24_000, output, Narration::Plain),
        ] {
            control
                .write_all(&frame::encode(&message, Pipe::Control).unwrap())
                .await
                .unwrap();
        }
        loop {
            let message = super::read::<ControlToShell>(control, Pipe::Control)
                .await
                .unwrap()
                .expect("the reserved issue session was accepted");
            if message == ControlToShell::QueuedAudioRegistered(output) {
                return;
            }
        }
    }

    #[cfg(windows)]
    async fn check_legacy_issue(control: &mut crate::platform::PipeClient, session: IssueSession) {
        use kumochi_protocol::message::{DraftId, IssueUnavailableKind};

        let cancel = frame::encode(&cancel_review(session), Pipe::Control).unwrap();
        control.write_all(&cancel).await.unwrap();
        control.write_all(&cancel).await.unwrap();
        let duplicate = ControlToShell::IssueUnavailable {
            request: 2,
            draft: DraftId(1),
            kind: IssueUnavailableKind::Action,
        };
        loop {
            let message = super::read::<ControlToShell>(control, Pipe::Control)
                .await
                .unwrap()
                .expect("the Legacy issue session remains connected");
            if message == duplicate {
                break;
            }
        }
    }

    /// A scratch profile whose hold tuning differs from the defaults, with the tuning it sends.
    #[cfg(windows)]
    fn tuned_scratch(name: &str) -> (Scratch, ShellTuning) {
        let scratch = Scratch::new(name);
        std::fs::create_dir_all(scratch.dir()).unwrap();
        std::fs::write(
            scratch.dir().join(kumochi_protocol::constants::CONFIG_FILE),
            "[tuning.hold]\nstart_ms = 2500\ndrain_ms = 750",
        )
        .unwrap();
        (scratch, ShellTuning::from_ms(2500, 750).unwrap())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn paired_handshake_accepts_only_matching_authenticated_legs() {
        let mut id = identity();
        id.shell_image = std::env::current_exe().unwrap();
        let (scratch, tuning) = tuned_scratch("queued-hello-pair");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        let queued = AudioSession::QueuedRequired;
        let legacy = AudioSession::Legacy;
        let mut first = None;
        for (index, control_mode, data_mode, secret) in [
            (0, queued, queued, 1),
            (1, queued, legacy, 1),
            (2, legacy, queued, 1),
            (3, queued, queued, 2),
            (4, queued, queued, 1),
            (5, legacy, legacy, 1),
            (6, queued, queued, 1),
        ] {
            let installed_before = brain.issue_session();
            let (control, mut peer_control) =
                crate::platform::test_pair(&format!("queued-hello-c-{index}"))
                    .await
                    .unwrap();
            let (data, mut peer_data) =
                crate::platform::test_pair(&format!("queued-hello-d-{index}"))
                    .await
                    .unwrap();
            let mut pipes = Pipes {
                session: 0,
                control,
                data,
                last_queued: None,
            };
            let peer = async {
                let welcomed = request_audio(&mut peer_control, control_mode).await;
                let echo = if index == 4 { first.unwrap() } else { welcomed };
                first.get_or_insert(welcomed);
                let greeting = hello(secret, BUILD_ID);
                let message = match data_mode {
                    AudioSession::Legacy => DataToBrain::Hello(greeting),
                    AudioSession::QueuedRequired => DataToBrain::QueuedHello(greeting, echo),
                };
                peer_data
                    .write_all(&frame::encode(&message, Pipe::Data).unwrap())
                    .await
                    .unwrap();
                if index == 6 {
                    let not_metrics = DataToBrain::Hello(hello(1, BUILD_ID));
                    peer_data
                        .write_all(&frame::encode(&not_metrics, Pipe::Data).unwrap())
                        .await
                        .unwrap();
                }
                if !matches!(index, 0 | 5) {
                    return;
                }
                peer_data
                    .write_all(&frame::encode(&DataToBrain::TextMetrics(None), Pipe::Data).unwrap())
                    .await
                    .unwrap();
                expect_startup(&mut peer_control, welcomed, tuning).await;
                if index == 0 {
                    register_queued_output(&mut peer_control, welcomed).await;
                } else {
                    check_legacy_issue(&mut peer_control, welcomed).await;
                }
                peer_control
                    .write_all(&frame::encode(&ControlToBrain::Quitting, Pipe::Control).unwrap())
                    .await
                    .unwrap();
            };
            let end = Instant::now() + HANDSHAKE_TIMEOUT;
            let serve = pipes.session(&id, &mut brain, &mut tap, end);
            let exchange = timeout(std::time::Duration::from_secs(7), async {
                tokio::join!(serve, peer)
            });
            let (result, ()) = Box::pin(exchange).await.unwrap();
            match index {
                0 | 5 => assert!(matches!(result, Ok(Session::Quit { .. }))),
                3 => assert!(matches!(result, Err(SessionError::WrongCapability))),
                6 => assert!(matches!(
                    result,
                    Err(SessionError::Protocol("expected startup TextMetrics"))
                )),
                _ => assert!(matches!(
                    result,
                    Err(SessionError::Protocol("data hello does not match control"))
                )),
            }
            if index == 1 {
                assert_eq!(brain.issue_session(), None);
            }
            if index == 6 {
                assert_eq!(brain.issue_session(), installed_before);
            }
        }
        brain.close().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn reconnect_cannot_replay_a_data_leg_and_refusal_closes_both_pipes() {
        async fn connect(
            session: u32,
            replay: Option<IssueSession>,
        ) -> (
            IssueSession,
            crate::platform::PipeClient,
            crate::platform::PipeClient,
        ) {
            let mut control = ClientOptions::new()
                .open(Pipe::Control.name(session))
                .unwrap();
            let welcome = request_audio(&mut control, AudioSession::QueuedRequired).await;
            let mut data = ClientOptions::new().open(Pipe::Data.name(session)).unwrap();
            let message = DataToBrain::QueuedHello(hello(1, BUILD_ID), replay.unwrap_or(welcome));
            data.write_all(&frame::encode(&message, Pipe::Data).unwrap())
                .await
                .unwrap();
            if replay.is_none() {
                data.write_all(
                    &frame::encode(&DataToBrain::TextMetrics(None), Pipe::Data).unwrap(),
                )
                .await
                .unwrap();
            }
            (welcome, control, data)
        }

        let session = u32::MAX - 17 - std::process::id();
        let mut pipes = Pipes::create(session).unwrap();
        let mut id = identity();
        id.shell_image = std::env::current_exe().unwrap();
        let scratch = Scratch::new("queued-hello-reconnect");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        let mut launched = None;
        let first_peer = async {
            let (welcome, mut control, _data) = connect(session, None).await;
            expect_startup(&mut control, welcome, ShellTuning::default()).await;
            welcome
        };
        let (result, old) = timeout(std::time::Duration::from_secs(7), async {
            tokio::join!(
                pipes.next_session(&id, &mut brain, &mut tap, &mut launched),
                first_peer
            )
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), Session::ShellLost);
        assert_eq!(old.epoch, 1);
        assert_eq!(brain.issue_session(), None);

        let reconnect = async {
            let (welcome, mut control, mut data) = connect(session, Some(old)).await;
            assert_eq!(welcome.incarnation, old.incarnation);
            assert_eq!(welcome.epoch, 2);
            assert_eq!(
                super::read::<ControlToShell>(&mut control, Pipe::Control)
                    .await
                    .unwrap(),
                None,
                "a replayed data hello must close control without serving"
            );
            assert_eq!(
                super::read::<DataToShell>(&mut data, Pipe::Data)
                    .await
                    .unwrap(),
                None,
                "a replayed data hello must also close data"
            );
            let (renewed, mut control, _data) = connect(session, None).await;
            assert_eq!(renewed.incarnation, old.incarnation);
            assert_eq!(renewed.epoch, 3);
            expect_startup(&mut control, renewed, ShellTuning::default()).await;
            control
                .write_all(&frame::encode(&ControlToBrain::Quitting, Pipe::Control).unwrap())
                .await
                .unwrap();
        };
        let (result, ()) = timeout(std::time::Duration::from_secs(9), async {
            tokio::join!(
                pipes.next_session(&id, &mut brain, &mut tap, &mut launched),
                reconnect
            )
        })
        .await
        .unwrap();
        assert!(matches!(result.unwrap(), Session::Quit { .. }));
        assert_eq!(brain.issue_session(), None);
        brain.close().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn queued_data_hello_rejects_malformed_frames_and_times_out_partial_ones() {
        let mut id = identity();
        id.shell_image = std::env::current_exe().unwrap();
        let scratch = Scratch::new("queued-hello-invalid");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        for (index, bytes) in [(0, &[1, 0, 0, 0, 255][..]), (1, &[1, 0][..])] {
            let (control, mut peer_control) =
                crate::platform::test_pair(&format!("queued-invalid-c-{index}"))
                    .await
                    .unwrap();
            let (data, mut peer_data) =
                crate::platform::test_pair(&format!("queued-invalid-d-{index}"))
                    .await
                    .unwrap();
            let mut pipes = Pipes {
                session: 0,
                control,
                data,
                last_queued: None,
            };
            let peer = async {
                request_audio(&mut peer_control, AudioSession::QueuedRequired).await;
                peer_data.write_all(bytes).await.unwrap();
            };
            let (result, ()) = timeout(std::time::Duration::from_secs(7), async {
                tokio::join!(
                    pipes.session(
                        &id,
                        &mut brain,
                        &mut tap,
                        Instant::now() + HANDSHAKE_TIMEOUT
                    ),
                    peer
                )
            })
            .await
            .unwrap();
            if index == 0 {
                assert!(matches!(
                    result,
                    Err(SessionError::Frame(FrameError::Malformed(_)))
                ));
            } else {
                assert!(matches!(result, Err(SessionError::Timeout)));
            }
        }
        brain.close().await;
    }

    #[test]
    fn hello_needs_the_capability_before_anything_else() {
        let id = identity();
        let check = |hello: Hello| {
            check_hello(PeerAuth::Capability {
                hello,
                expected: &id.capability,
            })
        };
        assert_eq!(check(hello(1, BUILD_ID)), Ok(()));
        assert_eq!(
            check(Hello::session(BUILD_ID.to_owned())),
            Err(Refusal::WrongCapability)
        );
        assert_eq!(check(hello(2, BUILD_ID)), Err(Refusal::WrongCapability));
        assert_eq!(check(hello(1, "0.0.0+other")), Err(Refusal::BuildMismatch));
        assert_eq!(
            check(hello(2, "0.0.0+other")),
            Err(Refusal::WrongCapability),
            "a client without the capability doesn't learn it has the wrong build",
        );
        let mut one_byte_off = [1; CAPABILITY_LEN];
        one_byte_off[CAPABILITY_LEN - 1] = 0;
        let near = Hello {
            build_id: BUILD_ID.to_owned(),
            capability: Secret::new(one_byte_off),
        };
        assert_eq!(check(near), Err(Refusal::WrongCapability));
    }

    #[test]
    fn session_greeting_requires_every_placeholder_byte_to_be_zero() {
        let greeting = Hello::session(BUILD_ID.to_owned());
        assert_eq!(require_session_placeholder(&greeting.capability), Ok(()));
        for index in 0..CAPABILITY_LEN {
            let mut nonzero = [0; CAPABILITY_LEN];
            nonzero[index] = 1;
            assert_eq!(
                require_session_placeholder(&Secret::new(nonzero)),
                Err(Refusal::WrongCapability),
                "nonzero byte at index {index}"
            );
        }
    }

    #[tokio::test]
    async fn unauthenticated_data_hello_refuses_before_echo_peer_binding_or_metrics() {
        use kumochi_protocol::message::{DataToBrain, IssueSession};

        let identity = identity();
        let expected = IssueSession {
            incarnation: identity.incarnation,
            epoch: 1,
        };
        let stale = IssueSession {
            epoch: 2,
            ..expected
        };
        for greeting in [hello(2, BUILD_ID), hello(1, "0.0.0+other")] {
            let bytes =
                frame::encode(&DataToBrain::QueuedHello(greeting, stale), Pipe::Data).unwrap();
            let mut legs = super::greeting_tests::Memory::new([].as_slice(), bytes.as_slice());
            let result = super::read_startup_data(
                &mut legs,
                Some(expected),
                &mut super::Tap::default(),
                super::Instant::now() + super::HANDSHAKE_TIMEOUT,
            )
            .await;
            assert!(matches!(result, Err(super::SessionError::WrongCapability)));
            assert!(!legs.bound, "a refused greeting must not bind a peer");
        }
    }

    #[test]
    fn the_rejected_image_is_not_in_the_refusal() {
        let foreign = std::path::Path::new(r"C:\synthetic-profile\foreign.exe");
        let error = super::check_client_image(foreign, &identity().shell_image).unwrap_err();
        assert_eq!(format!("{error:?}"), "NotOurShell");
        assert_eq!(
            error.to_string(),
            "client is not the shell beside the brain"
        );
    }

    #[tokio::test]
    async fn reads_one_complete_frame_and_distinguishes_between_frames_from_truncation() {
        let packet = frame::encode(&ControlToBrain::Quitting, Pipe::Control).unwrap();
        for end in [0, 1, super::HEADER_LEN, packet.len()] {
            let (mut tx, mut rx) = tokio::io::duplex(packet.len());
            tx.write_all(&packet[..end]).await.unwrap();
            drop(tx);
            let read = super::read_sized::<ControlToBrain>(&mut rx, Pipe::Control).await;
            match end {
                0 => assert!(matches!(read, Ok(None))),
                cut if cut == packet.len() => {
                    let (message, len) = read.unwrap().expect("one frame");
                    assert!(matches!(message, ControlToBrain::Quitting));
                    assert_eq!(len, packet.len() - super::HEADER_LEN);
                    assert!(matches!(
                        super::read::<ControlToBrain>(&mut rx, Pipe::Control).await,
                        Ok(None)
                    ));
                }
                _ => assert!(matches!(read, Err(FrameError::Truncated))),
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn creating_paired_native_pipes_refuses_without_transport() {
        assert!(matches!(
            super::Pipes::create(u32::MAX),
            Err(super::IpcError::Create { pipe: Pipe::Control, source })
                if source.kind() == std::io::ErrorKind::Unsupported
        ));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn data_hello_cannot_extend_the_original_startup_deadline_in_either_mode() {
        let mut id = identity();
        id.shell_image = std::env::current_exe().unwrap();
        let scratch = Scratch::new("shell-tuning-deadline");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        for (index, audio) in [AudioSession::Legacy, AudioSession::QueuedRequired]
            .into_iter()
            .enumerate()
        {
            let (control, mut peer_control) =
                crate::platform::test_pair(&format!("late-tuning-c-{index}"))
                    .await
                    .unwrap();
            let (data, mut peer_data) =
                crate::platform::test_pair(&format!("late-tuning-d-{index}"))
                    .await
                    .unwrap();
            let mut pipes = Pipes {
                session: 0,
                control,
                data,
                last_queued: None,
            };
            let deadline = Instant::now() + std::time::Duration::from_secs(4);
            let peer = async move {
                let session = request_audio(&mut peer_control, audio).await;
                tokio::time::pause();
                tokio::time::advance(
                    deadline.saturating_duration_since(Instant::now())
                        + std::time::Duration::from_millis(1),
                )
                .await;
                tokio::time::resume();
                let greeting = match audio {
                    AudioSession::Legacy => DataToBrain::Hello(hello(1, BUILD_ID)),
                    AudioSession::QueuedRequired => {
                        DataToBrain::QueuedHello(hello(1, BUILD_ID), session)
                    }
                };
                for message in [greeting, DataToBrain::TextMetrics(None)] {
                    peer_data
                        .write_all(&frame::encode(&message, Pipe::Data).unwrap())
                        .await
                        .unwrap();
                }
                peer_control
                    .write_all(&frame::encode(&ControlToBrain::Quitting, Pipe::Control).unwrap())
                    .await
                    .unwrap();
                (peer_control, peer_data)
            };
            let (result, _peer) = timeout(HANDSHAKE_TIMEOUT, async {
                tokio::join!(pipes.session(&id, &mut brain, &mut tap, deadline), peer)
            })
            .await
            .unwrap();
            assert!(matches!(result, Err(SessionError::Timeout)), "{result:?}");
        }
        brain.close().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_mixed_build_is_refused_before_queued_staging_frames_are_decoded() {
        use kumochi_protocol::frame::{self, Pipe};
        use kumochi_protocol::message::{ConnectionOp, ControlToBrain, ControlToShell, KeyStageId};
        use tokio::io::AsyncWriteExt as _;
        let (control, mut client) = crate::platform::test_pair("mixed-stage-control")
            .await
            .expect("control");
        let (data, client_data) = crate::platform::test_pair("mixed-stage-data")
            .await
            .expect("data");
        let mut pipes = Pipes {
            session: 0,
            control,
            data,
            last_queued: None,
        };
        let mut id = identity();
        id.shell_image = std::env::current_exe().expect("test executable");
        let scratch = Scratch::new("mixed-stage-build");
        let mut brain = test_support::brain(&scratch, "");
        let mut tap = Tap::from_env();
        let peer = async {
            let mut bytes = frame::encode(
                &ControlToBrain::Hello(hello(1, "different-staging-shape")),
                Pipe::Control,
            )
            .expect("hello");
            for op in [
                ConnectionOp::BeginAnthropicKeyStage {
                    request: 1,
                    len: 16,
                },
                ConnectionOp::AnthropicKeyStaged {
                    plan: KeyStageId([1; 16]),
                },
            ] {
                bytes.extend(
                    frame::encode(&ControlToBrain::Connection(op), Pipe::Control).expect("staging"),
                );
            }
            client.write_all(&bytes).await.expect("sent");
            assert_eq!(
                super::read::<ControlToShell>(&mut client, Pipe::Control)
                    .await
                    .expect("reply"),
                Some(ControlToShell::BuildMismatch {
                    brain_build: BUILD_ID.to_owned()
                })
            );
            drop((client, client_data));
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                pipes.session(
                    &id,
                    &mut brain,
                    &mut tap,
                    Instant::now() + HANDSHAKE_TIMEOUT
                ),
                peer
            )
        })
        .await
        .expect("handshake ends");
        assert_eq!(result.expect("session"), Session::BuildMismatch);
        brain.close().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_relaunched_shell_that_ends_before_connecting_counts_as_lost() {
        // A session id no real logon has, so the test never meets a running brain's pipes.
        let mut pipes =
            Pipes::create(u32::MAX - std::process::id()).expect("the test pipes are free");
        let scratch = Scratch::new("ipc");
        let mut brain = test_support::brain(&scratch, "");
        let (exited, exit) = oneshot::channel();
        exited.send(()).expect("the receiver is alive");
        let mut launched = Some(exit);
        let session = pipes
            .next_session(&identity(), &mut brain, &mut Tap::from_env(), &mut launched)
            .await
            .expect("no pipe failed");
        assert_eq!(session, Session::ShellLost);
        assert!(launched.is_none(), "the ended shell isn't waited on again");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_relaunch_that_never_started_a_shell_counts_as_lost() {
        let mut pipes =
            Pipes::create(u32::MAX - 3 - std::process::id()).expect("the test pipes are free");
        let scratch = Scratch::new("ipc");
        let mut brain = test_support::brain(&scratch, "");
        let (failed, exit) = oneshot::channel::<()>();
        drop(failed);
        let mut launched = Some(exit);
        let session = pipes
            .next_session(&identity(), &mut brain, &mut Tap::from_env(), &mut launched)
            .await
            .expect("no pipe failed");
        assert_eq!(session, Session::ShellLost);
        assert!(
            launched.is_none(),
            "the failed relaunch isn't waited on again"
        );
    }

    #[cfg(windows)]
    #[tokio::test(start_paused = true)]
    async fn a_brain_no_shell_reaches_gives_up() {
        let mut pipes =
            Pipes::create(u32::MAX - 1 - std::process::id()).expect("the test pipes are free");
        let scratch = Scratch::new("ipc");
        let mut brain = test_support::brain(&scratch, "");
        let session = pipes
            .next_session(&identity(), &mut brain, &mut Tap::from_env(), &mut None)
            .await
            .expect("no pipe failed");
        assert_eq!(session, Session::NoShell);
    }

    #[cfg(windows)]
    #[tokio::test(start_paused = true)]
    async fn a_brain_waits_for_the_shell_it_relaunched() {
        let mut pipes =
            Pipes::create(u32::MAX - 2 - std::process::id()).expect("the test pipes are free");
        let scratch = Scratch::new("ipc");
        let mut brain = test_support::brain(&scratch, "");
        let (_starting, exit) = oneshot::channel::<()>();
        let mut launched = Some(exit);
        let waited = tokio::time::timeout(
            super::NO_SHELL_TIMEOUT * 4,
            pipes.next_session(&identity(), &mut brain, &mut Tap::from_env(), &mut launched),
        )
        .await;
        assert!(waited.is_err(), "still waiting for the relaunched shell");
    }
}
