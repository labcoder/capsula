//! Native peer-checked pipes. Shared framing and session logic live outside the OS adapter.

/// Whether a pipe instance is the first with its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instance {
    /// Fails if any process already holds the name.
    First,
    /// Replaces an instance this process holds, so the name is never free in between.
    Next,
    /// The first and only instance: the OS refuses any later one, whatever its flags.
    Only,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BridgeConnectError {
    #[error("can't open or check the bridge pipe")]
    Io(#[from] std::io::Error),
    #[cfg_attr(
        not(windows),
        expect(
            dead_code,
            reason = "only a native pipe can report a server image mismatch"
        )
    )]
    #[error("bridge server image mismatch")]
    NotOurBrain,
}

#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only a native pipe can classify a peer")
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientRefused {
    Exited,
    NotOurImage,
    NotInJob,
    Query,
}

#[cfg(not(windows))]
mod unsupported {
    use std::future::Ready;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::{BridgeConnectError, ClientRefused, Instance};
    use crate::platform::{Job, ShellProcess};

    /// A native server can't exist without a platform pipe adapter.
    #[derive(Debug)]
    pub enum PipeServer {}
    pub(crate) type PipeClient = PipeServer;

    impl PipeServer {
        pub(crate) fn disconnect(&self) -> io::Result<()> {
            match *self {}
        }

        pub(crate) fn connect(&self) -> Ready<io::Result<()>> {
            match *self {}
        }
    }

    impl AsyncRead for PipeServer {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match *self {}
        }
    }

    impl AsyncWrite for PipeServer {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            match *self {}
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            match *self {}
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            match *self {}
        }
    }

    /// # Errors
    /// No native owner-only pipe adapter is available.
    pub fn create_owner_only(_: &str, _: Instance) -> io::Result<PipeServer> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn open_bridge(_: &str, _: &Path) -> Result<PipeClient, BridgeConnectError> {
        Err(io::Error::from(io::ErrorKind::Unsupported).into())
    }

    /// # Errors
    /// No native pipe session ID is available.
    pub fn session_id() -> io::Result<u32> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn bridge_peer(
        pipe: &PipeServer,
        _: &Job,
        _: &Path,
    ) -> Result<std::convert::Infallible, ClientRefused> {
        match *pipe {}
    }

    pub(crate) fn client_image(pipe: &PipeServer) -> io::Result<PathBuf> {
        match *pipe {}
    }

    pub(crate) fn shell_connection(
        control: &PipeServer,
        _: &PipeServer,
        _: &Path,
    ) -> io::Result<ShellProcess> {
        match *control {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_pipes_and_session_id_refuse_without_opening_a_transport() {
            for instance in [Instance::First, Instance::Next, Instance::Only] {
                assert_eq!(
                    create_owner_only("no native transport", instance)
                        .map(|_| ())
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::Unsupported
                );
            }
            let Err(BridgeConnectError::Io(error)) =
                open_bridge("no native transport", Path::new("expected-image"))
            else {
                panic!("an unsupported platform refuses to open the bridge");
            };
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(session_id().unwrap_err().kind(), io::ErrorKind::Unsupported);
        }
    }
}

#[cfg(not(windows))]
pub(crate) use unsupported::{
    PipeClient, bridge_peer, client_image, open_bridge, shell_connection,
};
#[cfg(not(windows))]
pub use unsupported::{PipeServer, create_owner_only, session_id};
