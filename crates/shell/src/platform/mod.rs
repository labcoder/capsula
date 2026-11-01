//! The operating system behind small, safe functions. Windows only for now.

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the shell's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::{
    PipeClient, PresenceError, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface,
    configure_presence_window, cursor_position, hide_presence_window, launch_detached,
    lock_single_instance, place_presence_window, session_id, work_area,
};
