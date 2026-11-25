use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use async_channel::Sender;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetCursorPos, GetWindowLongPtrW, HWND_TOPMOST, MA_NOACTIVATE,
    SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowLongPtrW,
    SetWindowPos, SystemParametersInfoW, WINDOW_EX_STYLE, WM_CANCELMODE, WM_CAPTURECHANGED,
    WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NCDESTROY, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WS_CAPTION, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    XBUTTON1, XBUTTON2,
};

use super::super::screen_watch;
use super::{
    PresenceError, PresenceEvent, PresenceSurface, performance_counter, send, struct_size,
};
use crate::platform::input::{self, Button};

/// Identifies Kumochi's hover subclass on presence windows.
const HOVER_SUBCLASS_ID: usize = 0xca5;

/// Turns pointer moves into one `PointerChanged` per enter and per leave.
#[derive(Debug)]
pub(super) struct Hover {
    pub(super) events: Sender<PresenceEvent>,
    inside: Cell<bool>,
    pub(super) destroyed: Rc<Cell<bool>>,
    pointer: bool,
}

impl Hover {
    pub(super) fn new(events: Sender<PresenceEvent>) -> Self {
        Self {
            events,
            inside: Cell::new(false),
            destroyed: Rc::new(Cell::new(false)),
            pointer: true,
        }
    }

    /// Handles `WM_MOUSEMOVE` and `WM_MOUSELEAVE`.
    pub(super) fn on_pointer(&self, hwnd: HWND, message: u32) {
        let inside = message == WM_MOUSEMOVE;
        if inside == self.inside.get() {
            return;
        }
        let entered_qpc = inside.then(performance_counter);
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
        send(&self.events, PresenceEvent::PointerChanged { entered_qpc });
    }
}

/// Configures a tool window, reporting pointer boundaries on status panels. Overlay and status
/// surfaces never activate, and overlays pass clicks through. A text surface activates only
/// through [`activate_presence_window`]. This neither shows nor resizes the window.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses a change.
pub(crate) fn configure_presence_window(
    window: &mut impl HasWindowHandle,
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
    if surface != PresenceSurface::Overlay {
        style &= !pass_through;
    }

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
    subclass_hover(hwnd, events, surface)
}

/// Installs `hover_subclass` on `hwnd` once, sending to `events`.
pub(super) fn subclass_hover(
    hwnd: HWND,
    events: Sender<PresenceEvent>,
    surface: PresenceSurface,
) -> Result<(), PresenceError> {
    // SAFETY: `hwnd` is live; this only asks whether our subclass is installed.
    if unsafe { GetWindowSubclass(hwnd, Some(hover_subclass), HOVER_SUBCLASS_ID, None) }.as_bool() {
        return Ok(());
    }
    let mut hover = Hover::new(events);
    hover.pointer = surface != PresenceSurface::Text;
    let hover = Box::into_raw(Box::new(hover));
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
    window: &mut impl HasWindowHandle,
    bounds: [i32; 4],
) -> Result<(), PresenceError> {
    #[cfg(test)]
    if HasWindowHandle::window_handle(window).is_err() {
        return Ok(());
    }
    Ok(place(hwnd_of(window)?, bounds)?)
}

/// Hides a presence window in place, without activating another window.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows refuses.
pub(crate) fn hide_presence_window(window: &mut impl HasWindowHandle) -> Result<(), PresenceError> {
    #[cfg(test)]
    if HasWindowHandle::window_handle(window).is_err() {
        return Ok(());
    }
    Ok(hide(hwnd_of(window)?)?)
}

pub(super) fn hide(hwnd: HWND) -> windows::core::Result<()> {
    let flags = SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
    // SAFETY: `hwnd` is live; it is hidden without being moved, sized or activated.
    unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, flags) }
}

pub(super) fn window_life(hwnd: HWND) -> Result<Rc<Cell<bool>>, PresenceError> {
    let mut data = 0;
    // SAFETY: the current GPUI Window owns hwnd; only this module installs this subclass/id.
    if !unsafe {
        GetWindowSubclass(
            hwnd,
            Some(hover_subclass),
            HOVER_SUBCLASS_ID,
            Some(&raw mut data),
        )
    }
    .as_bool()
    {
        return Err(PresenceError::Subclass);
    }
    let hover = std::ptr::with_exposed_provenance::<Hover>(data);
    // SAFETY: the installed subclass owns this Hover until this UI thread dispatches WM_NCDESTROY.
    let hover = unsafe { hover.as_ref() }.ok_or(PresenceError::Subclass)?;
    Ok(Rc::clone(&hover.destroyed))
}

/// The primary monitor's work area, `[x, y, width, height]` in physical pixels.
///
/// # Errors
/// Fails when Windows can't report it.
pub(crate) fn work_area() -> Result<[i32; 4], PresenceError> {
    #[cfg(test)]
    if let Some(area) = crate::platform::work_area_fixture::get() {
        return Ok(area);
    }
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
    #[cfg(test)]
    if let Some(at) = crate::platform::cursor_fixture::get() {
        return at.map_err(|()| PresenceError::NotWin32);
    }
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

pub(in crate::platform::windows) fn hwnd_of(
    window: &impl HasWindowHandle,
) -> Result<HWND, PresenceError> {
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window)?.as_raw() else {
        return Err(PresenceError::NotWin32);
    };
    Ok(HWND(std::ptr::with_exposed_provenance_mut(
        handle.hwnd.get().cast_unsigned(),
    )))
}

/// The button a press or release message is for.
fn button(message: u32, wparam: WPARAM) -> Option<Button> {
    match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => Some(Button::Left),
        WM_RBUTTONDOWN | WM_RBUTTONUP => Some(Button::Right),
        WM_MBUTTONDOWN | WM_MBUTTONUP => Some(Button::Middle),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match u16::try_from((wparam.0 >> 16) & 0xFFFF) {
            Ok(XBUTTON1) => Some(Button::Back),
            Ok(XBUTTON2) => Some(Button::Forward),
            _ => None,
        },
        _ => None,
    }
}

/// Forwards `released`'s release in its input scope. `events` is owned and taken before the
/// forward, and nothing here can reach the subclass's `Hover`: the forward may destroy the
/// window, which frees it.
fn release(
    released: Button,
    events: Option<Sender<PresenceEvent>>,
    forward: impl FnOnce() -> LRESULT,
) -> LRESULT {
    let (result, due) = input::input(|input| input.release(released, forward));
    if due && let Some(events) = events {
        send(&events, PresenceEvent::Reconcile);
    }
    result
}

/// Reports pointer enter and leave on a presence window and refuses mouse activation, which
/// GPUI grants. Stamps button presses and capture losses in the input sequence, and forwards
/// each release inside its scope, so a pull can tell its own release from a loss.
unsafe extern "system" fn hover_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let hover = std::ptr::with_exposed_provenance::<Hover>(data);
    // SAFETY: forwards the unmodified message down the chain.
    let forward = || unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    // SAFETY: `data` is the Hover boxed in `configure_presence_window`, freed only on
    // WM_NCDESTROY. Called only before `forward`, which can destroy the window.
    let events = || unsafe { hover.as_ref() }.map(|hover| &hover.events);
    // SAFETY: the subclass owns Hover until WM_NCDESTROY; no native call has run yet.
    let pointer = unsafe { hover.as_ref() }.is_some_and(|hover| hover.pointer);
    if !pointer && message != WM_NCDESTROY {
        return forward();
    }
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE.try_into().unwrap_or(isize::MAX)),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            return input::input(|input| input.press(forward));
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
            if let Some(released) = button(message, wparam) {
                return release(released, events().cloned(), forward);
            }
        }
        WM_CAPTURECHANGED => {
            let (seq, released) = input::input(input::Input::capture_changed);
            if let Some(events) = events() {
                send(events, PresenceEvent::CaptureLost { seq, released });
            }
        }
        WM_CANCELMODE => {
            let seq = input::input(input::Input::cancelled);
            if let Some(events) = events() {
                send(
                    events,
                    PresenceEvent::CaptureLost {
                        seq,
                        released: None,
                    },
                );
            }
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            // SAFETY: `data` is the Hover boxed in `configure_presence_window`, freed
            // only on WM_NCDESTROY below.
            if let Some(hover) = unsafe { hover.as_ref() } {
                hover.on_pointer(hwnd, message);
            }
        }
        WM_DPICHANGED => screen_watch::changed(),
        WM_NCDESTROY => {
            // SAFETY: the subclass owns Hover until this final destruction message.
            if let Some(hover) = unsafe { hover.as_ref() } {
                hover.destroyed.set(true);
            }
            // SAFETY: on the window's thread, during destruction, for this subclass. A failed
            // removal leaves a subclass on a window that is going away.
            let _removed = unsafe { RemoveWindowSubclass(hwnd, Some(hover_subclass), id) };
            // SAFETY: from `Box::into_raw`; this is the last message the subclass sees.
            drop(unsafe { Box::from_raw(hover.cast_mut()) });
        }
        _ => {}
    }
    forward()
}
