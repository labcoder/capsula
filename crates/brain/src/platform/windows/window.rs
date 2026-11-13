//! The window a hold points at (design B): the z-order walk from the pointer and the checks that
//! refuse a window before any pixels. Capture itself is `capture.rs`'s, through one window item.

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect, WindowRef};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, GetWindowRgn, HRGN, PtInRegion, RGN_ERROR,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GA_ROOT, GW_HWNDNEXT, GWL_EXSTYLE, GetAncestor, GetClassNameW,
    GetLayeredWindowAttributes, GetTopWindow, GetWindow, GetWindowDisplayAffinity,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, WDA_NONE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};
use windows::core::{PCWSTR, w};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::screen::{Refusal, Target, TopWindow};

/// The shell's desktop windows: reaching one means nothing eligible is under the pointer.
const DESKTOP: [&str; 2] = ["Progman", "WorkerW"];
/// A UWP app's root; the app itself runs in the hosted core window's process.
const FRAME_HOST: &str = "ApplicationFrameWindow";
const CORE_WINDOW: PCWSTR = w!("Windows.UI.Core.CoreWindow");
/// Top-level windows read before giving up, so a z-order that changes under the walk can't loop.
const MAX_WALK: usize = 4096;

/// What the walk knows about one top-level window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one fact per skip rule, read once by the walk's step"
)]
struct Facts {
    kumochi: bool,
    visible: bool,
    cloaked: bool,
    /// `WS_EX_TRANSPARENT`, or layered at zero alpha: clicks fall through it.
    transparent: bool,
    /// Inside its rectangle and its window region.
    contains: bool,
    desktop: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Skip,
    Stop,
    Take,
}

/// One window of the walk: a click passes through what's skipped, and the desktop ends it.
/// A disabled window is still taken, which `WindowFromPoint` wouldn't do.
fn step(facts: Facts) -> Step {
    if facts.kumochi || !facts.visible || facts.cloaked || facts.transparent || !facts.contains {
        Step::Skip
    } else if facts.desktop {
        Step::Stop
    } else {
        Step::Take
    }
}

/// The window a click at `point` would land on, Kumochi's aside, then its preflight.
pub(super) fn pointer_window(point: PhysicalPoint, shell: u32) -> Result<Target, Refusal> {
    // SAFETY: returns an opaque handle to the top of the desktop's z-order; nothing is read.
    let mut next = unsafe { GetTopWindow(None) }.ok();
    for _ in 0..MAX_WALK {
        let Some(window) = next else { break };
        match step(facts(window, point, shell)) {
            Step::Take => return preflight(window, shell),
            Step::Stop => break,
            Step::Skip => {}
        }
        // SAFETY: Windows validates the opaque handle and returns the next one down.
        next = unsafe { GetWindow(window, GW_HWNDNEXT) }.ok();
    }
    Err(Refusal::NoWindow)
}

fn facts(window: HWND, point: PhysicalPoint, shell: u32) -> Facts {
    // SAFETY: Windows validates the opaque handle and returns its visibility.
    if !unsafe { IsWindowVisible(window) }.as_bool() {
        return Facts::default();
    }
    Facts {
        kumochi: super::elements::excluded(owner(window).0, shell).unwrap_or(false),
        visible: true,
        cloaked: cloaked(window),
        transparent: transparent(window),
        contains: contains(window, point),
        desktop: DESKTOP.contains(&class(window).as_str()),
    }
}

/// Refuses before any pixels: minimised, cloaked, Kumochi's, elevated or capture-protected.
fn preflight(window: HWND, shell: u32) -> Result<Target, Refusal> {
    // SAFETY: this only queries the opaque handle.
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    // SAFETY: Windows validates the opaque handle.
    if unsafe { IsIconic(root) }.as_bool() {
        return Err(Refusal::Minimized);
    }
    if cloaked(root) {
        return Err(Refusal::Cloaked);
    }
    let (pid, tid) = owner(root);
    if pid == 0 || tid == 0 {
        return Err(Refusal::WindowChanged);
    }
    let hosted = (class(root) == FRAME_HOST)
        .then(|| core_window(root))
        .flatten();
    for process in std::iter::once(pid).chain(hosted) {
        if super::elements::excluded(process, shell).unwrap_or(false) {
            return Err(Refusal::Excluded);
        }
        if elevated(process) {
            return Err(Refusal::Elevated);
        }
    }
    let mut affinity = 0;
    // SAFETY: `affinity` is writable; Windows validates the handle.
    let read = unsafe { GetWindowDisplayAffinity(root, &raw mut affinity) };
    if read.is_err() || affinity != WDA_NONE.0 {
        return Err(Refusal::Protected);
    }
    let frame = frame(root).ok_or(Refusal::WindowChanged)?;
    // SAFETY: Windows validates the opaque handle.
    let dpi = unsafe { GetDpiForWindow(root) };
    if dpi == 0 {
        return Err(Refusal::WindowChanged);
    }
    Ok(Target {
        top: TopWindow {
            window: WindowRef(u64::try_from(root.0.addr()).map_err(|_| Refusal::WindowChanged)?),
            pid,
            rect: frame,
        },
        tid,
        dpi,
    })
}

/// The target's handle, as the walk resolved it.
pub(super) fn handle(target: &Target) -> Result<HWND, Refusal> {
    usize::try_from(target.top.window.0)
        .map(|addr| HWND(std::ptr::without_provenance_mut(addr)))
        .map_err(|_| Refusal::WindowChanged)
}

/// Whether the window is still the one the walk resolved: same process and thread, frame and
/// DPI, and not minimised.
pub(super) fn unchanged(target: &Target) -> bool {
    let Ok(window) = handle(target) else {
        return false;
    };
    // SAFETY: Windows validates the opaque handle.
    let iconic = unsafe { IsIconic(window) }.as_bool();
    // SAFETY: Windows validates the opaque handle.
    let dpi = unsafe { GetDpiForWindow(window) };
    !iconic
        && owner(window) == (target.top.pid, target.tid)
        && frame(window) == Some(target.top.rect)
        && dpi == target.dpi
}

fn owner(window: HWND) -> (u32, u32) {
    let mut pid = 0;
    // SAFETY: `pid` is writable; Windows validates the handle.
    let tid = unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    (pid, tid)
}

fn attribute<T: Default>(window: HWND, which: DWMWINDOWATTRIBUTE) -> Option<T> {
    let mut value = T::default();
    let size = u32::try_from(size_of::<T>()).ok()?;
    // SAFETY: `value` is writable for exactly `size` bytes, the attribute's documented type.
    unsafe { DwmGetWindowAttribute(window, which, (&raw mut value).cast(), size) }.ok()?;
    Some(value)
}

fn cloaked(window: HWND) -> bool {
    attribute::<u32>(window, DWMWA_CLOAKED).is_some_and(|cloak| cloak != 0)
}

/// The extended frame bounds: the window's visible surface, without its invisible borders.
fn frame(window: HWND) -> Option<PhysicalRect> {
    attribute::<RECT>(window, DWMWA_EXTENDED_FRAME_BOUNDS).map(super::elements::rectangle)
}

fn transparent(window: HWND) -> bool {
    // SAFETY: reads one style word of a validated handle.
    let styles = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) }.cast_unsigned() & 0xFFFF_FFFF;
    let styles = u32::try_from(styles).unwrap_or_default();
    if styles & WS_EX_TRANSPARENT.0 != 0 {
        return true;
    }
    if styles & WS_EX_LAYERED.0 == 0 {
        return false;
    }
    let (mut alpha, mut flags) = (0_u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS::default());
    // SAFETY: both outputs are writable; a window drawn by UpdateLayeredWindow fails here.
    let read = unsafe {
        GetLayeredWindowAttributes(window, None, Some(&raw mut alpha), Some(&raw mut flags))
    };
    read.is_ok() && flags.contains(LWA_ALPHA) && alpha == 0
}

fn contains(window: HWND, point: PhysicalPoint) -> bool {
    let mut rect = RECT::default();
    // SAFETY: `rect` is writable; Windows validates the handle.
    if unsafe { GetWindowRect(window, &raw mut rect) }.is_err() {
        return false;
    }
    let [x, y] = point.0;
    if !(rect.left <= x && x < rect.right && rect.top <= y && y < rect.bottom) {
        return false;
    }
    let region = Region::new();
    // SAFETY: copies the window's region, if it has one, into this owned region.
    if unsafe { GetWindowRgn(window, region.0) } == RGN_ERROR {
        return true;
    }
    // SAFETY: the region is live; its coordinates are relative to the window's corner.
    unsafe { PtInRegion(region.0, x - rect.left, y - rect.top) }.as_bool()
}

struct Region(HRGN);

impl Region {
    fn new() -> Self {
        // SAFETY: creates an empty region this guard deletes.
        Self(unsafe { CreateRectRgn(0, 0, 0, 0) })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the region came from CreateRectRgn and is deleted once.
        let deleted = unsafe { DeleteObject(self.0.into()) };
        if !deleted.as_bool() {
            tracing::debug!("window region not deleted");
        }
    }
}

fn class(window: HWND) -> String {
    let mut name = [0_u16; 64];
    // SAFETY: the buffer is writable; Windows writes at most its length.
    let len = unsafe { GetClassNameW(window, &mut name) };
    usize::try_from(len)
        .ok()
        .and_then(|len| name.get(..len))
        .map(String::from_utf16_lossy)
        .unwrap_or_default()
}

/// The process of a UWP frame's hosted core window, when it has one.
fn core_window(root: HWND) -> Option<u32> {
    // SAFETY: searches the frame's children by class; nothing is read.
    let core = unsafe { FindWindowExW(Some(root), None, CORE_WINDOW, PCWSTR::null()) }.ok()?;
    Some(owner(core).0).filter(|pid| *pid != 0)
}

/// Whether `pid` runs elevated. A process this worker can't open or read counts as elevated.
fn elevated(pid: u32) -> bool {
    // SAFETY: only query access is requested; Windows validates this process id.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return true;
    }
    // SAFETY: this successful OpenProcess result is owned and closed exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut token = std::ptr::null_mut();
    // SAFETY: the process handle is live and `token` is writable.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &raw mut token) } == 0 {
        return true;
    }
    // SAFETY: this successful OpenProcessToken result is owned and closed exactly once.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let size = u32::try_from(size_of::<TOKEN_ELEVATION>()).expect("a four-byte struct");
    let mut written = 0;
    // SAFETY: the token is live and `elevation` is writable for `size` bytes.
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&raw mut elevation).cast(),
            size,
            &raw mut written,
        )
    };
    read == 0 || elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests;
