//! Win32 implementations of the core's platform functions.

pub(super) mod clipboard;
mod credential;
mod folders;
pub(super) mod microphone;
pub(super) mod pipe;
mod process;

pub(crate) use credential::WindowsKeyStageWriter;
#[cfg(test)]
pub(crate) use credential::assert_offline_native_header;
pub use folders::data_dir;
pub(crate) use pipe::PipeClient;
pub use pipe::session_id;
pub(crate) use process::launch_detached;

/// Milliseconds since boot, on the clock used for Windows message timestamps.
pub fn keyboard_ticks() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;

    // SAFETY: the system clock read takes no arguments and writes no memory.
    unsafe { GetTickCount64() }
}
