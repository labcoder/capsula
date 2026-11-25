mod tray;

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use async_channel::Sender;
use kumochi_protocol::constants::OBJECT_PREFIX;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Shell::{
    NIM_DELETE, NIN_SELECT, NINF_KEY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HICON,
    HWND_TOPMOST, IDC_ARROW, IDI_APPLICATION, LoadCursorW, LoadIconW, MA_NOACTIVATE,
    RegisterClassExW, RegisterWindowMessageW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, UnregisterClassW, WDA_EXCLUDEFROMCAPTURE,
    WINDOW_DISPLAY_AFFINITY, WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_LBUTTONDOWN,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::super::hit_mask::{HitMask, MaskUpdate};
use super::super::keyboard::{self, EscapeHotkey, Keys};
use super::super::observer::{self, Observe, ProxyHandle};
use super::super::{screen_watch, taskbar};
use super::capture::{Affinity, snapshot_affinity};
use super::window::{Hover, hide};
use super::{
    PresenceError, PresenceEvent, PresenceLabels, performance_counter, send, signed_words,
    struct_size,
};
use crate::platform::Deliver;
use tray::{TRAY_ID, TRAY_MESSAGE, add_tray, pick_from_tray_menu};

/// What the proxy's window procedure reads. Only the UI thread touches it.
struct ProxyState {
    labels: PresenceLabels,
    keys: Keys,
    hover: Hover,
    icon: HICON,
    /// Explorer broadcasts this after it restarts, when every tray icon must be added again.
    taskbar_created: u32,
    position: Cell<usize>,
    retry_enabled: Cell<bool>,
    /// Set only by `--gate-observer`.
    observe: OnceCell<Observe>,
}

/// The tray icon and the dock's hit-test proxy: an invisible, non-activating per-pixel-alpha
/// window that takes the pointer only over the presence's hit band, so the dock window itself
/// can stay click-through. Owned by the UI thread that created it; dropping it removes both.
#[derive(Debug)]
pub(crate) struct PresenceNative {
    taskbar: taskbar::TaskbarButton,
    instance: HINSTANCE,
    atom: u16,
    hwnd: HWND,
    #[cfg(not(test))]
    mask: HitMask,
    #[cfg(test)]
    mask: Option<HitMask>,
    tray: bool,
    /// Raw Input and its lifecycle notifications are registered.
    keyboard: bool,
    state: *const ProxyState,
}

impl PresenceNative {
    pub(crate) fn set_taskbar_button(
        &mut self,
        choice: crate::settings::TaskbarChoice,
    ) -> Result<(), taskbar::TaskbarError> {
        self.taskbar.set(choice)
    }

    /// Adds the tray icon and creates the proxy, hidden until the first `show`, with a mask
    /// for proxies up to `mask_size` physical pixels.
    ///
    /// # Errors
    /// Fails when Windows refuses the mask, the window class, the window or the tray icon.
    pub(crate) fn new(
        labels: PresenceLabels,
        events: Sender<PresenceEvent>,
        mask_size: [i32; 2],
    ) -> Result<Self, PresenceError> {
        let mask = HitMask::new(mask_size)?;
        // SAFETY: plain call for this executable's own module.
        let instance: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }?.into();
        // SAFETY: loads a shared system icon, which is never destroyed.
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }?;
        // SAFETY: loads a shared system cursor, which is never destroyed.
        let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;
        // SAFETY: plain call with a NUL-terminated literal; 0 only means no re-add.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let state = Rc::new(ProxyState {
            labels,
            keys: Keys::default(),
            hover: Hover::new(events.clone()),
            icon,
            taskbar_created,
            position: Cell::new(1),
            retry_enabled: Cell::new(false),
            observe: OnceCell::new(),
        });
        // Every field Drop cleans up starts empty, so a failure below undoes only what was done.
        let mut native = Self {
            taskbar: taskbar::TaskbarButton::new(events),
            instance,
            atom: 0,
            hwnd: HWND::default(),
            #[cfg(not(test))]
            mask,
            #[cfg(test)]
            mask: Some(mask),
            tray: false,
            keyboard: false,
            state: Rc::into_raw(state),
        };

        let class_name = HSTRING::from(format!("{OBJECT_PREFIX}-presence"));
        let class = WNDCLASSEXW {
            cbSize: struct_size::<WNDCLASSEXW>(),
            lpfnWndProc: Some(proxy_proc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: `class` and the name it points to outlive the call.
        native.atom = unsafe { RegisterClassExW(&raw const class) };
        if native.atom == 0 {
            return Err(windows::core::Error::from_thread().into());
        }

        let style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
        // SAFETY: the class is registered for `instance`; no parent, menu or creation data.
        native.hwnd = unsafe {
            CreateWindowExW(
                style,
                native.class(),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }?;
        // SAFETY: `hwnd` is live; the slot holds the state, which Drop detaches before freeing.
        unsafe {
            SetWindowLongPtrW(
                native.hwnd,
                GWLP_USERDATA,
                native.state.expose_provenance().cast_signed(),
            )
        };

        add_tray(native.hwnd, native.state())?;
        native.tray = true;
        Ok(native)
    }

    /// Gives the proxy its owned pixels, where `spans` names a screen row's owned columns as
    /// up to [`ROW_SPANS`](crate::platform::ROW_SPANS) `[left, right)` spans, and moves it to
    /// `window` when that changed.
    ///
    /// # Errors
    /// Fails when `window` outgrows the mask or Windows refuses the update.
    pub(crate) fn reshape(
        &mut self,
        window: [i32; 4],
        spans: impl Fn(i32) -> crate::platform::RowSpans,
    ) -> Result<MaskUpdate, PresenceError> {
        #[cfg(not(test))]
        let mask = &mut self.mask;
        #[cfg(test)]
        let mask = self.mask.as_mut().ok_or(PresenceError::NotWin32)?;
        mask.update(self.hwnd, window, spans)
    }

    /// Shows the proxy topmost and checks `position` in the tray menu. Call after `reshape`.
    ///
    /// # Errors
    /// Fails when Windows refuses the change.
    pub(crate) fn show(&mut self, position: usize) -> Result<(), PresenceError> {
        #[cfg(test)]
        if self.mask.is_none() {
            return Err(PresenceError::NotWin32);
        }
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
        // SAFETY: `hwnd` is this thread's live window; only its z-order and visibility change.
        unsafe { SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) }?;
        self.state().position.set(position);
        Ok(())
    }

    pub(crate) fn input_proxy(&self) -> ProxyHandle {
        ProxyHandle(self.hwnd)
    }

    /// Hides the proxy, so it takes no pointer input; Raw Input and the tray keep working.
    ///
    /// # Errors
    /// Fails when Windows refuses the change.
    pub(crate) fn hide(&mut self) -> Result<(), PresenceError> {
        #[cfg(test)]
        if self.mask.is_none() {
            return Err(PresenceError::NotWin32);
        }
        Ok(hide(self.hwnd)?)
    }

    /// Sends every left Ctrl, left Alt and other key, from any app, to `deliver` on this
    /// thread, and arms the one-shot deadline it returns. Call once, before the brain connects.
    ///
    /// # Errors
    /// Fails when called twice or Windows refuses Raw Input or a reset watcher.
    pub(crate) fn arm_keyboard(&mut self, deliver: Deliver) -> Result<(), PresenceError> {
        keyboard::arm(self.hwnd, &self.state().keys, deliver)?;
        self.keyboard = true;
        Ok(())
    }

    /// Holds Esc system-wide until the returned value drops.
    ///
    /// # Errors
    /// Fails when another app, or this one, already holds Esc.
    pub(crate) fn register_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        #[cfg(test)]
        if self.hwnd.is_invalid() {
            return Err(PresenceError::NotWin32);
        }
        EscapeHotkey::register(self.hwnd, &self.state().keys)
    }

    /// Holds Esc with Ctrl and Alt down system-wide until the returned value drops.
    ///
    /// # Errors
    /// Fails when another app, or this one, already holds that chord.
    pub(crate) fn register_talk_escape(&self) -> Result<EscapeHotkey, PresenceError> {
        EscapeHotkey::register_with_talk_keys(self.hwnd, &self.state().keys)
    }

    pub(crate) fn escape_trusted(&self) -> bool {
        self.state().keys.escape_trusted()
    }

    /// Keeps the proxy, which shows over the dock, out of capture like the GPUI surfaces.
    ///
    /// # Errors
    /// Fails when Windows refuses the affinity.
    pub(crate) fn affinity(&self) -> Result<Affinity, PresenceError> {
        snapshot_affinity(self.hwnd, Rc::clone(&self.state().hover.destroyed))
    }

    pub(crate) fn exclude(&self, prior: &Affinity) -> Result<(), PresenceError> {
        prior.apply(
            self.hwnd,
            &self.state().hover.destroyed,
            WDA_EXCLUDEFROMCAPTURE,
        )
    }

    pub(crate) fn restore_affinity(&self, prior: &Affinity) -> Result<(), PresenceError> {
        if prior.gone() {
            return Ok(());
        }
        prior.apply(
            self.hwnd,
            &self.state().hover.destroyed,
            WINDOW_DISPLAY_AFFINITY(prior.value),
        )
    }

    /// `--gate-observer` only: stamps frame completions, harness repaint requests and tagged
    /// clicks on the proxy and hands them to `observe`. Returns where frame ids go.
    pub(crate) fn observe(&self, observe: Observe) -> ProxyHandle {
        if self.state().observe.set(observe).is_err() {
            tracing::warn!("the proxy is already observed");
        }
        ProxyHandle(self.hwnd)
    }

    pub(crate) fn enable_retry(&self) {
        self.state().retry_enabled.set(true);
    }

    pub(crate) fn disable_retry(&self) {
        self.state().retry_enabled.set(false);
    }

    fn state(&self) -> &ProxyState {
        // SAFETY: this owner retains the original Rc strong reference until Drop.
        unsafe { &*self.state }
    }

    fn class(&self) -> PCWSTR {
        PCWSTR(std::ptr::without_provenance(usize::from(self.atom)))
    }
}

#[cfg(test)]
impl PresenceNative {
    pub(crate) fn refuse_talk_escape_for_test(&self) {
        self.state().keys.refuse_talk_escape.set(true);
    }

    /// The owner's state with no window, tray icon, keyboard hook, hotkey or taskbar button:
    /// optionally a 1x1 memory DC for the mask. Nothing that needs the proxy window may be
    /// called on it.
    ///
    /// # Errors
    /// Fails when Windows refuses the mask's memory DC.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "one fixture choice controls whether to allocate a native mask"
    )]
    pub(crate) fn detached(
        labels: PresenceLabels,
        events: Sender<PresenceEvent>,
        with_mask: bool,
    ) -> Result<Self, PresenceError> {
        let mask = with_mask.then(|| HitMask::new([1, 1])).transpose()?;
        let state = Rc::new(ProxyState {
            labels,
            keys: Keys::default(),
            hover: Hover::new(events.clone()),
            icon: HICON::default(),
            taskbar_created: 0,
            position: Cell::new(1),
            retry_enabled: Cell::new(false),
            observe: OnceCell::new(),
        });
        Ok(Self {
            taskbar: taskbar::TaskbarButton::new(events),
            instance: HINSTANCE::default(),
            atom: 0,
            hwnd: HWND::default(),
            mask,
            tray: false,
            keyboard: false,
            state: Rc::into_raw(state),
        })
    }
}

impl Drop for PresenceNative {
    fn drop(&mut self) {
        if self.keyboard {
            keyboard::disarm(self.hwnd, &self.state().keys);
        }
        if self.tray {
            let data = NOTIFYICONDATAW {
                cbSize: struct_size::<NOTIFYICONDATAW>(),
                hWnd: self.hwnd,
                uID: TRAY_ID,
                ..Default::default()
            };
            // SAFETY: `data` is valid for the call and names our own icon.
            if !unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) }.as_bool() {
                tracing::debug!("the tray icon was already gone");
            }
        }
        if !self.hwnd.is_invalid() {
            // Detached first, so the state is never read after the free below.
            // SAFETY: `hwnd` is this thread's live window; the slot holds our pointer.
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0) };
            // SAFETY: `hwnd` was created by this thread and nothing else destroys it.
            if let Err(error) = unsafe { DestroyWindow(self.hwnd) } {
                tracing::warn!(%error, "can't destroy the dock proxy");
            } else {
                self.state().hover.destroyed.set(true);
            }
        }
        // SAFETY: consumes this owner's original Rc::into_raw reference after detaching the window.
        drop(unsafe { Rc::from_raw(self.state) });
        if self.atom != 0 {
            // SAFETY: the class is ours and its only window is gone.
            if let Err(error) = unsafe { UnregisterClassW(self.class(), Some(self.instance)) } {
                tracing::debug!(%error, "can't unregister the proxy class");
            }
        }
    }
}

/// The proxy's window procedure: tray callbacks and pointer moves.
pub(super) unsafe extern "system" fn proxy_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let click_qpc = (message == WM_LBUTTONDOWN).then(performance_counter);
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE.try_into().unwrap_or(isize::MAX));
    }
    // SAFETY: on the window's thread; the slot holds 0 or the state `PresenceNative` owns.
    let slot = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let state = std::ptr::with_exposed_provenance::<ProxyState>(slot.cast_unsigned());
    if !state.is_null() {
        // A tray menu pumps messages; keep its labels alive even if Quit drops the owner.
        // SAFETY: the UI-thread slot names the Rc allocation held by PresenceNative until detached.
        unsafe { Rc::increment_strong_count(state) };
        // SAFETY: consumes the strong reference added immediately above, not the owner's reference.
        let state = unsafe { Rc::from_raw(state) };
        if message == WM_NCDESTROY {
            state.hover.destroyed.set(true);
        }
        if state.keys.on_message(hwnd, message, wparam, lparam) {
            return LRESULT(0);
        }
        if observer::on_message(state.observe.get(), message, wparam, click_qpc) {
            return LRESULT(0);
        }
        if state.keys.plain_escape(message, wparam) {
            send(&state.hover.events, PresenceEvent::Escape);
            return LRESULT(0);
        }
        match message {
            WM_LBUTTONDOWN => {
                if let Some(qpc) = click_qpc {
                    send(&state.hover.events, PresenceEvent::DockPressed { qpc });
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                send(&state.hover.events, PresenceEvent::Quit);
                return LRESULT(0);
            }
            TRAY_MESSAGE => {
                let event = u32::try_from(lparam.0 & 0xFFFF).unwrap_or(0);
                if [NIN_SELECT, NIN_SELECT | NINF_KEY, WM_CONTEXTMENU].contains(&event) {
                    match pick_from_tray_menu(hwnd, &state, signed_words(wparam.0)) {
                        Ok(Some(event)) => send(&state.hover.events, event),
                        Ok(None) => {}
                        Err(error) => tracing::warn!(%error, "the tray menu failed"),
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE | WM_MOUSELEAVE => {
                state.hover.on_pointer(hwnd, message);
            }
            WM_DISPLAYCHANGE => screen_watch::changed(),
            _ if message == state.taskbar_created && message != 0 => {
                if let Err(error) = add_tray(hwnd, &state) {
                    tracing::warn!(%error, "can't add the tray icon back");
                }
            }
            _ => {}
        }
    }
    // SAFETY: forwards the unmodified message.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
