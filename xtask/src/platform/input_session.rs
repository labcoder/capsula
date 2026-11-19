//! Owned Win32 fixture, physical-input guard and tagged native input for approved sessions.

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{INPUT_SESSION_MARKER, OBSERVER_REPAINT_MESSAGE};
use windows::Win32::Foundation::{
    ERROR_SUCCESS, ERROR_TIMEOUT, GetLastError, HANDLE, HWND, LPARAM, POINT, RECT, SetLastError,
    WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetThreadDesktop,
    GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, NID_EXTERNAL_PEN, NID_EXTERNAL_TOUCH,
    NID_INTEGRATED_PEN, NID_INTEGRATED_TOUCH, NID_MULTI_INPUT, NID_READY, PostMessageW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_DIGITIZER, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_CLOSE,
};
use windows::core::{BOOL, PWSTR};

mod activation;
mod fixture;
mod gate;
mod guard;
mod quiet;
mod registry;
mod session;
mod switch_window;

pub(crate) use fixture::fixture;
use fixture::{
    CHANGES, CLICK_POINT, CLICK_QPC, CLICKS, ESCAPES, IdleRefusal, PREPARE_IDLE, property,
};
pub(crate) use quiet::quiet;
pub(crate) use registry::Registry;
pub(crate) use session::{Session, Target};

pub(crate) const MARKER: usize = INPUT_SESSION_MARKER;

/// The largest fixture cover, in physical pixels per side.
const MAX_COVER: i32 = 1024;

/// The longest a fixture may live.
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// Fails, read-only, unless Windows reports no pen or touch digitizer: the guards watch only the
/// mouse and keyboard, so pen or touch input could reach the desktop unseen.
fn mouse_and_keyboard_only() -> Result<()> {
    // SAFETY: reads one hardware-capability metric; nothing changes.
    let flags = unsafe { GetSystemMetrics(SM_DIGITIZER) }.cast_unsigned();
    ensure!(
        digitizer_free(flags),
        "unsupported desktop: SM_DIGITIZER reports {flags:#x} (pen, touch or unknown digitizer \
         bits), and the session guards cover only mouse and keyboard"
    );
    Ok(())
}

/// No pen or touch device bit, ready or not, and no bit the docs don't define. Multi-input and
/// ready alone describe no device, so they pass.
fn digitizer_free(flags: u32) -> bool {
    let devices = NID_INTEGRATED_TOUCH | NID_EXTERNAL_TOUCH | NID_INTEGRATED_PEN | NID_EXTERNAL_PEN;
    let known = devices | NID_MULTI_INPUT | NID_READY;
    flags & devices == 0 && flags & !known == 0
}

/// `GetLastInputInfo`'s tick of the last input of any kind; no content is read.
fn last_input() -> Result<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: size::<LASTINPUTINFO>(),
        ..Default::default()
    };
    // SAFETY: info has the documented writable size; no input content is read.
    let read = unsafe { GetLastInputInfo(&raw mut info) }.as_bool();
    ensure!(read, "cannot read the last input time");
    Ok(info.dwTime)
}

fn foreground_refusal() -> String {
    // SAFETY: samples only the current foreground handle without reading window text.
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_pid = pid(foreground);
    let process = foreground_process_name(foreground_pid);
    let desktop = input_desktop_state().map(|state| state.default);
    format!(
        "foreground_pid={foreground_pid} foreground_process={:?} input_desktop_default={:?}",
        process.ok(),
        desktop.ok()
    )
}

fn foreground_process_name(process_id: u32) -> Result<String> {
    // SAFETY: opens a numeric PID for read-only image-name queries, without inheriting it.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }?;
    // SAFETY: the successful OpenProcess call transfers its uniquely owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut image = vec![0u16; 32768];
    let mut length = u32::try_from(image.len())?;
    // SAFETY: the live handle allows queries and image/length describe the writable buffer.
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(handle.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &raw mut length,
        )
    }?;
    let image = image
        .get(..usize::try_from(length)?)
        .context("process image length exceeds its buffer")?;
    process_basename(&String::from_utf16_lossy(image))
}

fn process_basename(image: &str) -> Result<String> {
    Ok(Path::new(image)
        .file_name()
        .context("foreground process image has no basename")?
        .to_string_lossy()
        .into_owned())
}

fn desktop_is_default(name: &[u16]) -> bool {
    name.iter()
        .copied()
        .take_while(|unit| *unit != 0)
        .eq("Default".encode_utf16())
}

/// The input desktop is Default and this thread's own; a failed query counts as neither.
fn desktop_ours() -> bool {
    input_desktop_state().is_ok_and(|state| state.default && state.assigned)
}

fn input_desktop_available() -> Result<()> {
    ensure!(
        input_desktop_state()?.assigned,
        "input desktop changed or is locked"
    );
    Ok(())
}

struct DesktopState {
    default: bool,
    assigned: bool,
}

fn input_desktop_state() -> Result<DesktopState> {
    // SAFETY: opens the input desktop read-only, without inheriting or switching it.
    let input =
        unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS::default(), false, DESKTOP_READOBJECTS) }?;
    let checked: Result<DesktopState> = (|| {
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
        Ok(DesktopState {
            default: desktop_is_default(&input_name),
            assigned: input_name == assigned_name,
        })
    })();
    // SAFETY: only the handle owned by the successful OpenInputDesktop call is closed.
    let closed = unsafe { CloseDesktop(input) };
    match (checked, closed) {
        (Ok(state), Ok(())) => Ok(state),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FixturePlacement {
    Default,
    AwayFromCursor,
}

pub(super) const AWAY_FROM_CURSOR: &str = "--away-from-cursor";
pub(crate) const FIXTURE_PLACEMENT_REFUSED: i32 = 13;

/// Starts the fixture, which closes itself after `lifetime` (at most an hour). It shows the feel
/// verdicts and Ready, and arms nothing until the owner physically clicks Ready.
pub(crate) fn spawn_fixture(
    exe: &Path,
    lifetime: Duration,
    placement: FixturePlacement,
) -> Result<Child> {
    ensure!(
        (Duration::from_secs(1)..=MAX_LIFETIME).contains(&lifetime),
        "fixture lifetime must be 1 s to 1 h"
    );
    let mut command = Command::new(exe);
    command.arg("input-fixture");
    if placement == FixturePlacement::AwayFromCursor {
        command.arg(AWAY_FROM_CURSOR);
    }
    Ok(command
        .arg(format!("--lifetime-secs={}", lifetime.as_secs()))
        .creation_flags(NORMAL_PRIORITY_CLASS.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

fn monitor_work_area(monitor: HMONITOR) -> Result<RECT> {
    let mut info = MONITORINFO {
        cbSize: size::<MONITORINFO>(),
        ..Default::default()
    };
    // SAFETY: info has its documented writable size; the caller obtained the monitor from Win32.
    let found = unsafe { GetMonitorInfoW(monitor, &raw mut info) };
    ensure!(found.as_bool(), "cannot read fixture monitor work area");
    Ok(info.rcWork)
}

fn size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Win32 input structs fit in u32")
}

/// The absolute `SendInput` coordinate (0..=65535) of pixel `coordinate` on a virtual-desktop
/// axis starting at `origin` and `extent` pixels long, or `None` outside it. It aims at the
/// pixel's center, so Windows' mapping, pixel = coordinate * extent / 65536 rounded down, lands
/// on exactly that pixel at both ends and with a negative origin.
fn normalized(coordinate: i32, origin: i32, extent: i32) -> Option<i32> {
    let offset = i64::from(coordinate) - i64::from(origin);
    let extent = i64::from(extent);
    if !(0..extent).contains(&offset) {
        return None;
    }
    let center = ((2 * offset + 1) * 65536 + extent) / (2 * extent);
    i32::try_from(center.min(65535)).ok()
}

/// Two signed 32-bit values in one 64-bit message parameter.
fn pack(low: i32, high: i32) -> u64 {
    u64::from(low.cast_unsigned()) | (u64::from(high.cast_unsigned()) << 32)
}

fn unpack(value: u64) -> [i32; 2] {
    let bytes = value.to_le_bytes();
    [
        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    ]
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    hwnd: HWND,
    pub(crate) pid: u32,
}

/// The fixture EDIT's latest button-down after Ready, as its recipient saw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Receipt {
    pub(crate) count: usize,
    pub(crate) qpc: i64,
    /// Screen pixels.
    pub(crate) point: POINT,
}

fn pid(window: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: the API accepts an arbitrary handle and the PID output is writable.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    pid
}

fn class_name(window: HWND) -> String {
    let mut name = [0u16; 128];
    // SAFETY: name is a writable class-name buffer; no window text is read.
    let length = unsafe { GetClassNameW(window, &mut name) };
    String::from_utf16_lossy(&name[..usize::try_from(length).unwrap_or(0)])
}

/// Finds the child's one session window: the fixture by its class, before or after Ready, or
/// the product's presence proxy.
pub(crate) fn owned_window(child: &mut Child, kind: WindowKind) -> Result<Window> {
    struct Search {
        pid: u32,
        kind: WindowKind,
        fixture: String,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn collect(window: HWND, param: LPARAM) -> BOOL {
        let search = std::ptr::with_exposed_provenance_mut::<Search>(param.0.cast_unsigned());
        // SAFETY: EnumWindows synchronously passes the exclusive search pointer supplied below.
        let search = unsafe { &mut *search };
        // SAFETY: EnumWindows supplies the window handle for this callback.
        if pid(window) != search.pid || !unsafe { IsWindowVisible(window) }.as_bool() {
            return BOOL::from(true);
        }
        let name = class_name(window);
        let matches = match search.kind {
            WindowKind::Fixture => name == search.fixture,
            WindowKind::ProductProxy => name.ends_with("-presence"),
        };
        if matches {
            search.found.push(window);
        }
        BOOL::from(true)
    }
    ensure!(
        child.try_wait()?.is_none(),
        "owned child exited before window discovery"
    );
    let mut search = Search {
        pid: child.id(),
        kind,
        fixture: fixture::class_name(),
        found: Vec::new(),
    };
    // SAFETY: collect receives a live exclusive search pointer for this synchronous enumeration.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM((&raw mut search).expose_provenance().cast_signed()),
        )
    }?;
    ensure!(
        search.found.len() == 1,
        "expected exactly one owned session window, found {}",
        search.found.len()
    );
    Ok(Window {
        hwnd: search.found[0],
        pid: child.id(),
    })
}

/// Sends the fixture one numeric command; fails unless it accepts within a second.
fn request(fixture: Window, message: u32, wparam: WPARAM, lparam: LPARAM) -> Result<usize> {
    fixture
        .validate()
        .with_context(|| format!("fixture request {message:#x} send failed"))?;
    let mut response = 0usize;
    // SAFETY: clears this thread's error slot; the send can fail without setting it.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: the owned fixture accepts fixed numeric commands, never cross-process pointers.
    let sent = unsafe {
        SendMessageTimeoutW(
            fixture.hwnd,
            message,
            wparam,
            lparam,
            SMTO_ABORTIFHUNG,
            1000,
            Some(&raw mut response),
        )
    };
    let reply = if sent.0 != 0 {
        Ok(response)
    } else {
        // SAFETY: reads this thread's slot immediately after the failed send.
        Err(unsafe { GetLastError() })
    };
    request_result(message, reply)
}

fn request_result(message: u32, reply: std::result::Result<usize, WIN32_ERROR>) -> Result<usize> {
    let response = match reply {
        Ok(response) => response,
        Err(ERROR_TIMEOUT) => bail!("fixture request {message:#x} timed out"),
        Err(error) => {
            bail!(
                "fixture request {message:#x} send failed (Win32 code {})",
                error.0
            );
        }
    };
    if message == PREPARE_IDLE && response != 1 {
        if let Some((code, mask)) = IdleRefusal::queue_mask(response) {
            let reason = IdleRefusal::message(code);
            bail!(
                "fixture request {message:#x} refused (fixture code {code}): {reason}; queue_mask={mask:#06x}"
            );
        }
        let reason = IdleRefusal::message(response);
        bail!("fixture request {message:#x} refused (fixture code {response}): {reason}");
    }
    if response == 0 {
        bail!("fixture request {message:#x} refused (fixture code {response})");
    }
    Ok(response)
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

    /// Asks the product's gate observer for one dock repaint: a posted message to its own presence
    /// proxy, with no input and no foreground change.
    pub(crate) fn request_gate_repaint(self) -> Result<()> {
        self.validate()?;
        ensure!(
            class_name(self.hwnd).ends_with("-presence"),
            "gate repaints go only to the product's presence proxy"
        );
        // SAFETY: ownership and class were checked; the message carries no pointers.
        unsafe {
            PostMessageW(
                Some(self.hwnd),
                OBSERVER_REPAINT_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            )
        }?;
        Ok(())
    }

    pub(crate) fn is_foreground(self) -> bool {
        // SAFETY: this reads the current foreground HWND without accessing its content.
        pid(unsafe { GetForegroundWindow() }) == self.pid
    }

    /// Whether this exact window, rather than another window in its process, is foreground.
    pub(crate) fn is_foreground_window(self) -> bool {
        // SAFETY: this reads the foreground HWND without accessing its content.
        let foreground = unsafe { GetForegroundWindow() };
        foreground == self.hwnd
    }

    pub(crate) fn is_visible(self) -> Result<bool> {
        self.validate()?;
        // SAFETY: the queried handle still belongs to the expected process.
        Ok(unsafe { IsWindowVisible(self.hwnd) }.as_bool())
    }

    pub(crate) fn counts(self) -> Result<(usize, usize)> {
        self.validate()?;
        Ok((property(self.hwnd, CHANGES), property(self.hwnd, ESCAPES)))
    }

    /// The fixture EDIT's click receipts; zero until one arrives after Ready.
    pub(crate) fn receipt(self) -> Result<Receipt> {
        self.validate()?;
        let [x, y] = unpack(u64::try_from(property(self.hwnd, CLICK_POINT))?);
        Ok(Receipt {
            count: property(self.hwnd, CLICKS),
            qpc: i64::try_from(property(self.hwnd, CLICK_QPC))?,
            point: POINT { x, y },
        })
    }

    pub(crate) fn rect(self) -> Result<RECT> {
        self.validate()?;
        let mut rect = RECT::default();
        // SAFETY: the owned HWND is valid and rect is writable.
        unsafe { GetWindowRect(self.hwnd, &raw mut rect) }?;
        Ok(rect)
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

#[cfg(test)]
mod tests {
    use super::fixture::OPEN_GATE;
    use super::{
        ERROR_SUCCESS, ERROR_TIMEOUT, PREPARE_IDLE, desktop_is_default, digitizer_free, normalized,
        pack, process_basename, request_result, unpack,
    };
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    #[test]
    fn fixture_request_distinguishes_failed_sends_from_timeouts_and_refusals() {
        let error = |reply| request_result(OPEN_GATE, reply).unwrap_err().to_string();
        assert_eq!(
            error(Err(ERROR_TIMEOUT)),
            "fixture request 0x8017 timed out"
        );
        assert_eq!(
            error(Err(ERROR_ACCESS_DENIED)),
            "fixture request 0x8017 send failed (Win32 code 5)"
        );
        assert_eq!(
            error(Err(ERROR_SUCCESS)),
            "fixture request 0x8017 send failed (Win32 code 0)"
        );
        assert_eq!(
            error(Ok(0)),
            "fixture request 0x8017 refused (fixture code 0)"
        );
        assert_eq!(request_result(OPEN_GATE, Ok(2)).unwrap(), 2);
    }

    #[test]
    fn only_prepared_is_accepted_for_prepare_idle() {
        for (reply, label) in [
            (2, "fixture was already used or armed"),
            (3, "input desktop is not this session's Default desktop"),
            (4, "could not read last input"),
            (5, "invalid away duration"),
            (6, "could not register input guards"),
            (7, "input changed while the guard armed"),
            (8, "idle duration was too short"),
            (9, "the fixture had queued input"),
            (10, "could not update the activation marker"),
            (11, "could not raise the fixture without activation"),
            (12, "could not mark the fixture aborted"),
            (0, "unknown prepare-idle refusal"),
            (13, "unknown prepare-idle refusal"),
            (usize::MAX, "unknown prepare-idle refusal"),
        ] {
            assert_eq!(
                request_result(PREPARE_IDLE, Ok(reply))
                    .unwrap_err()
                    .to_string(),
                format!("fixture request 0x8019 refused (fixture code {reply}): {label}"),
            );
        }
        assert_eq!(request_result(PREPARE_IDLE, Ok(1)).unwrap(), 1);
    }

    #[test]
    fn queued_input_keeps_fixture_code_nine_and_names_the_high_word_mask() {
        assert_eq!(
            request_result(PREPARE_IDLE, Ok(0x0004_0209))
                .unwrap_err()
                .to_string(),
            "fixture request 0x8019 refused (fixture code 9): the fixture had queued input; queue_mask=0x0402"
        );
    }

    #[test]
    fn foreground_diagnostic_keeps_only_the_executable_basename() {
        let name = process_basename(r"C:\private-path-sentinel\nested\LogonUI.exe").unwrap();
        assert_eq!(name, "LogonUI.exe");
        assert!(!format!("{name:?}").contains("private-path-sentinel"));
        assert!(process_basename("").is_err());
        assert!(process_basename(r"C:\").is_err());
    }

    #[test]
    fn foreground_diagnostic_distinguishes_default_from_other_desktops() {
        for name in ["", "Defaul", "DefaultExtra", "Winlogon", "Screen-saver"] {
            let units = name.encode_utf16().chain([0]).collect::<Vec<_>>();
            assert!(!desktop_is_default(&units), "must reject {name}");
        }
        let mut default = [0u16; 256];
        for (to, from) in default.iter_mut().zip("Default".encode_utf16()) {
            *to = from;
        }
        assert!(desktop_is_default(&default));
    }

    #[test]
    fn any_pen_or_touch_bit_or_unknown_bit_is_unsupported() {
        for flags in [0, 0x40, 0x80, 0xc0] {
            assert!(digitizer_free(flags), "{flags:#x} names no device");
        }
        for flags in [0x01, 0x02, 0x04, 0x08, 0x81, 0xc2, 0x05, 0x10, 0x20, 0x100] {
            assert!(!digitizer_free(flags), "{flags:#x} must fail closed");
        }
    }

    /// Windows' documented inverse for absolute virtual-desktop moves.
    fn pixel(coordinate: i32, origin: i32, extent: i32) -> i32 {
        origin + i32::try_from(i64::from(coordinate) * i64::from(extent) / 65536).unwrap()
    }

    #[test]
    fn every_pixel_round_trips_through_the_absolute_mapping() {
        for (origin, extent) in [
            (0, 2560),
            (0, 1440),
            (-1920, 4480),
            (-1080, 2520),
            (0, 1),
            (5, 3),
        ] {
            for target in origin..origin + extent {
                let coordinate = normalized(target, origin, extent).unwrap();
                assert!((0..=65535).contains(&coordinate));
                assert_eq!(
                    pixel(coordinate, origin, extent),
                    target,
                    "{origin} {extent}"
                );
            }
        }
    }

    #[test]
    fn the_ends_map_to_the_first_and_last_pixel_and_outside_is_refused() {
        assert_eq!(normalized(0, 0, 2560), Some(13));
        assert_eq!(normalized(2559, 0, 2560), Some(65523));
        assert_eq!(normalized(-1920, -1920, 4480), Some(7));
        assert_eq!(normalized(2560, 0, 2560), None);
        assert_eq!(normalized(-1921, -1920, 4480), None);
        assert_eq!(normalized(0, 0, 0), None);
    }

    #[test]
    fn packed_message_coordinates_keep_their_sign() {
        assert_eq!(unpack(pack(-1920, 1080)), [-1920, 1080]);
        assert_eq!(pack(1, -1), 0xffff_ffff_0000_0001);
    }
}
