//! The brain's end of the two pipes: who may connect, the handshake, and the connection loop.
//!
//! One shell at a time. A client must run the shell executable from the brain's own folder and
//! present the instance capability before the brain decodes anything else from it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use capsula_protocol::BUILD_ID;
use capsula_protocol::frame::{self, FrameError, HEADER_LEN, Pipe};
use capsula_protocol::message::{
    Capability, ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation,
};
use capsula_protocol::tap::Direction;
use serde::Serialize;
use serde::de::DeserializeOwned;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::platform::{self, Instance, PipeServer};
use crate::runtime::{Brain, Ended};
use crate::tap::Tap;

/// How long a connected client has to complete each step of the handshake.
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
    /// The shell sent `Quitting`.
    Quit,
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
    #[error("client is {0:?}, not the shell beside the brain")]
    NotOurShell(PathBuf),
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
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What to do with a `Hello`.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Welcome,
    WrongCapability,
    BuildMismatch,
}

/// Checks the capability first, in constant time, so nothing else is answered to a client that
/// can't prove it's the same user.
fn check_hello(hello: &Hello, identity: &Identity) -> Verdict {
    let same = hello
        .capability
        .expose()
        .ct_eq(identity.capability.expose());
    if !bool::from(same) {
        Verdict::WrongCapability
    } else if hello.build_id == BUILD_ID {
        Verdict::Welcome
    } else {
        Verdict::BuildMismatch
    }
}

/// The control and data pipes.
#[derive(Debug)]
pub struct Pipes {
    session: u32,
    control: PipeServer,
    data: PipeServer,
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
            let session = self.session(identity, brain, tap).await;
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
    ) -> Result<Session, SessionError> {
        check_client(&self.control, &identity.shell_image)?;
        let first = timeout(
            HANDSHAKE_TIMEOUT,
            read::<ControlToBrain>(&mut self.control, Pipe::Control),
        )
        .await
        .map_err(|_| SessionError::Timeout)??;
        let Some(ControlToBrain::Hello(hello)) = first else {
            return Err(first.map_or(SessionError::ClosedEarly, |_| SessionError::NotHello));
        };
        tap.record(
            Pipe::Control,
            Direction::ToBrain,
            &ControlToBrain::Hello(hello.redacted()),
        );
        match check_hello(&hello, identity) {
            Verdict::WrongCapability => return Err(SessionError::WrongCapability),
            Verdict::BuildMismatch => {
                tracing::warn!(shell = hello.build_id, brain = BUILD_ID, "build mismatch");
                let reply = ControlToShell::BuildMismatch {
                    brain_build: BUILD_ID.to_owned(),
                };
                send(&mut self.control, &reply, tap).await?;
                return Ok(Session::BuildMismatch);
            }
            Verdict::Welcome => {}
        }
        let welcome = ControlToShell::Welcome {
            incarnation: identity.incarnation,
        };
        send(&mut self.control, &welcome, tap).await?;

        timeout(HANDSHAKE_TIMEOUT, self.data.connect())
            .await
            .map_err(|_| SessionError::Timeout)??;
        check_client(&self.data, &identity.shell_image)?;
        let first = timeout(
            HANDSHAKE_TIMEOUT,
            read::<DataToBrain>(&mut self.data, Pipe::Data),
        )
        .await
        .map_err(|_| SessionError::Timeout)??;
        let Some(DataToBrain::Hello(hello)) = first else {
            return Err(SessionError::ClosedEarly);
        };
        tap.record(
            Pipe::Data,
            Direction::ToBrain,
            &DataToBrain::Hello(hello.redacted()),
        );
        if check_hello(&hello, identity) != Verdict::Welcome {
            return Err(SessionError::WrongCapability);
        }
        tracing::info!("shell connected");
        Ok(
            match brain.serve(&mut self.control, &mut self.data, tap).await {
                Ended::Quit => Session::Quit,
                Ended::Lost => Session::ShellLost,
                Ended::Broken(reason) => return Err(SessionError::Protocol(reason)),
            },
        )
    }
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
    let image = platform::client_image(pipe)?;
    if image
        .as_os_str()
        .eq_ignore_ascii_case(shell_image.as_os_str())
    {
        Ok(())
    } else {
        Err(SessionError::NotOurShell(image))
    }
}

async fn send<T: Serialize>(
    pipe: &mut PipeServer,
    message: &T,
    tap: &mut Tap,
) -> Result<(), SessionError> {
    tap.record(Pipe::Control, Direction::ToShell, message);
    pipe.write_all(&frame::encode(message, Pipe::Control)?)
        .await?;
    Ok(())
}

/// Reads one frame. `None` means the client closed between frames, which on a named pipe
/// arrives as a broken-pipe error rather than a zero-length read.
pub(crate) async fn read<T: DeserializeOwned>(
    pipe: &mut (impl AsyncRead + Unpin),
    kind: Pipe,
) -> Result<Option<T>, FrameError> {
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
    let mut payload = vec![0; frame::payload_len(header, kind)?];
    pipe.read_exact(&mut payload).await.map_err(truncated)?;
    frame::decode(&payload).map(Some)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use capsula_protocol::message::{CAPABILITY_LEN, Hello, Incarnation};
    use capsula_protocol::{BUILD_ID, Secret};

    use tokio::sync::oneshot;

    use super::{Identity, Pipes, Session, Verdict, check_hello};
    use crate::tap::Tap;
    use crate::test_support::{self, Scratch};

    fn identity() -> Identity {
        Identity {
            capability: Secret::new([1; CAPABILITY_LEN]),
            incarnation: Incarnation(1),
            shell_image: PathBuf::from(r"C:\capsula.exe"),
        }
    }

    fn hello(byte: u8, build: &str) -> Hello {
        Hello {
            build_id: build.to_owned(),
            capability: Secret::new([byte; CAPABILITY_LEN]),
        }
    }

    #[test]
    fn hello_needs_the_capability_before_anything_else() {
        let id = identity();
        assert_eq!(check_hello(&hello(1, BUILD_ID), &id), Verdict::Welcome);
        assert_eq!(
            check_hello(&hello(2, BUILD_ID), &id),
            Verdict::WrongCapability
        );
        assert_eq!(
            check_hello(&hello(1, "0.0.0+other"), &id),
            Verdict::BuildMismatch
        );
        assert_eq!(
            check_hello(&hello(2, "0.0.0+other"), &id),
            Verdict::WrongCapability,
            "a client without the capability doesn't learn it has the wrong build",
        );
        let mut one_byte_off = [1; CAPABILITY_LEN];
        one_byte_off[CAPABILITY_LEN - 1] = 0;
        let near = Hello {
            build_id: BUILD_ID.to_owned(),
            capability: Secret::new(one_byte_off),
        };
        assert_eq!(check_hello(&near, &id), Verdict::WrongCapability);
    }

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
