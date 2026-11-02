//! Separate-process EDIT fixture: the owner's feel verdicts and physical Ready, then the
//! physical-input/session guards, click receipts, Esc lease and cover placement.

use std::cell::{Cell, RefCell};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{DISPLAY_NAME, OBJECT_PREFIX};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, EnableWindow, GetCapture, GetFocus,
    GetKeyState, GetKeyboardLayout, GetKeyboardLayoutList, GetKeyboardLayoutNameW, HKL,
    LoadKeyboardLayoutW, MOD_NOREPEAT, RegisterHotKey, SetFocus, UnloadKeyboardLayout,
    UnregisterHotKey, VK_LBUTTON,
};
use windows::Win32::UI::Input::{
    GetCurrentInputMessageSource, IMDT_MOUSE, IMO_HARDWARE, IMO_INJECTED, INPUT_MESSAGE_SOURCE,
    RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RIDEV_INPUTSINK, RIDEV_REMOVE, RegisterRawInputDevices,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST,
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_STYLE, GetClientRect,
    GetForegroundWindow, GetMessageExtraInfo, GetMessageTime, GetMessageW, GetPropW,
    GetQueueStatus, GetWindowLongPtrW, ISMEX_NOSEND, InSendMessageEx, IsWindow, MSG, MoveWindow,
    PostQuitMessage, QS_INPUT, RegisterClassW, SW_HIDE, SW_SHOWNA, SW_SHOWNOACTIVATE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetPropW, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_INPUT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_SETFOCUS, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_BORDER, WS_CHILD,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WS_VSCROLL, WTS_SESSION_LOCK,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::gate::{Feel, MouseSnapshot, Origin, Press, Verdict};
use super::guard::{self, StopCause, physical_input};
use super::switch_window;
use super::{
    FixtureEscMode, MARKER, MAX_COVER, MAX_LIFETIME, desktop_rect, last_input, pack, size, unpack,
};

pub(super) const ABORT: PCWSTR = w!("InputSessionAbort");
pub(super) const CHANGES: PCWSTR = w!("InputSessionChanges");
pub(super) const ESCAPES: PCWSTR = w!("InputSessionEscapes");
/// Set for the current Ready attempt; cleared only by the pre-open re-arm handshake.
pub(super) const READY: PCWSTR = w!("InputSessionReady");
pub(super) const OPEN: PCWSTR = w!("InputSessionOpen");
pub(super) const INTERRUPTED: PCWSTR = w!("InputSessionInterrupted");
const REGISTERED: PCWSTR = w!("InputSessionRegistered");
/// `GetLastInputInfo` just before the guards were armed.
pub(super) const ARMED_INPUT: PCWSTR = w!("InputSessionArmedInput");
pub(super) const FEEL: PCWSTR = w!("InputSessionFeel");
pub(super) const ESC_HELD: PCWSTR = w!("InputSessionEscHeld");
pub(super) const CLICKS: PCWSTR = w!("InputSessionClicks");
pub(super) const CLICK_QPC: PCWSTR = w!("InputSessionClickQpc");
pub(super) const CLICK_POINT: PCWSTR = w!("InputSessionClickPoint");

pub(super) const LOAD_LAYOUT: u32 = WM_APP + 17;
pub(super) const RESTORE_LAYOUTS: u32 = WM_APP + 18;
pub(super) const ESCAPE_LEASE: u32 = WM_APP + 19;
pub(super) const COVER: u32 = WM_APP + 20;
pub(super) const UNCOVER: u32 = WM_APP + 21;
pub(super) const REARM: u32 = WM_APP + 22;
pub(super) const OPEN_GATE: u32 = WM_APP + 23;

/// How often one fixture may take or give back its Esc reservation.
const MAX_ESCAPE_TOGGLES: u32 = 64;

/// The framed window, and its EDIT inside it.
const FRAME: [i32; 4] = [80, 80, 640, 490];
const EDIT_FRAME: [i32; 4] = [12, 84, 600, 260];

const GESTURES: [&str; 3] = ["Hold", "Double-tap", "Stretch"];

#[derive(Debug, Clone, Copy)]
struct Controls {
    edit: HWND,
    ready: HWND,
    verdicts: [HWND; 3],
}

thread_local! {
    static CONTROLS: Cell<Option<Controls>> = const { Cell::new(None) };
    static PRESS: Cell<Press> = Cell::new(Press::default());
    static ESCAPE_TOGGLES: Cell<u32> = const { Cell::new(0) };
}

pub(super) fn class_name() -> String {
    format!("{OBJECT_PREFIX}-input-fixture")
}

pub(super) fn property(window: HWND, name: PCWSTR) -> usize {
    // SAFETY: the name is a static NUL-terminated property key; no pointer is dereferenced.
    unsafe { GetPropW(window, name) }.0.addr()
}

pub(super) fn set_property(window: HWND, name: PCWSTR, count: usize) -> Result<()> {
    // SAFETY: property values are opaque integers, never dereferenced by this fixture.
    unsafe {
        SetPropW(
            window,
            name,
            Some(HANDLE(std::ptr::without_provenance_mut(count))),
        )
    }?;
    Ok(())
}

fn controls() -> Result<Controls> {
    CONTROLS.get().context("fixture controls are missing")
}

/// Where the button message being handled came from. A sent message never counts as the
/// mouse, whatever input came before it.
fn origin() -> Origin {
    // SAFETY: reports whether this thread is inside a message sent by another thread.
    if unsafe { InSendMessageEx(None) } != ISMEX_NOSEND {
        return Origin::Other;
    }
    let mut source = INPUT_MESSAGE_SOURCE::default();
    // SAFETY: source is a writable output describing the message this thread is handling.
    if unsafe { GetCurrentInputMessageSource(&raw mut source) }.is_err() {
        return Origin::Other;
    }
    if source.originId == IMO_INJECTED {
        Origin::Injected
    } else if source.originId == IMO_HARDWARE && source.deviceType == IMDT_MOUSE {
        Origin::Mouse
    } else {
        Origin::Other
    }
}

/// Whether the input state, which only real input changes, shows the left button down.
fn button_held() -> bool {
    // SAFETY: reads this thread's synchronous input state for one button; nothing is changed.
    let state = unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) };
    state < 0
}

/// A mouse message's client point.
fn client_point(lp: LPARAM) -> POINT {
    let [x0, x1, y0, y1, ..] = lp.0.to_le_bytes();
    POINT {
        x: i32::from(i16::from_le_bytes([x0, x1])),
        y: i32::from(i16::from_le_bytes([y0, y1])),
    }
}

fn inside(window: HWND, point: POINT) -> bool {
    let mut client = RECT::default();
    // SAFETY: client is writable and the window is this thread's live button.
    unsafe { GetClientRect(window, &raw mut client) }.is_ok()
        && point.x >= client.left
        && point.x < client.right
        && point.y >= client.top
        && point.y < client.bottom
}

unsafe extern "system" fn ready_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    root: usize,
) -> LRESULT {
    let root = HWND(std::ptr::with_exposed_provenance_mut(root));
    if message == WM_LBUTTONDOWN {
        let mut press = PRESS.get();
        press.down(MouseSnapshot {
            origin: origin(),
            held: button_held(),
            inside: inside(window, client_point(lp)),
            capture_released: false,
        });
        PRESS.set(press);
    }
    if message != WM_LBUTTONUP {
        // SAFETY: this callback is installed by SetWindowSubclass and forwards the unchanged message.
        return unsafe { DefSubclassProc(window, message, wp, lp) };
    }
    let mut up = MouseSnapshot {
        origin: origin(),
        held: button_held(),
        inside: inside(window, client_point(lp)),
        capture_released: false,
    };
    // SAFETY: reads the tick of the message being handled; nothing changes.
    let up_time = unsafe { GetMessageTime() }.cast_unsigned();
    // SAFETY: the button handles its own release first, which ends its normal capture.
    let result = unsafe { DefSubclassProc(window, message, wp, lp) };
    // SAFETY: reads this thread's capture without changing it.
    up.capture_released = unsafe { GetCapture() } != window;
    let mut press = PRESS.get();
    let ready = press.up(up);
    PRESS.set(press);
    if ready
        && property(root, READY) == 0
        && property(root, OPEN) == 0
        && property(root, ABORT) == 0
        && arm(root, up_time).is_err()
    {
        // SAFETY: a failed arm stops only this fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    result
}

/// Freezes the verdicts, focuses the EDIT, arms the physical-input and session guards, and only
/// then publishes Ready. Any input after the release spoils this attempt, not the session.
fn arm(root: HWND, up_time: u32) -> Result<()> {
    let controls = controls()?;
    let feel = selections()?;
    for verdict in controls.verdicts {
        // SAFETY: disables this thread's own combo box; it returns the prior state, not an error.
        let _was_disabled = unsafe { EnableWindow(verdict, false) };
    }
    set_property(root, FEEL, feel)?;
    // SAFETY: the owner's click made this fixture active; focus moves only inside it.
    unsafe { SetFocus(Some(controls.edit)) }?;
    // SAFETY: disables this thread's own button; it returns the prior state, not an error.
    let _was_disabled = unsafe { EnableWindow(controls.ready, false) };
    register_guards(root)?;
    let armed = last_input()?;
    set_property(root, INTERRUPTED, usize::from(armed != up_time))?;
    set_property(root, ARMED_INPUT, usize::try_from(armed)?)?;
    set_property(root, READY, 1)
}

fn selections() -> Result<usize> {
    let mut selections = [0; 3];
    for (selection, verdict) in selections.iter_mut().zip(controls()?.verdicts) {
        // SAFETY: an in-process query of this thread's own combo box; -1 means no selection.
        *selection = unsafe { SendMessageW(verdict, CB_GETCURSEL, None, None) }.0 - 1;
    }
    Feel::encode(selections)
}

fn enable_choices() -> Result<()> {
    let controls = controls()?;
    for verdict in controls.verdicts {
        // SAFETY: enables only this thread's own combo boxes.
        let _was_disabled = unsafe { EnableWindow(verdict, true) };
    }
    // SAFETY: enables this thread's button only when all three combo boxes have a valid selection.
    let _was_disabled = unsafe { EnableWindow(controls.ready, selections().is_ok()) };
    Ok(())
}

fn rearm(root: HWND) -> Result<()> {
    ensure!(
        property(root, READY) != 0 && property(root, OPEN) == 0 && property(root, ABORT) == 0,
        "only a pre-open Ready may rearm"
    );
    raw_devices(HWND::default(), RIDEV_REMOVE)?;
    set_property(root, READY, 0)?;
    set_property(root, INTERRUPTED, 0)?;
    PRESS.set(Press::default());
    enable_choices()
}

fn queued_messages() -> bool {
    // SAFETY: reads this fixture thread's queue status; no message is removed.
    (unsafe { GetQueueStatus(QS_INPUT) }) >> 16 != 0
}

fn open_gate(root: HWND, input: usize) -> Result<usize> {
    ensure!(
        property(root, READY) != 0 && property(root, OPEN) == 0 && property(root, ABORT) == 0,
        "only a live quiet attempt may open"
    );
    let current = usize::try_from(last_input()?)?;
    if property(root, INTERRUPTED) != 0
        || property(root, ARMED_INPUT) != input
        || current != input
        || queued_messages()
    {
        rearm(root)?;
        return Ok(2);
    }
    set_property(root, OPEN, 1)?;
    guard::opened();
    if usize::try_from(last_input()?)? != input || queued_messages() {
        guard::stop(
            root,
            StopCause::Input(guard::PhysicalInput {
                class: guard::InputClass::Other,
                marker: None,
            }),
        )?;
        anyhow::bail!("input raced the native gate opening");
    }
    Ok(1)
}

fn register_guards(root: HWND) -> Result<()> {
    raw_devices(root, RIDEV_INPUTSINK)?;
    if property(root, REGISTERED) == 0 {
        // SAFETY: notifications target this process's live window and current session only.
        unsafe { WTSRegisterSessionNotification(root, NOTIFY_FOR_THIS_SESSION) }?;
        set_property(root, REGISTERED, 1)?;
    }
    Ok(())
}

pub(super) fn raw_devices(root: HWND, flags: RAWINPUTDEVICE_FLAGS) -> Result<()> {
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 6,
            dwFlags: flags,
            hwndTarget: root,
        },
        RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: flags,
            hwndTarget: root,
        },
    ];
    // SAFETY: only this fixture process's registrations change; removal uses a null target as required.
    unsafe { RegisterRawInputDevices(&devices, size::<RAWINPUTDEVICE>()) }?;
    Ok(())
}

unsafe extern "system" fn edit_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    root: usize,
) -> LRESULT {
    let root = HWND(std::ptr::with_exposed_provenance_mut(root));
    let counted = if message == WM_KEYDOWN && wp.0 == 0x1b {
        set_property(root, ESCAPES, property(root, ESCAPES) + 1)
    } else if message == WM_LBUTTONDOWN && property(root, READY) != 0 {
        receive_click(window, root, lp)
    } else {
        Ok(())
    };
    if counted.is_err() {
        // SAFETY: this stops only the fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    // SAFETY: this callback is installed by SetWindowSubclass and forwards the unchanged message.
    unsafe { DefSubclassProc(window, message, wp, lp) }
}

/// Records a button-down the EDIT received from the session's tagged input: when, where on
/// screen, and how many so far. Untagged clicks leave no count, time or point.
fn receive_click(edit: HWND, root: HWND, lp: LPARAM) -> Result<()> {
    // SAFETY: reads the extra info of the input message being handled; nothing changes.
    let tagged = unsafe { GetMessageExtraInfo() }.0.cast_unsigned() == MARKER;
    if !tagged {
        // Only the session's own clicks count; an owner's click aborts through raw input.
        return Ok(());
    }
    let mut qpc = 0;
    // SAFETY: qpc is a writable counter output.
    unsafe { QueryPerformanceCounter(&raw mut qpc) }?;
    let mut point = client_point(lp);
    // SAFETY: point is writable and the EDIT is this thread's live window.
    let converted = unsafe { ClientToScreen(edit, &raw mut point) }.as_bool();
    ensure!(converted, "click point conversion failed");
    set_property(root, CLICK_QPC, usize::try_from(qpc)?)?;
    set_property(root, CLICK_POINT, usize::try_from(pack(point.x, point.y))?)?;
    set_property(root, CLICKS, property(root, CLICKS) + 1)
}

unsafe extern "system" fn fixture_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let answer = |done: Result<()>| LRESULT(isize::from(done.is_ok()));
    match message {
        guard::SET_PHASE => {
            return answer(u8::try_from(wp.0).map(guard::set_phase).map_err(Into::into));
        }
        LOAD_LAYOUT => {
            return load_layout(window, wp.0).map_or(LRESULT(0), |layout| {
                LRESULT(layout.0.expose_provenance().cast_signed())
            });
        }
        RESTORE_LAYOUTS => return answer(restore_layouts()),
        ESCAPE_LEASE => {
            let mode = if wp.0 == 0 {
                FixtureEscMode::Free
            } else {
                FixtureEscMode::Reserved
            };
            return answer(escape_lease(window, mode));
        }
        COVER => return answer(cover(window, wp, lp)),
        UNCOVER => return answer(uncover(window)),
        REARM => return answer(rearm(window)),
        OPEN_GATE => {
            return open_gate(window, wp.0)
                .map_or(LRESULT(0), |response| LRESULT(response.cast_signed()));
        }
        switch_window::CREATE => return answer(switch_window::create(window, wp)),
        switch_window::CLOSE => return answer(switch_window::cleanup()),
        _ => {}
    }
    if message == WM_SETFOCUS && focus_edit().is_err() {
        // SAFETY: failed fixture focus stops only this fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    let changed = message == WM_COMMAND && ((wp.0 >> 16) & 0xffff) == 0x300;
    if changed && set_property(window, CHANGES, property(window, CHANGES) + 1).is_err() {
        // SAFETY: this stops only the fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    let selected = message == WM_COMMAND
        && ((wp.0 >> 16) & 0xffff)
            == usize::try_from(CBN_SELCHANGE).expect("notification fits usize");
    if selected && property(window, READY) == 0 && enable_choices().is_err() {
        // SAFETY: a failed control update stops only this fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    let guard =
        if message == WM_INPUT && (property(window, READY) != 0 || property(window, OPEN) != 0) {
            match physical_input(lp) {
                Ok(Some(input)) if property(window, OPEN) != 0 => {
                    guard::stop(window, StopCause::Input(input))
                }
                Ok(Some(_)) if property(window, READY) != 0 => set_property(window, INTERRUPTED, 1),
                Ok(_) => Ok(()),
                Err(_) => guard::stop(window, StopCause::GuardReadFailure),
            }
        } else if message == WM_WTSSESSION_CHANGE
            && wp.0 == usize::try_from(WTS_SESSION_LOCK).expect("session event fits usize")
        {
            guard::stop(window, StopCause::Lock)
        } else {
            Ok(())
        };
    if guard.is_err() {
        // SAFETY: a failed guard aborts the fixture rather than permitting unguarded input.
        unsafe { PostQuitMessage(1) };
    }
    if message == WM_CLOSE || message == WM_TIMER {
        let mut failed = switch_window::cleanup().is_err();
        if property(window, ESC_HELD) != 0 {
            // SAFETY: this fixture owns hotkey 1 on this window and releases no other registration.
            failed |= unsafe { UnregisterHotKey(Some(window), 1) }.is_err();
        }
        if property(window, REGISTERED) != 0 {
            // SAFETY: session notifications were registered on this live fixture window.
            failed |= unsafe { WTSUnRegisterSessionNotification(window) }.is_err();
        }
        // SAFETY: this callback owns the fixture window on its creating thread.
        failed |= unsafe { DestroyWindow(window) }.is_err();
        if failed {
            // SAFETY: this stops only the fixture's own message loop.
            unsafe { PostQuitMessage(1) };
        }
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        if switch_window::cleanup().is_err() {
            // SAFETY: a failed alternate cleanup stops only this fixture's message loop.
            unsafe { PostQuitMessage(1) };
            return LRESULT(0);
        }
        // SAFETY: this stops only the fixture's own message loop.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    // SAFETY: unhandled messages are passed unchanged to the standard window procedure.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

/// Takes or gives back the Esc reservation, at most [`MAX_ESCAPE_TOGGLES`] times.
fn escape_lease(root: HWND, mode: FixtureEscMode) -> Result<()> {
    let reserve = mode.is_reserved();
    if (property(root, ESC_HELD) != 0) == reserve {
        return Ok(());
    }
    let toggles = ESCAPE_TOGGLES.get();
    ensure!(toggles < MAX_ESCAPE_TOGGLES, "Esc lease toggled too often");
    ESCAPE_TOGGLES.set(toggles + 1);
    if reserve {
        // SAFETY: only the bounded fixture's own window holds this temporary registration.
        unsafe { RegisterHotKey(Some(root), 1, MOD_NOREPEAT, 0x1b) }?;
    } else {
        // SAFETY: releases only this fixture's own Esc registration.
        unsafe { UnregisterHotKey(Some(root), 1) }?;
    }
    set_property(root, ESC_HELD, usize::from(reserve))
}

/// Restyles the fixture's own window and places it without activation or a Z-order change.
fn place(root: HWND, style: WINDOW_STYLE, [x, y, width, height]: [i32; 4]) -> Result<()> {
    // SAFETY: changes only this thread's own top-level window style.
    let _previous = unsafe { SetWindowLongPtrW(root, GWL_STYLE, isize::try_from(style.0)?) };
    // SAFETY: reads back this thread's own window style.
    let applied = u32::try_from(unsafe { GetWindowLongPtrW(root, GWL_STYLE) } & 0xffff_ffff)?;
    ensure!(applied & style.0 == style.0, "fixture style change failed");
    // SAFETY: moves only this thread's own window; NOACTIVATE and NOZORDER keep focus and order.
    unsafe {
        SetWindowPos(
            root,
            None,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
        )
    }?;
    Ok(())
}

fn cover(root: HWND, wp: WPARAM, lp: LPARAM) -> Result<()> {
    ensure!(
        property(root, OPEN) != 0,
        "cover needs injection open first"
    );
    let [x, y] = unpack(u64::try_from(wp.0)?);
    let [width, height] = unpack(u64::try_from(lp.0)?);
    let [left, top, desktop_width, desktop_height] = desktop_rect();
    ensure!(
        (1..=MAX_COVER).contains(&width)
            && (1..=MAX_COVER).contains(&height)
            && x >= left
            && y >= top
            && x + width <= left + desktop_width
            && y + height <= top + desktop_height,
        "cover placement out of bounds"
    );
    let controls = controls()?;
    for control in controls.verdicts.into_iter().chain([controls.ready]) {
        // SAFETY: hides this thread's own control; it returns the prior visibility.
        let _was_visible = unsafe { ShowWindow(control, SW_HIDE) };
    }
    place(root, WS_POPUP | WS_VISIBLE, [x, y, width, height])?;
    // SAFETY: resizes this thread's own EDIT to fill the borderless fixture.
    unsafe { MoveWindow(controls.edit, 0, 0, width, height, true) }?;
    Ok(())
}

fn uncover(root: HWND) -> Result<()> {
    let controls = controls()?;
    place(root, WS_OVERLAPPEDWINDOW | WS_VISIBLE, FRAME)?;
    let [x, y, width, height] = EDIT_FRAME;
    // SAFETY: puts this thread's own EDIT back in its framed place.
    unsafe { MoveWindow(controls.edit, x, y, width, height, true) }?;
    for control in controls.verdicts.into_iter().chain([controls.ready]) {
        // SAFETY: shows this thread's own control without activating anything.
        let _was_visible = unsafe { ShowWindow(control, SW_SHOWNA) };
    }
    Ok(())
}

pub(crate) fn fixture() -> Result<()> {
    let lifetime = std::env::args()
        .find_map(|arg| arg.strip_prefix("--lifetime-secs=").map(str::parse::<u64>))
        .context("the fixture needs --lifetime-secs")??;
    ensure!(
        (1..=MAX_LIFETIME.as_secs()).contains(&lifetime),
        "fixture lifetime must be 1 s to 1 h"
    );
    // SAFETY: this obtains the fixture executable's own loaded module.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let class_name = HSTRING::from(class_name());
    let class = WNDCLASSW {
        lpfnWndProc: Some(fixture_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class definition and its name remain live throughout registration.
    let atom = unsafe { RegisterClassW(&raw const class) };
    ensure!(atom != 0, "fixture class registration failed");
    switch_window::register(instance)?;
    let title = HSTRING::from(format!("{DISPLAY_NAME} input fixture"));
    let [x, y, width, height] = FRAME;
    // SAFETY: the registered class has a static callback; there is no borrowed creation data.
    let root = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class_name,
            &title,
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    let result = match (
        run_fixture(root, instance, u32::try_from(lifetime * 1000)?),
        switch_window::cleanup(),
    ) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("alternate cleanup also failed: {cleanup}")))
        }
    };
    let restored = restore_layouts();
    // SAFETY: querying a potentially destroyed fixture handle is supported by IsWindow.
    if unsafe { IsWindow(Some(root)) }.as_bool() {
        // SAFETY: the fixture window was created on this thread and has not been destroyed.
        unsafe { DestroyWindow(root) }?;
    }
    match (result, restored) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("fixture layout cleanup also failed: {cleanup}")))
        }
    }
}

fn child(
    class: PCWSTR,
    text: &HSTRING,
    style: WINDOW_STYLE,
    [x, y, width, height]: [i32; 4],
    root: HWND,
    instance: HINSTANCE,
) -> Result<HWND> {
    // SAFETY: a standard control class, parented to this thread's own live window.
    Ok(unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            text,
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            width,
            height,
            Some(root),
            None,
            Some(instance),
            None,
        )
    }?)
}

fn verdict_box(index: usize, gesture: &str, root: HWND, instance: HINSTANCE) -> Result<HWND> {
    let x = 12 + 200 * i32::try_from(index)?;
    let combo = child(
        w!("COMBOBOX"),
        &HSTRING::new(),
        WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST.cast_unsigned()),
        [x, 12, 190, 160],
        root,
        instance,
    )?;
    let placeholder = HSTRING::from(format!("{gesture}: select verdict"));
    // SAFETY: the owned combo copies this live placeholder string.
    let added = unsafe {
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(
                placeholder.as_ptr().expose_provenance().cast_signed(),
            )),
        )
    };
    ensure!(added.0 == 0, "verdict placeholder setup failed");
    for verdict in Verdict::ALL {
        let item = HSTRING::from(format!("{gesture}: {verdict:?}"));
        // SAFETY: an in-process message; the combo box copies the live string.
        let added = unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(item.as_ptr().expose_provenance().cast_signed())),
            )
        };
        ensure!(added.0 >= 0, "verdict list setup failed");
    }
    // SAFETY: an in-process selection message to this thread's own combo box.
    let selected = unsafe { SendMessageW(combo, CB_SETCURSEL, None, None) };
    ensure!(selected.0 == 0, "verdict placeholder selection failed");
    Ok(combo)
}

fn run_fixture(root: HWND, instance: HINSTANCE, lifetime_ms: u32) -> Result<()> {
    let edit = child(
        w!("EDIT"),
        &HSTRING::new(),
        WS_BORDER,
        EDIT_FRAME,
        root,
        instance,
    )?;
    let ready = child(
        w!("BUTTON"),
        &HSTRING::from("Ready"),
        WINDOW_STYLE(BS_PUSHBUTTON.cast_unsigned()),
        [12, 48, 120, 28],
        root,
        instance,
    )?;
    let mut verdicts = [HWND::default(); 3];
    for (index, (slot, gesture)) in verdicts.iter_mut().zip(GESTURES).enumerate() {
        *slot = verdict_box(index, gesture, root, instance)?;
    }
    CONTROLS.set(Some(Controls {
        edit,
        ready,
        verdicts,
    }));
    enable_choices()?;
    child(
        w!("STATIC"),
        &HSTRING::from(
            "The dock starts top center; Listening is its label.\r\nSelect all three verdicts (Unrated is valid), then click Ready.\r\nDo not touch mouse or keyboard until this window closes.\r\nAllow about 10 minutes (up to 1 hour), not just one second.\r\nBefore injection input re-arms Ready; afterwards it stops.",
        ),
        WINDOW_STYLE::default(),
        [12, 352, 600, 96],
        root,
        instance,
    )?;
    let root_data = root.0.expose_provenance();
    // SAFETY: this live EDIT belongs to this thread; the callback's root handle remains live until destruction.
    let edit_hooked = unsafe { SetWindowSubclass(edit, Some(edit_proc), 1, root_data) }.as_bool();
    // SAFETY: this live button belongs to this thread; the callback's root handle remains live until destruction.
    let ready_hooked =
        unsafe { SetWindowSubclass(ready, Some(ready_proc), 1, root_data) }.as_bool();
    ensure!(edit_hooked && ready_hooked, "fixture subclass failed");
    // SAFETY: this window owns timer 1 and the callback is delivered through its message loop.
    let timer = unsafe { SetTimer(Some(root), 1, lifetime_ms, None) };
    ensure!(timer != 0, "fixture autoexit timer failed");
    // SAFETY: shows only our fixture without activating it. False means previously hidden.
    let _was_visible = unsafe { ShowWindow(root, SW_SHOWNOACTIVATE) };
    let mut message = MSG::default();
    loop {
        // SAFETY: message is writable; this thread processes only its own queued messages.
        let got = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        ensure!(got >= 0, "fixture message loop failed");
        if got == 0 {
            ensure!(
                message.wParam.0 == 0,
                "fixture guard or window callback failed"
            );
            return Ok(());
        }
        // SAFETY: the message was obtained from this thread's queue.
        let _translated = unsafe { TranslateMessage(&raw const message) };
        // SAFETY: the message was obtained from this thread's queue.
        unsafe { DispatchMessageW(&raw const message) };
    }
}

/// Keeps keyboard focus on the EDIT whenever the fixture is activated.
fn focus_edit() -> Result<()> {
    let edit = controls()?.edit;
    // SAFETY: the fixture is already receiving focus; only its live child receives keyboard focus.
    if let Err(error) = unsafe { SetFocus(Some(edit)) } {
        // SAFETY: this reads focus on the fixture thread; a NULL previous focus is not itself failure.
        let focused = unsafe { GetFocus() };
        ensure!(focused == edit, "fixture EDIT focus failed: {error}");
    }
    Ok(())
}

thread_local! {
    static LAYOUTS: RefCell<Option<FixtureLayouts>> = const { RefCell::new(None) };
}

struct FixtureLayouts {
    original: HKL,
    added: Vec<HKL>,
}

fn loaded_layouts() -> Result<Vec<HKL>> {
    // SAFETY: querying the list size changes no layouts.
    let count = unsafe { GetKeyboardLayoutList(None) };
    ensure!(count > 0, "cannot snapshot fixture keyboard layouts");
    let mut layouts = vec![HKL::default(); usize::try_from(count)?];
    // SAFETY: layouts has capacity for the advertised number of handles.
    let read = unsafe { GetKeyboardLayoutList(Some(&mut layouts)) };
    ensure!(read == count, "layout list changed during fixture snapshot");
    Ok(layouts)
}

fn layout_id(selection: usize) -> Result<PCWSTR> {
    match selection {
        0 => Ok(w!("00000407")),
        1 => Ok(w!("0000040A")),
        _ => anyhow::bail!("unsupported fixture layout request"),
    }
}

fn load_layout(root: HWND, selection: usize) -> Result<HKL> {
    let klid = layout_id(selection)?;
    // SAFETY: reading the foreground does not change it.
    let foreground = unsafe { GetForegroundWindow() };
    // SAFETY: reading this fixture thread's focus does not change it.
    let focused = foreground == root && !unsafe { GetFocus() }.is_invalid();
    ensure!(
        focused && property(root, ABORT) == 0,
        "fixture does not own keyboard focus"
    );
    super::input_desktop_available()?;
    LAYOUTS.with(|slot| {
        let mut slot = slot.try_borrow_mut()?;
        let state = slot.get_or_insert_with(|| FixtureLayouts {
            // SAFETY: zero reads the calling fixture thread's active layout.
            original: unsafe { GetKeyboardLayout(0) },
            added: Vec::new(),
        });
        let before = loaded_layouts()?;
        // SAFETY: the fixture owns keyboard focus; no ACTIVATE/REORDER or preference flags are supplied.
        let layout =
            unsafe { LoadKeyboardLayoutW(klid, ACTIVATE_KEYBOARD_LAYOUT_FLAGS::default()) }?;
        if !before.contains(&layout) && !state.added.contains(&layout) {
            state.added.push(layout);
        }
        // SAFETY: this activates the returned loaded handle on the owned fixture thread.
        unsafe { ActivateKeyboardLayout(layout, ACTIVATE_KEYBOARD_LAYOUT_FLAGS::default()) }?;
        let mut name = [0u16; 9];
        // SAFETY: name has the documented nine-code-unit size.
        unsafe { GetKeyboardLayoutNameW(&mut name) }?;
        // SAFETY: klid points to one of the two static NUL-terminated layout identifiers above.
        let expected = unsafe { klid.as_wide() };
        ensure!(
            name[..8] == *expected,
            "fixture activated a different layout"
        );
        Ok(layout)
    })
}

pub(super) fn restore_layouts() -> Result<()> {
    LAYOUTS.with(|slot| {
        let mut slot = slot.try_borrow_mut()?;
        let Some(state) = slot.as_mut() else {
            return Ok(());
        };
        // SAFETY: restore only this fixture thread's original loaded layout.
        unsafe {
            ActivateKeyboardLayout(state.original, ACTIVATE_KEYBOARD_LAYOUT_FLAGS::default())
        }?;
        let mut failed = false;
        state.added.retain(|layout| {
            // SAFETY: only handles newly loaded by this fixture are unloaded.
            let error = unsafe { UnloadKeyboardLayout(*layout) }.is_err();
            failed |= error;
            error
        });
        ensure!(!failed, "fixture layout cleanup was incomplete");
        *slot = None;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::Input::{
        MOUSE_MOVE_ABSOLUTE, MOUSE_MOVE_NOCOALESCE, RAWINPUT, RAWKEYBOARD, RAWMOUSE, RAWMOUSE_0,
        RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };

    use super::super::guard::{InputClass, PhysicalInput, physical_event};
    use super::layout_id;
    use kumochi_protocol::constants::FOCUS_INPUT_MARKER;

    fn mouse_event(mouse: RAWMOUSE) -> RAWINPUT {
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEMOUSE.0;
        raw.data.mouse = mouse;
        raw
    }

    #[test]
    fn only_the_exact_relative_zero_motion_focus_marker_is_exempt() {
        let marker = u32::try_from(FOCUS_INPUT_MARKER).unwrap();
        let calm = RAWMOUSE {
            ulExtraInformation: marker,
            ..Default::default()
        };
        assert_eq!(physical_event(&mouse_event(calm)).unwrap(), None);
        for (mouse, class, expected_marker) in [
            (
                RAWMOUSE {
                    ulExtraInformation: 0,
                    ..calm
                },
                InputClass::Other,
                0,
            ),
            (
                RAWMOUSE {
                    ulExtraInformation: marker + 1,
                    ..calm
                },
                InputClass::Other,
                marker + 1,
            ),
            (
                RAWMOUSE { lLastX: 1, ..calm },
                InputClass::MouseMove { dx: 1, dy: 0 },
                marker,
            ),
            (
                RAWMOUSE { lLastY: -1, ..calm },
                InputClass::MouseMove { dx: 0, dy: -1 },
                marker,
            ),
            (
                RAWMOUSE {
                    usFlags: MOUSE_MOVE_ABSOLUTE,
                    ..calm
                },
                InputClass::Other,
                marker,
            ),
            (
                RAWMOUSE {
                    usFlags: MOUSE_MOVE_NOCOALESCE,
                    ..calm
                },
                InputClass::Other,
                marker,
            ),
            (
                RAWMOUSE {
                    Anonymous: RAWMOUSE_0 { ulButtons: 1 },
                    ..calm
                },
                InputClass::Button,
                marker,
            ),
            (
                RAWMOUSE {
                    Anonymous: RAWMOUSE_0 {
                        ulButtons: 0x0078_0400,
                    },
                    ..calm
                },
                InputClass::Wheel,
                marker,
            ),
            (
                RAWMOUSE {
                    Anonymous: RAWMOUSE_0 {
                        ulButtons: 0x0078_0800,
                    },
                    ..calm
                },
                InputClass::Wheel,
                marker,
            ),
            (
                RAWMOUSE {
                    ulRawButtons: 1,
                    ..calm
                },
                InputClass::Button,
                marker,
            ),
        ] {
            assert_eq!(
                physical_event(&mouse_event(mouse)).unwrap(),
                Some(PhysicalInput {
                    class,
                    marker: Some(expected_marker),
                })
            );
        }
    }

    #[test]
    fn a_focus_marker_never_exempts_another_device() {
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEKEYBOARD.0;
        raw.data.keyboard = RAWKEYBOARD {
            ExtraInformation: u32::try_from(FOCUS_INPUT_MARKER).unwrap(),
            ..Default::default()
        };
        assert_eq!(
            physical_event(&raw).unwrap(),
            Some(PhysicalInput {
                class: InputClass::Keyboard,
                marker: Some(u32::try_from(FOCUS_INPUT_MARKER).unwrap()),
            })
        );
        raw.header.dwType = RIM_TYPEHID.0;
        assert_eq!(
            physical_event(&raw).unwrap(),
            Some(PhysicalInput {
                class: InputClass::Other,
                marker: None,
            })
        );
    }

    #[test]
    fn fixture_accepts_only_the_two_approved_layout_identifiers() {
        for (request, expected) in [(0, "00000407"), (1, "0000040A")] {
            let layout = layout_id(request).unwrap();
            // SAFETY: the accepted request returns a static NUL-terminated identifier.
            let units = unsafe { layout.as_wide() };
            assert!(units.iter().copied().eq(expected.encode_utf16()));
        }
        assert!(layout_id(2).is_err());
        assert!(layout_id(usize::MAX).is_err());
    }
}
