use std::io;
use std::path::Path;

#[cfg(not(target_os = "macos"))]
pub(crate) use pipe::PipeClient;

/// No named pipes here. macOS has its own `PipeClient` over the brain child's pair.
#[cfg(not(target_os = "macos"))]
mod pipe {
    use std::io::{self, Read, Write};
    use std::path::PathBuf;
    use std::time::Duration;

    #[derive(Debug)]
    pub(crate) enum PipeClient {}

    impl PipeClient {
        pub(crate) fn open(_: &str) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn server_image(&self) -> io::Result<PathBuf> {
            match *self {}
        }

        pub(crate) fn abort(&self) {
            match *self {}
        }

        pub(crate) fn is_aborted(&self) -> bool {
            match *self {}
        }

        pub(crate) fn set_timeout(&self, _: Option<Duration>) {
            match *self {}
        }
    }

    impl Read for &PipeClient {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            match **self {}
        }
    }

    impl Write for &PipeClient {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            match **self {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match **self {}
        }
    }
}

pub fn session_id() -> io::Result<u32> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(crate) fn launch_detached(_: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use kumochi_protocol::message::GestureId;

    use super::*;
    use crate::platform::microphone::{CollectionGate, MicError, MicOwner};

    #[test]
    fn native_ipc_credentials_and_logging_refuse_without_a_backend() {
        let unsupported = io::ErrorKind::Unsupported;
        assert_eq!(session_id().unwrap_err().kind(), unsupported);
        let (events, _) = async_channel::unbounded();
        assert!(matches!(
            crate::link::prepare(events, kumochi_protocol::message::AudioSession::Legacy),
            Err(error) if error.kind() == unsupported
        ));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(PipeClient::open("unused").unwrap_err().kind(), unsupported);
        assert_eq!(
            launch_detached(Path::new("unused")).unwrap_err().kind(),
            unsupported
        );
        assert!(matches!(
            crate::credential::read(0),
            Err(crate::credential::CredentialError::Unsupported)
        ));
        assert!(matches!(
            crate::logging::init("unused"),
            Err(crate::logging::LoggingError::Unsupported)
        ));
    }

    #[test]
    fn unsupported_capture_returns_no_owner_and_retires_the_reservation() {
        let gate = CollectionGate::default();
        let generation = gate.arm(GestureId(1)).unwrap();
        let reserved = generation.reserve().unwrap();
        let session = Arc::new(AtomicBool::new(true));
        assert!(matches!(
            MicOwner::start_reserved(reserved, Arc::clone(&session)),
            Err(MicError::Unsupported)
        ));
        assert!(generation.retired());
        assert!(matches!(
            MicOwner::start(generation, session),
            Err(MicError::Stale)
        ));
        assert!(gate.arm(GestureId(2)).is_ok());
    }
}
