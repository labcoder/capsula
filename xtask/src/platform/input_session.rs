//! Owned Win32 fixture, physical-input guard and tagged native input for approved sessions.

use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetThreadDesktop,
    GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentThreadId, NORMAL_PRIORITY_CLASS};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, GetKeyboardLayoutList, GetLastInputInfo, HKL, INPUT,
    INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, LASTINPUTINFO, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GUITHREADINFO, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SMTO_ABORTIFHUNG, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageTimeoutW, SetForegroundWindow,
    SetWindowPos, WM_CLOSE, WM_GETTEXT, WM_SETTEXT, WindowFromPoint,
};
use windows::core::{BOOL, w};

mod fixture;

pub(crate) use fixture::fixture;
use fixture::{ABORT, CHANGES, ESCAPES, READY, property};

pub(crate) const MARKER: usize = 0x4350_5331;

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
        Ok(Self { original, fixture })
    }

    pub(crate) fn load(session: &Session, selection: Layout) -> Result<()> {
        session.guard()?;
        let selection = match selection {
            Layout::German => 0,
            Layout::Spanish => 1,
        };
        let loaded = Self::request(session, fixture::LOAD_LAYOUT, selection)?;
        // SAFETY: only the PID-validated fixture's thread is queried.
        let thread = unsafe { GetWindowThreadProcessId(session.fixture.hwnd, None) };
        // SAFETY: this reads the fixture thread's active layout after its acknowledgement.
        let active = unsafe { GetKeyboardLayout(thread) };
        ensure!(
            active.0.addr() == loaded,
            "fixture did not acknowledge the requested active layout"
        );
        session.guard()
    }

    fn request(session: &Session, message: u32, selection: usize) -> Result<usize> {
        session.fixture.validate()?;
        let mut response = 0usize;
        // SAFETY: the owned fixture accepts fixed numeric commands, never cross-process pointers.
        let delivered = unsafe {
            SendMessageTimeoutW(
                session.fixture.hwnd,
                message,
                WPARAM(selection),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&raw mut response),
            )
        };
        ensure!(
            delivered.0 != 0 && response != 0,
            "fixture layout request failed or timed out"
        );
        Ok(response)
    }

    pub(crate) fn restore(&self, session: &Session) -> Result<()> {
        Self::request(session, fixture::RESTORE_LAYOUTS, 0)?;
        // SAFETY: only the PID-validated fixture thread is queried.
        let thread = unsafe { GetWindowThreadProcessId(session.fixture.hwnd, None) };
        // SAFETY: reading its active layout changes no other thread.
        let restored = unsafe { GetKeyboardLayout(thread) };
        ensure!(
            restored == self.fixture,
            "fixture original layout was not restored"
        );
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
