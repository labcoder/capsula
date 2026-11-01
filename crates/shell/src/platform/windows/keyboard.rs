//! The keyboard behind Raw Input: left Ctrl and left Alt from any window, without a hook.
//!
//! Raw Input with `RIDEV_INPUTSINK` reports keys to the proxy window even when another app is in
//! front, and never removes them from that app. Device removal, session changes, desktop
//! switches (Ctrl+Alt+Del's secure desktop) and suspend reset the gesture, because the key-ups
//! they hide would otherwise leave keys held.

use std::cell::{Cell, OnceCell};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC_EX, MOD_NOREPEAT, MapVirtualKeyW, RegisterHotKey, UnregisterHotKey, VK_ESCAPE,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_DESKTOPSWITCH, GIDC_REMOVAL, KillTimer, PBT_APMSUSPEND, PostMessageW,
    RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, SetTimer, WINEVENT_OUTOFCONTEXT, WM_APP, WM_INPUT,
    WM_INPUT_DEVICE_CHANGE, WM_POWERBROADCAST, WM_TIMER, WM_WTSSESSION_CHANGE,
};

use super::presence::{PresenceError, performance_counter, struct_size};
use crate::input::{Edge, Event, Key};

/// The proxy's only timer: the gesture's next one-shot deadline.
const DEADLINE_TIMER: usize = 1;
/// The hotkey id for Esc on the proxy window.
const ESCAPE_HOTKEY: i32 = 1;
/// A keyboard overrun: keys were lost, so held state can't be trusted.
const OVERRUN_MAKE_CODE: u16 = 0xFF;
/// HID generic desktop page, keyboard usage.
const KEYBOARD: (u16, u16) = (0x01, 0x06);
/// Posted to the proxy when the input desktop changes.
const DESKTOP_SWITCHED: u32 = WM_APP + 2;

thread_local! {
    /// The proxy that hears desktop switches; the WinEvent callback has no context of its own.
    static PROXY: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

/// When the native callback that carried an event began, on both clocks: `instant` for the
/// gesture, `qpc` for joining diagnostics to `PresentMon`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EventTime {
    pub(crate) instant: Instant,
    pub(crate) qpc: i64,
}

/// Reduces one gesture event at its callback time and returns the next deadline to arm.
pub(crate) type Deliver = Box<dyn Fn(Event, EventTime) -> Option<Instant>>;

/// The proxy's keyboard state. Only the UI thread touches it.
#[derive(Default)]
pub(super) struct Keys {
    deliver: OnceCell<Deliver>,
    /// The deadline the timer is armed for, if any.
    deadline: Cell<Option<Instant>>,
    sessions: Cell<bool>,
    desktop: Cell<Option<HWINEVENTHOOK>>,
}

impl Keys {
    /// Handles keyboard and lifecycle messages for the proxy. True means the message is
    /// consumed; `WM_INPUT` always goes on to `DefWindowProc`, which frees its buffer.
    pub(super) fn on_message(
        &self,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> bool {
        // Stamped first, so measurements start at callback entry.
        let at = EventTime {
            instant: Instant::now(),
            qpc: performance_counter(),
        };
        let event = match message {
            WM_INPUT => match read_key(lparam) {
                Some(event) => event,
                None => return false,
            },
            // A stale or early WM_TIMER is harmless: the gesture ignores a deadline it hasn't
            // reached and returns the remaining one, which is armed again below.
            WM_TIMER if wparam.0 == DEADLINE_TIMER => {
                self.disarm(hwnd);
                Event::Deadline
            }
            WM_INPUT_DEVICE_CHANGE if wparam.0 == usize::try_from(GIDC_REMOVAL).unwrap_or(0) => {
                Event::Reset
            }
            WM_WTSSESSION_CHANGE | DESKTOP_SWITCHED => Event::Reset,
            WM_POWERBROADCAST if wparam.0 == usize::try_from(PBT_APMSUSPEND).unwrap_or(0) => {
                Event::Reset
            }
            _ => return false,
        };
        if let Some(deliver) = self.deliver.get() {
            // `at` stays the event's time; the timer counts from now, since delivery takes time.
            let deadline = deliver(event, at);
            self.arm(hwnd, deadline, Instant::now());
        }
        message == WM_TIMER || message == DESKTOP_SWITCHED
    }

    fn arm(&self, hwnd: HWND, deadline: Option<Instant>, now: Instant) {
        let Some(deadline) = deadline else {
            self.disarm(hwnd);
            return;
        };
        // Rounded up, so the timer doesn't fire before the deadline it was armed for.
        let millis = deadline
            .saturating_duration_since(now)
            .as_micros()
            .div_ceil(1000);
        let millis = u32::try_from(millis).unwrap_or(u32::MAX).max(1);
        // SAFETY: `hwnd` is the proxy on this thread; replacing the same id rearms it.
        if unsafe { SetTimer(Some(hwnd), DEADLINE_TIMER, millis, None) } == 0 {
            tracing::warn!("can't arm the gesture deadline");
            return;
        }
        self.deadline.set(Some(deadline));
    }

    fn disarm(&self, hwnd: HWND) {
        if self.deadline.take().is_none() {
            return;
        }
        // SAFETY: `hwnd` is the proxy on this thread and owns this timer.
        if let Err(error) = unsafe { KillTimer(Some(hwnd), DEADLINE_TIMER) } {
            tracing::debug!(%error, "the gesture deadline was already gone");
        }
    }
}

/// Starts Raw Input for `hwnd` with every reset source watched, then stores where its events
/// go. Without the session and desktop watchers a key held across a lock would stay held, so
/// arming fails closed and undoes what it registered.
///
/// # Errors
/// Fails when it was already armed or Windows refuses Raw Input or a watcher.
pub(super) fn arm(hwnd: HWND, keys: &Keys, deliver: Deliver) -> Result<(), PresenceError> {
    if keys.deliver.get().is_some() {
        return Err(PresenceError::KeyboardArmed);
    }
    if let Err(error) = watch(hwnd, keys) {
        disarm(hwnd, keys);
        return Err(error);
    }
    keys.deliver
        .set(deliver)
        .map_err(|_| PresenceError::KeyboardArmed)
}

fn watch(hwnd: HWND, keys: &Keys) -> Result<(), PresenceError> {
    let device = RAWINPUTDEVICE {
        usUsagePage: KEYBOARD.0,
        usUsage: KEYBOARD.1,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    };
    // SAFETY: one valid device record for this thread's live window.
    unsafe { RegisterRawInputDevices(&[device], struct_size::<RAWINPUTDEVICE>()) }?;
    // SAFETY: `hwnd` is live; the proxy unregisters it before it's destroyed.
    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
        .map_err(PresenceError::SessionWatch)?;
    keys.sessions.set(true);
    PROXY.set(hwnd);
    // SAFETY: an out-of-context hook calls back on this thread while it reads messages.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(desktop_switched),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        return Err(PresenceError::DesktopWatch);
    }
    keys.desktop.set(Some(hook));
    Ok(())
}

/// Stops what `arm` started, before the proxy window goes away.
pub(super) fn disarm(hwnd: HWND, keys: &Keys) {
    keys.disarm(hwnd);
    if let Some(hook) = keys.desktop.take() {
        // SAFETY: the hook was set on this thread by `arm` and is removed once.
        if !unsafe { UnhookWinEvent(hook) }.as_bool() {
            tracing::debug!("the desktop-switch hook was already gone");
        }
    }
    PROXY.set(HWND::default());
    let device = RAWINPUTDEVICE {
        usUsagePage: KEYBOARD.0,
        usUsage: KEYBOARD.1,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    // SAFETY: removal takes one valid record with no target window.
    let removed = unsafe { RegisterRawInputDevices(&[device], struct_size::<RAWINPUTDEVICE>()) };
    if let Err(error) = removed {
        tracing::debug!(%error, "can't stop Raw Input");
    }
    if keys.sessions.replace(false) {
        // SAFETY: `hwnd` is still live and was registered by `arm`.
        if let Err(error) = unsafe { WTSUnRegisterSessionNotification(hwnd) } {
            tracing::debug!(%error, "can't stop watching sessions");
        }
    }
}

/// Esc held system-wide for the proxy window; dropping it gives Esc back to other apps.
#[derive(Debug)]
pub(crate) struct EscapeHotkey(HWND);

impl EscapeHotkey {
    /// # Errors
    /// Fails when another app, or this one, already holds Esc.
    pub(super) fn register(hwnd: HWND) -> Result<Self, PresenceError> {
        // SAFETY: `hwnd` is a live window on this thread; Drop releases the same id.
        unsafe {
            RegisterHotKey(
                Some(hwnd),
                ESCAPE_HOTKEY,
                MOD_NOREPEAT,
                u32::from(VK_ESCAPE.0),
            )
        }?;
        Ok(Self(hwnd))
    }

    /// Whether `message` is this hotkey firing.
    pub(super) fn fired(message: u32, wparam: WPARAM) -> bool {
        message == windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY
            && wparam.0 == usize::try_from(ESCAPE_HOTKEY).unwrap_or(0)
    }
}

impl Drop for EscapeHotkey {
    fn drop(&mut self) {
        // SAFETY: releases the id this value registered on its window's thread.
        if let Err(error) = unsafe { UnregisterHotKey(Some(self.0), ESCAPE_HOTKEY) } {
            tracing::debug!(%error, "Esc was already released");
        }
    }
}

/// Turns a desktop switch into a proxy message, so it resets through the same path as keys.
unsafe extern "system" fn desktop_switched(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    let proxy = PROXY.get();
    if proxy.is_invalid() {
        return;
    }
    // SAFETY: posts a parameterless message to this thread's live proxy.
    if let Err(error) = unsafe { PostMessageW(Some(proxy), DESKTOP_SWITCHED, WPARAM(0), LPARAM(0)) }
    {
        tracing::warn!(%error, "can't reset the gesture after a desktop switch");
    }
}

/// Reads one keyboard record; mouse and HID records, and unreadable ones, are ignored.
fn read_key(lparam: LPARAM) -> Option<Event> {
    let handle = HRAWINPUT(std::ptr::with_exposed_provenance_mut(
        lparam.0.cast_unsigned(),
    ));
    let mut raw = RAWINPUT::default();
    let mut size = struct_size::<RAWINPUT>();
    // SAFETY: `raw` is writable for `size` bytes; the handle came with this WM_INPUT.
    let read = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &raw mut size,
            struct_size::<RAWINPUTHEADER>(),
        )
    };
    if read == u32::MAX || raw.header.dwType != RIM_TYPEKEYBOARD.0 {
        return None;
    }
    // SAFETY: the header says this record is a keyboard one.
    let keyboard = unsafe { raw.data.keyboard };
    if keyboard.MakeCode != 0 {
        return Some(key_event(keyboard.MakeCode, keyboard.Flags));
    }
    // Some media and vendor keys report no make code; their virtual key still names one.
    // SAFETY: a pure table lookup in the current layout.
    let scan = unsafe { MapVirtualKeyW(u32::from(keyboard.VKey), MAPVK_VK_TO_VSC_EX) };
    let prefix = match scan >> 8 {
        0xE0 => RI_KEY_E0,
        0xE1 => RI_KEY_E1,
        _ => 0,
    };
    let flags = (u32::from(keyboard.Flags) & RI_KEY_BREAK) | prefix;
    Some(key_event(
        u16::try_from(scan & 0xFF).unwrap_or(0),
        u16::try_from(flags).unwrap_or(0),
    ))
}

/// Names a key by make code and E0 prefix. Only unprefixed 0x1D and 0x38 are the left keys,
/// so `AltGr`'s E0 0x38 and right Ctrl's E0 0x1D are other keys and cancel. E1 (Pause) has no
/// reliable key-up, so it resets rather than leaving a key held forever.
fn key_event(make_code: u16, flags: u16) -> Event {
    let flags = u32::from(flags);
    if make_code == OVERRUN_MAKE_CODE || make_code > 0xFF || flags & RI_KEY_E1 != 0 {
        return Event::Reset;
    }
    let e0 = flags & RI_KEY_E0 != 0;
    let edge = if flags & RI_KEY_BREAK == 0 {
        Edge::Down
    } else {
        Edge::Up
    };
    let key = match (make_code, e0) {
        (0x1D, false) => Key::LeftControl,
        (0x38, false) => Key::LeftAlt,
        _ => Key::Other(make_code | u16::from(e0) << 8),
    };
    Event::Key { key, edge }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::{EscapeHotkey, key_event};
    use crate::escape::EscapeHold;
    use crate::input::{Edge, Event, Key};

    const BREAK: u16 = 1;
    const E0: u16 = 2;
    const E1: u16 = 4;

    #[test]
    fn only_unprefixed_left_modifiers_qualify() {
        let key = |key, edge| Event::Key { key, edge };
        assert_eq!(key_event(0x1D, 0), key(Key::LeftControl, Edge::Down));
        assert_eq!(key_event(0x38, BREAK), key(Key::LeftAlt, Edge::Up));
        assert_eq!(key_event(0x1D, E0), key(Key::Other(0x11D), Edge::Down));
        assert_eq!(
            key_event(0x38, E0 | BREAK),
            key(Key::Other(0x138), Edge::Up)
        );
        assert_eq!(key_event(0x1E, 0), key(Key::Other(0x1E), Edge::Down));
    }

    #[test]
    fn pause_resets_instead_of_holding_a_key_forever() {
        assert_eq!(key_event(0x1D, E1), Event::Reset);
        assert_eq!(key_event(0x1D, E1 | BREAK), Event::Reset);
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            // SAFETY: STATIC is a built-in class; a hidden popup with no parent or data.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    PCWSTR::null(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
            };
            Self(hwnd.expect("a hidden STATIC window can be created"))
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the test owns this live window on its creating thread.
            unsafe { DestroyWindow(self.0) }.expect("the test window remains owned by its thread");
        }
    }

    /// Holds Esc system-wide for milliseconds, so it runs only in an approved desktop session:
    /// `cargo test -p capsula-shell -- --ignored escape_hold`.
    #[test]
    #[ignore = "holds the desktop's Esc briefly; run only in an approved session"]
    fn escape_hold_registers_shows_stop_when_taken_and_releases() {
        let (first, second) = (TestWindow::new(), TestWindow::new());
        let mut holder = EscapeHold::default();
        holder.acquire(|| EscapeHotkey::register(first.0));
        assert!(
            !holder.shows_stop(),
            "Esc must be free before this fixture runs"
        );
        let mut blocked = EscapeHold::default();
        blocked.acquire(|| EscapeHotkey::register(second.0));
        assert!(blocked.shows_stop());
        holder.release();
        blocked.release();
        blocked.acquire(|| EscapeHotkey::register(second.0));
        assert!(!blocked.shows_stop(), "releasing the first hold frees Esc");
        blocked.release();
    }

    #[test]
    fn overruns_and_impossible_codes_reset() {
        assert_eq!(key_event(0xFF, 0), Event::Reset);
        assert_eq!(key_event(0x100, 0), Event::Reset);
    }
}
