//! The operating system behind small, safe functions. Windows only for now.

pub(crate) use kumochi_shell_core::platform::{host, input, microphone};

/// Where a test says the pointer is. A test that sets it never reads the real one, which is
/// wherever the person at the desk left it.
#[cfg(test)]
pub(crate) mod cursor_fixture {
    use std::cell::Cell;

    thread_local! {
        static AT: Cell<Option<Result<[i32; 2], ()>>> = const { Cell::new(None) };
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
pub(crate) use kumochi_shell_core::platform::{data_dir, session_id};
#[cfg(windows)]
pub(crate) use windows::ScreenSources;
#[cfg(windows)]
pub(crate) use windows::{
    Affinity, Deliver, EscapeHotkey, EventTime, Foreground, HostWatch, MaskUpdate, OverlayWindows,
    PresenceError, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface, ProxyHandle,
    ProxyNote, activate_presence_window, client_origin, configure_presence_window, cursor_position,
    exclude_window, hide_presence_window, host_at, performance_counter, performance_frequency,
    place_presence_window, restore_foreground, restore_window_affinity, watch_host,
    window_affinity, window_handle, work_area,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{with_raw_keys, with_raw_modifiers};

pub(crate) use crate::presence::line::machine::{ROW_SPANS, RowSpans};
