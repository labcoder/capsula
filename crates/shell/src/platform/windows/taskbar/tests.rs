use super::*;
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GetForegroundWindow, GetWindowRect,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, SC_MAXIMIZE, SC_MOVE, SC_SIZE,
    SendMessageW, WA_ACTIVE, WA_CLICKACTIVE, WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows::core::BOOL;

fn word(value: u32) -> usize {
    usize::try_from(value).expect("Win32 message fits the native word")
}

#[test]
fn taskbar_messages_open_status_quit_or_ignore_without_default_system_commands() {
    let registered = 0xc123;
    for active in [WA_ACTIVE, WA_CLICKACTIVE] {
        assert_eq!(
            action(WM_ACTIVATE, word(active), registered),
            Action::Status
        );
    }
    assert_eq!(
        action(WM_ACTIVATE, word(WA_INACTIVE), registered),
        Action::Default
    );
    for command in [SC_RESTORE, SC_MINIMIZE] {
        for low in [0, 1, 15] {
            assert_eq!(
                action(WM_SYSCOMMAND, word(command | low), registered),
                Action::Status
            );
        }
    }
    assert_eq!(action(WM_CLOSE, 0, registered), Action::Quit);
    assert_eq!(
        action(WM_SYSCOMMAND, word(SC_CLOSE | 15), registered),
        Action::Quit
    );
    for command in [SC_SIZE, SC_MOVE, SC_MAXIMIZE, 0, 0xfff0] {
        assert_eq!(
            action(WM_SYSCOMMAND, word(command), registered),
            Action::Ignore
        );
    }
    assert_eq!(action(registered, 0, registered), Action::Readd);
    assert_eq!(action(registered, 0, 0), Action::Default);
    assert_eq!(action(0, 0, 0), Action::Default);
}

#[test]
fn refused_creation_installs_no_window_and_off_never_calls_the_factory() {
    let (events, _) = async_channel::unbounded();
    let mut button = TaskbarButton::new(events);
    assert_eq!(
        button.apply(TaskbarChoice::Shown, |_| Err(TaskbarError)),
        Err(TaskbarError)
    );
    assert_eq!(button.window.as_ref().map(|_| ()), None);
    assert_eq!(
        button.apply(TaskbarChoice::Hidden, |_| panic!("off creates no window")),
        Ok(())
    );
    assert_eq!(button.window.as_ref().map(|_| ()), None);
}

fn app_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, value: LPARAM) -> BOOL {
        let mut pid = 0;
        // SAFETY: pid is writable storage for the enumerated live window.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        // SAFETY: reads the current process ID.
        let own_pid = unsafe { GetCurrentProcessId() };
        // SAFETY: reads styles of the enumerated live window.
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        if pid == own_pid && style.cast_unsigned() & word(WS_EX_APPWINDOW.0) != 0 {
            // SAFETY: EnumWindows runs synchronously with the caller's exclusive Vec pointer.
            let windows = unsafe {
                &mut *std::ptr::with_exposed_provenance_mut::<Vec<HWND>>(value.0.cast_unsigned())
            };
            windows.push(hwnd);
        }
        BOOL(1)
    }
    let mut windows = Vec::new();
    let pointer = (&raw mut windows).expose_provenance().cast_signed();
    // SAFETY: the Vec outlives synchronous enumeration and only collect accesses it.
    unsafe { EnumWindows(Some(collect), LPARAM(pointer)) }.expect("enumerate own windows");
    windows
}

#[test]
#[ignore = "creates a real taskbar button; run only in an approved window turn"]
fn taskbar_native_lifecycle_readd_commands_and_zero_area() {
    assert_eq!(app_windows(), vec![]);
    let (events, received) = async_channel::unbounded();
    let mut button = TaskbarButton::new(events);
    assert_eq!(button.set(TaskbarChoice::Hidden), Ok(()));
    // SAFETY: observes foreground without changing it.
    let foreground = unsafe { GetForegroundWindow() };
    assert_eq!(button.set(TaskbarChoice::Shown), Ok(()));
    assert!(matches!(
        received.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    let window = button.window.as_ref().expect("shown window");
    let hwnd = window.hwnd;
    assert_eq!(app_windows(), vec![hwnd]);
    // SAFETY: reads this test's live window styles.
    let exstyle = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    assert_eq!(
        u32::try_from(exstyle & 0xffff_ffff).expect("extended styles"),
        (WS_EX_APPWINDOW | WS_EX_NOACTIVATE).0
    );
    // SAFETY: reads this test's live window styles.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    assert_eq!(
        u32::try_from(style & 0xffff_ffff).expect("window styles"),
        (WS_POPUP | WS_SYSMENU | WS_VISIBLE | WS_CLIPSIBLINGS).0
    );
    // SAFETY: reads ownership of this test's live window.
    let owner = unsafe { GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) };
    assert_eq!(owner, 0);
    let mut before = RECT::default();
    // SAFETY: before is writable storage for this live HWND's rectangle.
    unsafe { GetWindowRect(hwnd, &raw mut before) }.expect("initial bounds");
    assert_eq!(
        [before.left, before.top, before.right, before.bottom],
        [0, 0, 0, 0]
    );
    for command in [SC_SIZE, SC_MOVE, SC_MAXIMIZE] {
        // SAFETY: dispatches only to this thread's own test window.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SYSCOMMAND,
                Some(WPARAM(word(command))),
                Some(LPARAM(0)),
            )
        };
        let mut after = RECT::default();
        // SAFETY: after is writable storage for this live HWND's rectangle.
        unsafe { GetWindowRect(hwnd, &raw mut after) }.expect("command bounds");
        assert_eq!(
            [after.left, after.top, after.right, after.bottom],
            [0, 0, 0, 0]
        );
    }
    // SAFETY: simulates Explorer's broadcast only on this test's own window.
    unsafe { SendMessageW(hwnd, window.state.taskbar_created, None, None) };
    // SAFETY: observes the test's live window.
    assert!(unsafe { IsWindowVisible(hwnd) }.as_bool());
    assert!(matches!(
        received.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    // SAFETY: simulates activation only on this thread's own test window.
    unsafe { SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(word(WA_ACTIVE))), None) };
    assert!(matches!(
        received.try_recv().expect("activation status"),
        PresenceEvent::ShowStatus
    ));
    for command in [SC_RESTORE, SC_MINIMIZE] {
        // SAFETY: dispatches only to this thread's own test window.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SYSCOMMAND,
                Some(WPARAM(word(command))),
                Some(LPARAM(0)),
            )
        };
        assert!(matches!(
            received.try_recv().expect("status event"),
            PresenceEvent::ShowStatus
        ));
    }
    // SAFETY: sends Close only to this thread's own test window.
    unsafe { SendMessageW(hwnd, WM_CLOSE, None, None) };
    assert!(matches!(
        received.try_recv().expect("quit event"),
        PresenceEvent::Quit
    ));
    // SAFETY: dispatches the system-menu close command only to this test's own window.
    unsafe { SendMessageW(hwnd, WM_SYSCOMMAND, Some(WPARAM(word(SC_CLOSE))), None) };
    assert!(matches!(
        received.try_recv().expect("system close event"),
        PresenceEvent::Quit
    ));
    assert!(matches!(
        received.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    // SAFETY: the close request must not destroy the window behind its owner.
    assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
    // SAFETY: observes foreground without changing it.
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    assert_eq!(button.set(TaskbarChoice::Hidden), Ok(()));
    // SAFETY: observes whether the now-destroyed handle is still a window.
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    assert_eq!(app_windows(), vec![]);
    assert_eq!(button.set(TaskbarChoice::Shown), Ok(()));
    assert_eq!(app_windows().len(), 1);
    drop(button);
    assert_eq!(app_windows(), vec![]);
}
