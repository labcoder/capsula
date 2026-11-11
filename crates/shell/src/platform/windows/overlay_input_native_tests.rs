use super::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GW_HWNDPREV, GetForegroundWindow, GetWindow, IsWindowVisible,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

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

fn above(upper: &Hidden, lower: &Hidden) {
    let mut next = lower.0;
    loop {
        // SAFETY: only reads z-order handles; never sends input or reads foreign content.
        let previous = unsafe { GetWindow(next, GW_HWNDPREV) }.expect("an upper surface exists");
        if previous == upper.0 {
            return;
        }
        assert!(!previous.is_invalid(), "the expected surface must be above");
        next = previous;
    }
}

fn pass_through(surface: &Hidden) {
    let flags = WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0;
    assert_eq!(surface.style() & flags, flags);
    // SAFETY: reads visibility only; all fixture windows must remain hidden.
    assert!(!unsafe { IsWindowVisible(surface.0) }.as_bool());
}

#[test]
fn drawing_and_dock_paint_pass_through_without_disabling_the_dock_proxy() {
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
    above(&proxy, &dock);
    above(&dock, &drawing);
}

#[test]
fn drawing_order_survives_panels_dock_moves_replacement_and_reconnection() {
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
        // SAFETY: reads foreground without changing it.
        let foreground = unsafe { GetForegroundWindow() };
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
            above(&panel, &proxy);
            above(&proxy, &dock);
            above(&dock, &region);
            above(&region, &drawing);
            above(&drawing, &selection);
            // SAFETY: reads foreground only; configuration must not activate a surface.
            assert_eq!(unsafe { GetForegroundWindow() }, foreground, "{event}");
        }
        windows
            .configure(OverlayInput::Selecting, &regions)
            .unwrap();
        pass_through(&drawing);
        above(&selection, &panel);
        windows
            .configure(OverlayInput::PassThrough, &Regions::new())
            .unwrap();
        pass_through(&drawing);
        above(&drawing, &selection);
    }
}
