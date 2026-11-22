//! The operating system behind the models: the plain host and input types, and the calls the
//! models make themselves (folders, the credential store, the microphone). Windows only for now.

pub mod clipboard;
pub mod credential;
pub mod host;
pub mod input;
pub mod microphone;
pub mod screen_watch;
#[cfg(any(windows, target_os = "macos"))]
pub mod speaker_cpal;

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls are isolated to this platform adapter"
)]
mod windows;

#[cfg(windows)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) use windows::pipe::test_pair;
#[cfg(windows)]
pub(crate) use windows::{PipeClient, launch_detached};
#[cfg(windows)]
pub use windows::{data_dir, session_id};
