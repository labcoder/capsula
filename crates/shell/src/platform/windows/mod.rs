//! Win32 implementations of the shell's platform functions.

mod credential;
mod folders;
mod hit_mask;
mod host;
mod keyboard;
pub(super) mod microphone;
mod observer;
mod overlay_input;
mod pipe;
mod presence;
mod process;
mod taskbar;

#[expect(unused_imports, reason = "C7a-2b supplies the Accounts paste caller")]
pub(crate) use credential::WindowsKeyStageWriter;
#[cfg(test)]
pub(crate) use credential::assert_offline_native_header;
pub(crate) use folders::data_dir;
pub(crate) use hit_mask::MaskUpdate;
pub(crate) use host::{HostWatch, host_at, watch_host};
pub(crate) use keyboard::{Deliver, EscapeHotkey, EventTime};
pub(crate) use observer::{
    ProxyHandle, ProxyNote, client_origin, performance_frequency, window_handle,
};
pub(crate) use overlay_input::OverlayWindows;
#[cfg(test)]
pub(crate) use pipe::test_pair;
pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use presence::{
    Affinity, Foreground, PresenceError, PresenceEvent, PresenceLabels, PresenceNative,
    PresenceSurface, activate_presence_window, configure_presence_window, cursor_position,
    exclude_window, hide_presence_window, performance_counter, place_presence_window,
    restore_foreground, restore_window_affinity, window_affinity, work_area,
};
pub(crate) use process::launch_detached;
