//! Win32 implementations of the shell's platform functions.

mod folders;
mod hit_mask;
#[expect(dead_code, reason = "C2b-2's carry is the first caller")]
mod host;
mod keyboard;
mod observer;
mod overlay_input;
mod pipe;
mod presence;
mod process;
mod taskbar;

pub(crate) use folders::data_dir;
pub(crate) use hit_mask::MaskUpdate;
pub(crate) use keyboard::{Deliver, EscapeHotkey, EventTime};
pub(crate) use observer::{
    ProxyHandle, ProxyNote, client_origin, performance_frequency, window_handle,
};
pub(crate) use overlay_input::OverlayWindows;
#[cfg(test)]
pub(crate) use pipe::test_pair;
pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use presence::{
    Capture, Foreground, PresenceError, PresenceEvent, PresenceLabels, PresenceNative,
    PresenceSurface, activate_presence_window, configure_presence_window, cursor_position,
    hide_presence_window, performance_counter, place_presence_window, restore_foreground,
    set_capture, work_area,
};
pub(crate) use process::launch_detached;
