use std::io::{BufRead as _, BufReader, Lines, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{PhysicalPoint, WindowRef};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, CreateRectRgn, CreateSolidBrush, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, SelectObject, SetWindowRgn,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ES_PASSWORD, FindWindowW,
    GA_ROOT, GetAncestor, GetMessageW, GetWindowRect, HMENU, LAYERED_WINDOW_ATTRIBUTES_FLAGS,
    LWA_ALPHA, LWA_COLORKEY, MSG, PostMessageW, PostThreadMessageW, RegisterClassW,
    SMTO_ABORTIFHUNG, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_NOZORDER, SendMessageTimeoutW, SendMessageW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_NCHITTEST, WM_QUIT,
    WNDCLASSW, WS_CHILD, WS_DISABLED, WS_EX_LAYERED, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WindowFromPoint,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use super::{
    Facts, Found, Hit, Layer, Step, drawn_hit, facts, frame, layer_of, owner, preflight, step,
    unchanged,
};
use crate::platform::windows::capture::screen_capturer;
use crate::screen::{CaptureId, Capturer, Failure, FrameMeta, RawFrame, Refusal, Target};

fn eligible() -> Facts {
    Facts {
        visible: true,
        contains: true,
        ..Facts::default()
    }
}

#[test]
fn the_walk_takes_the_first_window_a_click_would_land_on() {
    assert_eq!(step(eligible()), Step::Take);
    for (facts, expected) in [
        (
            Facts {
                unclear: true,
                ..eligible()
            },
            Step::Unclear,
        ),
        (
            Facts {
                desktop: true,
                ..eligible()
            },
            Step::Stop,
        ),
    ] {
        assert_eq!(step(facts), expected);
    }
    for (why, skipped) in [
        (
            "kumochi",
            Facts {
                kumochi: true,
                ..eligible()
            },
        ),
        (
            "hidden",
            Facts {
                visible: false,
                ..eligible()
            },
        ),
        (
            "cloaked",
            Facts {
                cloaked: true,
                ..eligible()
            },
        ),
        (
            "click-through",
            Facts {
                transparent: true,
                ..eligible()
            },
        ),
        (
            "elsewhere",
            Facts {
                contains: false,
                ..eligible()
            },
        ),
        (
            "hidden desktop",
            Facts {
                visible: false,
                desktop: true,
                ..eligible()
            },
        ),
        (
            "unclear but elsewhere",
            Facts {
                unclear: true,
                contains: false,
                ..eligible()
            },
        ),
        (
            "unclear but kumochi",
            Facts {
                unclear: true,
                kumochi: true,
                ..eligible()
            },
        ),
    ] {
        assert_eq!(step(skipped), Step::Skip, "{why}");
    }
}

#[test]
fn only_a_layered_window_can_be_click_through_and_unreadable_layers_are_drawn() {
    let layered = WS_EX_LAYERED.0;
    let transparent = WS_EX_TRANSPARENT.0;
    let alpha = |value, flags| Some((value, flags));
    for (why, styles, attributes, expected) in [
        ("transparent bit alone", transparent, None, Layer::Opaque),
        ("plain window", 0, None, Layer::Opaque),
        (
            "layered and transparent",
            layered | transparent,
            None,
            Layer::ClickThrough,
        ),
        (
            "zero constant alpha",
            layered,
            alpha(0, LWA_ALPHA),
            Layer::ClickThrough,
        ),
        (
            "full constant alpha",
            layered,
            alpha(255, LWA_ALPHA),
            Layer::Opaque,
        ),
        (
            "zero alpha not in use",
            layered,
            alpha(0, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0)),
            Layer::Opaque,
        ),
        (
            "colour key",
            layered,
            alpha(255, LWA_COLORKEY),
            Layer::Drawn,
        ),
        (
            "colour key at zero alpha",
            layered,
            alpha(0, LWA_ALPHA | LWA_COLORKEY),
            Layer::ClickThrough,
        ),
        ("drawn by UpdateLayeredWindow", layered, None, Layer::Drawn),
    ] {
        assert_eq!(layer_of(styles, attributes), expected, "{why}");
    }
}

#[test]
fn a_drawn_window_is_skipped_only_when_the_hit_test_lands_below_it() {
    for (styles, found, expected) in [
        (0, Found::Itself, Hit::Here),
        (0, Found::Below, Hit::Through),
        (0, Found::Elsewhere, Hit::Unclear),
        (WS_DISABLED.0, Found::Itself, Hit::Unclear),
        (WS_DISABLED.0, Found::Below, Hit::Unclear),
    ] {
        assert_eq!(
            drawn_hit(styles, || found),
            expected,
            "{styles:#x} {found:?}"
        );
    }
}

/// Set in the host process, which owns every sentinel window so none is this worker's own.
const HOST_ENV: &str = "KUMOCHI_WINDOW_PROBE_HOST";
const HOST_TEST: &str = "platform::windows::window::tests::native_probe_host";
const PATIENCE: Duration = Duration::from_secs(10);

/// One host window: colour (0x00BBGGRR), physical rectangle, owner index, styles, how a layered
/// one is drawn, and a window region relative to its corner.
struct Plan {
    color: u32,
    rect: [i32; 4],
    owner: Option<usize>,
    style: WINDOW_STYLE,
    ex: WINDOW_EX_STYLE,
    layer: Layering,
    region: Option<[i32; 4]>,
}

/// How a layered host window gets its pixels; `ex` must include `WS_EX_LAYERED` for all but
/// `Plain`.
#[derive(Clone, Copy)]
enum Layering {
    Plain,
    /// One alpha for the whole window.
    Constant(u8),
    /// `UpdateLayeredWindow` with every pixel this premultiplied ARGB value: zero is see-through
    /// and lets clicks pass, though the window's styles don't say so.
    Drawn(u32),
    /// Its own colour is the colour key, so every pixel is see-through.
    Keyed,
}

const RED: u32 = 0x0000_00FF;
const GREEN: u32 = 0x0000_FF00;
const MAGENTA: u32 = 0x00FF_00FF;
const YELLOW: u32 = 0x0000_FFFF;
const PURPLE: u32 = 0x0080_0080;

const T: usize = 0;
const O: usize = 1;
const C: usize = 3;
const X: usize = 4;
const M: usize = 5;
const D: usize = 6;
const U: usize = 8;
const P: usize = 9;
const K: usize = 10;
const DISABLED: usize = 11;
const REGION: usize = 12;
/// `WS_EX_TRANSPARENT` without `WS_EX_LAYERED`, alone over the target.
const BIT_ONLY: usize = 14;
/// An opaque drawn window, under a `WS_EX_TRANSPARENT`-only window (16).
const DRAWN_UNDER_BIT: usize = 15;
const BIT_OVER_DRAWN: usize = 16;
/// An opaque drawn window, under a window that the probe cloaks (18).
const DRAWN_UNDER_CLOAK: usize = 17;
const CLOAK_OVER_DRAWN: usize = 18;
const KEYED: usize = 19;
const DRAWN_OPAQUE: usize = 20;
const DRAWN_DISABLED: usize = 21;
/// Premultiplied opaque green.
const OPAQUE: u32 = 0xFF00_8000;

fn plan() -> [Plan; 22] {
    let popup = |color, rect, owner, ex| Plan {
        color,
        rect,
        owner,
        style: WS_POPUP | WS_VISIBLE,
        ex: WS_EX_TOPMOST | ex,
        layer: Layering::Plain,
        region: None,
    };
    [
        Plan {
            color: RED,
            rect: [100, 100, 400, 300],
            owner: None,
            style: WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            ex: WS_EX_TOPMOST,
            layer: Layering::Plain,
            region: None,
        },
        popup(GREEN, [300, 150, 200, 100], Some(T), WINDOW_EX_STYLE(0)),
        popup(MAGENTA, [380, 290, 80, 60], Some(T), WINDOW_EX_STYLE(0)),
        popup(YELLOW, [120, 320, 100, 60], None, WINDOW_EX_STYLE(0)),
        popup(0x00FF_FF00, [700, 300, 150, 100], None, WINDOW_EX_STYLE(0)),
        popup(0x0080_8080, [900, 100, 150, 100], None, WINDOW_EX_STYLE(0)),
        popup(0x0040_4040, [900, 300, 150, 100], None, WINDOW_EX_STYLE(0)),
        Plan {
            layer: Layering::Constant(255),
            ..popup(
                PURPLE,
                [150, 250, 80, 40],
                None,
                WS_EX_LAYERED | WS_EX_TRANSPARENT,
            )
        },
        Plan {
            layer: Layering::Drawn(0),
            ..popup(PURPLE, [250, 330, 60, 40], None, WS_EX_LAYERED)
        },
        popup(0x0000_8000, [1100, 100, 120, 80], None, WINDOW_EX_STYLE(0)),
        popup(0x0000_0080, [1100, 300, 120, 80], None, WINDOW_EX_STYLE(0)),
        Plan {
            style: WS_POPUP | WS_VISIBLE | WS_DISABLED,
            ..popup(0x0080_8000, [400, 110, 60, 30], None, WINDOW_EX_STYLE(0))
        },
        Plan {
            region: Some([30, 0, 60, 30]),
            ..popup(0x0000_8080, [320, 110, 60, 30], None, WINDOW_EX_STYLE(0))
        },
        Plan {
            layer: Layering::Constant(0),
            ..popup(PURPLE, [240, 150, 50, 40], None, WS_EX_LAYERED)
        },
        popup(GREEN, [330, 365, 30, 25], None, WS_EX_TRANSPARENT),
        Plan {
            layer: Layering::Drawn(OPAQUE),
            ..popup(GREEN, [370, 365, 30, 25], None, WS_EX_LAYERED)
        },
        popup(GREEN, [370, 365, 30, 25], None, WS_EX_TRANSPARENT),
        Plan {
            layer: Layering::Drawn(OPAQUE),
            ..popup(GREEN, [410, 365, 30, 25], None, WS_EX_LAYERED)
        },
        popup(YELLOW, [410, 365, 30, 25], None, WINDOW_EX_STYLE(0)),
        Plan {
            layer: Layering::Keyed,
            ..popup(MAGENTA, [450, 365, 30, 25], None, WS_EX_LAYERED)
        },
        Plan {
            layer: Layering::Drawn(OPAQUE),
            ..popup(GREEN, [235, 205, 30, 20], None, WS_EX_LAYERED)
        },
        Plan {
            layer: Layering::Drawn(OPAQUE),
            style: WS_POPUP | WS_VISIBLE | WS_DISABLED,
            ..popup(GREEN, [270, 205, 25, 20], None, WS_EX_LAYERED)
        },
    ]
}

/// `WM_NCHITTEST` messages the host's windows received, so the probe can tell whether its hit
/// test sent any to another process.
static HITS: AtomicU64 = AtomicU64::new(0);

const CLOAK: u32 = WM_APP + 1;
const PROTECT: u32 = WM_APP + 2;
const MINIMIZE: u32 = WM_APP + 3;
const MAXIMIZE: u32 = WM_APP + 4;
const RESTORE: u32 = WM_APP + 5;
const DESTROY: u32 = WM_APP + 6;
const HANG: u32 = WM_APP + 7;
const NUDGE: u32 = WM_APP + 8;
/// Sent, not posted: destroys the window and makes the same plan's again on this thread, and
/// returns the new handle.
const RECREATE: u32 = WM_APP + 9;
/// How long `hang` keeps the host's window thread from reading messages.
const HUNG: Duration = Duration::from_secs(3);

extern "system" fn host_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let done = match message {
        CLOAK => {
            let on = BOOL(1);
            let size = u32::try_from(size_of::<BOOL>()).expect("four bytes");
            // SAFETY: cloaks this process's own window; the value is a readable BOOL.
            unsafe { DwmSetWindowAttribute(window, DWMWA_CLOAK, (&raw const on).cast(), size) }
                .is_ok()
        }
        // SAFETY: changes only this process's own window's display affinity.
        PROTECT => unsafe { SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE) }.is_ok(),
        MINIMIZE | MAXIMIZE | RESTORE => {
            let command = match message {
                MINIMIZE => SW_MINIMIZE,
                MAXIMIZE => SW_MAXIMIZE,
                _ => SW_RESTORE,
            };
            // SAFETY: shows this process's own window; the result is its earlier visibility.
            let _was_visible = unsafe { ShowWindow(window, command) };
            true
        }
        // SAFETY: destroys this thread's own window.
        DESTROY => unsafe { DestroyWindow(window) }.is_ok(),
        NUDGE => {
            let was = rect(window);
            let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
            // SAFETY: moves this thread's own window one pixel right, keeping its size.
            unsafe { SetWindowPos(window, None, was.left + 1, was.top, 0, 0, flags) }.is_ok()
        }
        RECREATE => {
            let index = w.0;
            // SAFETY: destroys this thread's own window; nothing here touches it afterwards.
            let gone = unsafe { DestroyWindow(window) }.is_ok();
            let made = make(&plan()[index], index, None);
            println!("hwnd {index} {}", value(made));
            println!("done {gone}");
            return LRESULT(isize::try_from(made.0.addr()).expect("handles fit isize"));
        }
        HANG => {
            println!("done true");
            std::thread::sleep(HUNG);
            return LRESULT(0);
        }
        _ => {
            if message == WM_NCHITTEST {
                HITS.fetch_add(1, Ordering::Relaxed);
            }
            // SAFETY: the default handling of a message for this thread's own window.
            return unsafe { DefWindowProcW(window, message, w, l) };
        }
    };
    println!("done {done}");
    LRESULT(0)
}

fn class(index: usize) -> HSTRING {
    HSTRING::from(format!("kumochi-window-probe-{index}"))
}

fn create(plan: &Plan, index: usize, owner: Option<HWND>) -> HWND {
    let class = class(index);
    // SAFETY: a solid brush the class keeps for the process's life.
    let brush = unsafe { CreateSolidBrush(COLORREF(plan.color)) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(host_proc),
        hbrBackground: brush,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..WNDCLASSW::default()
    };
    // SAFETY: the class structure and its name outlive the call.
    assert_ne!(unsafe { RegisterClassW(&raw const wc) }, 0, "class");
    make(plan, index, owner)
}

/// A window of `index`'s registered class, drawn as `plan` says.
fn make(plan: &Plan, index: usize, owner: Option<HWND>) -> HWND {
    let class = class(index);
    let [x, y, width, height] = plan.rect;
    // SAFETY: creates this process's own window with a registered class.
    let window = unsafe {
        CreateWindowExW(
            plan.ex,
            PCWSTR(class.as_ptr()),
            w!(""),
            plan.style,
            x,
            y,
            width,
            height,
            owner,
            None,
            Some(HINSTANCE::default()),
            None,
        )
    }
    .expect("probe window");
    match plan.layer {
        Layering::Plain => {}
        Layering::Constant(alpha) => {
            // SAFETY: a layered window this process owns, at one constant alpha.
            let layered =
                unsafe { SetLayeredWindowAttributes(window, COLORREF(0), alpha, LWA_ALPHA) };
            layered.expect("constant layer");
        }
        Layering::Drawn(pixel) => draw(window, [width, height], pixel),
        Layering::Keyed => {
            // SAFETY: a layered window this process owns, keyed on its own colour.
            let keyed = unsafe {
                SetLayeredWindowAttributes(window, COLORREF(plan.color), 0, LWA_COLORKEY)
            };
            keyed.expect("colour key");
        }
    }
    if let Some([left, top, right, bottom]) = plan.region {
        // SAFETY: a new region whose ownership passes to the window on success.
        let region = unsafe { CreateRectRgn(left, top, right, bottom) };
        // SAFETY: this process's own window takes the region and frees it later.
        let taken = unsafe { SetWindowRgn(window, Some(region), true) };
        assert_ne!(taken, 0, "region");
    }
    window
}

/// Draws `window` with `UpdateLayeredWindow` from a 32-bit bitmap whose every pixel is `pixel`.
fn draw(window: HWND, [width, height]: [i32; 2], pixel: u32) {
    // SAFETY: a memory DC this function deletes.
    let dc = unsafe { CreateCompatibleDC(None) };
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).expect("small header"),
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: the header is readable and `bits` writable; a new section's memory starts zeroed.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(dc),
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    }
    .expect("bitmap");
    let pixels = usize::try_from(width * height).expect("a small bitmap");
    // SAFETY: the new section holds `width * height` 32-bit pixels that nothing else uses yet.
    unsafe { std::slice::from_raw_parts_mut(bits.cast::<u32>(), pixels) }.fill(pixel);
    // SAFETY: selects this function's bitmap into its own DC.
    let previous = unsafe { SelectObject(dc, bitmap.into()) };
    let size = SIZE {
        cx: width,
        cy: height,
    };
    let origin = POINT::default();
    let blend = BLENDFUNCTION {
        BlendOp: u8::try_from(AC_SRC_OVER).expect("a byte flag"),
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: u8::try_from(AC_SRC_ALPHA).expect("a byte flag"),
    };
    // SAFETY: every pointer is to a local that outlives the call; the window is this process's.
    unsafe {
        UpdateLayeredWindow(
            window,
            None,
            None,
            Some(&raw const size),
            Some(dc),
            Some(&raw const origin),
            COLORREF(0),
            Some(&raw const blend),
            ULW_ALPHA,
        )
    }
    .expect("drawn layer");
    // SAFETY: puts back the DC's first bitmap.
    let _ours = unsafe { SelectObject(dc, previous) };
    // SAFETY: the bitmap is no longer selected and is deleted once.
    let _deleted = unsafe { DeleteObject(bitmap.into()) };
    // SAFETY: the DC came from CreateCompatibleDC and is deleted once.
    let _gone = unsafe { DeleteDC(dc) };
}

/// Checks before any grab that the resolved target is the host's window `index`, so a
/// resolution mistake can't capture another app's window, even into memory.
fn ours(target: &Target, host: &Host, index: usize) {
    assert_eq!(
        target.top.window,
        WindowRef(value(host.windows[index])),
        "the probe's window {index}"
    );
}

fn value(window: HWND) -> u64 {
    u64::try_from(window.0.addr()).expect("handles fit u64")
}

fn hwnd(value: u64) -> HWND {
    HWND(std::ptr::without_provenance_mut(
        usize::try_from(value).expect("handles fit usize"),
    ))
}

/// The host side: makes the plan's windows, then serves one command per stdin line until EOF.
#[test]
#[ignore = "the window probe's host, started by native_window_probe"]
fn native_probe_host() {
    if std::env::var_os(HOST_ENV).is_none() {
        return;
    }
    // SAFETY: this host's own DPI awareness, so its rectangles are physical pixels.
    let _aware =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let plans = plan();
    let mut windows: Vec<HWND> = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        let owner = plan.owner.map(|owner| windows[owner]);
        windows.push(create(plan, index, owner));
    }
    // SAFETY: a password edit inside the target, owned by this host.
    let edit = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(u32::try_from(ES_PASSWORD).expect("style")),
            20,
            20,
            120,
            24,
            Some(windows[T]),
            Some(HMENU::default()),
            Some(HINSTANCE::default()),
            None,
        )
    }
    .expect("password edit");
    for (index, window) in windows.iter().enumerate() {
        println!("hwnd {index} {}", value(*window));
    }
    println!("edit {}", value(edit));
    println!("ready");
    // SAFETY: reads this thread's id.
    let main = unsafe { GetCurrentThreadId() };
    let mut posted: Vec<u64> = windows.iter().map(|window| value(*window)).collect();
    drop(std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let mut words = line.split(' ');
            let (Some(verb), Some(index)) = (words.next(), words.next()) else {
                continue;
            };
            if verb == "hits" {
                println!("hits {}", HITS.load(Ordering::Relaxed));
                continue;
            }
            if verb == "recreate" {
                let index: usize = index.parse().expect("window index");
                // SAFETY: waits for the host's own window thread to replace its window.
                let made = unsafe {
                    SendMessageW(hwnd(posted[index]), RECREATE, Some(WPARAM(index)), None)
                };
                posted[index] = u64::try_from(made.0).expect("a window handle");
                continue;
            }
            let message = match verb {
                "cloak" => CLOAK,
                "protect" => PROTECT,
                "minimize" => MINIMIZE,
                "maximize" => MAXIMIZE,
                "restore" => RESTORE,
                "destroy" => DESTROY,
                "hang" => HANG,
                "nudge" => NUDGE,
                _ => continue,
            };
            let index: usize = index.parse().expect("window index");
            // SAFETY: posts to the host's own window; Windows validates the handle.
            unsafe { PostMessageW(Some(hwnd(posted[index])), message, WPARAM(0), LPARAM(0)) }
                .expect("posted");
        }
        // SAFETY: ends the host's message loop once stdin closes.
        unsafe { PostThreadMessageW(main, WM_QUIT, WPARAM(0), LPARAM(0)) }.expect("quit");
    }));
    let mut message = MSG::default();
    // SAFETY: `message` is writable; this thread owns the windows it pumps.
    while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0 > 0 {
        // SAFETY: dispatches the message this loop just read.
        unsafe { DispatchMessageW(&raw const message) };
    }
}

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    windows: Vec<HWND>,
    edit: HWND,
}

impl Host {
    fn start() -> Self {
        let spawning = crate::platform::spawn_lock();
        #[expect(
            clippy::disallowed_methods,
            reason = "spawned under the spawn lock, held just above"
        )]
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                HOST_TEST,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HOST_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("host");
        drop(spawning);
        let stdin = child.stdin.take();
        let mut lines = BufReader::new(child.stdout.take().expect("host stdout")).lines();
        let (mut windows, mut edit) = (Vec::new(), HWND::default());
        // libtest prints `test <name> ... ` without a newline, so the first line has a prefix.
        for line in lines.by_ref().map_while(Result::ok) {
            let words: Vec<&str> = line.split(' ').collect();
            match words.as_slice() {
                [.., "hwnd", _, handle] => windows.push(hwnd(handle.parse().expect("handle"))),
                [.., "edit", handle] => edit = hwnd(handle.parse().expect("handle")),
                [.., "ready"] => break,
                _ => {}
            }
        }
        assert_eq!(windows.len(), plan().len(), "the host made every window");
        let mut host = Self {
            child,
            stdin,
            lines,
            windows,
            edit,
        };
        host.command("cloak", C);
        host.command("protect", X);
        std::thread::sleep(Duration::from_millis(500));
        host
    }

    fn command(&mut self, verb: &str, index: usize) {
        let stdin = self.stdin.as_mut().expect("host stdin");
        writeln!(stdin, "{verb} {index}").expect("command");
        stdin.flush().expect("command");
        let reply = self
            .lines
            .by_ref()
            .map_while(Result::ok)
            .find(|line| line.starts_with("done"));
        assert_eq!(reply.as_deref(), Some("done true"), "{verb} {index}");
        std::thread::sleep(Duration::from_millis(400));
    }

    /// Has the host destroy window `index` and make the same plan's again, at the same frame on
    /// the same thread, and takes the new handle.
    fn recreate(&mut self, index: usize) {
        let stdin = self.stdin.as_mut().expect("host stdin");
        writeln!(stdin, "recreate {index}").expect("command");
        stdin.flush().expect("command");
        for line in self.lines.by_ref().map_while(Result::ok) {
            if let Some(handle) = line.strip_prefix(&format!("hwnd {index} ")) {
                self.windows[index] = hwnd(handle.parse().expect("handle"));
            } else if line.starts_with("done") {
                assert_eq!(line, "done true", "recreate {index}");
                return;
            }
        }
        panic!("the host ended during recreate {index}");
    }

    /// `WM_NCHITTEST` messages the host's windows have received so far.
    fn hits(&mut self) -> u64 {
        let stdin = self.stdin.as_mut().expect("host stdin");
        writeln!(stdin, "hits 0").expect("command");
        stdin.flush().expect("command");
        self.lines
            .by_ref()
            .map_while(Result::ok)
            .find_map(|line| line.strip_prefix("hits ").map(str::to_owned))
            .and_then(|count| count.parse().ok())
            .expect("hit count")
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let until = Instant::now() + PATIENCE;
        while Instant::now() < until {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _killed = self.child.kill();
        let _reaped = self.child.wait();
    }
}

fn rect(window: HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: `rect` is writable; Windows validates the handle.
    unsafe { GetWindowRect(window, &raw mut rect) }.expect("window rect");
    rect
}

/// Pixels of `frame` within 40 of `color` (0x00BBGGRR) on every channel.
fn count(frame: &RawFrame, color: u32) -> usize {
    let (_, bgra) = frame.image().expect("committed frame");
    let [r, g, b, _] = color.to_le_bytes();
    bgra.as_chunks::<4>()
        .0
        .iter()
        .filter(|px| {
            [px[2].abs_diff(r), px[1].abs_diff(g), px[0].abs_diff(b)]
                .iter()
                .all(|d| *d < 40)
        })
        .count()
}

fn pixel(frame: &RawFrame, target: &Target, [x, y]: [i32; 2]) -> [u8; 3] {
    let (meta, bgra) = frame.image().expect("committed frame");
    let width = usize::try_from(meta.display.size[0]).expect("width");
    let column = usize::try_from(x - target.top.rect.origin[0]).expect("inside the frame");
    let row = usize::try_from(y - target.top.rect.origin[1]).expect("inside the frame");
    let at = (row * width + column) * 4;
    [bgra[at + 2], bgra[at + 1], bgra[at]]
}

fn at(capturer: &mut Box<dyn Capturer>, point: PhysicalPoint) -> (Result<Target, Refusal>, u128) {
    let start = Instant::now();
    let target = capturer.pointer_window(point, 0);
    (target, start.elapsed().as_millis())
}

fn grab(
    capturer: &mut Box<dyn Capturer>,
    target: &Target,
    frame: &mut RawFrame,
) -> (Result<(), Failure>, u128) {
    let start = Instant::now();
    let until = Instant::now() + Duration::from_secs(2);
    let acquired = capturer.capture_window(target, until, frame);
    (acquired.map(|_| ()), start.elapsed().as_millis())
}

/// M1–M4 and M7's forced-handle and destroyed-target parts, over windows a second process owns.
/// Run it on an idle Windows desktop: `--ignored --exact --nocapture`. Pixels stay in
/// memory; it prints booleans, counts and milliseconds only.
#[test]
#[ignore = "opens sentinel windows on an idle Windows desktop and captures them, in memory"]
#[expect(
    clippy::too_many_lines,
    reason = "one probe run, in the plan's order from M1 to M7"
)]
fn native_window_probe() {
    let mut capturer = screen_capturer().expect("window capture");
    let mut host = Host::start();
    let point = |x, y| PhysicalPoint([x, y]);
    let mut frame = RawFrame::default();

    // M1: the target alone, with the owned occluder, owned popup, cloaked window and click-through
    // overlay above or beside it left out.
    let (target, resolve_ms) = at(&mut capturer, point(200, 200));
    let target = target.expect("the target under the pointer");
    ours(&target, &host, T);
    let (m1_capture, capture_ms) = grab(&mut capturer, &target, &mut frame);
    assert_eq!(m1_capture, Ok(()), "M1 capture");
    let meta = FrameMeta {
        capture: CaptureId(1),
        display: target.top.rect,
        dpi: target.dpi,
        acquired_after_ms: 0,
    };
    frame.commit(meta).expect("a full window frame");
    let m1 = [RED, GREEN, MAGENTA, YELLOW, PURPLE].map(|color| count(&frame, color));
    println!(
        "M1 resolve_ms={resolve_ms} capture_ms={capture_ms} red={} occluder={} popup={} cloaked={} overlay={}",
        m1[0], m1[1], m1[2], m1[3], m1[4]
    );
    assert!(m1[0] > 10_000, "the target's own pixels");
    assert_eq!(m1[1..], [0; 4], "nothing above or beside the target");

    // M2 and R1: what the walk reaches under the occluder, the click-through overlay, the cloaked
    // window above the target, and this worker's own window above the target.
    let own = create(
        &Plan {
            color: GREEN,
            rect: [110, 110, 60, 40],
            owner: None,
            style: WS_POPUP | WS_VISIBLE,
            ex: WS_EX_TOPMOST,
            layer: Layering::Plain,
            region: None,
        },
        100,
        None,
    );
    for (name, p, expected) in [
        ("occluder", point(350, 200), host.windows[O]),
        ("overlay", point(180, 270), host.windows[T]),
        ("cloaked-above", point(150, 350), host.windows[T]),
        ("own-above", point(130, 130), host.windows[T]),
        ("disabled", point(430, 125), host.windows[DISABLED]),
        ("region-excluded", point(330, 125), host.windows[T]),
        ("region-included", point(365, 125), host.windows[REGION]),
        ("alpha-0", point(265, 170), host.windows[T]),
    ] {
        let (found, _) = at(&mut capturer, p);
        let window = found.map(|target| target.top.window);
        println!(
            "M2 {name} expected={}",
            window == Ok(WindowRef(value(expected)))
        );
        assert_eq!(window, Ok(WindowRef(value(expected))), "{name}");
    }

    // R1: the shell's desktop window ends the walk.
    // SAFETY: finds the shell's desktop window by class; nothing is read.
    let desktop = unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }.expect("desktop window");
    let whole = rect(desktop);
    let middle = point(
        whole.left + (whole.right - whole.left) / 2,
        whole.top + (whole.bottom - whole.top) / 2,
    );
    let stop = step(facts(desktop, middle, 0));
    println!("R1 desktop_stop={}", stop == Step::Stop);
    assert_eq!(stop, Step::Stop, "R1 desktop");

    // N4: a layered window drawn at zero alpha by UpdateLayeredWindow lets clicks through, though
    // nothing in its styles says so; the walk asks the window manager's hit test.
    let before = host.hits();
    let (found, drawn_ms) = at(&mut capturer, point(270, 350));
    let sent = host.hits() - before;
    let window = found.map(|target| target.top.window);
    println!(
        "N4 drawn_overlay_taken={} target_taken={} resolve_ms={drawn_ms} nchittest_to_host={sent}",
        window == Ok(WindowRef(value(host.windows[U]))),
        window == Ok(WindowRef(value(host.windows[T])))
    );
    assert_eq!(
        window,
        Ok(WindowRef(value(host.windows[T]))),
        "N4 drawn overlay"
    );
    // N4 with every host window hung: a hit test that waited on another process would block here.
    host.command("hang", U);
    let before = host.hits();
    let (found, hung_ms) = at(&mut capturer, point(270, 350));
    let sent = host.hits() - before;
    let window = found.map(|target| target.top.window);
    println!(
        "N4 hung_host target_taken={} resolve_ms={hung_ms} nchittest_to_host={sent}",
        window == Ok(WindowRef(value(host.windows[T])))
    );
    assert_eq!(
        window,
        Ok(WindowRef(value(host.windows[T]))),
        "N4 hung host"
    );
    assert!(hung_ms < 1_000, "N4: the hit test waited on the hung host");
    std::thread::sleep(HUNG);
    // The counter's own check: one hit test this probe sends the host on purpose.
    let before = host.hits();
    let mut answer = 0;
    // SAFETY: asks the host's own window for one hit test at (200, 200), for at most a second.
    let asked = unsafe {
        SendMessageTimeoutW(
            host.windows[T],
            WM_NCHITTEST,
            WPARAM(0),
            LPARAM(0x00C8_00C8),
            SMTO_ABORTIFHUNG,
            1_000,
            Some(&raw mut answer),
        )
    };
    let counted = host.hits() - before;
    println!("N4 counter_check sent={} counted={counted}", asked.0 != 0);
    assert_eq!(counted, 1, "N4: the host counts a hit test sent to it");
    let above = create(
        &Plan {
            color: GREEN,
            rect: [260, 340, 20, 20],
            owner: None,
            style: WS_POPUP | WS_VISIBLE,
            ex: WS_EX_TOPMOST,
            layer: Layering::Plain,
            region: None,
        },
        101,
        None,
    );
    let covered = at(&mut capturer, point(270, 350)).0.map(|_| ());
    println!("N4 own_window_above_drawn={covered:?}");
    assert_eq!(
        covered,
        Err(Refusal::Ambiguous),
        "N4 under Kumochi's window"
    );
    // SAFETY: destroys this thread's own window.
    unsafe { DestroyWindow(above) }.expect("own window");

    // F2: `WS_EX_TRANSPARENT` without `WS_EX_LAYERED` isn't click-through, so the walk takes
    // it; the hit test's own answer is recorded.
    let taken = at(&mut capturer, point(340, 375))
        .0
        .map(|target| target.top.window);
    // SAFETY: a hit test at a physical point; it returns an opaque handle.
    let hit = unsafe { WindowFromPoint(POINT { x: 340, y: 375 }) };
    // SAFETY: this only queries the opaque handle.
    let hit = unsafe { GetAncestor(hit, GA_ROOT) };
    println!(
        "F2 bit_only_taken={} hit_test_bit_only={} hit_test_target={}",
        taken == Ok(WindowRef(value(host.windows[BIT_ONLY]))),
        hit == host.windows[BIT_ONLY],
        hit == host.windows[T]
    );
    assert_eq!(
        taken,
        Ok(WindowRef(value(host.windows[BIT_ONLY]))),
        "F2 bit only"
    );

    // F1: an opaque drawn window under a window the walk takes, or under a cloaked one the walk
    // skips, never passes for the target beneath it.
    host.command("cloak", CLOAK_OVER_DRAWN);
    for (name, p, drawn) in [
        ("under_bit", point(380, 375), DRAWN_UNDER_BIT),
        ("under_cloak", point(420, 375), DRAWN_UNDER_CLOAK),
    ] {
        let found = at(&mut capturer, p).0.map(|target| target.top.window);
        println!(
            "F1 {name} target_taken={} drawn_taken={} bit_taken={} ambiguous={}",
            found == Ok(WindowRef(value(host.windows[T]))),
            found == Ok(WindowRef(value(host.windows[drawn]))),
            found == Ok(WindowRef(value(host.windows[BIT_OVER_DRAWN]))),
            found == Err(Refusal::Ambiguous)
        );
        assert_ne!(found, Ok(WindowRef(value(host.windows[T]))), "F1 {name}");
    }

    // F3: a window whose every pixel is its colour key lets the click reach the target.
    let keyed = at(&mut capturer, point(460, 375))
        .0
        .map(|target| target.top.window);
    println!(
        "F3 keyed_target_taken={} keyed_taken={}",
        keyed == Ok(WindowRef(value(host.windows[T]))),
        keyed == Ok(WindowRef(value(host.windows[KEYED])))
    );
    assert_eq!(keyed, Ok(WindowRef(value(host.windows[T]))), "F3 keyed");

    // F4: an opaque drawn window is taken; a disabled one can't be told and refuses.
    let opaque = at(&mut capturer, point(250, 215))
        .0
        .map(|target| target.top.window);
    let corner = rect(host.windows[DRAWN_DISABLED]);
    let disabled = at(&mut capturer, point(corner.left + 10, corner.top + 10))
        .0
        .map(|_| ());
    println!(
        "F4 opaque_drawn_taken={} disabled_drawn={disabled:?}",
        opaque == Ok(WindowRef(value(host.windows[DRAWN_OPAQUE])))
    );
    assert_eq!(
        opaque,
        Ok(WindowRef(value(host.windows[DRAWN_OPAQUE]))),
        "F4 opaque drawn"
    );
    assert_eq!(disabled, Err(Refusal::Ambiguous), "F4 disabled drawn");

    // M3: the password edit maps to the frame by the extended frame bounds' origin, to ±0 px.
    for state in ["normal", "maximized"] {
        if state == "maximized" {
            host.command("maximize", T);
        }
        let target = at(&mut capturer, point(200, 200)).0.expect("target");
        ours(&target, &host, T);
        assert_eq!(
            grab(&mut capturer, &target, &mut frame).0,
            Ok(()),
            "M3 {state}"
        );
        frame
            .commit(FrameMeta {
                display: target.top.rect,
                ..meta
            })
            .expect("a full window frame");
        let edit = rect(host.edit);
        let outer = rect(host.windows[T]);
        let inside = pixel(&frame, &target, [edit.left, edit.top + 5]);
        let left = pixel(&frame, &target, [edit.left - 1, edit.top + 5]);
        println!(
            "M3 {state} origin_minus_window_rect=({},{}) edit_edge_white={} left_of_edge_red={}",
            target.top.rect.origin[0] - outer.left,
            target.top.rect.origin[1] - outer.top,
            inside == [255, 255, 255],
            left[0] > 200 && left[1] < 40 && left[2] < 40,
        );
        assert_eq!(
            inside,
            [255, 255, 255],
            "M3 {state}: the edit's first column"
        );
        assert!(
            left[0] > 200 && left[1] < 40,
            "M3 {state}: the target just left of it"
        );
    }
    host.command("restore", T);

    // M4: refusals before any pixels.
    let protected = at(&mut capturer, point(750, 350)).0.map(|_| ());
    let cloaked = preflight(host.windows[C], 0).map(|_| ());
    let excluded = preflight(own, 0).map(|_| ());
    let minimised = at(&mut capturer, point(950, 150))
        .0
        .expect("before minimising");
    ours(&minimised, &host, M);
    host.command("minimize", M);
    let stale = grab(&mut capturer, &minimised, &mut frame).0;
    let refused = preflight(host.windows[M], 0).map(|_| ());
    println!(
        "M4 protected={protected:?} cloaked={cloaked:?} minimised_capture={stale:?} minimised_preflight={refused:?} own={excluded:?}"
    );
    assert_eq!(protected, Err(Refusal::Protected), "M4c affinity");
    assert_eq!(cloaked, Err(Refusal::Cloaked), "M4c cloaked target");
    assert_eq!(excluded, Err(Refusal::Excluded), "M7 own window");
    assert_eq!(refused, Err(Refusal::Minimized), "M4a preflight");
    assert!(
        stale.is_err(),
        "M4a: a window minimised after the walk gives no frame"
    );
    assert!(frame.image().is_none(), "M4a: no image left");

    // N2: a target cloaked or capture-protected after the walk gives no frame.
    for (name, index, verb) in [("protected", P, "protect"), ("cloaked", K, "cloak")] {
        let corner = rect(host.windows[index]);
        let walked = at(&mut capturer, point(corner.left + 10, corner.top + 10))
            .0
            .expect("before the change");
        ours(&walked, &host, index);
        host.command(verb, index);
        let changed = grab(&mut capturer, &walked, &mut frame).0;
        println!("N2 {name}_after_walk={changed:?}");
        assert_eq!(
            changed,
            Err(Failure::Refused(Refusal::WindowChanged)),
            "N2 {name}"
        );
        assert!(frame.image().is_none(), "N2 {name}: no image left");
    }

    // M7: a target destroyed after the walk gives no frame.
    let doomed = at(&mut capturer, point(950, 350))
        .0
        .expect("before destroying");
    ours(&doomed, &host, D);
    host.command("destroy", D);
    let destroyed = grab(&mut capturer, &doomed, &mut frame).0;
    println!("M7 destroyed={destroyed:?}");
    assert_eq!(
        destroyed,
        Err(Failure::Refused(Refusal::WindowChanged)),
        "M7: no frame from a destroyed window"
    );
    assert!(frame.image().is_none(), "M7: no image left");
    // SAFETY: destroys this thread's own window.
    unsafe { DestroyWindow(own) }.expect("own window");
}

/// Walks to the host's window `index` and checks it's the one resolved.
fn walked(capturer: &mut Box<dyn Capturer>, host: &Host, index: usize) -> Target {
    let corner = rect(host.windows[index]);
    let point = PhysicalPoint([corner.left + 10, corner.top + 10]);
    let target = at(capturer, point).0.expect("the window under the point");
    ours(&target, host, index);
    target
}

/// N5 and U0–U6: the seal-time identity check on real windows a second process owns, including
/// a window its thread destroys and makes again at the same frame. Run it on an idle Windows
/// desktop: `--ignored --exact --nocapture`. Pixels stay in memory; it prints booleans, counts,
/// handle values and nanoseconds only.
#[test]
#[ignore = "opens sentinel windows on an idle Windows desktop and captures them, in memory"]
fn native_unchanged_probe() {
    let mut capturer = screen_capturer().expect("window capture");
    let mut host = Host::start();
    let mut pixels = RawFrame::default();

    // N5: the same plan made again by the same thread differs from the walked window only by its
    // handle, which is what `unchanged` and the capture must notice.
    let before = walked(&mut capturer, &host, M);
    assert!(unchanged(&before), "N5: the walked window, untouched");
    let old = host.windows[M];
    host.recreate(M);
    std::thread::sleep(Duration::from_millis(100));
    let new = host.windows[M];
    let only_handle =
        owner(new) == (before.top.pid, before.instance) && frame(new) == Some(before.top.rect);
    let kept = unchanged(&before);
    let taken = grab(&mut capturer, &before, &mut pixels).0;
    println!(
        "N5 old={} new={} same_value={} only_handle_differs={only_handle} unchanged={kept} capture={taken:?}",
        value(old),
        value(new),
        old == new
    );
    assert!(
        only_handle,
        "N5: the recreated window keeps its thread and frame"
    );
    assert!(!kept, "N5: a recreated window isn't the walked one");
    assert_eq!(
        taken,
        Err(Failure::Refused(Refusal::WindowChanged)),
        "N5 capture"
    );
    assert!(pixels.image().is_none(), "N5: no image left");

    // N5b: whether the window manager hands the walked handle out again within a few hundred
    // remakes, and whether such a remake ever passes.
    let (mut repeats, mut passed) = (0_u32, 0_u32);
    for _ in 0..256 {
        host.recreate(M);
        repeats += u32::from(host.windows[M] == old);
        passed += u32::from(unchanged(&before));
    }
    println!("N5b cycles=256 repeats={repeats} unchanged={passed}");
    assert_eq!(
        passed, 0,
        "N5b: a remade window never passes for the walked one"
    );

    // U0: an untouched window passes every time, and what one check costs.
    let target = walked(&mut capturer, &host, M);
    let mut spent: Vec<u128> = (0..1_000)
        .map(|_| {
            let start = Instant::now();
            assert!(unchanged(&target), "U0: an untouched window");
            start.elapsed().as_nanos()
        })
        .collect();
    spent.sort_unstable();
    println!(
        "U0 calls=1000 median_ns={} max_ns={}",
        spent[500], spent[999]
    );

    // U1–U6: each change after the walk fails the check.
    let mut changed = Vec::new();
    for (name, verb, index) in [
        ("U1 nudged", "nudge", M),
        ("U2 maximized", "maximize", M),
        ("U3 minimised", "minimize", M),
        ("U4 cloaked", "cloak", D),
        ("U5 protected", "protect", P),
        ("U6 destroyed", "destroy", K),
    ] {
        let target = walked(&mut capturer, &host, index);
        host.command(verb, index);
        let kept = unchanged(&target);
        println!("{name}={}", !kept);
        changed.push((name, kept));
        if matches!(verb, "maximize" | "minimize") {
            host.command("restore", index);
        }
    }
    assert!(
        changed.iter().all(|(_, kept)| !kept),
        "every change fails the check: {changed:?}"
    );
}
