//! The operating system behind the models: the plain host and input types, and the calls the
//! models make themselves (folders, the credential store, the microphone). Windows only for now.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Mac connection starts its brain child from I1e-2"
    )
)]
mod child;
pub mod clipboard;
mod connect;
pub mod credential;
pub mod host;
pub mod input;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod microphone;
pub mod screen_watch;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod speaker_cpal;

#[cfg(not(windows))]
mod unsupported;

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls are isolated to this platform adapter"
)]
mod windows;

#[cfg(all(test, windows))]
pub(crate) mod connect_test {
    pub(crate) use super::connect::{LinkError, connect_with, handshake_pair, welcome};
}

#[cfg(target_os = "macos")]
pub(crate) use child::SpawnWord;
pub(crate) use connect::{Connection, HANDSHAKE_TIMEOUT, RETRY_DELAY, connect_brain};
#[cfg(windows)]
#[cfg(debug_assertions)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use connect::{handshake_pair, open};
#[cfg(not(windows))]
pub(crate) use unsupported::PipeClient;
#[cfg(not(windows))]
use unsupported::launch_detached;
#[cfg(not(windows))]
pub use unsupported::session_id;
#[cfg(windows)]
pub(crate) use windows::PipeClient;
#[cfg(windows)]
use windows::launch_detached;
#[cfg(windows)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use windows::pipe::test_pair;
#[cfg(windows)]
pub use windows::{data_dir, session_id};

#[cfg(not(target_os = "macos"))]
pub(crate) fn instance_lock_path() -> Result<std::path::PathBuf, crate::instance::SessionLockError>
{
    use crate::instance::SessionLockError;

    #[cfg(windows)]
    {
        let data_dir = data_dir().ok_or(SessionLockError::DataDirectory)?;
        let session = session_id().map_err(SessionLockError::Session)?;
        let name = kumochi_protocol::constants::SHELL_LOCK;
        let path = data_dir.join(format!("{name}-{session}.lock"));
        kumochi_storage::admit_leaf(&path).map_err(|_| SessionLockError::DataDirectory)?;
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        Err(SessionLockError::Unsupported)
    }
}

pub(crate) fn logging_data_dir() -> Result<std::path::PathBuf, crate::logging::LoggingError> {
    #[cfg(windows)]
    {
        data_dir().ok_or(crate::logging::LoggingError::NoLocalAppData)
    }
    #[cfg(not(windows))]
    {
        Err(crate::logging::LoggingError::Unsupported)
    }
}
