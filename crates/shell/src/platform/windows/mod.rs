//! Win32 implementations of the shell's platform functions.

mod hit_mask;
mod host;
mod keyboard;
mod observer;
mod overlay_input;
mod presence;
mod taskbar;

pub(crate) use hit_mask::MaskUpdate;
pub(crate) use host::{HostWatch, host_at, watch_host};
pub(crate) use keyboard::{Deliver, EscapeHotkey, EventTime};
pub(crate) use observer::{
    ProxyHandle, ProxyNote, client_origin, performance_frequency, window_handle,
};
pub(crate) use overlay_input::OverlayWindows;
pub(crate) use presence::{
    Affinity, Foreground, PresenceError, PresenceEvent, PresenceLabels, PresenceNative,
    PresenceSurface, activate_presence_window, configure_presence_window, cursor_position,
    exclude_window, hide_presence_window, performance_counter, place_presence_window,
    restore_foreground, restore_window_affinity, window_affinity, work_area,
};
