//! Finds or starts the brain and acquires both authenticated legs before link attendance.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, Hello, IssueSession, TextMetricsV1,
};

use super::{PipeClient, launch_detached};
use crate::credential::{self, CredentialError};
use crate::link::DrawingText;

const LINK_LOG_TARGET: &str = concat!(env!("CARGO_CRATE_NAME"), "::link");

/// Pause between tries while the brain starts or rotates its capability.
pub(crate) const RETRY_DELAY: Duration = Duration::from_millis(50);
/// How long each handshake read or write may take.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// `ERROR_PIPE_BUSY`: the pipe exists but another client holds it.
const PIPE_BUSY: i32 = 231;

/// Why a connection attempt failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("no brain pipe")]
    NoBrain,
    #[error("the brain pipe is busy")]
    Busy,
    #[error("can't start the brain")]
    Launch(#[source] std::io::Error),
    #[error("the pipe isn't served by the brain beside the shell")]
    NotOurBrain,
    #[error("no capability yet")]
    Capability(#[from] CredentialError),
    #[error("the brain closed the connection during the handshake")]
    Rejected,
    #[error("the queued connection attempt expired")]
    ConnectExpired,
    #[error("the brain is from another build")]
    BuildMismatch,
    #[error("quitting")]
    Quitting,
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(std::io::Error),
}

/// An authenticated connection. Dropping it closes both pipes.
#[derive(Debug)]
pub(crate) struct Connection {
    pub(crate) control: Arc<PipeClient>,
    pub(crate) data: Arc<PipeClient>,
    pub(crate) issue_origin: IssueSession,
    pub(crate) drawing_font_family: Option<Arc<str>>,
}

/// Connects, starting the brain once if none answers. Legacy uses the deadline for retries;
/// `QueuedRequired` uses it as a hard deadline for the whole attempt.
#[expect(
    clippy::too_many_arguments,
    reason = "the link supplies the pair identity, deadline and existing callbacks"
)]
pub(crate) fn connect_brain(
    brain: &Path,
    session: u32,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    quitting: impl Fn() -> bool,
    quit_parked: impl Fn(&PipeClient),
    mut text_metrics: impl FnMut(Instant) -> DrawingText,
) -> Result<Connection, LinkError> {
    connect_with(
        audio,
        deadline,
        quitting,
        quit_parked,
        |attempt| {
            handshake(brain, session, audio, repair_of, attempt, || {
                text_metrics(deadline)
            })
        },
        || launch_detached(brain),
    )
}

pub(crate) fn connect_with(
    audio: AudioSession,
    deadline: Instant,
    quitting: impl Fn() -> bool,
    quit_parked: impl Fn(&PipeClient),
    mut handshake: impl FnMut(Option<Instant>) -> Result<Connection, LinkError>,
    mut launch: impl FnMut() -> std::io::Result<()>,
) -> Result<Connection, LinkError> {
    let attempt = (audio == AudioSession::QueuedRequired).then_some(deadline);
    let mut launched = false;
    loop {
        check_attempt(attempt)?;
        let error = match handshake(attempt) {
            Ok(connection) if check_attempt(attempt).is_err() => {
                if quitting() {
                    quit_parked(&connection.control);
                }
                return Err(LinkError::ConnectExpired);
            }
            Ok(connection) => return Ok(connection),
            Err(LinkError::NoBrain) if quitting() && !launched => {
                return Err(LinkError::Quitting);
            }
            Err(LinkError::NoBrain) if !launched => {
                check_attempt(attempt)?;
                launch().map_err(LinkError::Launch)?;
                launched = true;
                continue;
            }
            // That brain exits by itself; start ours once its pipe is gone.
            Err(LinkError::BuildMismatch) => {
                launched = false;
                LinkError::BuildMismatch
            }
            Err(error @ LinkError::NotOurBrain) => {
                return Err(error);
            }
            Err(error) => error,
        };
        // A brain we started is kept so it can be told to quit; otherwise nothing waits on us.
        if quitting() && !launched {
            return Err(LinkError::Quitting);
        }
        if Instant::now() >= deadline {
            return Err(error);
        }
        tracing::debug!(target: LINK_LOG_TARGET, %error, "retrying");
        thread::sleep(attempt.map_or(RETRY_DELAY, |end| {
            RETRY_DELAY.min(end.saturating_duration_since(Instant::now()))
        }));
    }
}

fn handshake(
    brain: &Path,
    session: u32,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    attempt: Option<Instant>,
    text_metrics: impl FnOnce() -> DrawingText,
) -> Result<Connection, LinkError> {
    let control = open(&Pipe::Control.name(session), brain, attempt)?;
    // A brain that hangs mid-handshake must not hang the shell with it.
    control.set_timeout(Some(HANDSHAKE_TIMEOUT));
    let hello = Hello {
        build_id: BUILD_ID.to_owned(),
        capability: credential::read(session)?,
    };
    handshake_pair(
        control,
        hello,
        audio,
        repair_of,
        attempt,
        text_metrics(),
        || open(&Pipe::Data.name(session), brain, attempt),
    )
}

pub(crate) fn handshake_pair(
    control: PipeClient,
    hello: Hello,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    attempt: Option<Instant>,
    drawing: DrawingText,
    open_data: impl FnOnce() -> Result<PipeClient, LinkError>,
) -> Result<Connection, LinkError> {
    let mut handshake = HandshakeIo(&control, attempt);
    write_control_hello(&mut handshake, hello.clone(), audio, repair_of)?;
    let issue_origin = welcome(&mut handshake)?;
    check_attempt(attempt)?;
    let data_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let data_deadline = attempt.map_or(data_deadline, |deadline| deadline.min(data_deadline));
    let data = open_data()?;
    let mut handshake = HandshakeIo(&data, Some(data_deadline));
    write_data_hello(&mut handshake, hello, audio, issue_origin)?;
    let profile = drawing.profile.filter(TextMetricsV1::valid);
    frame::write(
        &mut handshake,
        &DataToBrain::TextMetrics(profile),
        Pipe::Data,
    )?;
    data.set_timeout(None);
    control.set_timeout(None);
    Ok(Connection {
        control: Arc::new(control),
        data: Arc::new(data),
        issue_origin,
        drawing_font_family: drawing.font_family,
    })
}

/// Starts a parent-bound brain as the shell's child and connects over its pair, all by
/// `deadline`. Returns the connection and the child's owner, which retires the child when dropped.
///
/// # Errors
/// `Quitting` when Quit came first, `Launch` when no child started, or why the pair failed.
#[cfg(target_os = "macos")]
#[expect(dead_code, reason = "the link doesn't connect over the pair until R2")]
#[expect(
    clippy::too_many_arguments,
    reason = "the link supplies the pair identity, deadline and existing callbacks"
)]
pub(crate) fn connect_pair(
    brain: &Path,
    word: Arc<super::SpawnWord>,
    secret: &[u8; kumochi_session::SECRET_LEN],
    channel: kumochi_session::Channel,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    text_metrics: impl FnOnce(Instant) -> DrawingText,
) -> Result<(Connection, super::child::ChildOwner), LinkError> {
    use super::child::Refusal;
    use super::macos::NotStarted;
    let (owner, ends) = super::macos::start_brain(brain, word).map_err(|error| match error {
        NotStarted::Refused(Refusal::Quitting) => LinkError::Quitting,
        NotStarted::Refused(Refusal::Spawn(error)) => LinkError::Launch(error),
        NotStarted::Refused(Refusal::Claimed | Refusal::AutoReap) => {
            LinkError::Launch(std::io::ErrorKind::ResourceBusy.into())
        }
        NotStarted::Pending => LinkError::Launch(std::io::ErrorKind::TimedOut.into()),
    })?;
    let connection = over_pair(
        ends,
        secret,
        channel,
        audio,
        repair_of,
        deadline,
        text_metrics(deadline),
    )?;
    Ok((connection, owner))
}

/// The session path's attempt: the initiator, both greetings with the zero placeholder, and then
/// the first brain control frame, which must be Settings. The runtime sends Settings first once
/// it has committed, so it's the attempt's completion point. It stays in the control leg, so the
/// link reads it first and in order.
#[cfg(target_os = "macos")]
pub(super) fn over_pair(
    ends: super::macos::ShellEnds,
    secret: &[u8; kumochi_session::SECRET_LEN],
    channel: kumochi_session::Channel,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    drawing: DrawingText,
) -> Result<Connection, LinkError> {
    let (control, data) =
        super::macos::initiate(ends, secret, channel, deadline).map_err(LinkError::Io)?;
    let hello = Hello::session(BUILD_ID.to_owned());
    let connection = handshake_pair(
        control,
        hello,
        audio,
        repair_of,
        Some(deadline),
        drawing,
        || Ok(data),
    )?;
    HandshakeIo(&connection.control, Some(deadline))
        .bounded()
        .map_err(LinkError::Io)?;
    let settings = connection.control.peek_control();
    connection.control.set_timeout(None);
    check_attempt(Some(deadline))?;
    match settings? {
        Some(ControlToShell::Settings { .. }) => Ok(connection),
        _ => Err(LinkError::Rejected),
    }
}

fn write_control_hello(
    writer: &mut impl Write,
    hello: Hello,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
) -> Result<(), FrameError> {
    let greeting = match (audio, repair_of) {
        (AudioSession::Legacy, _) => ControlToBrain::Hello(hello),
        (AudioSession::QueuedRequired, None) => ControlToBrain::QueuedHello(hello),
        (AudioSession::QueuedRequired, Some(previous)) => {
            ControlToBrain::QueuedAudioReconnect(Box::new(hello), previous)
        }
    };
    frame::write(writer, &greeting, Pipe::Control)
}

fn write_data_hello(
    writer: &mut impl Write,
    hello: Hello,
    audio: AudioSession,
    issue_origin: IssueSession,
) -> Result<(), FrameError> {
    let greeting = match audio {
        AudioSession::Legacy => DataToBrain::Hello(hello),
        AudioSession::QueuedRequired => DataToBrain::QueuedHello(hello, issue_origin),
    };
    frame::write(writer, &greeting, Pipe::Data)
}

fn check_attempt(attempt: Option<Instant>) -> Result<(), LinkError> {
    if attempt.is_some_and(|end| Instant::now() >= end) {
        return Err(LinkError::ConnectExpired);
    }
    Ok(())
}

struct HandshakeIo<'a>(&'a PipeClient, Option<Instant>);

impl<'a> HandshakeIo<'a> {
    fn bounded(&self) -> std::io::Result<&'a PipeClient> {
        if let Some(end) = self.1 {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.0.set_timeout(Some(remaining.min(HANDSHAKE_TIMEOUT)));
        }
        Ok(self.0)
    }
}

impl Read for HandshakeIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.bounded()?.read(bytes)
    }
}

impl Write for HandshakeIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bounded()?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The brain's answer to Hello: its issue session, or why this brain can't be used.
pub(crate) fn welcome(control: &mut impl Read) -> Result<IssueSession, LinkError> {
    match frame::read::<ControlToShell>(control, Pipe::Control)? {
        None
        | Some(
            ControlToShell::RunState { .. }
            | ControlToShell::Ping
            | ControlToShell::Scenes { .. }
            | ControlToShell::ThreadDeleted { .. }
            | ControlToShell::TurnRefused { .. }
            | ControlToShell::Settings { .. }
            | ControlToShell::SpokenRepliesDefaulted(_)
            | ControlToShell::Error(_)
            | ControlToShell::ScenesV1 { .. }
            | ControlToShell::NoVisual { .. }
            | ControlToShell::Account(_)
            | ControlToShell::AccountUnsupported(_)
            | ControlToShell::ThreadsChanged
            | ControlToShell::Focus { .. }
            | ControlToShell::CaptureDone { .. }
            | ControlToShell::CaptureFailed { .. }
            | ControlToShell::ThreadModelSet { .. }
            | ControlToShell::GitHub(_)
            | ControlToShell::GitHubError(_)
            | ControlToShell::DrawingReplacementFailed { .. }
            | ControlToShell::DrawingNotReturned { .. }
            | ControlToShell::ActionState { .. }
            | ControlToShell::IssueRefused { .. }
            | ControlToShell::IssueUnavailable { .. }
            | ControlToShell::Connections(_)
            | ControlToShell::AnthropicKeyStage { .. }
            | ControlToShell::AnthropicKeyStageDone { .. }
            | ControlToShell::AnthropicKeyRemoved { .. }
            | ControlToShell::AnthropicKeyReconciled { .. }
            | ControlToShell::SpeechRevoked { .. }
            | ControlToShell::SpeechDecisionTaken(..)
            | ControlToShell::QueuedAudioRegistered(..)
            | ControlToShell::VoiceHoldStatus { .. }
            | ControlToShell::VoiceHoldStopAccepted { .. }
            | ControlToShell::ScreenHolds(_)
            | ControlToShell::FixtureEngineActive
            | ControlToShell::HoldScreen { .. }
            | ControlToShell::Voice(_),
        ) => Err(LinkError::Rejected),
        Some(ControlToShell::BuildMismatch { .. }) => {
            tracing::warn!(target: LINK_LOG_TARGET, shell_build = BUILD_ID, "brain from another build");
            Err(LinkError::BuildMismatch)
        }
        Some(ControlToShell::Welcome {
            incarnation,
            issue_epoch,
        }) => Ok(IssueSession {
            incarnation,
            epoch: issue_epoch,
        }),
    }
}

/// Opens a pipe and checks that the brain beside the shell serves it.
pub(crate) fn open(
    name: &str,
    brain: &Path,
    attempt: Option<Instant>,
) -> Result<PipeClient, LinkError> {
    check_attempt(attempt)?;
    let pipe = PipeClient::open(name).map_err(|error| match error.raw_os_error() {
        _ if error.kind() == std::io::ErrorKind::NotFound => LinkError::NoBrain,
        Some(PIPE_BUSY) => LinkError::Busy,
        _ => LinkError::Io(error),
    })?;
    check_attempt(attempt)?;
    let server = pipe.server_image().map_err(LinkError::Io)?;
    if !server.as_os_str().eq_ignore_ascii_case(brain.as_os_str()) {
        return Err(LinkError::NotOurBrain);
    }
    check_attempt(attempt)?;
    Ok(pipe)
}

#[cfg(test)]
#[path = "connect_diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(test)]
#[path = "connect_greeting_tests.rs"]
mod greeting_tests;

#[cfg(all(test, windows))]
#[path = "connect_tests.rs"]
mod tests;
