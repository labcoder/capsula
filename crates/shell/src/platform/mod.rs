//! The operating system behind small, safe functions. Windows only for now.

pub(crate) mod credential;
pub(crate) mod host;
pub(crate) mod input;
#[expect(
    dead_code,
    reason = "dormant logger owner awaits coordinated caller integration"
)]
pub(crate) mod logging;
mod presence;

#[cfg(target_os = "macos")]
mod macos;

pub(crate) use presence::{
    Capture, Deliver, EventTime, MaskUpdate, PresenceEvent, PresenceLabels, PresenceSurface,
    ProxyNote,
};

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the shell's only unsafe code lives here"
)]
mod windows;

#[cfg(all(windows, test))]
pub(crate) use windows::test_pair;
#[cfg(windows)]
pub(crate) use windows::{
    EscapeHotkey, Foreground, HostWatch, OverlayWindows, PipeClient, PresenceError, PresenceNative,
    ProxyHandle, activate_presence_window, client_origin, configure_presence_window,
    cursor_position, data_dir, hide_presence_window, host_at, launch_detached, performance_counter,
    performance_frequency, place_presence_window, restore_foreground, session_id, set_capture,
    watch_host, window_handle, work_area,
};

/// The most owned `[left, right)` column spans one hit-mask row holds: two shapes, each with
/// its current and previous snapshot.
pub(crate) const ROW_SPANS: usize = 4;
/// One screen row's owned columns.
pub(crate) type RowSpans = [Option<[i32; 2]>; ROW_SPANS];
