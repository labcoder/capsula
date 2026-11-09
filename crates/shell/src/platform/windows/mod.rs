//! Win32 implementations of the shell's platform functions.

mod credential;
mod folders;
mod hit_mask;
mod host;
mod keyboard;
#[expect(dead_code, reason = "dormant logger backend has no startup caller")]
pub(crate) mod logging;
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
pub(crate) use host::{HostWatch, host_at, watch_host};
pub(crate) use keyboard::EscapeHotkey;
pub(crate) use observer::{ProxyHandle, client_origin, performance_frequency, window_handle};
pub(crate) use overlay_input::OverlayWindows;
#[cfg(test)]
pub(crate) use pipe::test_pair;
pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use presence::{
    Foreground, PresenceError, PresenceNative, activate_presence_window, configure_presence_window,
    cursor_position, hide_presence_window, performance_counter, place_presence_window,
    restore_foreground, set_capture, work_area,
};
pub(crate) use process::launch_detached;
