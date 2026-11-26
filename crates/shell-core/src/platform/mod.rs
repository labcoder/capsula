//! The operating system behind the models: the plain host and input types, and the calls the
//! models make themselves (folders, the credential store, the microphone). Windows only for now.

#[cfg_attr(
    not(any(test, target_os = "macos")),
    expect(dead_code, reason = "only macOS starts its brain as a child")
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

#[cfg(target_os = "macos")]
mod unix;
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
    pub(crate) use super::connect::{
        LinkError, connect_with, handshake_pair, read_shell_tuning, welcome,
    };
}

pub(crate) use child::{ChildOwner, Settled, SpawnWord};
pub(crate) use connect::{Connection, HANDSHAKE_TIMEOUT, LinkError, RETRY_DELAY, connect};
#[cfg(windows)]
#[cfg(debug_assertions)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use connect::{handshake_pair, open};
#[cfg(target_os = "macos")]
pub(crate) use macos::PipeClient;
#[cfg(target_os = "macos")]
pub use macos::session_id;
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) use unsupported::PipeClient;
#[cfg(not(any(windows, target_os = "macos")))]
use unsupported::launch_detached;
#[cfg(not(any(windows, target_os = "macos")))]
pub use unsupported::session_id;
#[cfg(windows)]
pub(crate) use windows::PipeClient;
#[cfg(windows)]
use windows::launch_detached;
#[cfg(windows)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use windows::pipe::test_pair;
#[cfg(windows)]
pub use windows::{data_dir, keyboard_ticks, session_id};

#[cfg(not(windows))]
pub(crate) const fn keyboard_ticks() -> u64 {
    0
}

/// The brain the shell starts: the sibling of its executable, or on macOS the helper nested in
/// its bundle.
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        clippy::unnecessary_wraps,
        reason = "only the macOS helper can be missing, when no bundle holds the shell"
    )
)]
pub(crate) fn brain_path(exe: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos::brain_helper(exe)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(exe.with_file_name(kumochi_protocol::constants::BRAIN_EXE))
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn instance_lock_path() -> Result<std::path::PathBuf, crate::instance::SessionLockError>
{
    use crate::instance::SessionLockError;

    #[cfg(windows)]
    {
        let data_dir = data_dir().ok_or(SessionLockError::DataDirectory)?;
        // The shell's first write, so the product folder is owner-only from its creation.
        kumochi_storage::product_folder(&data_dir).map_err(|_| SessionLockError::DataDirectory)?;
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
        use crate::logging::LoggingError;

        let data_dir = data_dir().ok_or(LoggingError::NoLocalAppData)?;
        kumochi_storage::product_folder(&data_dir).map_err(|_| LoggingError::NoLocalAppData)?;
        Ok(data_dir)
    }
    #[cfg(not(windows))]
    {
        Err(crate::logging::LoggingError::Unsupported)
    }
}
