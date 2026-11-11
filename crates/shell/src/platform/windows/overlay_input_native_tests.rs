use super::*;
use std::sync::Mutex;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, EnumWindows, IsWindowVisible, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, w};

static WINDOWS: Mutex<()> = Mutex::new(());

struct Hidden(HWND);

impl Hidden {
    fn new() -> Self {
        // SAFETY: STATIC is built in; no parent, borrowed data, activation or visible style.
        Self(
            unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    PCWSTR::null(),
                    WS_POPUP,
                    0,
                    0,
                    16,
                    16,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("a hidden test surface"),
        )
    }

    fn style(&self) -> u32 {
        // SAFETY: this fixture owns a live window on the test thread.
        let style = unsafe { GetWindowLongPtrW(self.0, GWL_EXSTYLE) };
        u32::try_from(style).expect("extended styles fit u32")
    }
}

impl Drop for Hidden {
    fn drop(&mut self) {
        // SAFETY: this fixture is the only owner and destroys its hidden window once.
        unsafe { DestroyWindow(self.0) }.expect("destroy the hidden test surface");
    }
}

fn order(expected: &[HWND]) {
    struct Snapshot<'a> {
        wanted: &'a [HWND],
        found: Vec<HWND>,
    }
    unsafe extern "system" fn collect(hwnd: HWND, value: LPARAM) -> BOOL {
        // SAFETY: EnumWindows synchronously borrows this exclusive stack-owned snapshot.
        let snapshot = unsafe {
            &mut *std::ptr::with_exposed_provenance_mut::<Snapshot<'_>>(value.0.cast_unsigned())
        };
        if snapshot.wanted.contains(&hwnd) {
            snapshot.found.push(hwnd);
        }
        BOOL::from(snapshot.found.len() != snapshot.wanted.len())
    }
    let mut snapshot = Snapshot {
        wanted: expected,
        found: Vec::with_capacity(expected.len()),
    };
    let pointer = (&raw mut snapshot).expose_provenance().cast_signed();
    // SAFETY: one synchronous enumeration; collect only retains the fixture's own handles.
    let result = unsafe { EnumWindows(Some(collect), LPARAM(pointer)) };
    if snapshot.found.len() != expected.len() {
        result.expect("enumerate the owned fixture windows");
    }
    assert_eq!(
        snapshot.found, expected,
        "owned surfaces must have this top-first order"
    );
}

fn pass_through(surface: &Hidden) {
    paint_pass_through(surface.0);
    // SAFETY: reads visibility only; all fixture windows must remain hidden.
    assert!(!unsafe { IsWindowVisible(surface.0) }.as_bool());
}

fn paint_pass_through(hwnd: HWND) {
    let flags = usize::try_from(WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0)
        .expect("style flags fit in usize");
    // SAFETY: the caller retains its live UI-thread fixture window.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    assert_eq!(style.cast_unsigned() & flags, flags);
}

pub(super) fn drawing(dock: HWND, drawing: HWND) {
    paint_pass_through(dock);
    paint_pass_through(drawing);
    order(&[dock, drawing]);
    // SAFETY: reads only this fixture thread's activation state.
    assert_eq!(unsafe { GetActiveWindow() }, HWND::default());
}

#[test]
fn drawing_and_dock_paint_pass_through_without_disabling_the_dock_proxy() {
    let _windows = WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dock = Hidden::new();
    let proxy = Hidden::new();
    let drawing = Hidden::new();
    let mut windows = OverlayWindows::default();
    windows.bind_hwnd(Layer::Dock, dock.0).unwrap();
    windows.proxy(super::super::observer::ProxyHandle(proxy.0));
    windows.bind_hwnd(Layer::Overlay, drawing.0).unwrap();
    windows
        .configure(OverlayInput::PassThrough, &Regions::new())
        .unwrap();
    pass_through(&drawing);
    pass_through(&dock);
    restack(drawing.0, Some([0, 0, 100, 100])).unwrap();
    windows
        .configure(OverlayInput::PassThrough, &Regions::new())
        .unwrap();
    assert_eq!(proxy.style() & WS_EX_TRANSPARENT.0, 0);
    order(&[proxy.0, dock.0, drawing.0]);
}

#[test]
fn drawing_order_survives_panels_dock_moves_replacement_and_reconnection() {
    let _windows = WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for drawing_first in [true, false] {
        let panel = Hidden::new();
        let dock = Hidden::new();
        let proxy = Hidden::new();
        let region = Hidden::new();
        let selection = Hidden::new();
        let mut drawing = Hidden::new();
        let mut windows = OverlayWindows::default();
        windows.bind_hwnd(Layer::Dock, dock.0).unwrap();
        windows.proxy(super::super::observer::ProxyHandle(proxy.0));
        windows.bind_hwnd(Layer::Selection, selection.0).unwrap();
        windows.regions.insert(RegionId(1), region.0);
        let regions = Regions::from([(RegionId(1), [0, 0, 16, 16])]);
        if drawing_first {
            windows.bind_hwnd(Layer::Overlay, drawing.0).unwrap();
            windows.configure(OverlayInput::Regions, &regions).unwrap();
        }
        windows.bind_hwnd(Layer::Panels, panel.0).unwrap();
        if !drawing_first {
            windows.bind_hwnd(Layer::Overlay, drawing.0).unwrap();
        }
        windows.configure(OverlayInput::Regions, &regions).unwrap();
        for event in [
            "initial",
            "panel reopened",
            "dock moved",
            "second drawing",
            "reconnected",
        ] {
            match event {
                "panel reopened" => restack(panel.0, None).unwrap(),
                "dock moved" => {
                    restack(dock.0, Some([30, 40, 16, 16])).unwrap();
                    restack(proxy.0, Some([30, 40, 16, 16])).unwrap();
                }
                "second drawing" | "reconnected" => {
                    drop(drawing);
                    drawing = Hidden::new();
                    windows.bind_hwnd(Layer::Overlay, drawing.0).unwrap();
                    windows.configure(OverlayInput::Regions, &regions).unwrap();
                    restack(drawing.0, Some([0, 0, 100, 100])).unwrap();
                }
                _ => restack(drawing.0, None).unwrap(),
            }
            windows.configure(OverlayInput::Regions, &regions).unwrap();
            pass_through(&drawing);
            pass_through(&dock);
            order(&[panel.0, proxy.0, dock.0, region.0, drawing.0, selection.0]);
            // SAFETY: reads only this test thread's activation state.
            assert_eq!(unsafe { GetActiveWindow() }, HWND::default(), "{event}");
        }
        windows
            .configure(OverlayInput::Selecting, &regions)
            .unwrap();
        pass_through(&drawing);
        order(&[selection.0, panel.0, proxy.0, dock.0, region.0, drawing.0]);
        windows
            .configure(OverlayInput::PassThrough, &Regions::new())
            .unwrap();
        pass_through(&drawing);
        order(&[panel.0, proxy.0, dock.0, region.0, drawing.0, selection.0]);
    }
}
