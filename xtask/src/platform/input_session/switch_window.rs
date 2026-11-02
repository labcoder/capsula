//! The fixture's bounded alternate top-level window for the wrong-foreground-restore check.

use std::cell::Cell;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{DISPLAY_NAME, OBJECT_PREFIX};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, GetMonitorInfoW, GetSysColorBrush, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_HWNDPARENT, GetWindowLongPtrW,
    RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, WINDOW_EX_STYLE, WM_APP, WM_DESTROY, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::fixture::{ABORT, OPEN, set_property};
use super::{Window, class_name, fixture, pack, property, request, size, unpack};

pub(super) const CREATE: u32 = WM_APP + 26;
pub(super) const CLOSE: u32 = WM_APP + 27;
const HANDLE: PCWSTR = w!("InputSessionSwitchWindow");
/// More than the two focus phases need, but never an unbounded stream of windows.
const MAX_CREATIONS: u8 = 8;
const WIDTH: i32 = 240;
const HEIGHT: i32 = 160;
const MARGIN: i32 = 16;

thread_local! {
    static ALTERNATE: Cell<Option<HWND>> = const { Cell::new(None) };
    static CREATIONS: Cell<u8> = const { Cell::new(0) };
}

fn alternate_class() -> String {
    format!("{OBJECT_PREFIX}-input-switch-fixture")
}

pub(super) fn register(instance: HINSTANCE) -> Result<()> {
    let name = HSTRING::from(alternate_class());
    // SAFETY: the system owns this standard background brush; the fixture never deletes it.
    let background = unsafe { GetSysColorBrush(COLOR_WINDOW) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hbrBackground: background,
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class has a static callback and its name lives through registration.
    let atom = unsafe { RegisterClassW(&raw const class) };
    ensure!(atom != 0, "alternate fixture class registration failed");
    Ok(())
}

fn work_area(window: HWND) -> Result<RECT> {
    // SAFETY: obtains only monitor geometry for the supplied window, without reading content.
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size::<MONITORINFO>(),
        ..Default::default()
    };
    // SAFETY: info has the documented writable size and monitor came from MonitorFromWindow.
    let found = unsafe { GetMonitorInfoW(monitor, &raw mut info) };
    ensure!(found.as_bool(), "cannot read fixture monitor work area");
    Ok(info.rcWork)
}

fn overlaps(a: RECT, b: RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

fn placement(work: RECT, avoid: [RECT; 3]) -> Option<RECT> {
    for y in [work.bottom - MARGIN - HEIGHT, work.top + MARGIN] {
        for x in [work.left + MARGIN, work.right - MARGIN - WIDTH] {
            let rect = RECT {
                left: x,
                top: y,
                right: x + WIDTH,
                bottom: y + HEIGHT,
            };
            if rect.left >= work.left
                && rect.top >= work.top
                && rect.right <= work.right
                && rect.bottom <= work.bottom
                && avoid.into_iter().all(|obstacle| !overlaps(rect, obstacle))
            {
                return Some(rect);
            }
        }
    }
    None
}

/// One live alternate at a time; its lifetime never exceeds the main fixture's timer.
pub(super) fn create(root: HWND, wp: WPARAM) -> Result<()> {
    ensure!(
        property(root, OPEN) != 0 && property(root, ABORT) == 0,
        "alternate needs the open fixture gate"
    );
    ensure!(
        ALTERNATE.get().is_none(),
        "alternate fixture already exists"
    );
    let count = CREATIONS.get();
    ensure!(
        count < MAX_CREATIONS,
        "alternate fixture creation limit reached"
    );
    CREATIONS.set(count + 1);
    let [x, y] = unpack(u64::try_from(wp.0)?);
    let work = work_area(root)?;
    ensure!(
        x >= work.left
            && y >= work.top
            && i64::from(x) + i64::from(WIDTH) <= i64::from(work.right)
            && i64::from(y) + i64::from(HEIGHT) <= i64::from(work.bottom),
        "alternate fixture placement out of bounds"
    );
    // SAFETY: obtains the fixture executable's own loaded module.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let name = HSTRING::from(alternate_class());
    let title = HSTRING::from(format!("{DISPLAY_NAME} switch fixture"));
    // SAFETY: this thread creates a top-level window with no owner, child, or borrowed data.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &name,
            &title,
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    ALTERNATE.set(Some(hwnd));
    // SAFETY: shows this thread's new window without activating it; false means previously hidden.
    let _was_visible = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    if let Err(error) = set_property(root, HANDLE, hwnd.0.expose_provenance()) {
        return match cleanup() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("alternate cleanup also failed: {cleanup}"))),
        };
    }
    Ok(())
}

pub(super) fn cleanup() -> Result<()> {
    if let Some(hwnd) = ALTERNATE.get() {
        // SAFETY: the stored HWND is created on this thread and cleared by its WM_DESTROY.
        unsafe { DestroyWindow(hwnd) }?;
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        ALTERNATE.set(None);
        return LRESULT(0);
    }
    // SAFETY: unhandled messages are forwarded unchanged to the default procedure.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

pub(super) fn open(fixture: Window, typing: Window, proxy: Window) -> Result<Window> {
    ensure!(
        class_name(fixture.hwnd) == fixture::class_name(),
        "alternate request needs the main fixture class"
    );
    let rect = placement(
        work_area(fixture.hwnd)?,
        [fixture.rect()?, typing.rect()?, proxy.rect()?],
    )
    .context("no alternate fixture corner avoids the main fixture, typing and dock")?;
    request(
        fixture,
        CREATE,
        WPARAM(usize::try_from(pack(rect.left, rect.top))?),
        LPARAM(0),
    )?;
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(property(
        fixture.hwnd,
        HANDLE,
    )));
    let window = Window {
        hwnd,
        pid: fixture.pid,
    };
    window.validate()?;
    ensure!(
        hwnd != fixture.hwnd && class_name(hwnd) == alternate_class(),
        "alternate fixture identity mismatch"
    );
    // SAFETY: reads the owner of the PID-validated top-level fixture, without accessing content.
    let owner = unsafe { GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) };
    ensure!(owner == 0, "alternate fixture must not have an owner");
    ensure!(
        window.is_visible()? && window.rect()? == rect,
        "alternate fixture visibility or placement mismatch"
    );
    ensure!(
        typing.is_foreground_window(),
        "showing the alternate changed the foreground"
    );
    Ok(window)
}

pub(super) fn failed(fixture: Window, error: anyhow::Error) -> anyhow::Error {
    match request(fixture, CLOSE, WPARAM(0), LPARAM(0)) {
        Ok(_) => error,
        Err(cleanup) => error.context(format!("alternate cleanup also failed: {cleanup}")),
    }
}

#[cfg(test)]
mod tests {
    use super::placement;
    use windows::Win32::Foundation::RECT;

    #[test]
    fn alternate_fits_a_negative_origin_monitor_and_avoids_the_main_and_product() {
        let work = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        let main = RECT {
            left: -1900,
            top: 400,
            right: -1000,
            bottom: 880,
        };
        let dock = RECT {
            left: -1100,
            top: -200,
            right: -700,
            bottom: 0,
        };
        let typing = RECT {
            left: -1100,
            top: 0,
            right: -700,
            bottom: 300,
        };
        assert_eq!(
            placement(work, [main, typing, dock]),
            Some(RECT {
                left: -256,
                top: 704,
                right: -16,
                bottom: 864
            }),
            "the lower-right corner is free on the fixture's monitor"
        );
        assert_eq!(placement(work, [work; 3]), None, "fully blocked work area");
        assert_eq!(
            placement(
                RECT {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 100
                },
                [RECT::default(); 3]
            ),
            None,
            "a window that cannot fit is refused"
        );
    }
}
