//! The operating system behind small, safe functions. Windows only for now.

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the shell's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::{
    PipeClient, ShowError, launch_detached, lock_single_instance, session_id,
    show_without_activating,
};
