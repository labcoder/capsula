//! Finds or starts the brain and acquires both authenticated legs before link attendance.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, Hello, IssueSession,
};

use super::{PipeClient, launch_detached};
use crate::credential::{self, CredentialError};

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
}

/// Connects, starting the brain once if none answers, until the link deadline.
pub(crate) fn connect_brain(
    brain: &Path,
    session: u32,
    audio: AudioSession,
    deadline: Instant,
    quitting: impl Fn() -> bool,
    quit_parked: impl Fn(&PipeClient),
) -> Result<Connection, LinkError> {
    connect_with(
        audio,
        deadline,
        quitting,
        quit_parked,
        |attempt| handshake(brain, session, audio, attempt),
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
    attempt: Option<Instant>,
) -> Result<Connection, LinkError> {
    let control = open(&Pipe::Control.name(session), brain, attempt)?;
    // A brain that hangs mid-handshake must not hang the shell with it.
    control.set_timeout(Some(HANDSHAKE_TIMEOUT));
    let hello = Hello {
        build_id: BUILD_ID.to_owned(),
        capability: credential::read(session)?,
    };
    handshake_pair(control, hello, audio, attempt, || {
        open(&Pipe::Data.name(session), brain, attempt)
    })
}

pub(crate) fn handshake_pair(
    control: PipeClient,
    hello: Hello,
    audio: AudioSession,
    attempt: Option<Instant>,
    open_data: impl FnOnce() -> Result<PipeClient, LinkError>,
) -> Result<Connection, LinkError> {
    let mut handshake = HandshakeIo(&control, attempt);
    write_control_hello(&mut handshake, hello.clone(), audio)?;
    let issue_origin = welcome(&mut handshake)?;
    check_attempt(attempt)?;
    let data = open_data()?;
    if audio == AudioSession::QueuedRequired {
        data.set_timeout(Some(HANDSHAKE_TIMEOUT));
    }
    write_data_hello(&mut HandshakeIo(&data, attempt), hello, audio, issue_origin)?;
    if audio == AudioSession::QueuedRequired {
        data.set_timeout(None);
    }
    control.set_timeout(None);
    Ok(Connection {
        control: Arc::new(control),
        data: Arc::new(data),
        issue_origin,
    })
}

fn write_control_hello(
    writer: &mut impl Write,
    hello: Hello,
    audio: AudioSession,
) -> Result<(), FrameError> {
    let greeting = match audio {
        AudioSession::Legacy => ControlToBrain::Hello(hello),
        AudioSession::QueuedRequired => ControlToBrain::QueuedHello(hello),
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
