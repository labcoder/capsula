//! Finds or starts the brain and acquires both authenticated legs before link attendance.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
#[cfg(not(target_os = "macos"))]
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, Hello, IssueSession, ShellTuning,
    TextMetricsV1,
};

use super::PipeClient;
#[cfg(not(target_os = "macos"))]
use super::launch_detached;
#[cfg(not(target_os = "macos"))]
use crate::credential;
use crate::credential::CredentialError;
use crate::link::DrawingText;

const LINK_LOG_TARGET: &str = concat!(env!("CARGO_CRATE_NAME"), "::link");

/// Pause between tries while the brain starts or rotates its capability.
pub(crate) const RETRY_DELAY: Duration = Duration::from_millis(50);
/// How long each handshake read or write may take.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// `ERROR_PIPE_BUSY`: the pipe exists but another client holds it.
#[cfg(not(target_os = "macos"))]
const PIPE_BUSY: i32 = 231;

/// A failed attempt: why, and what it knows of a child it may have started.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) error: LinkError,
    /// None when the attempt owns no child, as for a detached brain.
    pub(crate) child: Option<super::Unconnected>,
}

impl From<LinkError> for Failed {
    fn from(error: LinkError) -> Self {
        Self { error, child: None }
    }
}

/// Why a connection attempt failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[cfg(not(target_os = "macos"))]
    #[error("no brain pipe")]
    NoBrain,
    #[error("the brain pipe is busy")]
    Busy,
    #[error("can't start the brain")]
    Launch(#[source] std::io::Error),
    #[cfg(not(target_os = "macos"))]
    #[error("the pipe isn't served by the brain beside the shell")]
    NotOurBrain,
    #[error("no capability yet")]
    Capability(#[from] CredentialError),
    #[error("the brain closed the connection during the handshake")]
    Rejected,
    #[error("missing or invalid shell tuning")]
    Tuning,
    #[error("the queued connection attempt expired")]
    ConnectExpired,
    #[error("the brain is from another build")]
    BuildMismatch,
    #[cfg(target_os = "macos")]
    #[error("can't read the instance item")]
    Item(#[source] kumochi_storage::ItemError),
    #[cfg(target_os = "macos")]
    #[error("can't turn off core dumps")]
    Harden,
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
    pub(crate) tuning: ShellTuning,
}

/// Connects, starting the brain once if none answers. Legacy uses the deadline for retries;
/// `QueuedRequired` uses it as a hard deadline for the whole attempt.
#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
pub(crate) fn connect_with(
    deadline: Instant,
    quitting: impl Fn() -> bool,
    quit_parked: impl Fn(&PipeClient),
    mut handshake: impl FnMut(Option<Instant>) -> Result<Connection, LinkError>,
    mut launch: impl FnMut() -> std::io::Result<()>,
) -> Result<Connection, LinkError> {
    let attempt = Some(deadline);
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

#[cfg(not(target_os = "macos"))]
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
    let limit = Instant::now() + HANDSHAKE_TIMEOUT;
    let deadline = Some(attempt.map_or(limit, |end| end.min(limit)));
    let mut handshake = HandshakeIo(&control, deadline);
    write_control_hello(&mut handshake, hello.clone(), audio, repair_of)?;
    let issue_origin = welcome(&mut handshake)?;
    check_attempt(deadline)?;
    let data = open_data()?;
    let mut data_handshake = HandshakeIo(&data, deadline);
    write_data_hello(&mut data_handshake, hello, audio, issue_origin)?;
    let profile = drawing.profile.filter(TextMetricsV1::valid);
    frame::write(
        &mut data_handshake,
        &DataToBrain::TextMetrics(profile),
        Pipe::Data,
    )?;
    let tuning = read_shell_tuning(&mut handshake, issue_origin)?;
    check_attempt(deadline)?;
    data.set_timeout(None);
    control.set_timeout(None);
    Ok(Connection {
        control: Arc::new(control),
        data: Arc::new(data),
        issue_origin,
        drawing_font_family: drawing.font_family,
        tuning,
    })
}

pub(crate) fn read_shell_tuning(
    control: &mut impl Read,
    origin: IssueSession,
) -> Result<ShellTuning, LinkError> {
    match frame::read(control, Pipe::Control)? {
        Some(ControlToShell::ShellTuning { session, tuning })
            if session == origin && tuning.valid() =>
        {
            Ok(tuning)
        }
        _ => Err(LinkError::Tuning),
    }
}

/// One attempt to reach the brain, by `deadline`: a detached brain over its pipes, or on macOS
/// a parent-bound child over its pair. Returns the connection and, only for a child, its owner,
/// which the link settles once the connection ends.
///
/// # Errors
/// Why the attempt failed and, for a child, how far it got; a child that started is settled
/// before the error returns.
#[expect(
    clippy::too_many_arguments,
    reason = "the link supplies the brain, its identity, deadline and existing callbacks"
)]
pub(crate) fn connect(
    brain: &Path,
    #[cfg_attr(
        target_os = "macos",
        expect(unused_variables, reason = "the pair has no names")
    )]
    session: u32,
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_variables, reason = "only the pair starts a child")
    )]
    word: &Arc<super::SpawnWord>,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    #[cfg_attr(
        target_os = "macos",
        expect(unused_variables, reason = "the child's owner retires it after Quit")
    )]
    quitting: impl Fn() -> bool,
    #[cfg_attr(
        target_os = "macos",
        expect(unused_variables, reason = "the child's owner retires it after Quit")
    )]
    quit_parked: impl Fn(&PipeClient),
    text_metrics: impl FnMut(Instant) -> DrawingText,
) -> Result<(Connection, Option<super::ChildOwner>), Failed> {
    #[cfg(not(target_os = "macos"))]
    {
        connect_brain(
            brain,
            session,
            audio,
            repair_of,
            deadline,
            quitting,
            quit_parked,
            text_metrics,
        )
        .map(|connection| (connection, None))
        .map_err(Failed::from)
    }
    #[cfg(target_os = "macos")]
    {
        let channel =
            super::macos::channel().map_err(|error| not_spawned(LinkError::Launch(error)))?;
        connect_pair(
            brain,
            Arc::clone(word),
            channel,
            audio,
            repair_of,
            deadline,
            text_metrics,
        )
        .map(|(connection, owner)| (connection, Some(owner)))
    }
}

/// Starts a parent-bound brain as the shell's child and connects over its pair, all by
/// `deadline`: READY, then the instance item from the Keychain, then the initiator. Returns the
/// connection and the child's owner, which retires the child when dropped.
///
/// # Errors
/// `Quitting` when Quit came first, `Launch` when no child started, `Busy` when the child
/// exited as another session's brain holds this user's instance, or why the pair failed.
#[cfg(target_os = "macos")]
pub(crate) fn connect_pair(
    brain: &Path,
    word: Arc<super::SpawnWord>,
    channel: kumochi_session::Channel,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    text_metrics: impl FnOnce(Instant) -> DrawingText,
) -> Result<(Connection, super::child::ChildOwner), Failed> {
    check_attempt(Some(deadline)).map_err(not_spawned)?;
    connect_pair_with(
        |by| super::macos::start_brain(brain, word, by),
        super::macos::limit_core_dumps,
        super::macos::read_instance,
        channel,
        audio,
        repair_of,
        deadline,
        text_metrics,
    )
}

/// [`connect_pair`] with its spawn, its core-dump limit and its item read supplied. A failure
/// after the spawn settles the child before it returns, so no later attempt starts beside it; an
/// exit with [`BRAIN_EXIT_BUSY`](kumochi_protocol::constants::BRAIN_EXIT_BUSY) is `Busy`. Once
/// connected, custody ending aborts both legs, so a peer that keeps its ends open can't hold the
/// link.
#[cfg(target_os = "macos")]
#[expect(
    clippy::too_many_arguments,
    reason = "the pair attempt's identity, deadline and its three injected platform calls"
)]
pub(super) fn connect_pair_with(
    start: impl FnOnce(
        Instant,
    ) -> Result<
        (super::child::ChildOwner, super::macos::ShellEnds),
        super::macos::NotStarted,
    >,
    harden: impl FnOnce() -> std::io::Result<()>,
    read_item: impl FnOnce(
        &mut [u8; kumochi_session::SECRET_LEN],
    ) -> Result<(), kumochi_storage::ItemError>,
    channel: kumochi_session::Channel,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    text_metrics: impl FnOnce(Instant) -> DrawingText,
) -> Result<(Connection, super::child::ChildOwner), Failed> {
    use super::child::{Exit, Settled, Unconnected};
    use kumochi_protocol::constants::BRAIN_EXIT_BUSY;
    let (owner, ends) = start(deadline).map_err(|error| {
        let child = if matches!(error, super::macos::NotStarted::Pending) {
            Unconnected::Pending
        } else {
            Unconnected::NotSpawned
        };
        Failed {
            error: not_started(error, deadline),
            child: Some(child),
        }
    })?;
    let connected = check_attempt(Some(deadline)).and_then(|()| {
        after_ready(
            ends,
            channel,
            audio,
            repair_of,
            deadline,
            text_metrics,
            harden,
            read_item,
        )
    });
    match connected {
        Ok(connection) => {
            let legs = [&connection.control, &connection.data].map(Arc::downgrade);
            owner.on_settled(move || {
                for leg in legs.iter().filter_map(std::sync::Weak::upgrade) {
                    leg.abort();
                }
            });
            Ok((connection, owner))
        }
        Err(error) => {
            let settled = owner.settle();
            let error = match settled {
                Settled::Reaped(Exit::Code(code)) if code == i32::from(BRAIN_EXIT_BUSY) => {
                    LinkError::Busy
                }
                _ => error,
            };
            Err(Failed {
                error,
                child: Some(Unconnected::Settled(settled)),
            })
        }
    }
}

/// An attempt refused before any spawn.
#[cfg(target_os = "macos")]
fn not_spawned(error: LinkError) -> Failed {
    Failed {
        error,
        child: Some(super::Unconnected::NotSpawned),
    }
}

/// Waits for the brain's READY byte, then turns off core dumps with `harden`, then reads the
/// instance capability with `read_item`, then runs [`over_pair`]. Nothing is written and the item
/// isn't read until READY arrives, and a wrong byte or the stream ending is `Rejected`. A failed
/// `harden` is `Harden`, and the item is never read.
#[cfg(target_os = "macos")]
#[expect(
    clippy::too_many_arguments,
    reason = "the pair attempt's ends, identity, deadline and its two injected platform calls"
)]
pub(super) fn after_ready(
    ends: super::macos::ShellEnds,
    channel: kumochi_session::Channel,
    audio: AudioSession,
    repair_of: Option<IssueSession>,
    deadline: Instant,
    text_metrics: impl FnOnce(Instant) -> DrawingText,
    harden: impl FnOnce() -> std::io::Result<()>,
    read_item: impl FnOnce(
        &mut [u8; kumochi_session::SECRET_LEN],
    ) -> Result<(), kumochi_storage::ItemError>,
) -> Result<Connection, LinkError> {
    use kumochi_protocol::constants::SESSION_READY;
    match super::macos::read_ready(&ends, deadline) {
        Ok(Some(SESSION_READY)) => {}
        Ok(_) => return Err(LinkError::Rejected),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return Err(LinkError::ConnectExpired);
        }
        Err(error) => return Err(LinkError::Io(error)),
    }
    // Before the secret exists, so no core dump of this process can hold it.
    harden().map_err(|_| LinkError::Harden)?;
    let mut secret = super::macos::Secret::zeroed();
    check_attempt(Some(deadline))?;
    let read = read_item(secret.bytes_mut());
    // The Keychain call can outlast the attempt, so its answer counts only within the deadline.
    check_attempt(Some(deadline))?;
    read.map_err(LinkError::Item)?;
    over_pair(
        ends,
        secret,
        channel,
        audio,
        repair_of,
        deadline,
        text_metrics(deadline),
    )
}

/// Why the attempt ends when no child started: a spawn report still pending at the attempt's
/// deadline is that deadline, not a launch failure.
#[cfg(target_os = "macos")]
pub(super) fn not_started(error: super::macos::NotStarted, deadline: Instant) -> LinkError {
    use super::child::Refusal;
    use super::macos::NotStarted;
    match error {
        NotStarted::Refused(Refusal::Quitting) => LinkError::Quitting,
        NotStarted::Refused(Refusal::Spawn(error)) => LinkError::Launch(error),
        NotStarted::Refused(Refusal::Claimed | Refusal::AutoReap) => {
            LinkError::Launch(std::io::ErrorKind::ResourceBusy.into())
        }
        NotStarted::Pending if Instant::now() >= deadline => LinkError::ConnectExpired,
        NotStarted::Pending => LinkError::Launch(std::io::ErrorKind::TimedOut.into()),
    }
}

/// The session path's attempt: the initiator, both greetings with the zero placeholder and the
/// brain's `ShellTuning`, and then the next control frame, which must be Settings. The runtime
/// sends Settings first once it has committed, so it's the attempt's completion point. It stays in
/// the control leg, so the link reads it first and in order.
#[cfg(target_os = "macos")]
pub(super) fn over_pair(
    ends: super::macos::ShellEnds,
    secret: super::macos::Secret,
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
    let phase = deadline.min(Instant::now() + HANDSHAKE_TIMEOUT);
    let settings = connection.control.peek_control(phase);
    connection.control.set_timeout(None);
    check_attempt(Some(deadline))?;
    if Instant::now() >= phase {
        return Err(LinkError::Io(std::io::ErrorKind::TimedOut.into()));
    }
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
            | ControlToShell::ShellTuning { .. }
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
#[cfg(not(target_os = "macos"))]
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
