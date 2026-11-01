//! The shell's connection to the brain. It runs on its own thread, because pipe reads block, and
//! reports [`LinkState`] changes to the UI thread over a channel.
//!
//! Connecting starts the brain when no brain pipe answers, authenticates with the capability
//! from Credential Manager, and checks that the pipe's server is the brain beside the shell.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::Sender;
use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::BRAIN_EXE;
use capsula_protocol::frame::{self, FrameError, Pipe};
use capsula_protocol::message::{ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation};

use crate::credential::{self, CredentialError};
use crate::platform::{self, PipeClient};

/// How long one connection attempt may take, starting the brain included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between tries while the brain starts or rotates its capability.
const RETRY_DELAY: Duration = Duration::from_millis(50);
/// `ERROR_PIPE_BUSY`: the pipe exists but another client holds it.
const PIPE_BUSY: i32 = 231;

/// What the UI shows about the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

/// Why a connection attempt failed.
#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("no brain pipe")]
    NoBrain,
    #[error("the brain pipe is busy")]
    Busy,
    #[error("can't start the brain")]
    Launch(#[source] std::io::Error),
    #[error("the pipe is served by {0:?}, not the brain beside the shell")]
    NotOurBrain(PathBuf),
    #[error("no capability yet")]
    Capability(#[from] CredentialError),
    #[error("the brain closed the connection during the handshake")]
    Rejected,
    #[error("the brain is from another build")]
    BuildMismatch,
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(std::io::Error),
}

/// An authenticated connection. Dropping it closes both pipes.
#[derive(Debug)]
struct Connection {
    control: PipeClient,
    #[expect(dead_code, reason = "held open; slice 2 streams on it")]
    data: PipeClient,
    incarnation: Incarnation,
}

/// Where to find the brain.
#[derive(Debug)]
struct Link {
    brain: PathBuf,
    session: u32,
}

/// Starts the link thread. It reports every state change on `states` and ends when the UI
/// drops the receiver.
///
/// # Errors
/// Fails when the shell can't find its own path or session, or the thread can't start.
pub(crate) fn spawn(states: Sender<LinkState>) -> std::io::Result<()> {
    let link = Link {
        brain: std::env::current_exe()?.with_file_name(BRAIN_EXE),
        session: platform::session_id()?,
    };
    thread::Builder::new()
        .name("link".to_owned())
        .spawn(move || link.run(&states))?;
    Ok(())
}

impl Link {
    fn run(&self, states: &Sender<LinkState>) {
        let mut state = LinkState::Connecting;
        loop {
            if states.send_blocking(state).is_err() {
                return;
            }
            let connection = match self.connect() {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::error!(%error, "can't reach the brain");
                    let _ui_gone = states.send_blocking(LinkState::Failed);
                    return;
                }
            };
            tracing::info!(incarnation = ?connection.incarnation, "connected to the brain");
            if states.send_blocking(LinkState::Connected).is_err() {
                return;
            }
            watch(&connection);
            tracing::warn!("lost the brain");
            state = LinkState::Reconnecting;
        }
    }

    /// Connects, starting the brain once if none answers, until [`CONNECT_TIMEOUT`].
    fn connect(&self) -> Result<Connection, LinkError> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut launched = false;
        loop {
            let error = match self.handshake() {
                Ok(connection) => return Ok(connection),
                Err(LinkError::NoBrain) if !launched => {
                    platform::launch_detached(&self.brain).map_err(LinkError::Launch)?;
                    launched = true;
                    continue;
                }
                // That brain exits by itself; start ours once its pipe is gone.
                Err(LinkError::BuildMismatch) => {
                    launched = false;
                    LinkError::BuildMismatch
                }
                Err(error @ LinkError::NotOurBrain(_)) => return Err(error),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(error);
            }
            tracing::debug!(%error, "retrying");
            thread::sleep(RETRY_DELAY);
        }
    }

    fn handshake(&self) -> Result<Connection, LinkError> {
        let control = open(&Pipe::Control.name(self.session), &self.brain)?;
        let hello = Hello {
            build_id: BUILD_ID.to_owned(),
            capability: credential::read()?,
        };
        frame::write(
            &mut &control,
            &ControlToBrain::Hello(hello.clone()),
            Pipe::Control,
        )?;
        let incarnation = match frame::read::<ControlToShell>(&mut &control, Pipe::Control)? {
            None => return Err(LinkError::Rejected),
            Some(ControlToShell::BuildMismatch { brain_build }) => {
                tracing::warn!(
                    brain_build,
                    shell_build = BUILD_ID,
                    "brain from another build"
                );
                return Err(LinkError::BuildMismatch);
            }
            Some(ControlToShell::Welcome { incarnation }) => incarnation,
        };
        let data = open(&Pipe::Data.name(self.session), &self.brain)?;
        frame::write(&mut &data, &DataToBrain::Hello(hello), Pipe::Data)?;
        Ok(Connection {
            control,
            data,
            incarnation,
        })
    }
}

/// Opens a pipe and checks that the brain beside the shell serves it.
fn open(name: &str, brain: &Path) -> Result<PipeClient, LinkError> {
    let pipe = PipeClient::open(name).map_err(|error| match error.raw_os_error() {
        _ if error.kind() == std::io::ErrorKind::NotFound => LinkError::NoBrain,
        Some(PIPE_BUSY) => LinkError::Busy,
        _ => LinkError::Io(error),
    })?;
    let server = pipe.server_image().map_err(LinkError::Io)?;
    if !server.as_os_str().eq_ignore_ascii_case(brain.as_os_str()) {
        return Err(LinkError::NotOurBrain(server));
    }
    Ok(pipe)
}

/// Blocks until the brain goes away. Slice 0's brain sends nothing after `Welcome`.
fn watch(connection: &Connection) {
    loop {
        match frame::read::<ControlToShell>(&mut &connection.control, Pipe::Control) {
            Ok(None) => return,
            Ok(Some(message)) => tracing::warn!(?message, "unexpected message"),
            Err(error) => {
                tracing::warn!(%error, "control pipe failed");
                return;
            }
        }
    }
}
