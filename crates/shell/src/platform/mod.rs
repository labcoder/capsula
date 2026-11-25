//! The operating system behind small, safe functions. The live shell uses Windows.

#[cfg(target_os = "macos")]
#[expect(
    dead_code,
    reason = "dormant logger owner awaits coordinated startup and Quit integration"
)]
pub(crate) mod logging;
#[cfg(target_os = "macos")]
mod macos;

pub(crate) use kumochi_shell::presence_platform::{PresenceError, require_connected_geometry};
#[cfg(not(windows))]
pub(crate) use kumochi_shell::presence_platform::{
    client_origin, hide_presence_window, place_presence_window, work_area,
};

mod connected_events;
mod connected_keyboard;
pub(crate) use connected_events::{
    MaskUpdate, Observe, PresenceEvent, PresenceLabels, PresenceSurface, ProxyNote,
};
pub(crate) use connected_keyboard::{Deliver, EventTime};
#[cfg(not(windows))]
mod connected_desktop;
#[cfg(not(windows))]
pub(crate) use connected_desktop::{
    Affinity, EscapeHotkey, Foreground, HostWatch, OverlayWindows, PresenceNative, ProxyHandle,
    activate_presence_window, configure_presence_window, cursor_position, exclude_window, host_at,
    restore_foreground, restore_window_affinity, watch_host, window_affinity, window_handle,
};
pub(crate) use kumochi_shell_core::platform::{host, input, microphone};

/// Where a test says the pointer is. A test that sets it never reads the real one, which is
/// wherever the person at the desk left it.
#[cfg(test)]
pub(crate) mod cursor_fixture {
    use std::{cell::Cell, marker::PhantomData, rc::Rc};

    thread_local! {
        static AT: Cell<Option<Result<[i32; 2], ()>>> = const { Cell::new(None) };
    }

    #[derive(Debug)]
    #[must_use]
    pub(crate) struct Guard {
        previous: Option<Result<[i32; 2], ()>>,
        same_thread: PhantomData<Rc<()>>,
    }

    pub(crate) fn scoped(at: [i32; 2]) -> Guard {
        Guard {
            previous: AT.replace(Some(Ok(at))),
            same_thread: PhantomData,
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            AT.set(self.previous);
        }
    }

    /// Puts this test thread's pointer at `at`; none makes the read fail.
    pub(crate) fn set(at: Option<[i32; 2]>) {
        AT.set(Some(at.ok_or(())));
    }

    /// What this test thread set, if it set anything.
    pub(crate) fn get() -> Option<Result<[i32; 2], ()>> {
        AT.get()
    }
}

#[cfg(test)]
pub(crate) mod work_area_fixture {
    use std::{cell::Cell, marker::PhantomData, rc::Rc};

    thread_local! {
        static AREA: Cell<Option<[i32; 4]>> = const { Cell::new(None) };
    }

    #[derive(Debug)]
    #[must_use]
    pub(crate) struct Guard {
        previous: Option<[i32; 4]>,
        same_thread: PhantomData<Rc<()>>,
    }

    pub(crate) fn set(area: [i32; 4]) -> Guard {
        Guard {
            previous: AREA.replace(Some(area)),
            same_thread: PhantomData,
        }
    }

    pub(crate) fn get() -> Option<[i32; 4]> {
        AREA.get()
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            AREA.set(self.previous);
        }
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the shell's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::ScreenSources;
#[cfg(windows)]
pub(crate) use windows::{
    Affinity, EscapeHotkey, Foreground, HostWatch, OverlayWindows, PresenceNative, ProxyHandle,
    activate_presence_window, client_origin, configure_presence_window, cursor_position,
    exclude_window, hide_presence_window, host_at, performance_counter, performance_frequency,
    place_presence_window, restore_foreground, restore_window_affinity, watch_host,
    window_affinity, window_handle, work_area,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{with_raw_keys, with_raw_modifiers};

#[cfg(windows)]
pub(crate) const INPUT_COUNTER: Option<fn() -> i64> = Some(performance_counter);
#[cfg(not(windows))]
pub(crate) const INPUT_COUNTER: Option<fn() -> i64> = None;

pub(crate) fn diagnostic_clock() -> std::io::Result<kumochi_shell::QpcClock> {
    #[cfg(windows)]
    let backend = Some(kumochi_shell::QpcClock::windows(
        performance_counter,
        performance_frequency,
    ));
    #[cfg(not(windows))]
    let backend = None;
    kumochi_shell::QpcClock::require(backend)
}

pub(crate) use crate::presence::line::machine::ROW_SPANS;
#[cfg(windows)]
pub(crate) use crate::presence::line::machine::RowSpans;

pub(crate) fn connected_overlay_windows() -> Result<OverlayWindows, PresenceError> {
    #[cfg(windows)]
    let windows = Some(OverlayWindows::default());
    #[cfg(not(windows))]
    let windows = None;
    windows.ok_or(PresenceError::BindingsUnavailable)
}
