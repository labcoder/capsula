use async_channel::Receiver;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, SendMessageW, WM_CANCELMODE, WM_CAPTURECHANGED, WM_LBUTTONUP, WM_RBUTTONUP,
};

use super::{
    PresenceEvent, PresenceSurface,
    focus::focus_input,
    proxy::proxy_proc,
    signed_words,
    window::{hide, subclass_hover},
};
use crate::platform::input::{self, Button};
use kumochi_protocol::constants::FOCUS_INPUT_MARKER;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT_MOUSE, MOUSE_EVENT_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetForegroundWindow, GetWindowRect, IsWindowVisible,
    MA_NOACTIVATE, WM_MOUSEACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// Under `hover_subclass`: a release of the left button forwards a nested release of the
/// right one, then changes capture; the right one's release changes capture and drains
/// that loss. With `data` 1, the left release then destroys the window.
unsafe extern "system" fn nested_releases(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    let send = |message| {
        // SAFETY: a synchronous message to this thread's live window.
        unsafe { SendMessageW(hwnd, message, None, None) }
    };
    match message {
        WM_LBUTTONUP => {
            send(WM_RBUTTONUP);
            send(WM_CAPTURECHANGED);
            if data == 1 {
                // SAFETY: this thread's window; destroying it frees the subclass's Hover.
                let destroyed = unsafe { DestroyWindow(hwnd) };
                assert!(destroyed.is_ok(), "the test window is destroyed");
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            send(WM_CAPTURECHANGED);
            input::input(input::Input::reconcile_after_release);
            LRESULT(0)
        }
        // SAFETY: forwards the unmodified message down the chain.
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

/// A hidden window with `nested_releases` under the presence subclass.
fn bridged(destroy: usize) -> (HWND, Receiver<PresenceEvent>) {
    // SAFETY: STATIC is a built-in class; no borrowed data or parent, and no show style.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap();
    // SAFETY: `hwnd` is live; the stub keeps no data that could dangle.
    let stub = unsafe { SetWindowSubclass(hwnd, Some(nested_releases), 1, destroy) };
    assert!(stub.as_bool());
    let (events, received) = async_channel::unbounded();
    subclass_hover(hwnd, events, PresenceSurface::Overlay).unwrap();
    (hwnd, received)
}

fn drained(received: &Receiver<PresenceEvent>) -> Vec<PresenceEvent> {
    std::iter::from_fn(|| received.try_recv().ok()).collect()
}

const LOSSES_THEN_ONE_RECONCILE: [PresenceEvent; 3] = [
    PresenceEvent::CaptureLost {
        seq: 1,
        released: Some(Button::Right),
    },
    PresenceEvent::CaptureLost {
        seq: 2,
        released: Some(Button::Left),
    },
    PresenceEvent::Reconcile,
];

#[test]
fn a_loss_inside_nested_releases_reconciles_once_after_the_outer_one() {
    let (hwnd, received) = bridged(0);
    let window = TestWindow(hwnd);
    // SAFETY: a synchronous message to this thread's live window.
    unsafe { SendMessageW(window.0, WM_LBUTTONUP, None, None) };
    assert_eq!(
        drained(&received),
        LOSSES_THEN_ONE_RECONCILE,
        "each loss names the release it came in, the outer one restored after the inner"
    );
}

#[test]
fn the_window_destroyed_during_a_release_still_gets_its_reconcile() {
    let (hwnd, received) = bridged(1);
    // SAFETY: a synchronous message to this thread's live window, which it destroys.
    unsafe { SendMessageW(hwnd, WM_LBUTTONUP, None, None) };
    // SAFETY: only asks whether the handle still names a window.
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    assert_eq!(drained(&received), LOSSES_THEN_ONE_RECONCILE);
}

#[test]
fn a_cancel_mode_is_an_involuntary_loss_even_during_a_release() {
    let (hwnd, received) = bridged(0);
    let window = TestWindow(hwnd);
    // SAFETY: a synchronous message to this thread's live window.
    unsafe { SendMessageW(window.0, WM_CANCELMODE, None, None) };
    assert_eq!(
        drained(&received),
        [PresenceEvent::CaptureLost {
            seq: 1,
            released: None
        }]
    );
    let ((), _) = input::input(|input| {
        input.release(Button::Left, || {
            // SAFETY: as above.
            unsafe { SendMessageW(window.0, WM_CANCELMODE, None, None) };
        })
    });
    assert_eq!(
        drained(&received),
        [PresenceEvent::CaptureLost {
            seq: 2,
            released: None
        }]
    );
}

struct TestWindow(HWND);

impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: the test owns this live window on its creating thread.
        unsafe { DestroyWindow(self.0) }.expect("the test window remains owned by its thread");
    }
}

#[test]
fn focus_input_is_a_tagged_mouse_event_that_does_nothing() {
    let input = focus_input();
    assert_eq!(input.r#type, INPUT_MOUSE, "a mouse event, never a key");
    // SAFETY: `focus_input` initializes the mouse member, which `r#type` names.
    let mouse = unsafe { input.Anonymous.mi };
    assert_eq!((mouse.dx, mouse.dy), (0, 0), "no motion");
    assert_eq!(mouse.mouseData, 0, "no wheel or extra buttons");
    assert_eq!(
        mouse.dwFlags,
        MOUSE_EVENT_FLAGS(0),
        "no button, wheel, absolute or desktop flags"
    );
    assert_eq!(mouse.time, 0, "the system stamps it");
    assert_eq!(
        mouse.dwExtraInfo, FOCUS_INPUT_MARKER,
        "tagged for the gate harness"
    );
}

#[test]
fn hiding_a_panel_keeps_its_bounds_and_the_foreground() {
    // SAFETY: reads the foreground without changing it.
    let foreground = unsafe { GetForegroundWindow() };
    // SAFETY: STATIC is a built-in class; no borrowed data or parent, and no show style.
    let window = TestWindow(
        unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1600,
                900,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap(),
    );
    hide(window.0).unwrap();
    let mut rect = RECT::default();
    // SAFETY: the window is live and rect is writable.
    unsafe { GetWindowRect(window.0, &raw mut rect) }.unwrap();
    assert_eq!(
        [
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top
        ],
        [0, 0, 1600, 900]
    );
    // SAFETY: reads visibility of the live test window.
    assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
    // SAFETY: reads the foreground without changing it.
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
}

#[test]
fn resting_proxy_refuses_mouse_activation_without_swallowing_the_click() {
    // SAFETY: no pointer-valued message data; Win32 validates the empty HWND if reached.
    let response = unsafe { proxy_proc(HWND::default(), WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)) };
    assert_eq!(response, LRESULT(isize::try_from(MA_NOACTIVATE).unwrap()));
}

#[test]
fn packed_pointer_coordinates_preserve_negative_monitor_origins() {
    assert_eq!(signed_words(0xFF9C_FF38), [-200, -100]);
    assert_eq!(signed_words(0x7FFF_8000), [-32768, 32767]);
    assert_eq!(signed_words(0x05A0_0A00), [2560, 1440]);
}
