//! The sources that are started and stopped with a watch: the mouse, on its own thread, and
//! the foreground hook. `start` and `stop` run on the UI thread and never wait for the mouse
//! thread: the one lock they share with it is held for a push or a pop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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

/// Posted to the mouse window: arm it for the oldest watch whose bit waits in [`Shared`].
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

/// What the mouse thread knows: the watch it is armed for, or none.
#[derive(Default)]
struct Mouse {
    armed: Option<Armed>,
    events: Option<Sender<PresenceEvent>>,
    /// Where the bits of arms not dispatched yet wait.
    shared: Option<Arc<Shared>>,
    /// The window was destroyed, so the thread's end has none to destroy.
    closed: bool,
}

/// The watch the mouse thread is armed for.
struct Armed {
    changed: Changed,
    /// The tick of the arm's message.
    at: u32,
    /// This arm's one wake was queued.
    woke: bool,
}

impl Armed {
    /// Sets the watch's bit. True when this arm's wake is owed, which is once in its life:
    /// the presenter clears the bit when it reads it, and a record that comes after the watch
    /// stopped and before the disarm must not wake the UI a second time.
    fn change(&mut self) -> bool {
        self.changed.set() && !std::mem::replace(&mut self.woke, true)
    }
}

impl Mouse {
    /// One mouse record with button `flags`, whose message is from tick `time`. True when it
    /// owes its watch's wake. A move ends at the mask. A press or a wheel turn counts when the
    /// window under the pointer is another process's, which `foreign` says, and sets the bit
    /// every time. A record from before the arm is the watch before's; one from the arm's own
    /// tick counts.
    fn record(&mut self, time: u32, flags: u32, foreign: impl FnOnce() -> bool) -> bool {
        let Some(armed) = &mut self.armed else {
            return false;
        };
        // Ticks wrap, so "earlier" is a negative difference.
        let earlier = time.wrapping_sub(armed.at).cast_signed() < 0;
        !earlier && flags & PRESSES != 0 && foreign() && armed.change()
    }

    /// A record that couldn't be read, which counts as a change.
    fn unread(&mut self) -> bool {
        self.armed.as_mut().is_some_and(Armed::change)
    }

    /// An ARM message from tick `at` was dispatched: the oldest waiting bit is its watch's.
    fn arm(&mut self, at: u32) {
        let waiting = self.shared.as_ref();
        let changed = waiting.and_then(|shared| shared.pending().pop_front());
        self.armed = changed.map(|changed| Armed {
            changed,
            at,
            woke: false,
        });
    }

    /// The thread is ending. The watch it is armed for, and every watch in `pending` whose arm
    /// it never dispatched, has lost its mouse source, so each counts as changed. True when a
    /// wake is owed.
    fn lost(&mut self, pending: impl IntoIterator<Item = Changed>) -> bool {
        // The armed watch's one wake may be spent already; its bit is set all the same.
        let mut woke = self.armed.take().is_some_and(|mut armed| armed.change());
        // Every one of them is set: a wake for the first must not leave the rest trusted.
        for changed in pending {
            woke |= changed.set();
        }
        woke
    }
}

/// What the UI thread and the mouse thread share: the mouse window's handle, the thread's end
/// and the arms on their way. Each side writes its own flag and then reads the other's, so
/// the orderings are sequentially consistent: one of the two always sees the other's write.
#[derive(Default)]
struct Shared {
    /// The mouse window once it is published; 0 before, and after the thread ends.
    window: AtomicIsize,
    quit: AtomicBool,
    exited: AtomicBool,
    /// The bits of the arms posted and not dispatched yet, oldest first. `Win32::arm` adds
    /// one before it posts, the mouse thread takes one for each ARM it dispatches, and its
    /// end takes what is left. Never held while a record is handled.
    pending: Mutex<VecDeque<Changed>>,
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

    fn pending(&self) -> MutexGuard<'_, VecDeque<Changed>> {
        // A queue of bits is whole whoever panicked while holding it.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// Another component's.
    Other,
    /// The registrations couldn't be read, which proves nothing either way.
    Unreadable,
}

/// What `start` and `stop` ask of Windows, so tests can script their order and failures.
pub(crate) trait Native {
    fn registration(&self) -> Registration;
    /// Posts the mouse thread its arm for `changed`. False when it has no window.
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
            return Registration::Unreadable;
        }
        let mut devices =
            vec![RAWINPUTDEVICE::default(); usize::try_from(count).unwrap_or_default()];
        if !devices.is_empty() {
            // SAFETY: `devices` holds `count` writable records.
            let read = unsafe {
                GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &raw mut count, size)
            };
            if read == u32::MAX {
                return Registration::Unreadable;
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
        // The bit waits in `Shared` and not in the message, so the thread's end finds an arm
        // it never dispatched.
        self.shared.pending().push_back(changed);
        let posted = self.post(ARM, LPARAM(0));
        if !posted {
            // Only this thread adds to the back; the thread's end may have taken it already.
            self.shared.pending().pop_back();
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
            let code = error.raw_os_error();
            tracing::warn!(
                reason = "spawn",
                ?code,
                "the screen watch has no mouse source"
            );
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

    /// Gives back what `start` took. True when nothing is left. Only a registration read as
    /// still targeting the mouse window is removed. One read as gone, or as another
    /// component's since, is theirs and is forgotten. One that can't be read is still owed:
    /// nothing says it is gone.
    fn release(&mut self) -> bool {
        if self.registered {
            self.registered = match self.native.registration() {
                Registration::Ours => !self.native.remove(),
                Registration::None | Registration::Other => false,
                Registration::Unreadable => true,
            };
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
        // Registering over another component's mouse registration would take it from them,
        // and one that can't be read may be theirs.
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
// shortcut: a mouse thread that ends is never started again, so every later `start` says no
// and no marks show until the app restarts; respawn it if that is ever seen in use.
fn mouse_thread(shared: &Arc<Shared>, events: Sender<PresenceEvent>) {
    let class = HSTRING::from(format!("{OBJECT_PREFIX}-screen-watch"));
    MOUSE_WATCH.with_borrow_mut(|mouse| {
        mouse.events = Some(events);
        mouse.shared = Some(Arc::clone(shared));
    });
    let mut ending = Ending {
        shared,
        class: &class,
        made: None,
    };
    match mouse_window(&class) {
        Ok(made) => {
            ending.made = Some(made);
            if shared.publish(handle(made.1)) {
                pump();
            }
        }
        Err(error) => {
            let code = error.code().0;
            tracing::warn!(
                reason = "window",
                code,
                "the screen watch has no mouse source"
            );
        }
    }
}

/// Reads the thread's queue until its window closes.
fn pump() {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is writable; reads this thread's own queue.
        match unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0 {
            0 => return,
            -1 => {
                let code = windows::core::Error::from_thread().code().0;
                tracing::warn!(
                    reason = "pump",
                    code,
                    "the screen watch's mouse thread ended"
                );
                return;
            }
            _ => {
                // SAFETY: dispatches the message just read to this thread's window.
                unsafe { DispatchMessageW(&raw const message) };
            }
        }
    }
}

/// What the mouse thread owes when it ends, by a return or by an unwind: nothing stays
/// published, no watch goes on trusting a mouse source that is gone, and the window and its
/// class are given back.
struct Ending<'a> {
    shared: &'a Shared,
    class: &'a HSTRING,
    made: Option<(HINSTANCE, HWND)>,
}

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        // The publication goes first: an arm posted from here on finds no window and fails,
        // and one posted before is in `pending`.
        self.shared.exit();
        let pending = std::mem::take(&mut *self.shared.pending());
        let (woke, events, closed) = MOUSE_WATCH
            .with_borrow_mut(|mouse| (mouse.lost(pending), mouse.events.take(), mouse.closed));
        if woke && let Some(events) = &events {
            send(events, PresenceEvent::ScreenWoke);
        }
        let Some((instance, target)) = self.made else {
            return;
        };
        if !closed {
            // SAFETY: this thread made the window, and its procedure hasn't seen it destroyed.
            if let Err(error) = unsafe { DestroyWindow(target) } {
                let code = error.code().0;
                tracing::warn!(reason = "destroy", code, "the screen watch's window stays");
            }
        }
        // SAFETY: the class is this thread's, and its one window was destroyed above or before.
        let unregistered = unsafe { UnregisterClassW(PCWSTR(self.class.as_ptr()), Some(instance)) };
        if let Err(error) = unregistered {
            let code = error.code().0;
            tracing::debug!(reason = "class", code, "the screen watch's class stays");
        }
    }
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
/// a wheel turn, which also asks which window is under the pointer. An arm's one wake is
/// queued without waiting and may allocate. No record takes a lock, and none logs unless the
/// UI has stopped listening.
unsafe extern "system" fn mouse_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        ARM => {
            // The arm's own tick: a record queued before it belongs to the watch before.
            // SAFETY: reads the time of the message this thread is handling.
            let at = unsafe { GetMessageTime() }.cast_unsigned();
            MOUSE_WATCH.with_borrow_mut(|mouse| mouse.arm(at));
            return LRESULT(0);
        }
        DISARM => {
            MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = None);
            return LRESULT(0);
        }
        WM_INPUT => MOUSE_WATCH.with_borrow_mut(|mouse| {
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
        WM_DESTROY => {
            MOUSE_WATCH.with_borrow_mut(|mouse| mouse.closed = true);
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
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
