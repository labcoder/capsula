//! Separate-process EDIT fixture and its physical-input/session guards.

use std::cell::RefCell;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, GetKeyboardLayout,
    GetKeyboardLayoutList, GetKeyboardLayoutNameW, HKL, LoadKeyboardLayoutW, UnloadKeyboardLayout,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, WM_APP};

use anyhow::{Result, ensure};
use capsula_protocol::constants::{DISPLAY_NAME, OBJECT_PREFIX};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, MOD_NOREPEAT, RegisterHotKey, SetFocus, UnregisterHotKey,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW, GetMessageW,
    GetPropW, IsWindow, MSG, PostQuitMessage, RegisterClassW, SW_SHOWNOACTIVATE, SetPropW,
    SetTimer, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_INPUT, WM_KEYDOWN, WM_SETFOCUS, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_BORDER,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WTS_SESSION_LOCK,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::{MARKER, size};

pub(super) const ABORT: PCWSTR = w!("InputSessionAbort");
pub(super) const CHANGES: PCWSTR = w!("InputSessionChanges");
pub(super) const ESCAPES: PCWSTR = w!("InputSessionEscapes");
pub(super) const READY: PCWSTR = w!("InputSessionReady");
const ESC_HELD: PCWSTR = w!("InputSessionEscHeld");

pub(super) fn property(window: HWND, name: PCWSTR) -> usize {
    // SAFETY: the name is a static NUL-terminated property key; no pointer is dereferenced.
    unsafe { GetPropW(window, name) }.0.addr()
}

fn set_property(window: HWND, name: PCWSTR, count: usize) -> Result<()> {
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

fn physical_input(param: LPARAM) -> Result<bool> {
    let mut raw = RAWINPUT::default();
    let mut bytes = size::<RAWINPUT>();
    // SAFETY: RAWINPUT is aligned and writable for its stated size; the message owns this raw-input handle.
    let read = unsafe {
        GetRawInputData(
            HRAWINPUT(std::ptr::with_exposed_provenance_mut(
                param.0.cast_unsigned(),
            )),
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &raw mut bytes,
            size::<RAWINPUTHEADER>(),
        )
    };
    ensure!(
        read != u32::MAX && read >= size::<RAWINPUTHEADER>() && bytes <= size::<RAWINPUT>(),
        "raw-input guard could not read an event"
    );
    let marker = match raw.header.dwType {
        value if value == RIM_TYPEKEYBOARD.0 => {
            // SAFETY: the header identifies the keyboard union member.
            unsafe { raw.data.keyboard }.ExtraInformation
        }
        value if value == RIM_TYPEMOUSE.0 => {
            // SAFETY: the header identifies the mouse union member.
            unsafe { raw.data.mouse }.ulExtraInformation
        }
        _ => return Ok(true),
    };
    Ok(usize::try_from(marker)? != MARKER)
}

unsafe extern "system" fn edit_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    root: usize,
) -> LRESULT {
    if message == WM_KEYDOWN && wp.0 == 0x1b {
        let root = HWND(std::ptr::with_exposed_provenance_mut(root));
        if set_property(root, ESCAPES, property(root, ESCAPES) + 1).is_err() {
            // SAFETY: this stops only the fixture's own message loop.
            unsafe { PostQuitMessage(1) };
        }
    }
    // SAFETY: this callback is installed by SetWindowSubclass and forwards the unchanged message.
    unsafe { DefSubclassProc(window, message, wp, lp) }
}

unsafe extern "system" fn fixture_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == LOAD_LAYOUT {
        return load_layout(window, wp.0).map_or(LRESULT(0), |layout| {
            LRESULT(layout.0.expose_provenance().cast_signed())
        });
    }
    if message == RESTORE_LAYOUTS {
        return LRESULT(isize::from(restore_layouts().is_ok()));
    }
    if message == WM_SETFOCUS && focus_edit(window).is_err() {
        // SAFETY: failed fixture focus stops only this fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    let changed = message == WM_COMMAND && ((wp.0 >> 16) & 0xffff) == 0x300;
    if changed && set_property(window, CHANGES, property(window, CHANGES) + 1).is_err() {
        // SAFETY: this stops only the fixture's own message loop.
        unsafe { PostQuitMessage(1) };
    }
    if ((message == WM_INPUT && physical_input(lp).unwrap_or(true))
        || (message == WM_WTSSESSION_CHANGE
            && wp.0 == usize::try_from(WTS_SESSION_LOCK).expect("session event fits usize")))
        && set_property(window, ABORT, 1).is_err()
    {
        // SAFETY: a failed guard aborts the fixture rather than permitting unguarded input.
        unsafe { PostQuitMessage(1) };
    }
    if message == WM_CLOSE || message == WM_TIMER {
        let mut failed = false;
        if property(window, ESC_HELD) != 0 {
            // SAFETY: this fixture owns hotkey 1 on this window and releases no other registration.
            failed |= unsafe { UnregisterHotKey(Some(window), 1) }.is_err();
        }
        if property(window, READY) != 0 {
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
        // SAFETY: this stops only the fixture's own message loop.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    // SAFETY: unhandled messages are passed unchanged to the standard window procedure.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

pub(crate) fn fixture() -> Result<()> {
    // SAFETY: this obtains the fixture executable's own loaded module.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let class_name = HSTRING::from(format!("{OBJECT_PREFIX}-input-fixture"));
    let class = WNDCLASSW {
        lpfnWndProc: Some(fixture_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class definition and its name remain live throughout registration.
    let atom = unsafe { RegisterClassW(&raw const class) };
    ensure!(atom != 0, "fixture class registration failed");
    let title = HSTRING::from(format!("{DISPLAY_NAME} input fixture"));
    // SAFETY: the registered class has a static callback; there is no borrowed creation data.
    let root = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class_name,
            &title,
            WS_OVERLAPPEDWINDOW,
            80,
            80,
            640,
            400,
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    let result = run_fixture(root, instance);
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

fn run_fixture(root: HWND, instance: HINSTANCE) -> Result<()> {
    // SAFETY: EDIT is a standard control and its parent is our own live window.
    let edit = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER,
            12,
            12,
            580,
            300,
            Some(root),
            None,
            Some(instance),
            None,
        )
    }?;
    // SAFETY: this live EDIT belongs to this thread; the callback's root handle remains live until destruction.
    let installed =
        unsafe { SetWindowSubclass(edit, Some(edit_proc), 1, root.0.expose_provenance()) }
            .as_bool();
    ensure!(installed, "fixture edit subclass failed");
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 6,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: root,
        },
        RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: root,
        },
    ];
    // SAFETY: both device entries target this process's live window and use the documented structure size.
    unsafe { RegisterRawInputDevices(&devices, size::<RAWINPUTDEVICE>()) }?;
    // SAFETY: notifications target this process's live window and current session only.
    unsafe { WTSRegisterSessionNotification(root, NOTIFY_FOR_THIS_SESSION) }?;
    // SAFETY: this window owns timer 1 and the callback is delivered through its message loop.
    let timer = unsafe { SetTimer(Some(root), 1, 105_000, None) };
    ensure!(timer != 0, "fixture autoexit timer failed");
    for key in 1..256 {
        // SAFETY: querying the down state does not generate input or record key identities.
        if unsafe { GetAsyncKeyState(key) } < 0 {
            set_property(root, ABORT, 1)?;
            break;
        }
    }
    ensure!(
        property(root, ABORT) == 0,
        "physical activity before fixture readiness"
    );
    if std::env::args().any(|arg| arg == "--reserve-escape") {
        // SAFETY: only the bounded fixture's own window holds this temporary registration.
        unsafe { RegisterHotKey(Some(root), 1, MOD_NOREPEAT, 0x1b) }?;
        set_property(root, ESC_HELD, 1)?;
    }
    set_property(root, READY, 1)?;
    // SAFETY: guards are armed; this shows only our fixture without activating it. False means previously hidden.
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

fn focus_edit(root: HWND) -> Result<()> {
    // SAFETY: only the standard EDIT child of this fixture's own window is searched.
    let edit = unsafe { FindWindowExW(Some(root), None, w!("EDIT"), None) }?;
    // SAFETY: the fixture is already receiving focus; only its live child receives keyboard focus.
    if let Err(error) = unsafe { SetFocus(Some(edit)) } {
        // SAFETY: this reads focus on the fixture thread; a NULL previous focus is not itself failure.
        let focused = unsafe { GetFocus() };
        ensure!(focused == edit, "fixture EDIT focus failed: {error}");
    }
    Ok(())
}

pub(super) const LOAD_LAYOUT: u32 = WM_APP + 17;
pub(super) const RESTORE_LAYOUTS: u32 = WM_APP + 18;

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
    use super::layout_id;

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
