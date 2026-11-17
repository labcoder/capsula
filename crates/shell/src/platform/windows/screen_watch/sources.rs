//! The sources that are started and stopped with a watch: the mouse, on its own thread, and
//! the foreground hook. `start` and `stop` run on the UI thread and never wait.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use async_channel::Sender;
use kumochi_protocol::constants::OBJECT_PREFIX;
use kumochi_shell_core::platform::screen_watch::{Changed, Sources};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_SYSTEM_FOREGROUND,
    GetCursorPos, GetMessageTime, GetMessageW, HWND_MESSAGE, MSG, PostMessageW, PostQuitMessage,
    RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_HWHEEL, RI_MOUSE_LEFT_BUTTON_DOWN,
    RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_WHEEL, RegisterClassExW,
    UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_INPUT, WNDCLASSEXW, WindowFromPoint,
};
use windows::core::{HSTRING, PCWSTR};

use super::super::presence::{PresenceEvent, send, struct_size};
use crate::input::TalkHeld;

/// Posted to the mouse window: arm it for the watch whose bit the message owns.
const ARM: u32 = WM_APP + 1;
/// Posted to the mouse window: its watch stopped.
const DISARM: u32 = WM_APP + 2;
/// HID generic desktop page, mouse usage.
const MOUSE: (u16, u16) = (0x01, 0x02);
/// The button flags of a press or a wheel turn. A move carries none of them.
const PRESSES: u32 = RI_MOUSE_LEFT_BUTTON_DOWN
    | RI_MOUSE_RIGHT_BUTTON_DOWN
    | RI_MOUSE_MIDDLE_BUTTON_DOWN
    | RI_MOUSE_BUTTON_4_DOWN
    | RI_MOUSE_BUTTON_5_DOWN
    | RI_MOUSE_WHEEL
    | RI_MOUSE_HWHEEL;

thread_local! {
    /// The mouse thread's state. Only that thread's window procedure touches it, one message
    /// at a time, so an arm or a disarm never interrupts a record being handled.
    static MOUSE_WATCH: RefCell<Mouse> = RefCell::new(Mouse::default());
}

/// What the mouse thread knows: the watch it is armed for, with the tick it was armed at, or
/// none.
#[derive(Default)]
struct Mouse {
    armed: Option<(Changed, u32)>,
    events: Option<Sender<PresenceEvent>>,
}

impl Mouse {
    /// One mouse record with button `flags`, whose message is from tick `time`. True when it
    /// set its watch's bit, which is when that watch's one wake is owed. A move ends at the
    /// mask. A press or a wheel turn counts when the window under the pointer is another
    /// process's, which `foreign` says. A record from before the arm is the watch before's;
    /// one from the arm's own tick counts.
    fn record(&self, time: u32, flags: u32, foreign: impl FnOnce() -> bool) -> bool {
        let Some((changed, armed_at)) = &self.armed else {
            return false;
        };
        // Ticks wrap, so "earlier" is a negative difference.
        let earlier = time.wrapping_sub(*armed_at).cast_signed() < 0;
        !earlier && flags & PRESSES != 0 && foreign() && changed.set()
    }

    /// A record that couldn't be read, which counts as a change.
    fn unread(&self) -> bool {
        self.armed
            .as_ref()
            .is_some_and(|(changed, _)| changed.set())
    }
}

/// What the UI thread and the mouse thread share: the mouse window's handle and the thread's
/// end. Each side writes its own flag and then reads the other's, so the orderings are
/// sequentially consistent: one of the two always sees the other's write.
#[derive(Default)]
struct Shared {
    /// The mouse window once it is published; 0 before, and after the thread ends.
    window: AtomicIsize,
    quit: AtomicBool,
    exited: AtomicBool,
}

impl Shared {
    /// The thread made `window`. False tells it to destroy the window and end: quit came
    /// first, before or during the publication.
    fn publish(&self, window: isize) -> bool {
        if self.quit.load(Ordering::SeqCst) {
            return false;
        }
        self.window.store(window, Ordering::SeqCst);
        !self.quit.load(Ordering::SeqCst)
    }

    /// The thread is ending, by any road.
    fn exit(&self) {
        self.window.store(0, Ordering::SeqCst);
        self.exited.store(true, Ordering::SeqCst);
    }

    /// Asks the thread to end, and returns the window to close if one is published.
    fn quit(&self) -> Option<isize> {
        self.quit.store(true, Ordering::SeqCst);
        self.window()
    }

    fn window(&self) -> Option<isize> {
        Some(self.window.load(Ordering::SeqCst)).filter(|window| *window != 0)
    }
}

fn handle(window: HWND) -> isize {
    window.0.expose_provenance().cast_signed()
}

fn window(handle: isize) -> HWND {
    HWND(std::ptr::with_exposed_provenance_mut(
        handle.cast_unsigned(),
    ))
}

/// The process's mouse registration, as `start` and a retried removal need to know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Registration {
    None,
    /// It targets the mouse window, so this source made it.
    Ours,
    /// Another component's, or the registrations couldn't be read.
    Other,
}

/// What `start` and `stop` ask of Windows, so tests can script their order and failures.
pub(crate) trait Native {
    fn registration(&self) -> Registration;
    /// Posts the mouse thread its arm, which owns `changed`. False when it has no window.
    fn arm(&self, changed: Changed) -> bool;
    fn disarm(&self) -> bool;
    fn register(&self) -> bool;
    fn remove(&self) -> bool;
    fn hook(&mut self) -> bool;
    fn unhook(&mut self) -> bool;
    /// The process is ending.
    fn quit(&self) {}
}

/// The real thing.
pub(crate) struct Win32 {
    shared: Arc<Shared>,
    hook: Option<HWINEVENTHOOK>,
}

impl Win32 {
    fn post(&self, message: u32, lparam: LPARAM) -> bool {
        let Some(target) = self.shared.window() else {
            return false;
        };
        // SAFETY: posts to the mouse thread's window; a handle that is gone only fails.
        unsafe { PostMessageW(Some(window(target)), message, WPARAM(0), lparam) }.is_ok()
    }

    /// Registers the mouse to `target`, or removes its registration, which names no window.
    fn device(flags: RAWINPUTDEVICE_FLAGS, target: HWND) -> bool {
        let device = RAWINPUTDEVICE {
            usUsagePage: MOUSE.0,
            usUsage: MOUSE.1,
            dwFlags: flags,
            hwndTarget: target,
        };
        // SAFETY: one valid device record for a live window, or for none.
        unsafe { RegisterRawInputDevices(&[device], struct_size::<RAWINPUTDEVICE>()) }.is_ok()
    }
}

impl Native for Win32 {
    fn registration(&self) -> Registration {
        let size = struct_size::<RAWINPUTDEVICE>();
        let mut count = 0;
        // SAFETY: with no buffer this only writes the count.
        if unsafe { GetRegisteredRawInputDevices(None, &raw mut count, size) } == u32::MAX {
            return Registration::Other;
        }
        let mut devices =
            vec![RAWINPUTDEVICE::default(); usize::try_from(count).unwrap_or_default()];
        if !devices.is_empty() {
            // SAFETY: `devices` holds `count` writable records.
            let read = unsafe {
                GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &raw mut count, size)
            };
            if read == u32::MAX {
                return Registration::Other;
            }
            devices.truncate(usize::try_from(read).unwrap_or_default());
        }
        let mouse = devices
            .iter()
            .find(|device| (device.usUsagePage, device.usUsage) == MOUSE);
        match mouse {
            None => Registration::None,
            Some(device) if Some(handle(device.hwndTarget)) == self.shared.window() => {
                Registration::Ours
            }
            Some(_) => Registration::Other,
        }
    }

    fn arm(&self, changed: Changed) -> bool {
        let bit = Box::into_raw(Box::new(changed));
        let posted = self.post(ARM, LPARAM(bit.expose_provenance().cast_signed()));
        if !posted {
            // SAFETY: from `Box::into_raw` above; the message that would own it wasn't queued.
            drop(unsafe { Box::from_raw(bit) });
        }
        posted
    }

    fn disarm(&self) -> bool {
        self.post(DISARM, LPARAM(0))
    }

    fn register(&self) -> bool {
        self.shared
            .window()
            .is_some_and(|target| Self::device(RIDEV_INPUTSINK, window(target)))
    }

    fn remove(&self) -> bool {
        Self::device(RIDEV_REMOVE, HWND::default())
    }

    fn hook(&mut self) -> bool {
        // SAFETY: an out-of-context hook calls back on this thread while it reads messages.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(foreground_changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        self.hook = (!hook.is_invalid()).then_some(hook);
        self.hook.is_some()
    }

    fn unhook(&mut self) -> bool {
        let Some(hook) = self.hook else {
            return true;
        };
        // SAFETY: the hook was set on this thread by `hook` and is removed once.
        let removed = unsafe { UnhookWinEvent(hook) }.as_bool();
        if removed {
            self.hook = None;
        }
        removed
    }

    fn quit(&self) {
        // The flag first: a thread that hasn't published its window yet ends on its own.
        if let Some(target) = self.shared.quit() {
            // SAFETY: posts a close to the mouse thread's window.
            let closed =
                unsafe { PostMessageW(Some(window(target)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            if closed.is_err() {
                tracing::warn!(reason = "close", "the screen watch's mouse thread stays");
            }
        }
    }
}

/// The screen watch's sources. The presenter owns them and starts and stops them on the UI
/// thread. What a failed `stop` leaves behind is remembered here, and nothing is armed over it.
pub(crate) struct ScreenSources<N: Native = Win32> {
    native: N,
    events: Sender<PresenceEvent>,
    armed: bool,
    /// The mouse registration `start` made and `stop` hasn't removed.
    registered: bool,
    hooked: bool,
}

impl ScreenSources {
    /// Spawns the mouse thread, which lives as long as the process. If it can't be spawned or
    /// can't make its window, every `start` says no and no marks are shown.
    pub(crate) fn new(events: Sender<PresenceEvent>) -> Self {
        let shared = Arc::new(Shared::default());
        let (thread, wake) = (Arc::clone(&shared), events.clone());
        let spawned = std::thread::Builder::new()
            .name("screen-watch".to_owned())
            .spawn(move || mouse_thread(&thread, wake));
        if let Err(error) = spawned {
            tracing::warn!(%error, "the screen watch has no mouse source");
            shared.exit();
        }
        Self::over(Win32 { shared, hook: None }, events)
    }
}

impl<N: Native> ScreenSources<N> {
    fn over(native: N, events: Sender<PresenceEvent>) -> Self {
        Self {
            native,
            events,
            armed: false,
            registered: false,
            hooked: false,
        }
    }

    /// Gives back what `start` took. True when nothing is left. Only a registration that
    /// still targets the mouse window is removed: one another component has since replaced
    /// is theirs, and is forgotten.
    fn release(&mut self) -> bool {
        if self.registered {
            let ours = self.native.registration() == Registration::Ours;
            self.registered = ours && !self.native.remove();
        }
        if self.hooked {
            self.hooked = !self.native.unhook();
        }
        !self.registered && !self.hooked
    }
}

impl<N: Native> Sources for ScreenSources<N> {
    fn start(&mut self, changed: Changed, held: TalkHeld) -> bool {
        // What a failed stop left goes first, and nothing is armed over it.
        if !self.release() {
            return false;
        }
        // Registering over another component's mouse registration would take it from them.
        if self.native.registration() != Registration::None {
            return false;
        }
        // The arm is queued before the registration exists, so no record is handled ahead of
        // it.
        if !self.native.arm(changed.clone()) {
            return false;
        }
        self.registered = self.native.register();
        self.hooked = self.registered && self.native.hook();
        if !self.hooked {
            let left = !self.release();
            if !self.native.disarm() || left {
                tracing::warn!(reason = "start", "a screen watch source was left behind");
            }
            return false;
        }
        self.armed = true;
        super::arm(changed, self.events.clone(), held);
        true
    }

    fn stop(&mut self) {
        if std::mem::take(&mut self.armed) {
            super::disarm();
            if !self.native.disarm() {
                tracing::warn!(reason = "disarm", "a screen watch source was left behind");
            }
        }
        if !self.release() {
            tracing::warn!(reason = "stop", "a screen watch source was left behind");
        }
    }
}

impl<N: Native> Drop for ScreenSources<N> {
    fn drop(&mut self) {
        self.stop();
        self.native.quit();
    }
}

/// Another window came to the front. Out of context, so on the UI thread.
unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if super::foreign(hwnd) {
        super::changed();
    }
}

/// The mouse thread: one message-only window, read until it closes.
fn mouse_thread(shared: &Shared, events: Sender<PresenceEvent>) {
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.events = Some(events));
    let class = HSTRING::from(format!("{OBJECT_PREFIX}-screen-watch"));
    match mouse_window(&class) {
        Ok((instance, target)) => {
            if shared.publish(handle(target)) {
                let mut message = MSG::default();
                // SAFETY: `message` is writable; reads this thread's own queue until WM_QUIT.
                while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0 > 0 {
                    // SAFETY: dispatches the message just read to this thread's window.
                    unsafe { DispatchMessageW(&raw const message) };
                }
            } else {
                // SAFETY: this thread made the window and nothing else destroys it.
                if let Err(error) = unsafe { DestroyWindow(target) } {
                    tracing::debug!(%error, "the screen watch's window was already gone");
                }
            }
            // SAFETY: the class is this thread's and its only window is gone.
            if let Err(error) = unsafe { UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance)) }
            {
                tracing::debug!(%error, "can't unregister the screen watch's class");
            }
        }
        Err(error) => tracing::warn!(%error, "the screen watch has no mouse source"),
    }
    shared.exit();
}

fn mouse_window(class: &HSTRING) -> windows::core::Result<(HINSTANCE, HWND)> {
    // SAFETY: plain call for this executable's own module.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }?.into();
    let registration = WNDCLASSEXW {
        cbSize: struct_size::<WNDCLASSEXW>(),
        lpfnWndProc: Some(mouse_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // SAFETY: `registration` and the name it points to outlive the call.
    if unsafe { RegisterClassExW(&raw const registration) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    // SAFETY: the class is registered for `instance`; a message-only window with no data.
    let made = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    };
    if made.is_err() {
        // SAFETY: the class was registered above and has no window.
        let _unregistered = unsafe { UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance)) };
    }
    Ok((instance, made?))
}

/// The mouse window's procedure. A record costs one read and one mask unless it is a press or
/// a wheel turn; nothing here allocates, locks or logs while records arrive.
unsafe extern "system" fn mouse_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        ARM => {
            let bit = std::ptr::with_exposed_provenance_mut::<Changed>(lparam.0.cast_unsigned());
            // SAFETY: `Win32::arm` leaked this box into the message, which is handled once.
            let changed = *unsafe { Box::from_raw(bit) };
            // The arm's own tick: a record queued before it belongs to the watch before.
            // SAFETY: reads the time of the message this thread is handling.
            let at = unsafe { GetMessageTime() }.cast_unsigned();
            MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = Some((changed, at)));
            return LRESULT(0);
        }
        DISARM => {
            MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = None);
            return LRESULT(0);
        }
        WM_INPUT => MOUSE_WATCH.with_borrow(|mouse| {
            let woke = match buttons(lparam) {
                Some(flags) => {
                    // SAFETY: reads the time of the message this thread is handling.
                    let time = unsafe { GetMessageTime() }.cast_unsigned();
                    mouse.record(time, flags, pointer_foreign)
                }
                None => mouse.unread(),
            };
            if woke && let Some(events) = &mouse.events {
                send(events, PresenceEvent::ScreenWoke);
            }
        }),
        // SAFETY: ends this thread's message loop.
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        _ => {}
    }
    // WM_INPUT goes on too: DefWindowProc frees its buffer.
    // SAFETY: forwards the unmodified message.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// One record's button flags: none for a record that isn't a mouse's, and nothing at all for
/// one that can't be read.
fn buttons(lparam: LPARAM) -> Option<u32> {
    let record = HRAWINPUT(std::ptr::with_exposed_provenance_mut(
        lparam.0.cast_unsigned(),
    ));
    let mut raw = RAWINPUT::default();
    let mut size = struct_size::<RAWINPUT>();
    // SAFETY: `raw` is writable for `size` bytes; the handle came with this WM_INPUT.
    let read = unsafe {
        GetRawInputData(
            record,
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &raw mut size,
            struct_size::<RAWINPUTHEADER>(),
        )
    };
    if read == u32::MAX {
        return None;
    }
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return Some(0);
    }
    // SAFETY: the header says this record is a mouse one.
    let mouse = unsafe { raw.data.mouse };
    // SAFETY: the button flags and their data are plain bits in every reading of the union.
    Some(u32::from(
        unsafe { mouse.Anonymous.Anonymous }.usButtonFlags,
    ))
}

/// Whether the window under the pointer is another process's. A pointer or a window that
/// can't be read counts as one. The position goes no further than this.
fn pointer_foreign() -> bool {
    let mut at = POINT::default();
    // SAFETY: `at` is writable for the call.
    if unsafe { GetCursorPos(&raw mut at) }.is_err() {
        return true;
    }
    // SAFETY: a plain query by value.
    super::foreign(unsafe { WindowFromPoint(at) })
}

#[cfg(test)]
#[path = "sources_tests.rs"]
mod tests;
