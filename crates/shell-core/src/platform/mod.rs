//! The operating system behind the models: the plain host and input types, and the calls the
//! models make themselves (folders, the credential store, the microphone). Windows only for now.

pub mod clipboard;
pub mod credential;
pub mod host;
pub mod input;
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

#[cfg(not(windows))]
pub use unsupported::session_id;
#[cfg(not(windows))]
pub(crate) use unsupported::{PipeClient, launch_detached};
#[cfg(windows)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use windows::pipe::test_pair;
#[cfg(windows)]
pub(crate) use windows::{PipeClient, launch_detached};
#[cfg(windows)]
pub use windows::{data_dir, session_id};

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
