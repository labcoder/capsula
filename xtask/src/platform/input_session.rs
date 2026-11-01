//! Owned Win32 fixture, physical-input guard and tagged native input for approved sessions.

use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use capsula_protocol::constants::{DISPLAY_NAME, OBJECT_PREFIX};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetThreadDesktop,
    GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentThreadId, NORMAL_PRIORITY_CLASS};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, GetAsyncKeyState, GetFocus,
    GetKeyboardLayout, GetKeyboardLayoutList, GetLastInputInfo, HKL, INPUT, INPUT_0,
    INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, LASTINPUTINFO, LoadKeyboardLayoutW, MOD_NOREPEAT, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, RegisterHotKey, SendInput, SetFocus,
    UnloadKeyboardLayout, UnregisterHotKey, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW,
    GUITHREADINFO, GetClassNameW, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetMessageW,
    GetPropW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SMTO_ABORTIFHUNG, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOZORDER, SendMessageTimeoutW, SetForegroundWindow, SetPropW, SetTimer, SetWindowPos,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_GETTEXT,
    WM_INPUT, WM_INPUTLANGCHANGEREQUEST, WM_KEYDOWN, WM_SETFOCUS, WM_SETTEXT, WM_TIMER,
    WM_WTSSESSION_CHANGE, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    WTS_SESSION_LOCK, WindowFromPoint,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

pub(crate) const MARKER: usize = 0x4350_5331;
const ABORT: PCWSTR = w!("InputSessionAbort");
const CHANGES: PCWSTR = w!("InputSessionChanges");
const ESCAPES: PCWSTR = w!("InputSessionEscapes");
const READY: PCWSTR = w!("InputSessionReady");
const ESC_HELD: PCWSTR = w!("InputSessionEscHeld");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputCheckpoint(u32);

pub(crate) fn input_checkpoint() -> Result<InputCheckpoint> {
    let mut info = LASTINPUTINFO {
        cbSize: size::<LASTINPUTINFO>(),
        ..Default::default()
    };
    // SAFETY: info has the documented writable size; no input content is read.
    let read = unsafe { GetLastInputInfo(&raw mut info) }.as_bool();
    ensure!(read, "cannot obtain startup input checkpoint");
    Ok(InputCheckpoint(info.dwTime))
}

fn input_desktop_available() -> Result<()> {
    // SAFETY: opens the input desktop read-only, without inheriting or switching it.
    let input =
        unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS::default(), false, DESKTOP_READOBJECTS) }?;
    let checked: Result<()> = (|| {
        // SAFETY: this only obtains the calling thread's numeric identity.
        let thread = unsafe { GetCurrentThreadId() };
        // SAFETY: the returned thread desktop is borrowed and is never closed here.
        let assigned = unsafe { GetThreadDesktop(thread) }?;
        let mut input_name = [0u16; 256];
        let mut assigned_name = [0u16; 256];
        // SAFETY: the desktop handle is live and input_name is writable for its byte size.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(input.0),
                UOI_NAME,
                Some((&raw mut input_name).cast()),
                size::<[u16; 256]>(),
                None,
            )
        }?;
        // SAFETY: the borrowed desktop is live and assigned_name is writable for its byte size.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(assigned.0),
                UOI_NAME,
                Some((&raw mut assigned_name).cast()),
                size::<[u16; 256]>(),
                None,
            )
        }?;
        ensure!(
            input_name == assigned_name,
            "input desktop changed or is locked"
        );
        Ok(())
    })();
    // SAFETY: only the handle owned by the successful OpenInputDesktop call is closed.
    let closed = unsafe { CloseDesktop(input) };
    match (checked, closed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(close)) => {
            Err(error.context(format!("closing input desktop also failed: {close}")))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputEdge {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FixtureEscMode {
    Free,
    Reserved,
}

impl FixtureEscMode {
    pub(crate) fn is_reserved(self) -> bool {
        self == Self::Reserved
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowKind {
    Fixture,
    ProductProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    German,
    Spanish,
}

pub(crate) fn spawn_fixture(exe: &Path, mode: FixtureEscMode) -> Result<Child> {
    Ok(Command::new(exe)
        .arg("input-fixture")
        .args(mode.is_reserved().then_some("--reserve-escape"))
        .creation_flags(NORMAL_PRIORITY_CLASS.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

fn size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Win32 input structs fit in u32")
}

fn property(window: HWND, name: PCWSTR) -> usize {
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
    // SAFETY: querying a potentially destroyed fixture handle is supported by IsWindow.
    if unsafe { IsWindow(Some(root)) }.as_bool() {
        // SAFETY: the fixture window was created on this thread and has not been destroyed.
        unsafe { DestroyWindow(root) }?;
    }
    result
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    hwnd: HWND,
    pub(crate) pid: u32,
}

fn pid(window: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: the API accepts an arbitrary handle and the PID output is writable.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    pid
}

pub(crate) fn owned_window(child: &mut Child, kind: WindowKind) -> Result<Window> {
    unsafe extern "system" fn collect(window: HWND, param: LPARAM) -> BOOL {
        let state = std::ptr::with_exposed_provenance_mut::<(u32, WindowKind, Vec<HWND>)>(
            param.0.cast_unsigned(),
        );
        // SAFETY: EnumWindows synchronously passes the exclusive state pointer supplied below.
        let state = unsafe { &mut *state };
        // SAFETY: EnumWindows supplies the window handle for this callback.
        if pid(window) != state.0 || !unsafe { IsWindowVisible(window) }.as_bool() {
            return BOOL::from(true);
        }
        let mut name = [0u16; 128];
        // SAFETY: name is a writable class-name buffer; no window text is read.
        let length = unsafe { GetClassNameW(window, &mut name) };
        let name = String::from_utf16_lossy(&name[..usize::try_from(length).unwrap_or(0)]);
        if state.1 == WindowKind::Fixture || name.ends_with("-presence") {
            state.2.push(window);
        }
        BOOL::from(true)
    }
    ensure!(
        child.try_wait()?.is_none(),
        "owned child exited before window discovery"
    );
    let mut state = (child.id(), kind, Vec::new());
    // SAFETY: collect receives a live exclusive state pointer for this synchronous enumeration.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM((&raw mut state).expose_provenance().cast_signed()),
        )
    }?;
    ensure!(
        state.2.len() == 1,
        "expected exactly one owned session window, found {}",
        state.2.len()
    );
    ensure!(
        kind == WindowKind::ProductProxy || property(state.2[0], READY) == 1,
        "fixture guard is not ready"
    );
    Ok(Window {
        hwnd: state.2[0],
        pid: child.id(),
    })
}

impl Window {
    fn validate(self) -> Result<()> {
        ensure!(
            pid(self.hwnd) == self.pid,
            "owned window disappeared or changed PID"
        );
        Ok(())
    }

    pub(crate) fn close(self) -> Result<()> {
        self.validate()?;
        // SAFETY: ownership was checked; WM_CLOSE has no pointer arguments.
        unsafe { PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }?;
        Ok(())
    }

    pub(crate) fn is_foreground(self) -> bool {
        // SAFETY: this reads the current foreground HWND without accessing its content.
        pid(unsafe { GetForegroundWindow() }) == self.pid
    }

    pub(crate) fn counts(self) -> Result<(usize, usize)> {
        self.validate()?;
        Ok((property(self.hwnd, CHANGES), property(self.hwnd, ESCAPES)))
    }

    pub(crate) fn rect(self) -> Result<RECT> {
        self.validate()?;
        let mut rect = RECT::default();
        // SAFETY: the owned HWND is valid and rect is writable.
        unsafe { GetWindowRect(self.hwnd, &raw mut rect) }?;
        Ok(rect)
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) fixture: Window,
    pub(crate) shell: Window,
    edit: HWND,
    end: Instant,
    held: [bool; 512],
    original_cursor: POINT,
    pointer_moved: bool,
    pub(crate) batches: u32,
    pub(crate) first_qpc: Option<i64>,
    pub(crate) last_qpc: Option<i64>,
    pub(crate) qpc_frequency: i64,
}

impl Session {
    pub(crate) fn new(fixture: Window, shell: Window, end: Instant) -> Result<Self> {
        fixture.validate()?;
        ensure!(
            property(fixture.hwnd, READY) == 1,
            "fixture guard is not ready"
        );
        ensure!(
            property(fixture.hwnd, ABORT) == 0,
            "physical input or session lock before session setup"
        );
        // SAFETY: only a standard EDIT child under the PID-validated fixture is searched.
        let edit = unsafe { FindWindowExW(Some(fixture.hwnd), None, w!("EDIT"), None) }?;
        ensure!(pid(edit) == fixture.pid, "fixture EDIT ownership mismatch");
        let mut qpc_frequency = 0;
        // SAFETY: qpc_frequency is a writable counter-frequency output.
        unsafe { QueryPerformanceFrequency(&raw mut qpc_frequency) }?;
        let mut original_cursor = POINT::default();
        // SAFETY: original_cursor is a writable output; no input or focus is changed.
        unsafe { GetCursorPos(&raw mut original_cursor) }?;
        Ok(Self {
            fixture,
            shell,
            edit,
            end,
            held: [false; 512],
            original_cursor,
            pointer_moved: false,
            batches: 0,
            first_qpc: None,
            last_qpc: None,
            qpc_frequency,
        })
    }

    pub(crate) fn guard(&self) -> Result<()> {
        ensure!(Instant::now() < self.end, "input phase deadline exceeded");
        self.fixture.validate()?;
        self.shell.validate()?;
        ensure!(
            property(self.fixture.hwnd, ABORT) == 0,
            "physical input or session lock: session aborted, no retry"
        );
        // SAFETY: only the PID is inspected, never another application's window content.
        let foreground = pid(unsafe { GetForegroundWindow() });
        ensure!(
            foreground == self.fixture.pid || foreground == self.shell.pid,
            "foreground left the owned fixture/product processes"
        );
        input_desktop_available()?;
        self.check_capture()?;
        Ok(())
    }

    pub(crate) fn activate_fixture(&self, checkpoint: InputCheckpoint) -> Result<()> {
        self.fixture.validate()?;
        self.shell.validate()?;
        ensure!(
            Instant::now() < self.end && property(self.fixture.hwnd, ABORT) == 0,
            "physical activity, session lock or deadline before foreground transfer"
        );
        input_desktop_available()?;
        self.check_capture()?;
        for key in 1..256 {
            // SAFETY: preflight reads down states only, before any session input is injected.
            let down = unsafe { GetAsyncKeyState(key) } < 0;
            ensure!(
                !down,
                "a key or mouse button is already held; foreground transfer aborted"
            );
        }
        ensure!(
            Instant::now() < self.end && property(self.fixture.hwnd, ABORT) == 0,
            "physical activity, session lock or deadline before foreground transfer"
        );
        ensure!(
            input_checkpoint()? == checkpoint,
            "input activity changed during fixture startup; foreground transfer aborted"
        );
        // SAFETY: the owned fixture passed the non-activating preflight; this is the single authorized foreground transfer.
        let activated = unsafe { SetForegroundWindow(self.fixture.hwnd) }.as_bool();
        ensure!(activated, "Windows refused owned foreground activation");
        self.guard()
    }

    fn check_capture(&self) -> Result<()> {
        let mut info = GUITHREADINFO {
            cbSize: size::<GUITHREADINFO>(),
            ..Default::default()
        };
        // SAFETY: thread zero queries the foreground thread and info has its documented writable size.
        unsafe { GetGUIThreadInfo(0, &raw mut info) }?;
        ensure!(
            [info.hwndCapture, info.hwndMenuOwner, info.hwndMoveSize]
                .into_iter()
                .all(|window| window.is_invalid()
                    || pid(window) == self.fixture.pid
                    || pid(window) == self.shell.pid),
            "another application's window owns mouse capture, a menu or a move/size loop; session aborted without releasing it"
        );
        Ok(())
    }

    pub(crate) fn wait(&self, duration: Duration) -> Result<()> {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            self.guard()?;
            sleep(Duration::from_millis(1));
        }
        self.guard()
    }

    fn send(&mut self, inputs: &[INPUT]) -> Result<()> {
        self.guard()?;
        let mut qpc = 0;
        // SAFETY: qpc is a writable counter output.
        unsafe { QueryPerformanceCounter(&raw mut qpc) }?;
        self.first_qpc.get_or_insert(qpc);
        self.last_qpc = Some(qpc);
        // SAFETY: every input is fully initialized and carries the session's injection marker.
        let accepted = unsafe { SendInput(inputs, i32::try_from(size::<INPUT>())?) };
        for input in inputs.iter().take(usize::try_from(accepted)?) {
            if input.r#type == INPUT_KEYBOARD {
                // SAFETY: the INPUT tag identifies the initialized keyboard union member.
                let key = unsafe { input.Anonymous.ki };
                let scan = key.wScan
                    | if key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY) {
                        0x100
                    } else {
                        0
                    };
                self.held[usize::from(scan)] = !key.dwFlags.contains(KEYEVENTF_KEYUP);
            } else if input.r#type == INPUT_MOUSE {
                self.pointer_moved = true;
            }
        }
        ensure!(
            usize::try_from(accepted)? == inputs.len(),
            "SendInput accepted an incomplete batch"
        );
        self.batches += 1;
        self.guard()
    }

    pub(crate) fn modifier(&mut self, scan: u16, edge: InputEdge) -> Result<()> {
        ensure!(
            matches!(scan, 0x1d | 0x38 | 0x138),
            "unsupported session modifier"
        );
        self.send(&[scan_input(scan, edge)])
    }

    pub(crate) fn tap(&mut self, scan: u16) -> Result<()> {
        ensure!(scan < 512, "unsupported session scan code");
        self.send(&[
            scan_input(scan, InputEdge::Down),
            scan_input(scan, InputEdge::Up),
        ])
    }

    pub(crate) fn release_injected(&mut self) -> Result<()> {
        let mut failed = false;
        for index in 0..self.held.len() {
            if self.held[index] {
                let scan = u16::try_from(index)?;
                let input = scan_input(scan, InputEdge::Up);
                // SAFETY: cleanup releases only a key acknowledged by SendInput, even after an abort or partial batch.
                let accepted = unsafe { SendInput(&[input], i32::try_from(size::<INPUT>())?) };
                failed |= accepted != 1;
                self.held[index] = false;
            }
        }
        ensure!(!failed, "best-effort injected-key release was incomplete");
        Ok(())
    }

    pub(crate) fn move_pointer(&mut self, point: POINT, owner: Window) -> Result<()> {
        self.guard()?;
        // SAFETY: this only resolves ownership at the proposed destination, without sending input.
        let actual = unsafe { WindowFromPoint(point) };
        ensure!(
            pid(actual) == owner.pid,
            "pointer destination is not owned by the expected fixture/product PID"
        );
        let [left, top, width, height] = desktop_rect();
        ensure!(
            width > 1
                && height > 1
                && point.x >= left
                && point.y >= top
                && point.x < left + width
                && point.y < top + height,
            "pointer destination is outside the virtual desktop"
        );
        let dx = i32::try_from(i64::from(point.x - left) * 65535 / i64::from(width - 1))?;
        let dy = i32::try_from(i64::from(point.y - top) * 65535 / i64::from(height - 1))?;
        self.send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    dwExtraInfo: MARKER,
                    ..Default::default()
                },
            },
        }])
    }

    pub(crate) fn restore_cursor(&mut self) -> Result<&'static str> {
        if !self.pointer_moved {
            return Ok("unchanged");
        }
        self.guard()?;
        // SAFETY: this resolves ownership only; no cursor movement has occurred.
        let destination = unsafe { WindowFromPoint(self.original_cursor) };
        let owner = if pid(destination) == self.fixture.pid {
            self.fixture
        } else if pid(destination) == self.shell.pid {
            self.shell
        } else {
            return Ok("skipped_non_owned_destination");
        };
        self.move_pointer(self.original_cursor, owner)?;
        self.pointer_moved = false;
        Ok("restored")
    }

    pub(crate) fn hover_points(&self) -> Result<[POINT; 2]> {
        self.guard()?;
        let dock = self.shell.rect()?;
        let [left, top, width, height] = desktop_rect();
        ensure!(
            width >= 640 && height >= 400,
            "desktop is too small for the owned hover fixture"
        );
        let center = POINT {
            x: i32::midpoint(dock.left, dock.right),
            y: i32::midpoint(dock.top, dock.bottom),
        };
        let x = (center.x - 320).clamp(left, left + width - 640);
        let y = (center.y - 200).clamp(top, top + height - 400);
        // SAFETY: only the PID-validated fixture is moved/resized, without activation or Z-order changes.
        unsafe {
            SetWindowPos(
                self.fixture.hwnd,
                None,
                x,
                y,
                640,
                400,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }?;
        let leave_x = if dock.left - 25 >= left {
            dock.left - 25
        } else {
            dock.right + 25
        };
        Ok([
            center,
            POINT {
                x: leave_x,
                y: center.y,
            },
        ])
    }

    pub(crate) fn panel_visible(&self) -> Result<bool> {
        unsafe extern "system" fn count(window: HWND, param: LPARAM) -> BOOL {
            let state = std::ptr::with_exposed_provenance_mut::<(u32, RECT, u32, bool)>(
                param.0.cast_unsigned(),
            );
            // SAFETY: EnumWindows synchronously passes the live exclusive state pointer supplied below.
            let state = unsafe { &mut *state };
            // SAFETY: EnumWindows supplies the handle; visibility reads no content.
            if pid(window) == state.0 && unsafe { IsWindowVisible(window) }.as_bool() {
                let mut rect = RECT::default();
                // SAFETY: rect is writable and EnumWindows supplies the window handle.
                if unsafe { GetWindowRect(window, &raw mut rect) }.is_err() {
                    state.3 = true;
                } else if rect.right - rect.left > state.1.right - state.1.left
                    && rect.bottom - rect.top > state.1.bottom - state.1.top
                {
                    state.2 += 1;
                }
            }
            BOOL::from(true)
        }
        self.guard()?;
        let dock = self.shell.rect()?;
        let mut state = (self.shell.pid, dock, 0, false);
        // SAFETY: count receives the live state pointer for this synchronous enumeration only.
        unsafe {
            EnumWindows(
                Some(count),
                LPARAM((&raw mut state).expose_provenance().cast_signed()),
            )
        }?;
        ensure!(
            !state.3 && state.2 <= 1,
            "owned panel visibility/geometry could not be identified uniquely"
        );
        Ok(state.2 == 1)
    }

    pub(crate) fn clear_fixture(&self) -> Result<()> {
        self.guard()?;
        ensure!(
            self.fixture.is_foreground(),
            "fixture must be foreground for controlled text setup"
        );
        ensure!(
            pid(self.edit) == self.fixture.pid,
            "fixture EDIT ownership changed"
        );
        let mut cleared = 0;
        // SAFETY: WM_SETTEXT marshals this static empty string; only the fixture's known test content is cleared.
        let delivered = unsafe {
            SendMessageTimeoutW(
                self.edit,
                WM_SETTEXT,
                WPARAM(0),
                LPARAM(w!("").0.expose_provenance().cast_signed()),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&raw mut cleared),
            )
        };
        ensure!(
            delivered.0 != 0 && cleared != 0,
            "owned fixture text setup timed out or failed"
        );
        self.guard()
    }

    pub(crate) fn expected_layout_text(&self) -> Result<bool> {
        self.guard()?;
        ensure!(
            pid(self.edit) == self.fixture.pid,
            "fixture EDIT ownership changed"
        );
        let mut text = [0u16; 16];
        let mut length = 0;
        // SAFETY: WM_GETTEXT marshals this writable buffer across processes; only our owned EDIT is read after controlled typing and a clear guard.
        let delivered = unsafe {
            SendMessageTimeoutW(
                self.edit,
                WM_GETTEXT,
                WPARAM(text.len()),
                LPARAM(text.as_mut_ptr().expose_provenance().cast_signed()),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&raw mut length),
            )
        };
        ensure!(delivered.0 != 0, "owned fixture text inspection timed out");
        self.guard()?;
        Ok(length == 2 && text[..2] == [0x40, 0x20ac])
    }
}

fn scan_input(scan: u16, edge: InputEdge) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if scan & 0x100 != 0 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if edge == InputEdge::Up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan & 0xff,
                dwFlags: flags,
                dwExtraInfo: MARKER,
                ..Default::default()
            },
        },
    }
}

fn desktop_rect() -> [i32; 4] {
    // SAFETY: these calls read virtual-desktop geometry only.
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    // SAFETY: this call reads virtual-desktop geometry only.
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    // SAFETY: this call reads virtual-desktop geometry only.
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    // SAFETY: this call reads virtual-desktop geometry only.
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    [left, top, width, height]
}

#[derive(Debug)]
pub(crate) struct Layouts {
    original: Vec<HKL>,
    fixture: HKL,
    runner: HKL,
    added: Vec<HKL>,
}

impl Layouts {
    pub(crate) fn capture(session: &Session) -> Result<Self> {
        session.guard()?;
        // SAFETY: querying the list size does not change layouts.
        let count = unsafe { GetKeyboardLayoutList(None) };
        ensure!(count > 0, "cannot snapshot loaded keyboard layouts");
        let mut original = vec![HKL::default(); usize::try_from(count)?];
        // SAFETY: original is writable for the advertised number of handles.
        let read = unsafe { GetKeyboardLayoutList(Some(&mut original)) };
        ensure!(
            read == count,
            "keyboard layout list changed during snapshot"
        );
        // SAFETY: the fixture window's PID was validated by the session guard.
        let thread = unsafe { GetWindowThreadProcessId(session.fixture.hwnd, None) };
        // SAFETY: this reads only the fixture thread's active layout.
        let fixture = unsafe { GetKeyboardLayout(thread) };
        // SAFETY: this reads the runner thread's active layout without changing it.
        let runner = unsafe { GetKeyboardLayout(0) };
        Ok(Self {
            original,
            fixture,
            runner,
            added: Vec::new(),
        })
    }

    pub(crate) fn load(&mut self, session: &Session, selection: Layout) -> Result<()> {
        session.guard()?;
        let klid = match selection {
            Layout::German => w!("00000407"),
            Layout::Spanish => w!("0000040A"),
        };
        // SAFETY: loading without ACTIVATE/REORDER does not activate this thread or alter preferred languages.
        let layout =
            unsafe { LoadKeyboardLayoutW(klid, ACTIVATE_KEYBOARD_LAYOUT_FLAGS::default()) }?;
        if !self.original.contains(&layout) && !self.added.contains(&layout) {
            self.added.push(layout);
        }
        Self::request(session, layout)
    }

    fn request(session: &Session, layout: HKL) -> Result<()> {
        session.fixture.validate()?;
        // SAFETY: only the owned fixture is asked to change its thread layout; no persistent settings are touched.
        unsafe {
            PostMessageW(
                Some(session.fixture.hwnd),
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(layout.0.expose_provenance().cast_signed()),
            )
        }?;
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: this queries the owned fixture's thread ID.
            let thread = unsafe { GetWindowThreadProcessId(session.fixture.hwnd, None) };
            // SAFETY: this reads only that thread's layout handle.
            if unsafe { GetKeyboardLayout(thread) } == layout {
                return Ok(());
            }
            ensure!(
                Instant::now() < until,
                "owned fixture layout change timed out"
            );
            sleep(Duration::from_millis(1));
        }
    }

    pub(crate) fn restore(&self, session: &Session) -> Result<()> {
        let restore = Self::request(session, self.fixture);
        let mut failed = false;
        // SAFETY: this reads only the runner's own thread layout.
        if unsafe { GetKeyboardLayout(0) } != self.runner {
            // SAFETY: restoration affects only this runner thread, never another application's active layout.
            failed |= unsafe {
                ActivateKeyboardLayout(self.runner, ACTIVATE_KEYBOARD_LAYOUT_FLAGS::default())
            }
            .is_err();
        }
        for layout in &self.added {
            // SAFETY: only handles absent from the original list and loaded by this session are unloaded.
            failed |= unsafe { UnloadKeyboardLayout(*layout) }.is_err();
        }
        ensure!(
            !failed,
            "layout unload/runner restore failed; fixture restore result={restore:?}"
        );
        restore?;
        // SAFETY: this reads only the number of currently loaded layouts.
        let count = unsafe { GetKeyboardLayoutList(None) };
        ensure!(count > 0, "cannot verify restored keyboard layout list");
        let mut current = vec![HKL::default(); usize::try_from(count)?];
        // SAFETY: current is writable for the advertised number of layout handles.
        let read = unsafe { GetKeyboardLayoutList(Some(&mut current)) };
        ensure!(
            read == count
                && current.len() == self.original.len()
                && self.original.iter().all(|layout| current.contains(layout)),
            "loaded keyboard layout list differs from its snapshot; external changes were not undone"
        );
        Ok(())
    }
}
