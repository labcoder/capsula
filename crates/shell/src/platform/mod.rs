//! The operating system behind small, safe functions. Windows only for now.

pub(crate) mod credential;
pub(crate) use kumochi_shell_core::platform::{host, input};
pub(crate) mod microphone;

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

    #[test]
    fn scoped_work_areas_restore_after_panic_and_do_not_cross_threads() {
        assert_eq!(get(), None);
        let outer = set([0, 0, 2560, 1400]);
        assert_eq!(
            super::work_area().expect("fixed work area"),
            [0, 0, 2560, 1400]
        );
        assert_eq!(std::thread::spawn(get).join().expect("joined thread"), None);
        let failed = std::panic::catch_unwind(|| {
            let _inner = set([0, 0, 640, 480]);
            assert_eq!(
                super::work_area().expect("nested work area"),
                [0, 0, 640, 480]
            );
            panic!("exercise scoped restoration");
        });
        assert!(failed.is_err());
        assert_eq!(
            super::work_area().expect("restored work area"),
            [0, 0, 2560, 1400]
        );
        drop(outer);
        assert_eq!(get(), None);
    }
}

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
    Affinity, Deliver, EscapeHotkey, EventTime, Foreground, HostWatch, MaskUpdate, OverlayWindows,
    PipeClient, PresenceError, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface,
    ProxyHandle, ProxyNote, activate_presence_window, client_origin, configure_presence_window,
    cursor_position, data_dir, exclude_window, hide_presence_window, host_at, launch_detached,
    performance_counter, performance_frequency, place_presence_window, restore_foreground,
    restore_window_affinity, session_id, watch_host, window_affinity, window_handle, work_area,
};

pub(crate) use crate::presence::line::machine::{ROW_SPANS, RowSpans};
