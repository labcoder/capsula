use kumochi_protocol::constants::FOCUS_INPUT_MARKER;
use raw_window_handle::HasWindowHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
};

use super::{PresenceError, hwnd_of};

/// The window that was in front before Kumochi activated one of its own, with its process, so
/// a handle Windows reused for another app's window is never brought forward.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Foreground(HWND, u32);

pub(in crate::platform::windows) fn process_of(hwnd: HWND) -> u32 {
    let mut process = 0;
    // SAFETY: `process` is writable; a handle that names no window leaves it 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process)) };
    process
}

/// The one input Kumochi sends: a mouse event with no motion, buttons, wheel or keys, tagged
/// so the gate harness can tell it from the user's input.
pub(super) fn focus_input() -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwExtraInfo: FOCUS_INPUT_MARKER,
                ..MOUSEINPUT::default()
            },
        },
    }
}

/// Brings a text surface to the front so it takes typing. Returns the window it replaced,
/// or `None` when this window already had the front. Call it once per open.
///
/// Windows lets the process that received the last input event set the foreground, and the
/// user's keys went to the app in front, so Kumochi first sends [`focus_input`]. With an
/// elevated app in front, UIPI drops it and the activation fails.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows keeps the current foreground.
pub(crate) fn activate_presence_window(
    window: &impl HasWindowHandle,
) -> Result<Option<Foreground>, PresenceError> {
    #[cfg(test)]
    if HasWindowHandle::window_handle(window).is_err() {
        return Ok(None);
    }
    let hwnd = hwnd_of(window)?;
    // SAFETY: reads the foreground handle without taking ownership.
    let previous = unsafe { GetForegroundWindow() };
    let size = i32::try_from(size_of::<INPUT>()).expect("INPUT's size fits in i32");
    // SAFETY: one initialized INPUT, with its exact size.
    if unsafe { SendInput(&[focus_input()], size) } != 1 {
        tracing::debug!("the focus input was refused");
    }
    // SAFETY: `hwnd` is this thread's live window.
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        return Err(PresenceError::Activate);
    }
    Ok((previous != hwnd && !previous.is_invalid())
        .then(|| Foreground(previous, process_of(previous))))
}

/// Gives the front back to `previous`, only while `window` still has it and `previous` is still
/// the same process's window, so a window the user chose since is never overridden.
pub(crate) fn restore_foreground(window: &impl HasWindowHandle, previous: Foreground) {
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
