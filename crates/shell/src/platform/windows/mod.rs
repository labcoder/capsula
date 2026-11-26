//! Win32 implementations of the shell's platform functions.

mod hit_mask;
mod host;
mod keyboard;
mod observer;
mod overlay_input;
mod presence;
mod screen_watch;
mod taskbar;

pub(crate) use host::{HostWatch, host_at, watch_host};
pub(crate) use keyboard::EscapeHotkey;
pub(super) use keyboard::foreground_layout;
#[cfg(test)]
pub(crate) use keyboard::{with_raw_keys, with_raw_modifiers};
pub(crate) use observer::client_origin;
#[cfg(any(test, feature = "gate-recorder"))]
pub(crate) use observer::{ProxyHandle, performance_frequency, window_handle};
pub(crate) use overlay_input::OverlayWindows;
pub(crate) use presence::{
    Affinity, Foreground, PresenceNative, activate_presence_window, configure_presence_window,
    cursor_position, exclude_window, hide_presence_window, performance_counter,
    place_presence_window, restore_foreground, restore_window_affinity, window_affinity, work_area,
};
pub(crate) use screen_watch::ScreenSources;
