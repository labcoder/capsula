//! Capsula's presence beyond what GPUI draws: the tray icon and its menu, the dock's hit-test
//! proxy, and the window styles that keep presence windows out of the taskbar and focus. The
//! proxy's alpha mask lives in `hit_mask.rs`.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use async_channel::Sender;
use capsula_protocol::constants::OBJECT_PREFIX;
use gpui_kit::Window;
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_SETVERSION, NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, GWL_EXSTYLE, GWLP_USERDATA, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, HICON, HMENU, HWND_TOPMOST, IDC_ARROW, IDI_APPLICATION, LWA_ALPHA,
    LoadCursorW, LoadIconW, MA_NOACTIVATE, MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, PostMessageW, RegisterClassExW, RegisterWindowMessageW, SPI_GETWORKAREA,
    SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetForegroundWindow, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenuEx, UnregisterClassW, WINDOW_EX_STYLE, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WM_NULL, WNDCLASSEXW,
    WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, SWP_FRAMECHANGED, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
    WS_THICKFRAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::hit_mask::{HitMask, MaskUpdate};
use super::keyboard::{self, Deliver, EscapeHotkey, Keys};

/// The tray's callback message to the proxy window.
const TRAY_MESSAGE: u32 = WM_APP + 1;
/// The only icon this process puts in the tray.
const TRAY_ID: u32 = 1;
/// Identifies Capsula's hover subclass on presence windows.
const HOVER_SUBCLASS_ID: usize = 0xca5;
/// Menu commands: positions are `1..=9`, so 0 stays "nothing picked".
const RETRY_COMMAND: usize = 10;
/// See [`RETRY_COMMAND`].
const QUIT_COMMAND: usize = 11;

/// What the presence surfaces tell the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceEvent {
    /// The pointer entered or left the proxy or a presence window; ask `cursor_position`.
    PointerChanged,
    /// The pointer moved over the proxy after `PresenceNative::watch_pointer`; sent once.
    PointerMoved,
    /// The user picked a dock position in the tray menu, an index into `positions`.
    Position(usize),
    /// The user picked Retry in the tray menu.
    Retry,
    /// A panel close request hides the panel without quitting the shell.
    DismissPanel,
    /// Esc reached Capsula: the global hotkey, a focused panel, or the Stop fallback.
    Escape,
    /// The user picked Quit in the tray menu.
    Quit,
}

/// The localized text of the tray icon and its menu.
#[derive(Clone, Debug)]
pub(crate) struct PresenceLabels {
    /// The tray icon's tooltip; Windows shows at most 127 UTF-16 units.
    pub(crate) tooltip: String,
    /// The title of the submenu that holds `positions`.
    pub(crate) position_menu: String,
    /// The nine dock positions, in the order `PresenceEvent::Position` indexes.
    pub(crate) positions: [String; 9],
    /// The Retry item.
    pub(crate) retry: String,
    /// The Quit item.
    pub(crate) quit: String,
}

/// Why a presence surface couldn't be set up or moved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PresenceError {
    /// GPUI gave no Win32 handle.
    #[error("no Win32 window handle")]
    NoHandle(#[from] HandleError),
    /// The handle wasn't a Win32 one.
    #[error("not a Win32 window")]
    NotWin32,
    /// A Win32 call failed.
    #[error("a Win32 call failed")]
    Os(#[from] windows::core::Error),
    /// The hover subclass couldn't be installed.
    #[error("can't subclass the window")]
    Subclass,
    /// The shell refused the tray icon.
    #[error("the tray refused the icon")]
    Tray,
    /// Raw Input was started twice.
    #[error("the keyboard is already armed")]
    KeyboardArmed,
    /// Session changes can't be watched, so a lock could leave keys held.
    #[error("can't watch session changes")]
    SessionWatch(#[source] windows::core::Error),
    /// Desktop switches can't be watched, so the secure desktop could leave keys held.
    #[error("can't watch desktop switches")]
    DesktopWatch,
    /// Windows kept the current foreground window.
    #[error("Windows refused to activate the window")]
    Activate,
    /// The hit mask was empty, couldn't be allocated, or was smaller than the proxy.
    #[error("the hit mask doesn't fit the proxy")]
    MaskSize,
}

/// Turns pointer moves into one `PointerChanged` per enter and per leave.
#[derive(Debug)]
struct Hover {
    events: Sender<PresenceEvent>,
    inside: Cell<bool>,
}

impl Hover {
    fn new(events: Sender<PresenceEvent>) -> Self {
        Self {
            events,
            inside: Cell::new(false),
        }
    }

    /// Handles `WM_MOUSEMOVE` and `WM_MOUSELEAVE`.
    fn on_pointer(&self, hwnd: HWND, message: u32) {
        let inside = message == WM_MOUSEMOVE;
        if inside == self.inside.get() {
            return;
        }
        if inside {
            let mut track = TRACKMOUSEEVENT {
                cbSize: struct_size::<TRACKMOUSEEVENT>(),
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: `track` is a valid, writable struct for the call.
            if let Err(error) = unsafe { TrackMouseEvent(&raw mut track) } {
                tracing::debug!(%error, "can't watch for the pointer leaving");
                return;
            }
        }
        self.inside.set(inside);
        send(&self.events, PresenceEvent::PointerChanged);
    }
}

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
    /// Set by `watch_pointer`; the next pointer move over the proxy sends `PointerMoved`.
    watch: Cell<bool>,
}

/// The tray icon and the dock's hit-test proxy: an invisible, non-activating per-pixel-alpha
/// window that takes the pointer only over the presence's outline, so the dock window itself
/// can stay click-through. Owned by the UI thread that created it; dropping it removes both.
#[derive(Debug)]
pub(crate) struct PresenceNative {
    instance: HINSTANCE,
    atom: u16,
    hwnd: HWND,
    mask: HitMask,
    tray: bool,
    /// Raw Input and its lifecycle notifications are registered.
    keyboard: bool,
    state: *const ProxyState,
}

impl PresenceNative {
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
            hover: Hover::new(events),
            icon,
            taskbar_created,
            position: Cell::new(1),
            retry_enabled: Cell::new(false),
            watch: Cell::new(false),
        });
        // Every field Drop cleans up starts empty, so a failure below undoes only what was done.
        let mut native = Self {
            instance,
            atom: 0,
            hwnd: HWND::default(),
            mask,
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
    /// up to two `[left, right)` spans, and moves it to `window` when that changed.
    ///
    /// # Errors
    /// Fails when `window` outgrows the mask or Windows refuses the update.
    pub(crate) fn reshape(
        &mut self,
        window: [i32; 4],
        spans: impl Fn(i32) -> [Option<[i32; 2]>; 2],
    ) -> Result<MaskUpdate, PresenceError> {
        self.mask.update(self.hwnd, window, spans)
    }

    /// Shows the proxy topmost and checks `position` in the tray menu. Call after `reshape`.
    ///
    /// # Errors
    /// Fails when Windows refuses the change.
    pub(crate) fn show(&mut self, position: usize) -> Result<(), PresenceError> {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
        // SAFETY: `hwnd` is this thread's live window; only its z-order and visibility change.
        unsafe { SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) }?;
        self.state().position.set(position);
        Ok(())
    }

    /// Sends one `PointerMoved` on the next pointer move over the proxy.
    pub(crate) fn watch_pointer(&self) {
        self.state().watch.set(true);
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
        EscapeHotkey::register(self.hwnd)
    }

    /// Keeps the proxy, which shows over the dock, out of capture like the GPUI surfaces.
    ///
    /// # Errors
    /// Fails when Windows refuses the affinity.
    pub(crate) fn set_capture(&self, capture: Capture) -> Result<(), PresenceError> {
        set_affinity(self.hwnd, capture)
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

/// Whether a GPUI surface paints only, owns hover controls, or takes typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceSurface {
    Overlay,
    Panel,
    /// The only surface Capsula activates, and only when asked to.
    Text,
}

/// Configures a tool window, reporting pointer boundaries on hover panels. Overlay and hover
/// surfaces never activate, and overlays pass clicks through. A text surface activates only
/// through [`activate_presence_window`]. This neither shows nor resizes the window.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses a change.
pub(crate) fn configure_presence_window(
    window: &mut Window,
    surface: PresenceSurface,
    events: Sender<PresenceEvent>,
) -> Result<(), PresenceError> {
    let hwnd = hwnd_of(window)?;
    let flag = |style: WINDOW_EX_STYLE| isize::try_from(style.0).expect("style flags fit in isize");
    let pass_through = flag(WS_EX_LAYERED | WS_EX_TRANSPARENT);
    // SAFETY: `hwnd` is this thread's live window; the slot holds flags, not a pointer.
    let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    style = (style & !flag(WS_EX_APPWINDOW | WS_EX_NOACTIVATE)) | flag(WS_EX_TOOLWINDOW);
    if surface != PresenceSurface::Text {
        style |= flag(WS_EX_NOACTIVATE);
    }
    style = if surface == PresenceSurface::Overlay {
        style | pass_through
    } else {
        style & !pass_through
    };
    // SAFETY: as above; only flags change.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
    // SAFETY: reads ordinary style flags on this thread's live window.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    let frame = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
    let style = (style & !isize::try_from(frame.0).expect("style flags fit in isize"))
        | isize::try_from(WS_POPUP.0).expect("style flags fit in isize");
    // SAFETY: changes only ordinary style flags on this thread's window.
    unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style) };
    // SAFETY: recomputes the frame without moving or activating the window.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }?;
    let border = DWMWA_COLOR_NONE;
    // SAFETY: the border attribute reads one u32 from this live local value.
    if let Err(error) = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border).cast(),
            struct_size::<u32>(),
        )
    } {
        tracing::debug!(%error, "can't remove the DWM border");
    }
    let corner = DWMWCP_DONOTROUND;
    // SAFETY: the corner attribute reads the documented enum from this live local value.
    if let Err(error) = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            struct_size::<DWM_WINDOW_CORNER_PREFERENCE>(),
        )
    } {
        tracing::debug!(%error, "can't remove DWM corner rounding");
    }
    if surface == PresenceSurface::Overlay {
        // A layered window stays invisible until it has attributes; full opacity keeps GPUI's alpha.
        // SAFETY: `hwnd` is live and now layered.
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) }?;
    }

    if surface == PresenceSurface::Text {
        return Ok(());
    }
    // SAFETY: `hwnd` is live; this only asks whether our subclass is installed.
    if unsafe { GetWindowSubclass(hwnd, Some(hover_subclass), HOVER_SUBCLASS_ID, None) }.as_bool() {
        return Ok(());
    }
    let hover = Box::into_raw(Box::new(Hover::new(events)));
    // SAFETY: `hwnd` is live; the subclass owns `hover` and frees it on WM_NCDESTROY.
    let installed = unsafe {
        SetWindowSubclass(
            hwnd,
            Some(hover_subclass),
            HOVER_SUBCLASS_ID,
            hover.expose_provenance(),
        )
    };
    if !installed.as_bool() {
        // SAFETY: from `Box::into_raw` above; the subclass that would own it wasn't installed.
        drop(unsafe { Box::from_raw(hover) });
        return Err(PresenceError::Subclass);
    }
    Ok(())
}

/// Moves a presence window to `[x, y, width, height]` in physical pixels, topmost, and shows
/// it without activating it. GPUI must hear about it: call `window.bounds_changed(cx)` after.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses the move.
pub(crate) fn place_presence_window(
    window: &mut Window,
    bounds: [i32; 4],
) -> Result<(), PresenceError> {
    Ok(place(hwnd_of(window)?, bounds)?)
}

/// Positions and sizes a hidden presence window without showing or activating it.
/// Call `window.bounds_changed(cx)` afterwards so GPUI observes the resize.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses.
pub(crate) fn place_hidden_presence_window(
    window: &mut Window,
    bounds: [i32; 4],
) -> Result<(), PresenceError> {
    place_hidden(hwnd_of(window)?, bounds)?;
    Ok(())
}

fn place_hidden(hwnd: HWND, [x, y, width, height]: [i32; 4]) -> windows::core::Result<()> {
    let flags = SWP_HIDEWINDOW | SWP_NOZORDER | SWP_NOACTIVATE;
    // SAFETY: `hwnd` is live; it is sized and positioned without being shown or activated.
    unsafe { SetWindowPos(hwnd, None, x, y, width, height, flags) }
}

/// Whether screenshots and recordings may show a presence window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capture {
    Excluded,
    Allowed,
}

/// Keeps a presence window out of screenshots and recordings, or lets it back in.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses the affinity.
pub(crate) fn set_capture(window: &Window, capture: Capture) -> Result<(), PresenceError> {
    set_affinity(hwnd_of(window)?, capture)
}

fn set_affinity(hwnd: HWND, capture: Capture) -> Result<(), PresenceError> {
    let affinity = match capture {
        Capture::Excluded => WDA_EXCLUDEFROMCAPTURE,
        Capture::Allowed => WDA_NONE,
    };
    // SAFETY: `hwnd` is this thread's live top-level window.
    unsafe { SetWindowDisplayAffinity(hwnd, affinity) }?;
    Ok(())
}

/// The window that was in front before Capsula activated one of its own, with its process, so
/// a handle Windows reused for another app's window is never brought forward.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Foreground(HWND, u32);

fn process_of(hwnd: HWND) -> u32 {
    let mut process = 0;
    // SAFETY: `process` is writable; a handle that names no window leaves it 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process)) };
    process
}

/// Brings a text surface to the front so it takes typing. Returns the window it replaced,
/// or `None` when this window already had the front. Windows allows it right after the
/// user's own Alt press, which the shortcut includes.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows keeps the current foreground.
pub(crate) fn activate_presence_window(
    window: &Window,
) -> Result<Option<Foreground>, PresenceError> {
    let hwnd = hwnd_of(window)?;
    // SAFETY: reads the foreground handle without taking ownership.
    let previous = unsafe { GetForegroundWindow() };
    // SAFETY: `hwnd` is this thread's live window.
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        return Err(PresenceError::Activate);
    }
    Ok((previous != hwnd && !previous.is_invalid())
        .then(|| Foreground(previous, process_of(previous))))
}

/// Gives the front back to `previous`, only while `window` still has it and `previous` is still
/// the same process's window, so a window the user chose since is never overridden.
pub(crate) fn restore_foreground(window: &Window, previous: Foreground) {
    let Ok(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: reads the foreground handle without taking ownership.
    if unsafe { GetForegroundWindow() } != hwnd {
        return;
    }
    if previous.1 == 0 || process_of(previous.0) != previous.1 {
        return;
    }
    // SAFETY: we hold the foreground, so handing it back is allowed.
    if !unsafe { SetForegroundWindow(previous.0) }.as_bool() {
        tracing::debug!("can't give the foreground back");
    }
}

/// The performance counter, the clock `PresentMon`'s `--qpc_time` reports in.
pub(crate) fn performance_counter() -> i64 {
    let mut counter = 0;
    // SAFETY: `counter` is writable; the call can't fail on Windows XP or later.
    if let Err(error) = unsafe { QueryPerformanceCounter(&raw mut counter) } {
        tracing::debug!(%error, "can't read the performance counter");
    }
    counter
}

/// The primary monitor's work area, `[x, y, width, height]` in physical pixels.
///
/// # Errors
/// Fails when Windows can't report it.
pub(crate) fn work_area() -> Result<[i32; 4], PresenceError> {
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes one RECT into `rect`, which is writable for the call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut rect).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }?;
    Ok([
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ])
}

/// Where the pointer is, `[x, y]` in physical screen pixels.
///
/// # Errors
/// Fails when Windows can't report it, such as while the secure desktop is up.
pub(crate) fn cursor_position() -> Result<[i32; 2], PresenceError> {
    let mut point = POINT::default();
    // SAFETY: `point` is writable for the call.
    unsafe { GetCursorPos(&raw mut point) }?;
    Ok([point.x, point.y])
}

fn place(hwnd: HWND, [x, y, width, height]: [i32; 4]) -> windows::core::Result<()> {
    let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
    // SAFETY: `hwnd` is this thread's live window.
    unsafe { SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, flags) }
}

fn hwnd_of(window: &Window) -> Result<HWND, PresenceError> {
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window)?.as_raw() else {
        return Err(PresenceError::NotWin32);
    };
    Ok(HWND(std::ptr::with_exposed_provenance_mut(
        handle.hwnd.get().cast_unsigned(),
    )))
}

/// Adds the tray icon and opts into version 4 callbacks, which report keyboard use too.
fn add_tray(hwnd: HWND, state: &ProxyState) -> Result<(), PresenceError> {
    let mut data = NOTIFYICONDATAW {
        cbSize: struct_size::<NOTIFYICONDATAW>(),
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: state.icon,
        ..Default::default()
    };
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let tip = state
        .labels
        .tooltip
        .encode_utf16()
        .take(data.szTip.len() - 1);
    for (slot, unit) in data.szTip.iter_mut().zip(tip) {
        *slot = unit;
    }
    // SAFETY: `data` is valid for the call.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
        return Err(PresenceError::Tray);
    }
    // SAFETY: as above, for the icon just added.
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &raw const data) }.as_bool() {
        // SAFETY: removes the icon this function added before its version request failed.
        if !unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) }.as_bool() {
            tracing::warn!("can't remove the partially initialized tray icon");
        }
        return Err(PresenceError::Tray);
    }
    Ok(())
}

/// Owns a popup menu and destroys it, with any submenu attached to it.
#[derive(Debug)]
struct Menu(HMENU);

impl Menu {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain call; the guard owns the result.
        Ok(Self(unsafe { CreatePopupMenu() }?))
    }

    fn append(&self, flags: MENU_ITEM_FLAGS, id: usize, label: &str) -> windows::core::Result<()> {
        // SAFETY: the menu is live and the label outlives the call.
        unsafe { AppendMenuW(self.0, flags, id, &HSTRING::from(label)) }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        // SAFETY: the menu is live and owned only by this guard.
        if let Err(error) = unsafe { DestroyMenu(self.0) } {
            tracing::debug!(%error, "can't destroy a menu");
        }
    }
}

/// Shows the tray menu at `[x, y]` and returns what the user picked, if anything.
// shortcut: the tray owns dock positioning until slice 10 adds Settings → General.
fn pick_from_tray_menu(
    hwnd: HWND,
    state: &ProxyState,
    [x, y]: [i32; 2],
) -> windows::core::Result<Option<PresenceEvent>> {
    let labels = &state.labels;
    let menu = Menu::new()?;
    let positions = Menu::new()?;
    for (index, label) in labels.positions.iter().enumerate() {
        positions.append(MF_STRING, index + 1, label)?;
    }
    let selected = u32::try_from(state.position.get() + 1).unwrap_or(2);
    // SAFETY: the submenu owns command IDs 1 through 9, and flag 0 selects by command ID.
    unsafe { CheckMenuRadioItem(positions.0, 1, 9, selected, 0) }?;
    menu.append(
        MF_POPUP,
        positions.0.0.expose_provenance(),
        &labels.position_menu,
    )?;
    // The parent menu destroys the submenu from here on.
    std::mem::forget(positions);
    menu.append(MF_SEPARATOR, 0, "")?;
    let retry_flags = if state.retry_enabled.get() {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    menu.append(retry_flags, RETRY_COMMAND, &labels.retry)?;
    menu.append(MF_STRING, QUIT_COMMAND, &labels.quit)?;

    // SAFETY: reads the foreground handle without transferring ownership.
    let previous = unsafe { GetForegroundWindow() };
    // Without the foreground, the menu wouldn't close when the user clicks elsewhere.
    // SAFETY: `hwnd` is this thread's live window.
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        tracing::debug!("the tray menu didn't get the foreground");
    }
    let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON;
    // SAFETY: the menu and `hwnd` are live; it returns the picked command, 0 for none.
    let command = unsafe { TrackPopupMenuEx(menu.0, flags.0, x, y, hwnd, None) }.0;
    // Lets the menu close cleanly on the next click, as Win32 tray menus need.
    // SAFETY: `hwnd` is live; WM_NULL does nothing.
    if let Err(error) = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) } {
        tracing::debug!(%error, "can't nudge the tray menu closed");
    }
    // A click elsewhere already chose a new foreground; never override that choice.
    // SAFETY: reads the current foreground handle.
    let current = unsafe { GetForegroundWindow() };
    if current == hwnd && previous != hwnd && !previous.is_invalid() {
        // SAFETY: this only requests restoration of the foreground we temporarily replaced.
        if !unsafe { SetForegroundWindow(previous) }.as_bool() {
            tracing::debug!("can't restore focus after the tray menu");
        }
    }
    Ok(match usize::try_from(command).unwrap_or(0) {
        position @ 1..=9 => Some(PresenceEvent::Position(position - 1)),
        RETRY_COMMAND => Some(PresenceEvent::Retry),
        QUIT_COMMAND => Some(PresenceEvent::Quit),
        _ => None,
    })
}

/// Queues an event for the UI thread; the queue is unbounded, so this never blocks.
fn send(events: &Sender<PresenceEvent>, event: PresenceEvent) {
    if events.try_send(event).is_err() {
        tracing::debug!("the UI stopped listening to presence events");
    }
}

/// The signed low and high words Win32 packs into one parameter.
fn signed_words(value: usize) -> [i32; 2] {
    let word = |shift: u32| {
        u16::try_from((value >> shift) & 0xFFFF)
            .unwrap_or(0)
            .cast_signed()
    };
    [i32::from(word(0)), i32::from(word(16))]
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "Win32 structs are a few hundred bytes"
)]
pub(super) const fn struct_size<T>() -> u32 {
    size_of::<T>() as u32
}

/// The proxy's window procedure: tray callbacks and pointer moves.
unsafe extern "system" fn proxy_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
        if state.keys.on_message(hwnd, message, wparam, lparam) {
            return LRESULT(0);
        }
        if EscapeHotkey::fired(message, wparam) {
            send(&state.hover.events, PresenceEvent::Escape);
            return LRESULT(0);
        }
        match message {
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
                if message == WM_MOUSEMOVE && state.watch.replace(false) {
                    send(&state.hover.events, PresenceEvent::PointerMoved);
                }
                state.hover.on_pointer(hwnd, message);
            }
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

/// Reports pointer enter and leave on a presence window and refuses mouse activation, which
/// GPUI grants.
unsafe extern "system" fn hover_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let hover = std::ptr::with_exposed_provenance::<Hover>(data);
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE.try_into().unwrap_or(isize::MAX)),
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            // SAFETY: `data` is the Hover boxed in `configure_presence_window`, freed
            // only on WM_NCDESTROY below.
            if let Some(hover) = unsafe { hover.as_ref() } {
                hover.on_pointer(hwnd, message);
            }
        }
        WM_NCDESTROY => {
            // SAFETY: on the window's thread, during destruction, for this subclass. A failed
            // removal leaves a subclass on a window that is going away.
            let _removed = unsafe { RemoveWindowSubclass(hwnd, Some(hover_subclass), id) };
            // SAFETY: from `Box::into_raw`; this is the last message the subclass sees.
            drop(unsafe { Box::from_raw(hover.cast_mut()) });
        }
        _ => {}
    }
    // SAFETY: forwards the unmodified message down the chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::{place_hidden, proxy_proc, signed_words};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetForegroundWindow, GetWindowRect, IsWindowVisible,
        MA_NOACTIVATE, WM_MOUSEACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the test owns this live window on its creating thread.
            unsafe { DestroyWindow(self.0) }.expect("the test window remains owned by its thread");
        }
    }

    #[test]
    fn hidden_panel_is_sized_without_becoming_visible_or_taking_focus() {
        // SAFETY: reads the foreground without changing it.
        let foreground = unsafe { GetForegroundWindow() };
        // SAFETY: STATIC is a built-in class; no borrowed data or parent, and no show style.
        let window = TestWindow(
            unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    PCWSTR::null(),
                    WS_POPUP,
                    0,
                    0,
                    1600,
                    900,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap(),
        );
        place_hidden(window.0, [80, 90, 300, 140]).unwrap();
        let mut rect = RECT::default();
        // SAFETY: the window is live and rect is writable.
        unsafe { GetWindowRect(window.0, &raw mut rect) }.unwrap();
        assert_eq!(
            [
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top
            ],
            [80, 90, 300, 140]
        );
        // SAFETY: reads visibility of the live test window.
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        // SAFETY: reads the foreground without changing it.
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    }

    #[test]
    fn resting_proxy_refuses_mouse_activation_without_swallowing_the_click() {
        // SAFETY: no pointer-valued message data; Win32 validates the empty HWND if reached.
        let response =
            unsafe { proxy_proc(HWND::default(), WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)) };
        assert_eq!(response, LRESULT(isize::try_from(MA_NOACTIVATE).unwrap()));
    }

    #[test]
    fn packed_pointer_coordinates_preserve_negative_monitor_origins() {
        assert_eq!(signed_words(0xFF9C_FF38), [-200, -100]);
        assert_eq!(signed_words(0x7FFF_8000), [-32768, 32767]);
        assert_eq!(signed_words(0x05A0_0A00), [2560, 1440]);
    }
}
