//! The operating system behind small, safe functions. Windows only for now.

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the shell's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::{
    Capture, Deliver, EscapeHotkey, EventTime, Foreground, MaskUpdate, PipeClient, PresenceError,
    PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface, activate_presence_window,
    configure_presence_window, cursor_position, launch_detached, lock_single_instance,
    performance_counter, place_hidden_presence_window, place_presence_window, restore_foreground,
    session_id, set_capture, work_area,
};
