//! Win32 calls the shell needs that GPUI doesn't expose.

use gpui_kit::{App, Window};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, WM_MOUSEACTIVATE, WM_NCDESTROY,
    WS_EX_NOACTIVATE,
};

/// Identifies Capsula's subclass on a window.
const SUBCLASS_ID: usize = 0xca5;

/// Why a window couldn't be shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ShowError {
    /// GPUI gave no Win32 handle.
    #[error("no Win32 window handle")]
    NoHandle(#[from] HandleError),
    /// The handle wasn't a Win32 one.
    #[error("not a Win32 window")]
    NotWin32,
    /// `SetWindowPos` failed.
    #[error("showing the window failed")]
    Show(#[from] windows::core::Error),
    /// The subclass that refuses mouse activation couldn't be installed.
    #[error("can't subclass the window")]
    Subclass,
}

/// Shows a window GPUI created hidden, where GPUI put it, without activating it, and keeps
/// clicks and hover from activating it. Capsula never takes focus from the app in front.
pub(crate) fn show_without_activating(window: &mut Window, cx: &mut App) -> Result<(), ShowError> {
    let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
        return Err(ShowError::NotWin32);
    };
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(
        handle.hwnd.get().cast_unsigned(),
    ));
    // SAFETY: `hwnd` is this thread's live window; the extended-style slot holds flags, not a pointer.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let no_activate = isize::try_from(WS_EX_NOACTIVATE.0).expect("the flag fits in isize");
    // SAFETY: as above; only a flag is added.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | no_activate) };
    // SAFETY: `hwnd` is live; the subclass carries no data and removes itself on WM_NCDESTROY.
    if !unsafe { SetWindowSubclass(hwnd, Some(refuse_activation), SUBCLASS_ID, 0) }.as_bool() {
        return Err(ShowError::Subclass);
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW;
    // SAFETY: `hwnd` is live; with these flags only visibility changes.
    unsafe { SetWindowPos(hwnd, None, 0, 0, 0, 0, flags) }?;
    // GPUI drops the resize it gets during this synchronous call, so it must hear about it (P1).
    window.bounds_changed(cx);
    Ok(())
}

/// Answers `WM_MOUSEACTIVATE` with "don't activate". GPUI answers "activate", which defeats
/// `WS_EX_NOACTIVATE` when Windows' activate-on-hover setting is on.
unsafe extern "system" fn refuse_activation(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE.try_into().unwrap_or(isize::MAX)),
        WM_NCDESTROY => {
            // SAFETY: called on the window's own thread during destruction, for this subclass.
            // A failed removal only leaves a no-op subclass on a window being destroyed.
            let _removed = unsafe { RemoveWindowSubclass(hwnd, Some(refuse_activation), id) };
            // SAFETY: forwards the unmodified message down the chain.
            unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
        }
        // SAFETY: forwards the unmodified message down the chain.
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}
