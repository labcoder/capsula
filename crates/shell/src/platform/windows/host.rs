//! The perch's two reads of other apps' windows: what's under the pointer, and a watch that
//! reports the host moving or going away. Both stay on the UI thread. Neither sends a message
//! into another app's thread, so a hung app can't stall Kumochi, and neither logs anything.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use async_channel::Sender;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE,
    DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, AreDpiAwarenessContextsEqual,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForWindow,
    GetSystemMetricsForDpi, GetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_MINIMIZESTART, GW_HWNDNEXT, GWL_EXSTYLE, GWL_STYLE,
    GetClassNameW, GetShellWindow, GetTitleBarInfo, GetTopWindow, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    OBJID_WINDOW, SM_CXSIZE, SM_CXSMSIZE, TITLEBARINFO, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINEVENT_OUTOFCONTEXT, WS_CAPTION, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_OVERLAPPEDWINDOW,
};

use super::presence::{PresenceError, PresenceEvent, send};
use crate::platform::host::{
    Facts, HostChange, HostFacts, HostId, LABEL_UNITS, Label, PhysicalRect, WatchGen,
};

/// Top-level windows the z-order walk looks at before giving up.
const WALK_LIMIT: usize = 4096;
/// Windows the pointer can be over that are never a host: the taskbars and the desktop.
const SHELL_CLASSES: [&str; 4] = [
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Progman",
    "WorkerW",
];
/// `rgstate[0]`'s bit for a title bar that isn't shown.
const STATE_SYSTEM_INVISIBLE: u32 = 0x8000;

/// The topmost eligible window under `point`, skipping Kumochi's own windows rather than
/// stopping at them. The desktop and the taskbars are under the pointer but aren't hosts, so
/// they end the walk with `None`. Minimized, cloaked, hidden and click-through windows are
/// passed over.
///
/// shortcut: a custom caption can put tabs, a search box or account buttons in the inferred
/// band, and the inferred strip only estimates where standard buttons sit; a per-app or
/// accessibility-tree check is the upgrade.
pub(crate) fn host_at([x, y]: [i32; 2]) -> Option<HostFacts> {
    // SAFETY: takes no arguments.
    let own = unsafe { GetCurrentProcessId() };
    // SAFETY: takes no arguments.
    let shell = unsafe { GetShellWindow() };
    // SAFETY: `None` asks for the top of the z-order.
    let mut next = unsafe { GetTopWindow(None) }.ok();
    for _ in 0..WALK_LIMIT {
        let window = next?;
        // SAFETY: `window` came from the z-order walk; a stale one only fails the call.
        next = unsafe { GetWindow(window, GW_HWNDNEXT) }.ok();
        if !bounds(window).is_some_and(|rect| contains(rect, x, y)) {
            continue;
        }
        let Some(frame) = frame(window).filter(|_| eligible(window)) else {
            continue;
        };
        if !contains(frame, x, y) {
            continue;
        }
        let mut process = 0;
        // SAFETY: `process` outlives the call.
        let thread = unsafe { GetWindowThreadProcessId(window, Some(&raw mut process)) };
        if process == own {
            continue;
        }
        if window == shell || is_shell(window) {
            return None;
        }
        let id = HostId {
            window: window.0.expose_provenance(),
            process,
            thread,
        };
        return Some(HostFacts {
            id,
            caption: facts(window)?,
            label: title(window),
        });
    }
    None
}

fn eligible(window: HWND) -> bool {
    // SAFETY: a stale handle only makes these report false or zero.
    let shown = unsafe { IsWindowVisible(window) }.as_bool();
    // SAFETY: as above.
    let minimized = unsafe { IsIconic(window) }.as_bool();
    let mut cloaked = 0_u32;
    let cloak = attribute(window, DWMWA_CLOAKED, &mut cloaked).is_ok() && cloaked != 0;
    shown && !minimized && !cloak && ex_style(window).0 & WS_EX_TRANSPARENT.0 == 0
}

fn contains(rect: PhysicalRect, x: i32, y: i32) -> bool {
    (rect.left..rect.right).contains(&x) && (rect.top..rect.bottom).contains(&y)
}

fn is_shell(window: HWND) -> bool {
    let mut name = [0_u16; 32];
    // SAFETY: `name` is a writable buffer for the class name; this sends no message.
    let len = usize::try_from(unsafe { GetClassNameW(window, &mut name) }).unwrap_or(0);
    SHELL_CLASSES
        .iter()
        .any(|class| class.encode_utf16().eq(name[..len].iter().copied()))
}

/// Every raw read §10.4's caption rules need, for one snapshot of `window`.
fn facts(window: HWND) -> Option<Facts> {
    let mut raw = RECT::default();
    // SAFETY: `raw` outlives the call.
    unsafe { GetWindowRect(window, &raw mut raw) }.ok()?;
    let frame = frame(window)?;
    let frame_rect = rect_of(frame);
    // SAFETY: `frame_rect` outlives the call.
    let monitor = unsafe { MonitorFromRect(&raw const frame_rect, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).ok()?,
        ..MONITORINFO::default()
    };
    // SAFETY: `info` is sized for the call and outlives it.
    unsafe { GetMonitorInfoW(monitor, &raw mut info) }
        .ok()
        .ok()?;
    let (mut monitor_dpi, mut unused) = (0, 0);
    // SAFETY: both outputs outlive the call.
    unsafe {
        GetDpiForMonitor(
            monitor,
            MDT_EFFECTIVE_DPI,
            &raw mut monitor_dpi,
            &raw mut unused,
        )
    }
    .ok()?;
    // SAFETY: a stale handle only returns 0, which the caption rules refuse.
    let layout_dpi = unsafe { GetDpiForWindow(window) };
    let style = style(window);
    let (top_inset, button_width) = metrics(style, ex_style(window), layout_dpi)?;
    // SAFETY: reads this thread's own context.
    let context = unsafe { GetThreadDpiAwarenessContext() };
    // SAFETY: compares two context values; neither is freed.
    let per_monitor_v2 = unsafe {
        AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    }
    .as_bool();
    let mut buttons = RECT::default();
    Some(Facts {
        per_monitor_v2,
        captioned: captioned(style),
        bounds: physical(raw),
        frame,
        work_area: physical(info.rcWork),
        layout_dpi,
        monitor_dpi,
        title_bar: title_bar(window),
        buttons: attribute(window, DWMWA_CAPTION_BUTTON_BOUNDS, &mut buttons)
            .ok()
            .map(|()| physical(buttons)),
        top_inset,
        button_width,
    })
}

/// The top inset and caption-button width in host units at `layout_dpi`. A borderless host's
/// own inset is zero, so its guess takes a standard resizable window's, tool window or not.
fn metrics(style: WINDOW_STYLE, ex_style: WINDOW_EX_STYLE, layout_dpi: u32) -> Option<(i32, i32)> {
    let (style, ex_style) = if captioned(style) {
        (style, ex_style)
    } else {
        (WS_OVERLAPPEDWINDOW, WINDOW_EX_STYLE::default())
    };
    let mut zero = RECT::default();
    // SAFETY: `zero` outlives the call.
    unsafe { AdjustWindowRectExForDpi(&raw mut zero, style, false, ex_style, layout_dpi) }.ok()?;
    let tool = ex_style.0 & WS_EX_TOOLWINDOW.0 != 0;
    // SAFETY: plain metric read.
    let button =
        unsafe { GetSystemMetricsForDpi(if tool { SM_CXSMSIZE } else { SM_CXSIZE }, layout_dpi) };
    Some((-zero.top, button))
}

/// The full composite: `WS_CAPTION` is `WS_BORDER | WS_DLGFRAME`, and a border alone isn't a
/// caption.
fn captioned(style: WINDOW_STYLE) -> bool {
    style.0 & WS_CAPTION.0 == WS_CAPTION.0
}

/// The window rectangle, which holds the DWM frame: one cheap read rules out most windows
/// before the DWM and style reads.
fn bounds(window: HWND) -> Option<PhysicalRect> {
    let mut raw = RECT::default();
    // SAFETY: `raw` outlives the call; a stale handle only fails it.
    unsafe { GetWindowRect(window, &raw mut raw) }.ok()?;
    Some(physical(raw))
}

fn frame(window: HWND) -> Option<PhysicalRect> {
    let mut frame = RECT::default();
    attribute(window, DWMWA_EXTENDED_FRAME_BOUNDS, &mut frame).ok()?;
    Some(physical(frame))
}

fn title_bar(window: HWND) -> Option<PhysicalRect> {
    let mut info = TITLEBARINFO {
        cbSize: u32::try_from(size_of::<TITLEBARINFO>()).ok()?,
        ..TITLEBARINFO::default()
    };
    // SAFETY: `info` is sized for the call and outlives it.
    unsafe { GetTitleBarInfo(window, &raw mut info) }.ok()?;
    (info.rgstate[0] & STATE_SYSTEM_INVISIBLE == 0).then(|| physical(info.rcTitleBar))
}

/// The title, read from the copy the system keeps. For another process's window
/// `GetWindowTextW` doesn't send `WM_GETTEXT` into its thread, and Kumochi's own windows
/// are skipped before this. An unreadable title is an empty label.
fn title(window: HWND) -> Label {
    let mut units = [0_u16; LABEL_UNITS + 1];
    // SAFETY: `units` is a writable buffer; the call writes at most its length.
    let len = usize::try_from(unsafe { GetWindowTextW(window, &mut units) }).unwrap_or(0);
    Label::new(&units[..len.min(LABEL_UNITS + 1)])
}

fn attribute<T>(window: HWND, which: DWMWINDOWATTRIBUTE, out: &mut T) -> windows::core::Result<()> {
    let size = u32::try_from(size_of::<T>()).unwrap_or(0);
    // SAFETY: `out` is a writable `T` of `size` bytes for the attribute's type.
    unsafe { DwmGetWindowAttribute(window, which, std::ptr::from_mut(out).cast(), size) }
}

fn style(window: HWND) -> WINDOW_STYLE {
    // SAFETY: a stale handle only returns 0.
    WINDOW_STYLE(
        u32::try_from(unsafe { GetWindowLongPtrW(window, GWL_STYLE) } & 0xFFFF_FFFF).unwrap_or(0),
    )
}

fn ex_style(window: HWND) -> WINDOW_EX_STYLE {
    // SAFETY: a stale handle only returns 0.
    WINDOW_EX_STYLE(
        u32::try_from(unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } & 0xFFFF_FFFF).unwrap_or(0),
    )
}

fn physical(rect: RECT) -> PhysicalRect {
    PhysicalRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn rect_of(rect: PhysicalRect) -> RECT {
    RECT {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// The events a watch hooks, each with what it means for the host.
const EVENTS: [(u32, HostChange); 5] = [
    (EVENT_OBJECT_LOCATIONCHANGE, HostChange::Moved),
    (EVENT_SYSTEM_MINIMIZESTART, HostChange::Gone),
    (EVENT_OBJECT_HIDE, HostChange::Gone),
    (EVENT_OBJECT_CLOAKED, HostChange::Gone),
    (EVENT_OBJECT_DESTROY, HostChange::Gone),
];
/// Watches alive at once: the armed one and the one replacing it.
const WATCHES: usize = 2;

/// Where a hook's callback sends: the host it's for, its generation and the channel.
struct Route {
    /// Still installed; a removed hook is reset to the default handle.
    hooks: [HWINEVENTHOOK; EVENTS.len()],
    /// False once dropped or rolled back: what's left only waits to be unhooked.
    live: bool,
    window: HWND,
    w: WatchGen,
    events: Sender<PresenceEvent>,
}

thread_local! {
    static ROUTES: RefCell<[Option<Route>; WATCHES]> = const { RefCell::new([None, None]) };
}

/// Reports the host's move, resize, minimize, hide, cloak or destroy as
/// `PresenceEvent::HostChanged` tagged `w`, until dropped. Must be made and dropped on the UI
/// thread, whose message loop delivers the out-of-context hooks.
#[derive(Debug)]
pub(crate) struct HostWatch {
    slot: usize,
    /// Drop reads this thread's `ROUTES`, so the watch can't move to or be shared with another.
    _thread: PhantomData<Rc<()>>,
}

/// Arms a watch on `id`. Fails closed when any hook can't be set or two watches are already
/// alive. A hook that wouldn't come off keeps its slot until a later call removes it.
pub(crate) fn watch_host(
    id: HostId,
    w: WatchGen,
    events: Sender<PresenceEvent>,
) -> Result<HostWatch, PresenceError> {
    let slot = ROUTES
        .with_borrow_mut(|routes| {
            for slot in routes.iter_mut() {
                teardown(slot, |route| {
                    !route.live && unhook_all(&mut route.hooks, unhook)
                });
            }
            routes.iter().position(Option::is_none)
        })
        .ok_or(PresenceError::HostWatch)?;
    let window = HWND(std::ptr::with_exposed_provenance_mut(id.window));
    let (live, hooks) = match install(
        |index| {
            let (event, _) = EVENTS[index];
            // SAFETY: `on_event` is a plain function that outlives the hook; out of context,
            // it runs on this thread's message loop.
            let hook = unsafe {
                SetWinEventHook(
                    event,
                    event,
                    None,
                    Some(on_event),
                    id.process,
                    id.thread,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            (!hook.is_invalid()).then_some(hook)
        },
        unhook,
    ) {
        Ok(hooks) => (true, hooks),
        Err(left) => (false, left),
    };
    if live || hooks.iter().any(|hook| !hook.is_invalid()) {
        ROUTES.with_borrow_mut(|routes| {
            routes[slot] = Some(Route {
                hooks,
                live,
                window,
                w,
                events,
            });
        });
    }
    if !live {
        return Err(PresenceError::HostWatch);
    }
    Ok(HostWatch {
        slot,
        _thread: PhantomData,
    })
}

impl Drop for HostWatch {
    fn drop(&mut self) {
        ROUTES.with_borrow_mut(|routes| {
            teardown(&mut routes[self.slot], |route| {
                route.live = false;
                unhook_all(&mut route.hooks, unhook)
            });
        });
    }
}

/// Removes one hook `watch_host` installed, and says whether it's gone.
fn unhook(hook: HWINEVENTHOOK) -> bool {
    // SAFETY: `hook` was installed by `watch_host` and is still owned by its route.
    let removed = unsafe { UnhookWinEvent(hook) }.as_bool();
    if !removed {
        tracing::warn!("UnhookWinEvent failed");
    }
    removed
}

/// Installs one hook per event in order. If one fails, the ones before it are removed in
/// reverse and `Err` holds any that wouldn't come off, so none is ever forgotten.
fn install<H: Copy + Default + PartialEq, const N: usize>(
    mut add: impl FnMut(usize) -> Option<H>,
    remove: impl FnMut(H) -> bool,
) -> Result<[H; N], [H; N]> {
    let mut hooks = [H::default(); N];
    for index in 0..N {
        let Some(hook) = add(index) else {
            unhook_all(&mut hooks[..index], remove);
            return Err(hooks);
        };
        hooks[index] = hook;
    }
    Ok(hooks)
}

/// Removes every hook still held, last first, keeping the ones that failed. True once none is left.
fn unhook_all<H: Copy + Default + PartialEq>(
    hooks: &mut [H],
    mut remove: impl FnMut(H) -> bool,
) -> bool {
    for hook in hooks.iter_mut().rev() {
        if *hook != H::default() && remove(*hook) {
            *hook = H::default();
        }
    }
    hooks.iter().all(|hook| *hook == H::default())
}

/// Clears a route only once `unhook` confirms its hooks are gone, so a hook never outlives
/// what its callback reads; otherwise the slot stays taken.
fn teardown<R>(slot: &mut Option<R>, unhook: impl FnOnce(&mut R) -> bool) {
    if slot.as_mut().is_none_or(unhook) {
        *slot = None;
    }
}

unsafe extern "system" fn on_event(
    hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW.0 || child != i32::try_from(CHILDID_SELF).unwrap_or(0) {
        return;
    }
    let Some((_, change)) = EVENTS.iter().find(|(id, _)| *id == event) else {
        return;
    };
    ROUTES.with(|routes| {
        let Ok(routes) = routes.try_borrow() else {
            return;
        };
        let route = routes
            .iter()
            .flatten()
            .find(|route| route.live && route.hooks.contains(&hook));
        if let Some(route) = route.filter(|route| route.window == window) {
            send(
                &route.events,
                PresenceEvent::HostChanged {
                    w: route.w,
                    change: *change,
                },
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    use windows::Win32::UI::WindowsAndMessaging::{
        SM_CXPADDEDBORDER, SM_CXSIZE, SM_CYCAPTION, SM_CYSIZEFRAME, WINDOW_EX_STYLE, WS_BORDER,
        WS_CAPTION, WS_DLGFRAME, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
    };

    use super::{captioned, install, metrics, teardown, unhook_all};

    #[test]
    fn only_the_whole_caption_style_counts_as_a_caption() {
        assert!(captioned(WS_CAPTION));
        assert!(captioned(WS_OVERLAPPEDWINDOW));
        assert!(!captioned(WS_BORDER), "a border alone");
        assert!(!captioned(WS_DLGFRAME));
        assert!(!captioned(WS_POPUP | WS_BORDER));
    }

    /// Compiles only while `HostWatch` is neither `Send` nor `Sync`: either impl would make
    /// the call ambiguous.
    trait NotSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> NotSend<()> for T {}
    impl<T: ?Sized + Send> NotSend<u8> for T {}
    trait NotSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> NotSync<()> for T {}
    impl<T: ?Sized + Sync> NotSync<u8> for T {}
    const _: fn() = || {
        <super::HostWatch as NotSend<_>>::check();
        <super::HostWatch as NotSync<_>>::check();
    };

    /// A standard captioned frame from the system metrics it's made of, read apart from
    /// `AdjustWindowRectExForDpi`: the top inset and the caption-button width.
    fn standard(dpi: u32) -> (i32, i32) {
        // SAFETY: plain metric reads.
        let metric = |index| unsafe { GetSystemMetricsForDpi(index, dpi) };
        (
            metric(SM_CYCAPTION) + metric(SM_CYSIZEFRAME) + metric(SM_CXPADDEDBORDER),
            metric(SM_CXSIZE),
        )
    }

    #[test]
    fn a_borderless_host_gets_the_standard_captioned_metrics() {
        for dpi in [96, 144] {
            let standard = Some(standard(dpi));
            let none = WINDOW_EX_STYLE::default();
            assert_eq!(metrics(WS_OVERLAPPEDWINDOW, none, dpi), standard, "{dpi}");
            assert_eq!(metrics(WS_POPUP, none, dpi), standard, "{dpi}");
            assert_eq!(
                metrics(WS_POPUP | WS_BORDER, WS_EX_TOOLWINDOW, dpi),
                standard,
                "a tool window at {dpi}"
            );
        }
    }

    #[test]
    #[ignore = "Rez's default system metrics; run on Rez with --ignored"]
    fn rez_borderless_metrics_at_96_dpi_are_the_design_notes() {
        assert_eq!(
            metrics(WS_POPUP, WINDOW_EX_STYLE::default(), 96),
            Some((31, 36))
        );
    }

    #[test]
    fn a_failed_hook_removes_the_ones_before_it_in_reverse() {
        let log = RefCell::new(Vec::new());
        let hooks: Result<[u32; 5], _> = install(
            |index| {
                log.borrow_mut().push(format!("add {index}"));
                (index != 2).then_some(10 + u32::try_from(index).unwrap())
            },
            |hook| {
                log.borrow_mut().push(format!("remove {hook}"));
                true
            },
        );
        assert_eq!(hooks, Err([0; 5]));
        assert_eq!(
            *log.borrow(),
            ["add 0", "add 1", "add 2", "remove 11", "remove 10"]
        );
        let all: Result<[u32; 3], _> = install(
            |index| Some(u32::try_from(index).unwrap() + 1),
            |_| panic!("nothing fails"),
        );
        assert_eq!(all, Ok([1, 2, 3]));
    }

    #[test]
    fn a_hook_that_wont_come_off_stays_owned_until_it_does() {
        let hooks: Result<[u32; 5], _> = install(
            |index| (index != 3).then_some(10 + u32::try_from(index).unwrap()),
            |hook| hook != 11,
        );
        assert_eq!(hooks, Err([0, 11, 0, 0, 0]), "the rollback keeps 11");
        let mut left = [11, 12];
        assert!(!unhook_all(&mut left, |hook| hook != 11));
        assert_eq!(left, [11, 0]);
        let mut tries = Vec::new();
        assert!(unhook_all(&mut left, |hook| {
            tries.push(hook);
            true
        }));
        assert_eq!(
            (left, tries),
            ([0, 0], vec![11]),
            "a retry removes only what's left"
        );
    }

    #[test]
    fn a_route_is_unhooked_before_its_slot_clears() {
        let mut slot = Some(7);
        let mut seen = None;
        teardown(&mut slot, |route| {
            seen = Some(*route);
            true
        });
        assert_eq!((seen, slot), (Some(7), None));
        teardown(&mut slot, |_| panic!("an empty slot has nothing to unhook"));
        let mut stuck = Some(8);
        teardown(&mut stuck, |_| false);
        assert_eq!(stuck, Some(8), "a failed unhook keeps the slot taken");
    }
}
