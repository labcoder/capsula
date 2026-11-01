//! Win32 implementations of the shell's platform functions.

mod hit_mask;
mod keyboard;
mod observer;
mod pipe;
mod presence;
mod process;

pub(crate) use hit_mask::MaskUpdate;
pub(crate) use keyboard::{Deliver, EscapeHotkey, EventTime};
pub(crate) use observer::{
    ProxyHandle, ProxyNote, client_origin, performance_frequency, window_handle,
};
pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use presence::{
    Capture, Foreground, PresenceError, PresenceEvent, PresenceLabels, PresenceNative,
    PresenceSurface, activate_presence_window, configure_presence_window, cursor_position,
    performance_counter, place_hidden_presence_window, place_presence_window, restore_foreground,
    set_capture, work_area,
};
pub(crate) use process::{launch_detached, lock_single_instance};
