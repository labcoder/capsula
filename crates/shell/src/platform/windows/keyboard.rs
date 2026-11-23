//! The keyboard behind Raw Input: left Ctrl and left Alt from any window, without a hook.
//!
//! Raw Input with `RIDEV_INPUTSINK` reports keys to the proxy window even when another app is in
//! front, and never removes them from that app. Device removal, session changes, desktop
//! switches (Ctrl+Alt+Del's secure desktop) and suspend reset the gesture, because the key-ups
//! they hide would otherwise leave keys held.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, HKL, HOT_KEY_MODIFIERS, MAPVK_VK_TO_VSC_EX, MOD_ALT,
    MOD_CONTROL, MOD_NOREPEAT, MapVirtualKeyW, RegisterHotKey, UnregisterHotKey, VK_CONTROL,
    VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RID_INPUT,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_DESKTOPSWITCH, GetForegroundWindow, GetMessageTime, GetQueueStatus,
    GetWindowThreadProcessId, KillTimer, PBT_APMSUSPEND, PostMessageW, QS_ALLINPUT, RI_KEY_BREAK,
    RI_KEY_E0, RI_KEY_E1, SetTimer, WINEVENT_OUTOFCONTEXT, WM_APP, WM_HOTKEY, WM_INPUT,
    WM_INPUT_DEVICE_CHANGE, WM_INPUTLANGCHANGE, WM_NCDESTROY, WM_POWERBROADCAST, WM_TIMER,
    WM_WTSSESSION_CHANGE,
};

use super::presence::{PresenceError, struct_size};
use crate::input::hold::{InputReason, InputStage};
use crate::input::{Edge, Event, Key, PhysicalKey, TalkHeld};

mod physical;
use physical::Physical;

/// The proxy's only timer: the gesture's next one-shot deadline.
const DEADLINE_TIMER: usize = 1;
/// Esc's make code.
const ESCAPE_MAKE_CODE: u16 = 0x01;
/// A keyboard overrun: keys were lost, so held state can't be trusted.
const OVERRUN_MAKE_CODE: u16 = 0xFF;
/// HID generic desktop page, keyboard usage.
const KEYBOARD: (u16, u16) = (0x01, 0x06);
/// Posted to the proxy when the input desktop changes.
const DESKTOP_SWITCHED: u32 = WM_APP + 2;
/// How late an armed deadline may be at a chord key before the message queue is logged.
const OVERDUE: Duration = Duration::from_millis(50);

thread_local! {
    /// The proxy that hears desktop switches; the WinEvent callback has no context of its own.
    static PROXY: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static NEXT_ESCAPE: Cell<i32> = const { Cell::new(0) };
}

/// When the native callback that carried an event began, for gesture timing.
#[derive(Clone, Copy)]
pub(crate) struct EventTime {
    pub(crate) instant: Instant,
    pub(crate) held: TalkHeld,
    pub(crate) repeated_modifier: bool,
    pub(crate) defer_terminal: bool,
    pub(crate) queued_press: Option<u64>,
    read_chord: Option<fn(Instant) -> Option<Instant>>,
    layout: Option<HKL>,
}

impl EventTime {
    pub(crate) fn new(instant: Instant) -> Self {
        Self {
            instant,
            held: TalkHeld::default(),
            repeated_modifier: false,
            defer_terminal: false,
            queued_press: None,
            read_chord: None,
            layout: None,
        }
    }

    pub(crate) fn layout_current(self) -> bool {
        self.layout
            .is_none_or(|layout| foreground_layout() == Some(layout))
    }

    pub(crate) fn recovery_time(self) -> Option<Instant> {
        let now = self.read_chord?(self.instant)?;
        self.layout_current().then_some(now)
    }

    #[cfg(test)]
    pub(crate) fn with_keyboard_read(mut self, read: fn(Instant) -> Option<Instant>) -> Self {
        self.read_chord = Some(read);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_queued_press(mut self, at: u64) -> Self {
        self.queued_press = Some(at);
        self
    }
}

/// Reduces one gesture event at its callback time and returns the next deadline to arm.
pub(crate) type Deliver = Box<dyn Fn(Event, EventTime) -> Option<Instant>>;

#[cfg(test)]
pub(crate) fn with_raw_modifiers(deliver: Deliver) -> Deliver {
    let keys = Keys::default();
    Box::new(move |event, mut at| {
        let raw = match event {
            Event::Key {
                key: Key::LeftControl | Key::LeftAlt,
                edge,
            } => {
                let make = if matches!(
                    event,
                    Event::Key {
                        key: Key::LeftControl,
                        ..
                    }
                ) {
                    0x1D
                } else {
                    0x38
                };
                Ok(Some((
                    HANDLE(std::ptr::without_provenance_mut(1)),
                    RAWKEYBOARD {
                        MakeCode: make,
                        Flags: if edge == Edge::Up {
                            u16::try_from(RI_KEY_BREAK).unwrap()
                        } else {
                            0
                        },
                        ..Default::default()
                    },
                )))
            }
            Event::Reset => Err(()),
            _ => return deliver(event, at),
        };
        let (event, _) = keys.input(raw, &mut at, || {
            Some(HKL(std::ptr::without_provenance_mut(1)))
        })?;
        at.layout = None;
        deliver(event, at)
    })
}

#[cfg(test)]
pub(crate) fn with_raw_keys(deliver: &Deliver) -> impl Fn(usize, u16, Edge, EventTime) + '_ {
    let keys = Keys::default();
    move |device, make, edge, mut at| {
        let raw = RAWKEYBOARD {
            MakeCode: make,
            Flags: if edge == Edge::Up {
                u16::try_from(RI_KEY_BREAK).unwrap()
            } else {
                0
            },
            ..Default::default()
        };
        if let Some((event, _)) = keys.input(
            Ok(Some((
                HANDLE(std::ptr::without_provenance_mut(device)),
                raw,
            ))),
            &mut at,
            || Some(HKL(std::ptr::without_provenance_mut(1))),
        ) {
            at.layout = None;
            deliver(event, at);
        }
    }
}

/// The proxy's keyboard state. Only the UI thread touches it.
#[derive(Default)]
pub(super) struct Keys {
    deliver: OnceCell<Deliver>,
    /// The deadline the timer is armed for, if any.
    deadline: Cell<Option<Instant>>,
    /// The overdue deadline whose message queue was already logged.
    overdue: Cell<Option<Instant>>,
    sessions: Cell<bool>,
    desktop: Cell<Option<HWINEVENTHOOK>>,
    physical: RefCell<Physical>,
    layout: Cell<Option<HKL>>,
    escapes: Rc<Escapes>,
    #[cfg(test)]
    pub(super) refuse_talk_escape: Cell<bool>,
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
        // Stamped first, so gesture timing starts at callback entry.
        let mut at = EventTime::new(Instant::now());
        let mut id = None;
        let event = match message {
            _ if self.plain_escape(message, wparam) => {
                self.physical.borrow_mut().lose();
                Event::Escape
            }
            _ if hotkey(message, wparam, self.escapes.talk.get()) => {
                self.physical.borrow_mut().lose()
            }
            WM_INPUT => {
                at.read_chord = Some(read_chord);
                let Some((event, physical)) =
                    self.input(read_key(lparam), &mut at, foreground_layout)
                else {
                    return false;
                };
                if !at.repeated_modifier
                    && matches!(
                        event,
                        Event::Key {
                            key: Key::LeftControl | Key::LeftAlt,
                            edge: Edge::Down
                        }
                    )
                {
                    // SAFETY: this thread is dispatching the queued WM_INPUT message.
                    let queued = unsafe { GetMessageTime() }.cast_unsigned();
                    let now = kumochi_shell_core::platform::keyboard_ticks();
                    let low = u32::try_from(now & u64::from(u32::MAX)).expect("low clock word");
                    at.queued_press = Some(now.saturating_sub(u64::from(low.wrapping_sub(queued))));
                }
                id = physical;
                event
            }
            // A stale or early WM_TIMER is harmless: the gesture ignores a deadline it hasn't
            // reached and returns the remaining one, which is armed again below.
            WM_TIMER if wparam.0 == DEADLINE_TIMER => {
                tracing::debug!(stage = ?InputStage::Native, reason = ?InputReason::DeadlineDelivered, "held speech input boundary");
                self.disarm(hwnd);
                Event::Deadline
            }
            WM_INPUT_DEVICE_CHANGE => self.physical.borrow_mut().lose(),
            WM_INPUTLANGCHANGE | WM_WTSSESSION_CHANGE | DESKTOP_SWITCHED | WM_NCDESTROY => {
                self.layout.set(None);
                self.physical.borrow_mut().lose()
            }
            WM_POWERBROADCAST if wparam.0 == usize::try_from(PBT_APMSUSPEND).unwrap_or(0) => {
                self.physical.borrow_mut().lose()
            }
            _ => return false,
        };
        if !hwnd.is_invalid() && matches!(event, Event::Reset | Event::Escape) {
            self.physical
                .borrow_mut()
                .observe_held_on_loss(read_held_modifiers());
        }
        self.forward(hwnd, event, id, at);
        message == WM_TIMER
            || message == DESKTOP_SWITCHED
            || (message == WM_HOTKEY && self.deliver.get().is_some())
    }

    fn forward(&self, hwnd: HWND, event: Event, id: Option<PhysicalKey>, mut at: EventTime) {
        let retiring = if self.escapes.talk.get() != 0
            && matches!(
                event,
                Event::Reset
                    | Event::Escape
                    | Event::Key {
                        key: Key::LeftControl | Key::LeftAlt,
                        edge: Edge::Up,
                    }
                    | Event::Key {
                        key: Key::Other(_) | Key::Escape(_),
                        edge: Edge::Down,
                    }
            ) {
            self.escapes.talk.replace(0)
        } else {
            0
        };
        at.defer_terminal = retiring != 0;
        // The screen watch hears what the gesture hears: a talk key or another key, or a reset.
        super::screen_watch::heard(event, id);
        if self.overdue(event, at.instant) {
            // SAFETY: reads only this thread's own message queue state.
            let queue = unsafe { GetQueueStatus(QS_ALLINPUT) };
            tracing::debug!(stage = ?InputStage::Native, reason = ?InputReason::DeadlineOverdue, queue, "held speech input boundary");
        }
        let deliver = self.deliver.get();
        let mut deadline = if let Some(deliver) = deliver {
            // `at` stays the event's time; the timer counts from now, since delivery takes time.
            deliver(event, at)
        } else {
            if matches!(
                event,
                Event::Deadline
                    | Event::Key {
                        key: Key::LeftControl | Key::LeftAlt,
                        ..
                    }
            ) {
                tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::CallbackMissing, "held speech input failed");
            }
            None
        };
        if retiring != 0 {
            self.escapes.release_native(hwnd, retiring);
            if !self.escape_trusted() && !matches!(event, Event::Reset | Event::Escape) {
                let reset = self.physical.borrow_mut().lose();
                self.physical
                    .borrow_mut()
                    .observe_held_on_loss(read_held_modifiers());
                super::screen_watch::heard(reset, None);
                deadline = deliver.and_then(|deliver| deliver(reset, at));
            }
        }
        self.arm(hwnd, deadline, Instant::now());
    }

    fn input(
        &self,
        raw: Result<Option<RawKey>, ()>,
        at: &mut EventTime,
        layout: impl FnOnce() -> Option<HKL>,
    ) -> Option<(Event, Option<PhysicalKey>)> {
        let mut physical = self.physical.borrow_mut();
        let (device, raw) = match raw {
            Ok(raw) => raw?,
            Err(()) => return Some((physical.lose(), None)),
        };
        if !self.escape_trusted() {
            return Some((physical.lose(), None));
        }
        let event = key_event(raw.MakeCode, raw.Flags);
        let slot = raw.MakeCode | (u16::from(u32::from(raw.Flags) & RI_KEY_E0 != 0) << 8);
        if matches!(
            event,
            Event::Key {
                key: Key::LeftControl | Key::LeftAlt,
                ..
            }
        ) {
            let layout = layout();
            let previous = self.layout.replace(layout);
            if layout.is_none() || previous.is_some_and(|old| Some(old) != layout) {
                return Some((physical.lose(), None));
            }
            at.layout = layout;
        }
        let (event, id, repeated) = physical.key(device, slot, event);
        if id.is_some() && matches!(event, Event::Key { edge: Edge::Up, .. }) {
            at.held = physical.held();
        }
        at.repeated_modifier = repeated;
        if repeated {
            at.read_chord = None;
        }
        Some((event, id))
    }

    pub(super) fn plain_escape(&self, message: u32, wparam: WPARAM) -> bool {
        hotkey(message, wparam, self.escapes.plain.get())
    }

    pub(super) fn escape_trusted(&self) -> bool {
        !self.escapes.uncertain.get()
    }

    /// True once per armed deadline, when a chord key arrives more than `OVERDUE` after it.
    fn overdue(&self, event: Event, at: Instant) -> bool {
        matches!(
            event,
            Event::Key {
                key: Key::LeftControl | Key::LeftAlt,
                ..
            }
        ) && self
            .deadline
            .get()
            .filter(|deadline| at.saturating_duration_since(*deadline) > OVERDUE)
            .is_some_and(|deadline| self.overdue.replace(Some(deadline)) != Some(deadline))
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
            tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::DeadlineArm, "held speech input failed");
            return;
        }
        if self.deadline.replace(Some(deadline)) != Some(deadline) {
            tracing::debug!(stage = ?InputStage::Native, reason = ?InputReason::DeadlineArmed, "held speech input boundary");
        }
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
        tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::AlreadyArmed, "held speech input failed");
        return Err(PresenceError::KeyboardArmed);
    }
    if let Err(error) = watch(hwnd, keys) {
        disarm(hwnd, keys);
        return Err(error);
    }
    let armed = keys
        .deliver
        .set(deliver)
        .map_err(|_| PresenceError::KeyboardArmed);
    if armed.is_ok() {
        tracing::info!(stage = ?InputStage::Native, reason = ?InputReason::Armed, "held speech input boundary");
    }
    armed
}

fn watch(hwnd: HWND, keys: &Keys) -> Result<(), PresenceError> {
    let device = RAWINPUTDEVICE {
        usUsagePage: KEYBOARD.0,
        usUsage: KEYBOARD.1,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    };
    // SAFETY: one valid device record for this thread's live window.
    unsafe { RegisterRawInputDevices(&[device], struct_size::<RAWINPUTDEVICE>()) }
        .inspect_err(|_| tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::RawInputRegistration, "held speech input failed"))?;
    // SAFETY: `hwnd` is live; the proxy unregisters it before it's destroyed.
    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
        .inspect_err(|_| tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::SessionWatcher, "held speech input failed"))
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
        tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::DesktopWatcher, "held speech input failed");
        return Err(PresenceError::DesktopWatch);
    }
    keys.desktop.set(Some(hook));
    Ok(())
}

/// Stops what `arm` started, before the proxy window goes away.
pub(super) fn disarm(hwnd: HWND, keys: &Keys) {
    keys.forward(hwnd, Event::Reset, None, EventTime::new(Instant::now()));
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

/// Esc held system-wide for the proxy window, alone or with the talk keys down; dropping it
/// gives that Esc back to other apps.
pub(crate) struct EscapeHotkey(HWND, i32, Rc<Escapes>, EscapeKind);

#[derive(Clone, Copy)]
enum EscapeKind {
    Plain,
    Talk,
}

#[derive(Default)]
struct Escapes {
    plain: Cell<i32>,
    talk: Cell<i32>,
    uncertain: Cell<bool>,
}

impl Escapes {
    fn slot(&self, kind: EscapeKind) -> &Cell<i32> {
        match kind {
            EscapeKind::Plain => &self.plain,
            EscapeKind::Talk => &self.talk,
        }
    }

    fn retire(&self, kind: EscapeKind, id: i32, release: impl FnOnce() -> bool) {
        let slot = self.slot(kind);
        if id != 0 && slot.get() == id {
            slot.set(0);
            if !release() {
                self.uncertain.set(true);
            }
        }
    }

    fn unregister(&self, hwnd: HWND, kind: EscapeKind, id: i32) {
        self.retire(kind, id, || unregister_hotkey(hwnd, id));
    }

    fn release_native(&self, hwnd: HWND, id: i32) {
        if !unregister_hotkey(hwnd, id) {
            self.uncertain.set(true);
        }
    }
}

fn unregister_hotkey(hwnd: HWND, id: i32) -> bool {
    #[cfg(test)]
    if hwnd.is_invalid() {
        return true;
    }
    // SAFETY: this is the id registered on the proxy's owning thread.
    unsafe { UnregisterHotKey(Some(hwnd), id) }.is_ok()
}

impl std::fmt::Debug for EscapeHotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EscapeHotkey(..)")
    }
}

impl EscapeHotkey {
    /// # Errors
    /// Fails when another app, or this one, already holds Esc.
    pub(super) fn register(hwnd: HWND, keys: &Keys) -> Result<Self, PresenceError> {
        Self::hold(hwnd, keys, EscapeKind::Plain, MOD_NOREPEAT)
    }

    /// Esc with Ctrl and Alt down, which plain Esc doesn't match. Those are the only modifiers
    /// a hold's keys can have down: any other key going down cancels the hold.
    ///
    /// # Errors
    /// Fails when another app, or this one, already holds that chord.
    pub(super) fn register_with_talk_keys(hwnd: HWND, keys: &Keys) -> Result<Self, PresenceError> {
        Self::hold(
            hwnd,
            keys,
            EscapeKind::Talk,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
        )
    }

    fn hold(
        hwnd: HWND,
        keys: &Keys,
        kind: EscapeKind,
        modifiers: HOT_KEY_MODIFIERS,
    ) -> Result<Self, PresenceError> {
        let slot = keys.escapes.slot(kind);
        if !keys.escape_trusted() || slot.get() != 0 {
            return Err(PresenceError::EscapeLease);
        }
        let id = NEXT_ESCAPE
            .get()
            .checked_add(1)
            .filter(|id| *id <= 0xBFFF)
            .ok_or(PresenceError::EscapeLease)?;
        NEXT_ESCAPE.set(id);
        #[cfg(test)]
        if hwnd.is_invalid() {
            if keys.refuse_talk_escape.get() {
                return Err(PresenceError::EscapeLease);
            }
            slot.set(id);
            return Ok(Self(hwnd, id, Rc::clone(&keys.escapes), kind));
        }
        // SAFETY: `hwnd` is a live window on this thread; Drop releases the same id.
        unsafe { RegisterHotKey(Some(hwnd), id, modifiers, u32::from(VK_ESCAPE.0)) }?;
        slot.set(id);
        Ok(Self(hwnd, id, Rc::clone(&keys.escapes), kind))
    }
}

/// Whether `message` is the proxy's hotkey `id` firing.
fn hotkey(message: u32, wparam: WPARAM, id: i32) -> bool {
    message == WM_HOTKEY && id != 0 && wparam.0 == usize::try_from(id).unwrap_or(0)
}

impl Drop for EscapeHotkey {
    fn drop(&mut self) {
        self.2.unregister(self.0, self.3, self.1);
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

fn foreground_layout() -> Option<HKL> {
    // SAFETY: a read-only query of the current foreground.
    let window = unsafe { GetForegroundWindow() };
    // SAFETY: no output pointer; an invalid window returns zero.
    let thread = unsafe { GetWindowThreadProcessId(window, None) };
    if thread == 0 {
        return None;
    }
    // SAFETY: looks up a layout by thread ID without writing to either process.
    let layout = unsafe { GetKeyboardLayout(thread) };
    (!layout.is_invalid()).then_some(layout)
}

type RawKey = (HANDLE, RAWKEYBOARD);

/// A non-keyboard record has no effect. An unreadable record invalidates input authority.
fn read_key(lparam: LPARAM) -> Result<Option<RawKey>, ()> {
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
    if read == u32::MAX || read < struct_size::<RAWINPUTHEADER>() || raw.header.dwSize != read {
        tracing::warn!(stage = ?InputStage::Native, reason = ?InputReason::RawInputRead, "held speech input failed");
        return Err(());
    }
    if raw.header.dwType != RIM_TYPEKEYBOARD.0 {
        return Ok(None);
    }
    if read < struct_size::<RAWINPUTHEADER>() + struct_size::<RAWKEYBOARD>() {
        return Err(());
    }
    // SAFETY: the header says this record is a keyboard one.
    let keyboard = unsafe { raw.data.keyboard };
    let keyboard = normalize_keyboard(keyboard, |vk| {
        // SAFETY: a pure table lookup in the current layout.
        unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) }
    })?;
    Ok(Some((raw.header.hDevice, keyboard)))
}

fn normalize_keyboard(
    mut keyboard: RAWKEYBOARD,
    map: impl FnOnce(u16) -> u32,
) -> Result<RAWKEYBOARD, ()> {
    if keyboard.MakeCode != 0 {
        return Ok(keyboard);
    }
    let vk = keyboard.VKey;
    if [VK_CONTROL.0, VK_MENU.0, VK_SHIFT.0].contains(&vk) {
        return Err(());
    }
    let scan = map(vk);
    let prefix = match scan >> 8 {
        0 => 0,
        0xE0 => RI_KEY_E0,
        0xE1 => RI_KEY_E1,
        _ => return Err(()),
    };
    let flags = u32::from(keyboard.Flags);
    let observed = flags & (RI_KEY_E0 | RI_KEY_E1);
    if observed != 0 && observed != prefix {
        return Err(());
    }
    let make = u16::try_from(scan & 0xFF).map_err(|_| ())?;
    let sides = [
        (VK_LCONTROL, 0x1D, 0),
        (VK_RCONTROL, 0x1D, RI_KEY_E0),
        (VK_LMENU, 0x38, 0),
        (VK_RMENU, 0x38, RI_KEY_E0),
        (VK_LSHIFT, 0x2A, 0),
        (VK_RSHIFT, 0x36, 0),
        (VK_LWIN, 0x5B, RI_KEY_E0),
        (VK_RWIN, 0x5C, RI_KEY_E0),
    ];
    if make == 0
        || sides
            .into_iter()
            .any(|(side, code, flags)| (vk == side.0) != (make == code && prefix == flags))
    {
        return Err(());
    }
    keyboard.MakeCode = make;
    keyboard.Flags = u16::try_from(flags | prefix).map_err(|_| ())?;
    Ok(keyboard)
}

fn read_chord(_: Instant) -> Option<Instant> {
    // SAFETY: queries the current foreground window without touching it.
    let foreground = unsafe { GetForegroundWindow() };
    let exact = exact_chord(|key| {
        // SAFETY: queries a bounded virtual-key code without modifying input.
        unsafe { GetAsyncKeyState(i32::from(key)) < 0 }
    });
    // SAFETY: rechecks the foreground without touching it.
    (exact && foreground == unsafe { GetForegroundWindow() }).then(Instant::now)
}

fn read_held_modifiers() -> [bool; 2] {
    // SAFETY: reads only the current foreground window and two virtual keys.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return [true; 2];
    }
    let held = [VK_LCONTROL, VK_LMENU].map(|key| {
        // SAFETY: reads a fixed virtual-key code without modifying input.
        unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
    });
    // SAFETY: rechecks the foreground without touching it.
    if foreground == unsafe { GetForegroundWindow() } {
        held
    } else {
        [true; 2]
    }
}

fn exact_chord(mut down: impl FnMut(u16) -> bool) -> bool {
    let left = [VK_LCONTROL.0, VK_LMENU.0];
    if !left.into_iter().all(&mut down) {
        return false;
    }
    for key in std::iter::once(0x03).chain(0x08..=0xFE) {
        if !left.contains(&key)
            && key != VK_CONTROL.0
            && key != VK_MENU.0
            && !(0xC3..=0xDA).contains(&key)
            && down(key)
        {
            return false;
        }
    }
    left.into_iter().all(down)
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
        _ => {
            let id = PhysicalKey::new(make_code | u16::from(e0) << 8)
                .expect("validated make code and E0 fit a physical slot");
            if make_code == ESCAPE_MAKE_CODE && !e0 {
                Key::Escape(id)
            } else {
                Key::Other(id)
            }
        }
    };
    Event::Key { key, edge }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WM_HOTKEY, WS_POPUP,
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
        assert_eq!(
            key_event(0x1D, E0),
            key(
                Key::Other(crate::input::PhysicalKey::new(0x11D).unwrap()),
                Edge::Down
            )
        );
        assert_eq!(
            key_event(0x38, E0 | BREAK),
            key(
                Key::Other(crate::input::PhysicalKey::new(0x138).unwrap()),
                Edge::Up
            )
        );
        assert_eq!(
            key_event(0x1E, 0),
            key(
                Key::Other(crate::input::PhysicalKey::new(0x1E).unwrap()),
                Edge::Down
            )
        );
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
    /// `cargo test -p kumochi-shell -- --ignored escape_hold`.
    #[test]
    #[ignore = "holds the desktop's Esc briefly; run only in an approved session"]
    fn escape_hold_registers_shows_stop_when_taken_and_releases() {
        let (first, second) = (TestWindow::new(), TestWindow::new());
        let (first_keys, second_keys) = (super::Keys::default(), super::Keys::default());
        let mut holder = EscapeHold::default();
        holder.acquire(|| EscapeHotkey::register(first.0, &first_keys));
        assert!(
            !holder.shows_stop(),
            "Esc must be free before this fixture runs"
        );
        let mut blocked = EscapeHold::default();
        blocked.acquire(|| EscapeHotkey::register(second.0, &second_keys));
        assert!(blocked.shows_stop());
        holder.release();
        blocked.release();
        blocked.acquire(|| EscapeHotkey::register(second.0, &second_keys));
        assert!(!blocked.shows_stop(), "releasing the first hold frees Esc");
        blocked.release();
    }

    /// Holds the desktop's Ctrl+Alt+Esc and Esc for milliseconds, so it runs only in an
    /// approved desktop session: `cargo test -p kumochi-shell -- --ignored talk_keys_escape`.
    #[test]
    #[ignore = "holds the desktop's Ctrl+Alt+Esc and Esc briefly; run only in an approved session"]
    fn talk_keys_escape_registers_beside_plain_esc_is_refused_when_taken_and_releases_only_itself()
    {
        let (first, second) = (TestWindow::new(), TestWindow::new());
        let (first_keys, second_keys) = (super::Keys::default(), super::Keys::default());
        let chord = EscapeHotkey::register_with_talk_keys(first.0, &first_keys)
            .expect("Ctrl+Alt+Esc must be free before this fixture runs");
        let plain =
            EscapeHotkey::register(first.0, &first_keys).expect("plain Esc is another hotkey");
        assert!(EscapeHotkey::register_with_talk_keys(second.0, &second_keys).is_err());
        drop(chord);
        drop(
            EscapeHotkey::register_with_talk_keys(second.0, &second_keys)
                .expect("dropping the first hold frees the chord"),
        );
        assert!(
            EscapeHotkey::register(second.0, &second_keys).is_err(),
            "dropping the chord leaves plain Esc held"
        );
        drop(plain);
    }

    #[test]
    fn registered_talk_escape_cancels_without_inventing_a_break() {
        let keys = super::Keys::default();
        let talk = EscapeHotkey::register_with_talk_keys(HWND::default(), &keys).unwrap();
        let plain = EscapeHotkey::register(HWND::default(), &keys).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::clone(&seen);
        let deliver: super::Deliver = Box::new(move |event, _| {
            events.borrow_mut().push(event);
            None
        });
        assert!(keys.deliver.set(deliver).is_ok());
        let hotkey = |id| keys.on_message(HWND::default(), WM_HOTKEY, WPARAM(id), LPARAM(0));
        let old = usize::try_from(talk.1).unwrap();
        assert!(hotkey(old), "the chord's message is consumed");
        assert_eq!(*seen.borrow(), [Event::Reset]);
        assert!(hotkey(usize::try_from(plain.1).unwrap()));
        assert_eq!(
            seen.borrow()[1..],
            [Event::Escape],
            "plain Esc is unchanged"
        );
        drop(talk);
        assert!(!hotkey(old), "the retired message is not delivered");
        let next = EscapeHotkey::register_with_talk_keys(HWND::default(), &keys).unwrap();
        assert_ne!(usize::try_from(next.1).unwrap(), old);
        assert!(
            !hotkey(old),
            "the successor cannot receive the retired message"
        );
        assert_eq!(seen.borrow().len(), 2);
    }

    #[test]
    fn native_end_returns_talk_escape_before_the_next_chord() {
        for reset in [false, true] {
            let hwnd = HWND::default();
            let keys = super::Keys::default();
            let seen = Rc::new(RefCell::new(Vec::new()));
            let events = Rc::clone(&seen);
            assert!(
                keys.deliver
                    .set(Box::new(move |event, _| {
                        events.borrow_mut().push(event);
                        None
                    }))
                    .is_ok()
            );
            for make in [0x1D, 0x38] {
                let (event, at) = raw(&keys, 1, make, 0).unwrap();
                keys.forward(hwnd, event, None, at);
            }
            let first = EscapeHotkey::register_with_talk_keys(hwnd, &keys).unwrap();
            let plain = EscapeHotkey::register(hwnd, &keys).unwrap();
            if reset {
                assert!(!keys.on_message(hwnd, super::WM_WTSSESSION_CHANGE, WPARAM(0), LPARAM(0)));
            } else {
                let (event, at) = raw(&keys, 1, 0x38, super::RI_KEY_BREAK).unwrap();
                keys.forward(hwnd, event, None, at);
                let (event, at) = raw(&keys, 1, 0x38, 0).unwrap();
                keys.forward(hwnd, event, None, at);
            }
            let queued = seen.borrow().len();
            assert!(
                !keys.on_message(
                    hwnd,
                    WM_HOTKEY,
                    WPARAM(usize::try_from(first.1).unwrap()),
                    LPARAM(0),
                ),
                "the native terminal edge returns the old registration before the UI applies End"
            );
            assert_eq!(seen.borrow().len(), queued);
            assert!(keys.plain_escape(WM_HOTKEY, WPARAM(usize::try_from(plain.1).unwrap())));
            let next = EscapeHotkey::register_with_talk_keys(hwnd, &keys).unwrap();
            drop(first);
            assert_eq!(
                keys.escapes.talk.get(),
                next.1,
                "late UI cleanup cannot retire the successor"
            );
        }
    }

    #[test]
    fn an_uncertain_escape_release_rejects_its_message_and_next_lease() {
        let keys = super::Keys::default();
        let lease = EscapeHotkey::register_with_talk_keys(HWND::default(), &keys).unwrap();
        let id = usize::try_from(lease.1).unwrap();
        let attempts = std::cell::Cell::new(0);
        keys.escapes.retire(lease.3, lease.1, || {
            attempts.set(attempts.get() + 1);
            false
        });
        keys.escapes.retire(lease.3, lease.1, || {
            panic!("the same lease can only be retired once")
        });
        assert_eq!(attempts.get(), 1);
        assert!(!keys.escape_trusted());
        assert!(!keys.on_message(HWND::default(), WM_HOTKEY, WPARAM(id), LPARAM(0)));
        assert!(matches!(
            EscapeHotkey::register_with_talk_keys(HWND::default(), &keys),
            Err(super::PresenceError::EscapeLease)
        ));
    }

    fn raw(
        keys: &super::Keys,
        device: usize,
        make: u16,
        flags: u32,
    ) -> Option<(Event, super::EventTime)> {
        raw_in_layout(keys, device, make, flags, 1)
    }

    fn raw_in_layout(
        keys: &super::Keys,
        device: usize,
        make: u16,
        flags: u32,
        layout: usize,
    ) -> Option<(Event, super::EventTime)> {
        let mut time = super::EventTime::new(std::time::Instant::now());
        keys.input(
            Ok(Some((
                super::HANDLE(std::ptr::without_provenance_mut(device)),
                super::RAWKEYBOARD {
                    MakeCode: make,
                    Flags: u16::try_from(flags).unwrap(),
                    ..Default::default()
                },
            ))),
            &mut time,
            || Some(super::HKL(std::ptr::without_provenance_mut(layout))),
        )
        .map(|(event, _)| (event, time))
    }

    #[test]
    fn a_layout_change_at_release_resets_and_needs_the_new_layout_to_rearm() {
        let keys = super::Keys::default();
        let up = super::RI_KEY_BREAK;
        assert!(raw(&keys, 1, 0x1D, 0).is_some());
        assert!(raw(&keys, 1, 0x38, 0).is_some());
        assert_eq!(
            raw_in_layout(&keys, 1, 0x38, up, 2).unwrap().0,
            Event::Reset
        );
        assert!(matches!(
            raw_in_layout(&keys, 1, 0x1D, up, 2).unwrap().0,
            Event::Key {
                key: Key::LeftControl,
                edge: Edge::Up
            }
        ));
        assert!(matches!(
            raw_in_layout(&keys, 1, 0x1D, 0, 2),
            Some((
                Event::Key {
                    key: crate::input::Key::LeftControl,
                    edge: crate::input::Edge::Down,
                },
                _
            ))
        ));
    }

    #[test]
    fn lost_raw_input_preserves_uncertainty_until_the_owning_key_releases() {
        let keys = super::Keys::default();
        let mut time = super::EventTime::new(std::time::Instant::now());
        assert!(raw(&keys, 1, 0x1D, 0).is_some());
        assert!(
            keys.input(Ok(None), &mut time, || panic!("not keyboard input"))
                .is_none()
        );
        assert!(raw(&keys, 1, 0x1D, 0).unwrap().1.repeated_modifier);
        assert_eq!(
            keys.input(Err(()), &mut time, || panic!("record was unreadable")),
            Some((Event::Reset, None))
        );

        assert!(raw(&keys, 1, 0x1D, 0).unwrap().1.repeated_modifier);
        assert_eq!(
            raw(&keys, 2, 0x1D, super::RI_KEY_BREAK).unwrap().0,
            Event::Reset
        );
        assert!(raw(&keys, 1, 0x1D, 0).unwrap().1.repeated_modifier);
        assert!(raw(&keys, 1, 0x1D, super::RI_KEY_BREAK).is_some());
        assert!(!raw(&keys, 1, 0x1D, 0).unwrap().1.repeated_modifier);
    }

    #[test]
    fn overflow_discards_bank_occupancy_instead_of_keeping_recovery_debt() {
        let keys = super::Keys::default();
        assert!(raw(&keys, 1, 0x25, 0).is_some());
        assert!(raw(&keys, 2, 0x24, 0).is_some());
        assert_eq!(raw(&keys, 3, 0x2A, 0).unwrap().0, Event::Reset);
        assert!(matches!(
            raw(&keys, 4, 0x1D, 0),
            Some((
                Event::Key {
                    key: crate::input::Key::LeftControl,
                    ..
                },
                _
            ))
        ));
    }

    #[test]
    fn a_second_device_cannot_repeat_or_release_another_devices_modifier() {
        for flags in [0, super::RI_KEY_BREAK] {
            let keys = super::Keys::default();
            assert!(raw(&keys, 1, 0x1D, 0).is_some());
            assert_eq!(raw(&keys, 2, 0x1D, flags).unwrap().0, Event::Reset);
            assert!(raw(&keys, 1, 0x1D, super::RI_KEY_BREAK).is_some());
            if flags == 0 {
                assert!(raw(&keys, 2, 0x1D, 0).unwrap().1.repeated_modifier);
                assert!(raw(&keys, 2, 0x1D, super::RI_KEY_BREAK).is_some());
            }
            assert!(!raw(&keys, 2, 0x1D, 0).unwrap().1.repeated_modifier);
        }
    }

    #[test]
    fn null_device_can_hold_release_and_double_tap() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        use crate::input::hold::{Fence, Intent};
        use crate::input::{Effect, Gesture};
        use crate::platform::microphone::CollectionGate;
        use kumochi_protocol::message::GestureId;

        let keys = super::Keys::default();
        let mut fence = Fence::new(CollectionGate::default());
        let mut gesture = Gesture::default();
        let start = Instant::now();
        let mut send = |event, ms| {
            fence.reduce(
                &mut gesture,
                event,
                start + Duration::from_millis(ms),
                &Cell::new(false),
            )
        };
        let key = |scan, flags| raw(&keys, 0, scan, flags).unwrap().0;
        let up = super::RI_KEY_BREAK;
        send(key(0x1D, 0), 0);
        assert!(matches!(
            send(key(0x38, 0), 1).intents,
            [Some(Intent::Warm(GestureId(1))), None, None]
        ));
        assert!(matches!(
            send(Event::Deadline, 301).intents,
            [
                Some(Intent::Recognized {
                    gesture: GestureId(1),
                    generation: Ok(_),
                }),
                None,
                None
            ]
        ));
        assert!(matches!(
            send(key(0x38, up), 400).intents,
            [
                None,
                Some(Intent::End {
                    gesture: GestureId(1),
                    cancelled: false,
                }),
                None
            ]
        ));
        send(key(0x1D, up), 401);

        for (start, expected) in [(500, None), (600, Some(Effect::OpenInput))] {
            send(key(0x1D, 0), start);
            send(key(0x38, 0), start + 1);
            assert_eq!(
                send(key(0x38, up), start + 50).effects,
                [Some(Effect::Resting), expected, None]
            );
            send(key(0x1D, up), start + 51);
        }
    }

    #[test]
    fn null_device_break_during_a_physical_hold_matches_a_second_keyboard() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        use crate::input::hold::{Fence, Intent};
        use crate::input::{Effect, Gesture};
        use crate::platform::microphone::CollectionGate;
        use kumochi_protocol::message::GestureId;

        for other in [2, 0] {
            let keys = super::Keys::default();
            let mut fence = Fence::new(CollectionGate::default());
            let mut gesture = Gesture::default();
            let start = Instant::now();
            let mut send = |event, ms| {
                fence.reduce(
                    &mut gesture,
                    event,
                    start + Duration::from_millis(ms),
                    &Cell::new(false),
                )
            };
            let key = |source, scan, flags| raw(&keys, source, scan, flags).unwrap().0;
            let up = super::RI_KEY_BREAK;
            send(key(1, 0x1D, 0), 0);
            send(key(1, 0x38, 0), 1);
            assert!(matches!(
                send(Event::Deadline, 301).intents[0],
                Some(Intent::Recognized {
                    gesture: GestureId(1),
                    generation: Ok(_),
                })
            ));
            let unrelated_break = send(key(other, 0x25, up), 350);
            assert!(unrelated_break.intents.iter().all(Option::is_none));
            assert_eq!(unrelated_break.effects, [None; 3]);
            let release = send(key(1, 0x38, up), 400);
            assert!(matches!(
                release.intents[1],
                Some(Intent::End {
                    gesture: GestureId(1),
                    cancelled: false,
                })
            ));
            assert_eq!(release.effects, [None, None, Some(Effect::Resting)]);
        }
    }

    #[test]
    fn null_and_physical_devices_cannot_repeat_or_release_each_others_modifiers() {
        use crate::input::TalkHeld;

        for (owner, other) in [(0, 1), (1, 0)] {
            for (scan, key) in [(0x1D, Key::LeftControl), (0x38, Key::LeftAlt)] {
                for flags in [0, super::RI_KEY_BREAK] {
                    let keys = super::Keys::default();
                    let (event, at) = raw(&keys, owner, scan, 0).unwrap();
                    assert_eq!(
                        event,
                        Event::Key {
                            key,
                            edge: Edge::Down
                        }
                    );
                    assert!(!at.repeated_modifier);
                    assert!(raw(&keys, owner, scan, 0).unwrap().1.repeated_modifier);
                    assert_eq!(raw(&keys, other, scan, flags).unwrap().0, Event::Reset);
                    assert_eq!(keys.physical.borrow().held(), TalkHeld::default());
                }
            }
        }
    }

    #[test]
    fn os_read_uses_side_specific_keys_without_aliases_and_rechecks_the_pair() {
        use super::{VK_LCONTROL, VK_LMENU, exact_chord};

        let pair = [VK_LCONTROL.0, VK_LMENU.0];
        let allowed = |key| pair.contains(&key) || matches!(key, 0x01 | 0x11 | 0x12);
        assert!(
            exact_chord(allowed),
            "mouse and aggregate aliases aren't extras"
        );
        for extra in [0xA3, 0xA5, 0xA0, 0xA1, 0x1B, 0x4B] {
            assert!(!exact_chord(|key| allowed(key) || key == extra));
        }
        assert!(!exact_chord(|key| matches!(key, 0xA3 | 0xA5 | 0x11 | 0x12)));
        for released in pair {
            let mut seen = false;
            assert!(!exact_chord(|key| {
                if key == released {
                    !std::mem::replace(&mut seen, true)
                } else {
                    allowed(key)
                }
            }));
        }
    }

    #[test]
    fn zero_make_code_preserves_prefix_evidence_and_never_guesses_a_modifier_side() {
        use super::{
            RAWKEYBOARD, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, VK_CONTROL, VK_LCONTROL, VK_LMENU,
            VK_MENU, VK_RCONTROL, VK_RMENU, normalize_keyboard,
        };

        for (generic, left, right, scan) in [
            (VK_CONTROL, VK_LCONTROL, VK_RCONTROL, 0x1D),
            (VK_MENU, VK_LMENU, VK_RMENU, 0x38),
        ] {
            let raw = |vk, flags| RAWKEYBOARD {
                VKey: vk,
                Flags: u16::try_from(flags).unwrap(),
                ..Default::default()
            };
            for flags in [0, RI_KEY_E0, RI_KEY_E1] {
                assert!(normalize_keyboard(raw(generic.0, flags), |_| u32::from(scan)).is_err());
            }
            assert!(normalize_keyboard(raw(left.0, RI_KEY_E0), |_| u32::from(scan)).is_err());
            assert!(
                normalize_keyboard(raw(right.0, RI_KEY_E1), |_| 0xE000 | u32::from(scan)).is_err()
            );
            assert!(normalize_keyboard(raw(right.0, 0), |_| u32::from(scan)).is_err());
            assert!(normalize_keyboard(raw(right.0, 0), |_| 0x1E).is_err());
            for (vk, prefix, code) in [
                (left, 0, u32::from(scan)),
                (right, RI_KEY_E0, 0xE000 | u32::from(scan)),
            ] {
                let key = normalize_keyboard(raw(vk.0, prefix | RI_KEY_BREAK), |_| code).unwrap();
                assert_eq!(
                    (key.MakeCode, u32::from(key.Flags)),
                    (scan, prefix | RI_KEY_BREAK)
                );
            }
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one recovery assertion matrix across native loss sources"
    )]
    fn coverage_loss_refuses_dirty_or_blind_attempts_and_admits_the_first_clean_one() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        use crate::input::Gesture;
        use crate::input::hold::{Delivery, Fence, Intent};
        use crate::platform::microphone::CollectionGate;
        use kumochi_protocol::message::GestureId;

        for loss in 0..9 {
            let keys = super::Keys::default();
            let state = Rc::new(RefCell::new((
                Fence::new(CollectionGate::default()),
                Gesture::default(),
            )));
            let reads = Rc::new(Cell::new((false, false, 0)));
            let results = Rc::new(RefCell::new(Vec::new()));
            let reduce = {
                let (state, reads, results) =
                    (Rc::clone(&state), Rc::clone(&reads), Rc::clone(&results));
                move |event, at: super::EventTime| {
                    let (fence, gesture) = &mut *state.borrow_mut();
                    let (ctrl, alt, extra) = reads.get();
                    let result = fence.reduce_with_keyboard_read(
                        gesture,
                        event,
                        at.instant,
                        (!at.repeated_modifier).then_some(0),
                        || Some(0),
                        || {
                            (!at.repeated_modifier
                                && super::exact_chord(|key| match key {
                                    0xA2 => ctrl,
                                    0xA4 => alt,
                                    _ => key == extra,
                                }))
                            .then_some(at.instant)
                        },
                    );
                    results.borrow_mut().push(result);
                    None
                }
            };
            assert!(keys.deliver.set(Box::new(reduce)).is_ok());
            let deliver = keys.deliver.get().unwrap();
            let now = Instant::now();
            let send = |event, ms| {
                deliver(
                    event,
                    super::EventTime::new(now + Duration::from_millis(ms)),
                )
            };
            let input = |device, scan, flags, ms| {
                let (event, mut at) = raw(&keys, device, scan, flags).unwrap();
                at.instant = now + Duration::from_millis(ms);
                deliver(event, at);
            };
            let unreadable = |ms| {
                let mut at = super::EventTime::new(now + Duration::from_millis(ms));
                let (event, _) = keys.input(Err(()), &mut at, || unreachable!()).unwrap();
                deliver(event, at);
            };
            let hwnd = HWND::default();
            match loss {
                0 | 8 => {
                    unreadable(1);
                }
                1 => {
                    input(1, 0x25, 0, 0);
                    unreadable(1);
                }
                2 => {
                    input(1, 0x25, 0, 0);
                    keys.on_message(hwnd, super::WM_WTSSESSION_CHANGE, WPARAM(0), LPARAM(0));
                }
                3 => {
                    input(1, 0x25, 0, 0);
                    input(2, 0x24, 0, 0);
                    input(3, 0x2A, 0, 1);
                }
                4 | 5 => {
                    let lease = if loss == 4 {
                        EscapeHotkey::register(hwnd, &keys).unwrap()
                    } else {
                        EscapeHotkey::register_with_talk_keys(hwnd, &keys).unwrap()
                    };
                    assert!(keys.on_message(
                        hwnd,
                        WM_HOTKEY,
                        WPARAM(usize::try_from(lease.1).unwrap()),
                        LPARAM(0)
                    ));
                }
                _ => {
                    let (vk, scan, mate) = if loss == 6 {
                        (super::VK_CONTROL, 0x1D, 0x38)
                    } else {
                        (super::VK_MENU, 0x38, 0x1D)
                    };
                    input(1, mate, 0, 0);
                    let mut at = super::EventTime::new(now + Duration::from_millis(1));
                    let record = super::normalize_keyboard(
                        super::RAWKEYBOARD {
                            VKey: vk.0,
                            Flags: u16::try_from(super::RI_KEY_E0).unwrap(),
                            ..Default::default()
                        },
                        |_| scan,
                    );
                    let (event, _) = keys
                        .input(
                            record.map(|key| {
                                Some((super::HANDLE(std::ptr::without_provenance_mut(1)), key))
                            }),
                            &mut at,
                            || Some(super::HKL(std::ptr::without_provenance_mut(1))),
                        )
                        .unwrap();
                    deliver(event, at);
                }
            }
            let up = super::RI_KEY_BREAK;
            let device = usize::from(loss != 8);
            let attempts = [
                (false, false, 0),
                (true, true, 0x1B),
                (true, true, 0xA0),
                (true, true, 0x4B),
            ];
            for (n, snapshot) in attempts.into_iter().enumerate() {
                reads.set(snapshot);
                let ms = 100 + u64::try_from(n).unwrap() * 500;
                input(device, 0x1D, 0, ms);
                input(device, 0x1D, 0, ms + 1);
                input(device, 0x38, 0, ms + 2);
                send(Event::Deadline, ms + 305);
                input(device, 0x38, up, ms + 306);
                input(device, 0x1D, up, ms + 307);
            }
            let ineligible = |delivery: &Delivery| {
                delivery.intents.iter().all(Option::is_none)
                    && delivery.effects.iter().all(Option::is_none)
            };
            assert!(
                results.borrow().iter().all(ineligible),
                "loss {loss} admitted a dirty or blind attempt"
            );
            reads.set((true, true, 0));
            input(device, 0x1D, 0, 2_200);
            input(device, 0x38, 0, 2_201);
            send(Event::Deadline, 2_501);
            let results = results.borrow();
            assert!(
                ineligible(&results[results.len() - 3]),
                "loss {loss} cannot start the hold before the second raw make"
            );
            assert!(
                matches!(
                    results[results.len() - 2].intents[0],
                    Some(Intent::Warm(GestureId(1)))
                ),
                "loss {loss} must admit the first clean pair of raw makes"
            );
            assert!(matches!(
                results[results.len() - 1].intents[0],
                Some(Intent::Recognized {
                    gesture: GestureId(1),
                    generation: Ok(_),
                    ..
                })
            ));
        }
    }

    #[test]
    fn right_alt_after_synthetic_left_control_never_warms_or_arms_the_legacy_hold() {
        use crate::input::Gesture;
        use crate::input::hold::{Fence, Intent};
        use crate::platform::microphone::CollectionGate;
        let keys = super::Keys::default();
        let mut gesture = Gesture::default();
        let mut fence = Fence::new(CollectionGate::default());
        let paused = std::cell::Cell::new(false);
        for flags in [0, super::RI_KEY_BREAK] {
            for (code, flags) in [(0x1D, flags), (0x38, super::RI_KEY_E0 | flags)] {
                let (event, time) = raw(&keys, 1, code, flags).unwrap();
                let delivery = fence.reduce(&mut gesture, event, time.instant, &paused);
                assert!(!delivery.intents.iter().any(|intent| matches!(
                    intent,
                    Some(Intent::Warm(_) | Intent::Recognized { .. })
                )));
                assert_eq!(gesture.deadline(), None);
            }
        }
    }

    #[test]
    fn an_overdue_deadline_is_reported_once_at_the_first_late_chord_key() {
        let keys = super::Keys::default();
        let start = std::time::Instant::now();
        let at = |millis| start + std::time::Duration::from_millis(millis);
        let chord = Event::Key {
            key: Key::LeftAlt,
            edge: Edge::Down,
        };
        assert!(!keys.overdue(chord, at(400)), "nothing armed");
        keys.deadline.set(Some(at(300)));
        assert!(!keys.overdue(chord, at(350)), "exactly 50 ms late");
        let other = Event::Key {
            key: Key::Other(crate::input::PhysicalKey::new(0x1E).unwrap()),
            edge: Edge::Down,
        };
        assert!(!keys.overdue(other, at(351)), "not a chord key");
        assert!(!keys.overdue(Event::Deadline, at(351)), "not a key");
        assert!(keys.overdue(chord, at(351)), "51 ms late");
        assert!(!keys.overdue(chord, at(900)), "once per deadline");
        keys.deadline.set(Some(at(1_300)));
        assert!(
            keys.overdue(chord, at(1_400)),
            "the next gesture's deadline"
        );
    }

    #[test]
    fn overruns_and_impossible_codes_reset() {
        assert_eq!(key_event(0xFF, 0), Event::Reset);
        assert_eq!(key_event(0x100, 0), Event::Reset);
    }
}
