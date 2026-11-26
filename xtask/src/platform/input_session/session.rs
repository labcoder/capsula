//! The approved session's guarded, tagged native input. Nothing is injectable until the owner's
//! physical Ready, or the idle opener's one guarded fixture click, and one quiet second open the
//! lease.

use std::cell::Cell;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GUITHREADINFO, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo,
    IsWindowVisible, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETTEXT, WM_SETTEXT,
    WindowFromPoint,
};
use windows::core::w;

use super::activation::{Activation, Owner};
use super::fixture::{
    ABORT, ARMED_INPUT, COVER, ESC_HELD, ESCAPE_LEASE, FEEL, INTERRUPTED, OPEN, OPEN_GATE, READY,
    REARM, UNCOVER,
};
use super::gate::{Feel, Gate, Lease, NullForeground, Quiet};
use super::guard::{self, StopMetadata};
use super::{
    FixtureEscMode, InputEdge, MARKER, MAX_COVER, Receipt, Window, class_name, desktop_rect,
    input_desktop_available, last_input, mouse_and_keyboard_only, normalized, pack, pid, property,
    request, size,
};

mod idle_activation;

/// How old a destination snapshot may be when its input is sent.
pub(super) const FRESH: Duration = Duration::from_millis(20);

/// Who must own the pixel under a pointer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// The bound product's presence proxy.
    Product,
    /// The fixture's EDIT, the harmless receiver under pass-through pixels.
    Edit,
    /// Any fixture window.
    Fixture,
    /// Covered only: exactly the bound product's proxy or the fixture's EDIT, whichever owns the
    /// pixel when the input is sent. A moving mask can hand a pixel over between frames, so the
    /// recipient is observed from receipts rather than predicted.
    Receipt,
    /// Exactly the bound panel window, as for its Stop button.
    Panel,
}

/// The class GPUI gives every window it creates on Windows.
const GPUI_WINDOW: &str = "Zed::Window";

#[derive(Debug, Clone, Copy)]
enum Aim {
    Move(POINT, Target),
    Click(POINT, Target),
}

#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) fixture: Window,
    product: Option<Window>,
    /// The bound product's panel window, from its observer.
    panel: Option<HWND>,
    /// When the bound product's input must end, so its cleanup fits its lifetime.
    product_end: Option<Instant>,
    edit: HWND,
    activation: Activation,
    lease: Cell<Lease>,
    null_foreground: Cell<NullForeground>,
    first_stop: Cell<Option<StopMetadata>>,
    end: Instant,
    held: [bool; 512],
    button_down: bool,
    covered: Option<RECT>,
    original_cursor: Option<POINT>,
    pointer_moved: bool,
    /// How long the idle activation's confirm waited for the fixture's root to come to the front.
    pub(crate) root_wait: Duration,
    /// When the driver found the fixture root: the session is made as discovery returns it.
    found: Instant,
    pub(crate) batches: u32,
    pub(crate) first_qpc: Option<i64>,
    /// QPC just before the last batch's `SendInput`; `None` if that batch never reached it.
    pub(crate) last_qpc: Option<i64>,
    /// QPC as soon as the last batch's `SendInput` returned.
    pub(crate) last_end_qpc: Option<i64>,
    /// How many inputs of the last batch `SendInput` accepted.
    pub(crate) last_accepted: usize,
    pub(crate) qpc_frequency: i64,
}

impl Session {
    /// A session on `fixture`, closed to injection until Ready and the quiet second.
    pub(crate) fn new(fixture: Window, end: Instant) -> Result<Self> {
        let found = Instant::now();
        fixture.validate()?;
        mouse_and_keyboard_only()?;
        // SAFETY: only a standard EDIT child under the PID-validated fixture is searched.
        let edit = unsafe { FindWindowExW(Some(fixture.hwnd), None, w!("EDIT"), None) }?;
        ensure!(pid(edit) == fixture.pid, "fixture EDIT ownership mismatch");
        let mut qpc_frequency = 0;
        // SAFETY: qpc_frequency is a writable counter-frequency output.
        unsafe { QueryPerformanceFrequency(&raw mut qpc_frequency) }?;
        Ok(Self {
            fixture,
            product: None,
            panel: None,
            product_end: None,
            edit,
            activation: Activation::new(owner(fixture.hwnd), owner(edit)),
            lease: Cell::new(Lease::new(fixture.pid)),
            null_foreground: Cell::new(NullForeground::default()),
            first_stop: Cell::new(None),
            end,
            held: [false; 512],
            button_down: false,
            covered: None,
            original_cursor: None,
            pointer_moved: false,
            root_wait: Duration::ZERO,
            found,
            batches: 0,
            first_qpc: None,
            last_qpc: None,
            last_end_qpc: None,
            last_accepted: 0,
            qpc_frequency,
        })
    }

    /// The NULL-foreground graces used since the last call, and the longest of them.
    pub(crate) fn take_null_graces(&self) -> (u32, Duration) {
        let mut null = self.null_foreground.get();
        let used = null.take_used(Instant::now());
        self.null_foreground.set(null);
        used
    }

    /// Whether the fixture's own window, not merely an owned process, is foreground.
    pub(crate) fn fixture_foreground(&self) -> bool {
        // SAFETY: reads the foreground HWND without touching its content.
        let foreground = unsafe { GetForegroundWindow() };
        foreground == self.fixture.hwnd
    }

    pub(crate) fn typing_window(&self) -> Result<Window> {
        self.guard()?;
        let product = self.product()?;
        // SAFETY: reads the current foreground handle without changing it.
        let hwnd = unsafe { GetForegroundWindow() };
        ensure!(
            pid(hwnd) == product.pid && class_name(hwnd) == GPUI_WINDOW && Some(hwnd) != self.panel,
            "foreground is not the bound product's typing window"
        );
        Ok(Window {
            hwnd,
            pid: product.pid,
        })
    }

    /// Shows a separate, unowned fixture window without activating it; sends no input.
    pub(crate) fn switch_window(&self) -> Result<Window> {
        self.guard()?;
        ensure!(
            self.covered.is_none(),
            "switch window needs the fixture uncovered"
        );
        let result = (|| {
            let window =
                super::switch_window::open(self.fixture, self.typing_window()?, self.product()?)?;
            self.guard()?;
            Ok(window)
        })();
        self.or_abort(result.map_err(|error| super::switch_window::failed(self.fixture, error)))
    }

    pub(crate) fn edit_point(&self) -> Result<POINT> {
        self.guard()?;
        self.edit_center()
    }

    fn edit_center(&self) -> Result<POINT> {
        let rect = Window {
            hwnd: self.edit,
            pid: self.fixture.pid,
        }
        .rect()?;
        ensure!(
            rect.left < rect.right && rect.top < rect.bottom,
            "fixture EDIT has no area"
        );
        Ok(POINT {
            x: rect.left.midpoint(rect.right),
            y: rect.top.midpoint(rect.bottom),
        })
    }

    /// The fixture EDIT's click receipts so far.
    pub(crate) fn fixture_receipt(&self) -> Result<Receipt> {
        self.fixture.receipt()
    }

    pub(crate) fn gate(&self) -> Gate {
        self.lease.get().gate()
    }

    /// Sets a bounded caller-defined numeric phase; zero means unspecified. Sends no input.
    pub(crate) fn set_phase(&self, phase: u8) -> Result<()> {
        self.or_abort(request(
            self.fixture,
            guard::SET_PHASE,
            WPARAM(usize::from(phase)),
            LPARAM(0),
        ))?;
        Ok(())
    }

    /// Retains the first native stop snapshot even after the fixture closes.
    pub(crate) fn stop_metadata(&self) -> Option<StopMetadata> {
        if let Some(first) = self.first_stop.get() {
            return Some(first);
        }
        self.fixture.validate().ok()?;
        let first = guard::snapshot(self.fixture.hwnd);
        self.first_stop.set(first);
        first
    }

    fn abort(&self) {
        self.first_stop.set(self.stop_metadata());
        let mut lease = self.lease.get();
        lease.abort();
        self.lease.set(lease);
    }

    /// Passes `result` through, aborting the session for good if it failed.
    fn or_abort<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.abort();
        }
        result
    }

    /// Trusts a newly launched product until `input_end`, under the same lease: no new Ready.
    pub(crate) fn bind_product(&mut self, product: Window, input_end: Instant) -> Result<()> {
        product.validate()?;
        ensure!(
            product.pid != self.fixture.pid,
            "the product can't be the fixture"
        );
        self.product = Some(product);
        self.panel = None;
        self.product_end = Some(input_end);
        let mut lease = self.lease.get();
        lease.rebind(Some(product.pid));
        self.lease.set(lease);
        Ok(())
    }

    /// Forgets the product and its input cutoff; the session's own deadline still holds.
    pub(crate) fn unbind_product(&mut self) {
        self.product = None;
        self.panel = None;
        self.product_end = None;
        let mut lease = self.lease.get();
        lease.rebind(None);
        self.lease.set(lease);
    }

    pub(crate) fn product(&self) -> Result<Window> {
        self.product.context("no product is bound to the session")
    }

    /// Waits, sending nothing, for the owner's physical Ready; returns the frozen feel verdicts.
    pub(crate) fn await_ready(&self, deadline: Instant) -> Result<Feel> {
        ensure!(self.gate() == Gate::Waiting, "Ready was already taken");
        while property(self.fixture.hwnd, READY) == 0 {
            ensure!(
                Instant::now() < deadline.min(self.end),
                "no Ready before the deadline"
            );
            self.fixture.validate()?;
            ensure!(
                property(self.fixture.hwnd, ABORT) == 0,
                "fixture guard aborted before Ready"
            );
            sleep(Duration::from_millis(10));
        }
        ensure!(
            Instant::now() < deadline.min(self.end),
            "Ready arrived after the deadline"
        );
        ensure!(
            property(self.fixture.hwnd, ABORT) == 0,
            "fixture guard aborted at Ready"
        );
        let armed = u32::try_from(property(self.fixture.hwnd, ARMED_INPUT));
        let feel = Feel::decode(property(self.fixture.hwnd, FEEL));
        let mut lease = self.lease.get();
        lease.ready(armed.unwrap_or(0), Instant::now());
        self.lease.set(lease);
        self.or_abort(armed.map_err(anyhow::Error::from).and(feel))
    }

    /// Waits, sending nothing, until Rez has had no input for `away`, then makes the fixture
    /// foreground with its one guarded, tagged EDIT click. The quiet second follows as after
    /// Ready, except that any input before it opens is terminal.
    pub(crate) fn await_idle(&mut self, deadline: Instant, away: Duration) -> Result<()> {
        let result = self
            .activate(deadline.min(self.end), away)
            .with_context(|| {
                format!(
                    "fixture activation refused; {}",
                    super::foreground_refusal()
                )
            });
        let armed = self.or_abort(result)?;
        let mut lease = self.lease.get();
        lease.activated(armed, Instant::now());
        self.lease.set(lease);
        Ok(())
    }

    /// True after one guarded quiet second opens injection; false after rearming for a new Ready.
    pub(crate) fn await_quiet(&mut self, deadline: Instant) -> Result<bool> {
        let result = (|| {
            ensure!(
                matches!(self.gate(), Gate::Quiet { .. }),
                "the quiet second needs Ready first"
            );
            loop {
                ensure!(
                    Instant::now() < deadline.min(self.end),
                    "quiet second deadline exceeded"
                );
                self.fixture.validate()?;
                let settled = self.settled();
                let quiet = Quiet {
                    aborted: property(self.fixture.hwnd, ABORT) != 0,
                    interrupted: property(self.fixture.hwnd, INTERRUPTED) != 0,
                    last_input: last_input()?,
                    settled: settled.is_ok(),
                };
                let mut lease = self.lease.get();
                lease.observe(quiet, Instant::now());
                match lease.gate() {
                    Gate::Open => {
                        let input = property(self.fixture.hwnd, ARMED_INPUT);
                        match request(self.fixture, OPEN_GATE, WPARAM(input), LPARAM(0))? {
                            1 => {
                                ensure!(
                                    Instant::now() < deadline.min(self.end)
                                        && last_input()? == quiet.last_input
                                        && property(self.fixture.hwnd, OPEN) != 0
                                        && property(self.fixture.hwnd, ABORT) == 0,
                                    "input or deadline raced the native opening"
                                );
                                self.lease.set(lease);
                                break;
                            }
                            2 => {
                                println!(
                                    "ready_rearm reason=native_opening_race injection_opened=false"
                                );
                                let mut waiting = self.lease.get();
                                waiting.observe(
                                    Quiet {
                                        interrupted: true,
                                        ..quiet
                                    },
                                    Instant::now(),
                                );
                                self.lease.set(waiting);
                                return Ok(false);
                            }
                            _ => bail!("unknown native gate response"),
                        }
                    }
                    Gate::Quiet { .. } => sleep(Duration::from_millis(1)),
                    Gate::Waiting => {
                        println!(
                            "ready_rearm interrupted={} last_input_changed={} settled_error={:?}",
                            quiet.interrupted,
                            usize::try_from(quiet.last_input)?
                                != property(self.fixture.hwnd, ARMED_INPUT),
                            settled.err()
                        );
                        request(self.fixture, REARM, WPARAM(0), LPARAM(0))?;
                        self.lease.set(lease);
                        return Ok(false);
                    }
                    Gate::Aborted => bail!("session lock or guard failure during the quiet second"),
                }
            }
            let mut cursor = POINT::default();
            // SAFETY: cursor is a writable output; nothing moves.
            unsafe { GetCursorPos(&raw mut cursor) }?;
            self.original_cursor.get_or_insert(cursor);
            Ok(true)
        })();
        self.or_abort(result)
    }

    /// The fixture is foreground with its EDIT focused, nothing is held, captured, in a menu or in
    /// a move loop, and the input desktop is ours.
    fn settled(&self) -> Result<()> {
        ensure!(Instant::now() < self.end, "input phase deadline exceeded");
        mouse_and_keyboard_only()?;
        self.fixture.validate()?;
        // SAFETY: reads the foreground HWND only.
        let foreground = unsafe { GetForegroundWindow() };
        ensure!(
            foreground == self.fixture.hwnd,
            "the fixture is not foreground"
        );
        let mut info = GUITHREADINFO {
            cbSize: size::<GUITHREADINFO>(),
            ..Default::default()
        };
        // SAFETY: thread zero is the foreground thread, here the fixture's; info is writable.
        unsafe { GetGUIThreadInfo(0, &raw mut info) }?;
        ensure!(
            info.hwndFocus == self.edit,
            "the fixture EDIT doesn't have focus"
        );
        ensure!(
            [info.hwndCapture, info.hwndMenuOwner, info.hwndMoveSize]
                .iter()
                .all(HWND::is_invalid),
            "a capture, menu or move loop is active"
        );
        for key in 1..256 {
            // SAFETY: reads a down state only; no key identity is kept.
            let down = unsafe { GetAsyncKeyState(key) } < 0;
            ensure!(!down, "a key or button is held");
        }
        input_desktop_available()
    }

    pub(crate) fn guard(&self) -> Result<()> {
        ensure!(
            self.lease.get().injectable(),
            "injection is closed ({:?}); no retry",
            self.gate()
        );
        self.or_abort(self.check())
    }

    fn check(&self) -> Result<()> {
        ensure!(Instant::now() < self.end, "input phase deadline exceeded");
        mouse_and_keyboard_only()?;
        self.fixture.validate()?;
        if let Some(product) = self.product {
            product.validate()?;
        }
        ensure!(
            self.product_end.is_none_or(|end| Instant::now() < end),
            "the product's input interval is over"
        );
        ensure!(
            property(self.fixture.hwnd, OPEN) != 0 && property(self.fixture.hwnd, ABORT) == 0,
            "native input gate stopped (first cause: {:?}); session aborted, no retry",
            self.stop_metadata().map(|stop| stop.cause)
        );
        // SAFETY: only the PID is inspected, never another application's window content.
        let foreground = pid(unsafe { GetForegroundWindow() });
        let mut null = self.null_foreground.get();
        let seen = null.observe(foreground, &self.lease.get(), Instant::now());
        self.null_foreground.set(null);
        seen?;
        input_desktop_available()?;
        let mut info = GUITHREADINFO {
            cbSize: size::<GUITHREADINFO>(),
            ..Default::default()
        };
        // SAFETY: thread zero queries the foreground thread and info has its documented writable size.
        unsafe { GetGUIThreadInfo(0, &raw mut info) }?;
        let lease = self.lease.get();
        ensure!(
            [info.hwndCapture, info.hwndMenuOwner, info.hwndMoveSize]
                .into_iter()
                .all(|window| window.is_invalid() || lease.owns(pid(window))),
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

    /// Checks who owns `point` now, and for a click that the pointer is already there.
    fn aim(&self, aim: Aim) -> Result<()> {
        let (point, target) = match aim {
            Aim::Move(point, target) => (point, target),
            Aim::Click(point, target) => {
                let mut cursor = POINT::default();
                // SAFETY: cursor is a writable output; nothing moves.
                unsafe { GetCursorPos(&raw mut cursor) }?;
                ensure!(cursor == point, "the pointer is not at the click point");
                (point, target)
            }
        };
        if let Some(cover) = self.covered {
            ensure!(
                point.x >= cover.left
                    && point.x < cover.right
                    && point.y >= cover.top
                    && point.y < cover.bottom,
                "pointer point is outside the fixture's cover"
            );
        }
        // SAFETY: resolves ownership at the point without sending input.
        let actual = unsafe { WindowFromPoint(point) };
        let owner = pid(actual);
        let edit = actual == self.edit && owner == self.fixture.pid;
        let proxy = |exact: bool| -> Result<bool> {
            let product = self.product()?;
            Ok(owner == product.pid
                && (!exact || actual == product.hwnd)
                && class_name(actual).ends_with("-presence"))
        };
        let matches = match target {
            Target::Edit => edit,
            Target::Fixture => owner == self.fixture.pid,
            Target::Product => proxy(false)?,
            Target::Panel => Some(actual) == self.panel && owner == self.product()?.pid,
            Target::Receipt => {
                ensure!(
                    self.covered.is_some(),
                    "receipt input needs the fixture's cover"
                );
                edit || proxy(true)?
            }
        };
        ensure!(
            matches,
            "the window at the pointer point isn't the expected {target:?}"
        );
        Ok(())
    }

    fn send(&mut self, inputs: &[INPUT], aim: Option<Aim>) -> Result<()> {
        self.last_qpc = None;
        self.last_end_qpc = None;
        self.last_accepted = 0;
        let input_size = i32::try_from(size::<INPUT>())?;
        self.guard()?;
        let snapshot = Instant::now();
        if let Some(aim) = aim {
            self.or_abort(self.aim(aim))?;
        }
        let mut qpc = 0;
        // SAFETY: qpc is a writable counter output.
        unsafe { QueryPerformanceCounter(&raw mut qpc) }?;
        let fresh = snapshot.elapsed() <= FRESH;
        self.or_abort(
            fresh
                .then_some(())
                .context("destination snapshot went stale before sending"),
        )?;
        // SAFETY: only the PID is inspected; the NULL grace never covers sending.
        let foreground = pid(unsafe { GetForegroundWindow() });
        self.or_abort(
            self.lease
                .get()
                .owns(foreground)
                .then_some(())
                .with_context(|| {
                    format!("no owned window in front to send to (foreground_pid={foreground})")
                }),
        )?;
        self.first_qpc.get_or_insert(qpc);
        self.last_qpc = Some(qpc);
        // SAFETY: every input is fully initialized and carries the session's injection marker.
        let accepted = unsafe { SendInput(inputs, input_size) };
        let mut end = 0;
        // SAFETY: end is a writable counter output.
        let ended = unsafe { QueryPerformanceCounter(&raw mut end) };
        self.last_end_qpc = ended.is_ok().then_some(end);
        self.last_accepted = usize::try_from(accepted)?;
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
                // SAFETY: the INPUT tag identifies the initialized mouse union member.
                let flags = unsafe { input.Anonymous.mi }.dwFlags;
                self.pointer_moved |= flags.contains(MOUSEEVENTF_MOVE);
                if flags.contains(MOUSEEVENTF_LEFTDOWN) {
                    self.button_down = true;
                }
                if flags.contains(MOUSEEVENTF_LEFTUP) {
                    self.button_down = false;
                }
            }
        }
        if usize::try_from(accepted)? != inputs.len() {
            self.abort();
            let released = self.release_button();
            bail!(
                "SendInput accepted an incomplete batch (acknowledged button released: {released})"
            );
        }
        self.batches += 1;
        self.guard()
    }

    pub(crate) fn modifier(&mut self, scan: u16, edge: InputEdge) -> Result<()> {
        ensure!(
            matches!(scan, 0x1d | 0x38 | 0x138),
            "unsupported session modifier"
        );
        self.send(&[scan_input(scan, edge)], None)
    }

    pub(crate) fn tap(&mut self, scan: u16) -> Result<()> {
        ensure!(scan < 512, "unsupported session scan code");
        self.send(
            &[
                scan_input(scan, InputEdge::Down),
                scan_input(scan, InputEdge::Up),
            ],
            None,
        )
    }

    /// Releases only what `SendInput` acknowledged as down: keys and the left button. Runs
    /// after an abort too, and sends nothing when nothing is held.
    pub(crate) fn release_injected(&mut self) -> Result<()> {
        let mut failed = !self.release_button();
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
        ensure!(!failed, "best-effort injected release was incomplete");
        Ok(())
    }

    /// Releases an acknowledged left-button down, if any; false if Windows refused the up.
    fn release_button(&mut self) -> bool {
        if !std::mem::take(&mut self.button_down) {
            return true;
        }
        // SAFETY: releases only the down SendInput acknowledged for this session.
        let accepted = unsafe {
            SendInput(
                &[mouse_input(MOUSEEVENTF_LEFTUP, 0, 0)],
                i32::try_from(size::<INPUT>()).unwrap_or(0),
            )
        };
        accepted == 1
    }

    /// A tagged absolute move to `point`. A click then checks the pointer really landed there, so
    /// a mapping mismatch aborts rather than clicking elsewhere.
    pub(crate) fn move_pointer(&mut self, point: POINT, target: Target) -> Result<()> {
        let [left, top, width, height] = desktop_rect();
        let (Some(dx), Some(dy)) = (
            normalized(point.x, left, width),
            normalized(point.y, top, height),
        ) else {
            bail!("pointer destination is outside the virtual desktop");
        };
        self.send(
            &[mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                dx,
                dy,
            )],
            Some(Aim::Move(point, target)),
        )
    }

    /// One tagged absolute left click at `point`, where the pointer must already be over `target`.
    pub(crate) fn click(&mut self, point: POINT, target: Target) -> Result<()> {
        let [left, top, width, height] = desktop_rect();
        let (dx, dy) = self.or_abort(
            normalized(point.x, left, width)
                .zip(normalized(point.y, top, height))
                .context("click destination is outside the virtual desktop"),
        )?;
        self.send(&click_inputs(dx, dy), Some(Aim::Click(point, target)))
    }

    /// Whether the session moved the pointer and hasn't put it back.
    pub(crate) fn pointer_left(&self) -> bool {
        self.pointer_moved
    }

    pub(crate) fn restore_cursor(&mut self) -> Result<&'static str> {
        let Some(original) = self.original_cursor.filter(|_| self.pointer_moved) else {
            return Ok("unchanged");
        };
        self.guard()?;
        // SAFETY: this resolves ownership only; no cursor movement has occurred.
        let owner = pid(unsafe { WindowFromPoint(original) });
        let target = if owner == self.fixture.pid {
            Target::Fixture
        } else if self.product.is_some_and(|product| product.pid == owner) {
            Target::Product
        } else {
            return Ok("skipped_non_owned_destination");
        };
        if self.covered.is_some() {
            return Ok("skipped_while_covered");
        }
        self.move_pointer(original, target)?;
        self.pointer_moved = false;
        Ok("restored")
    }

    /// Toggles the fixture's reserved Esc, so free and contested phases share one fixture.
    pub(crate) fn set_escape(&mut self, mode: FixtureEscMode) -> Result<()> {
        self.guard()?;
        let reserve = usize::from(mode.is_reserved());
        request(self.fixture, ESCAPE_LEASE, WPARAM(reserve), LPARAM(0))?;
        ensure!(
            property(self.fixture.hwnd, ESC_HELD) == reserve,
            "the fixture's Esc lease didn't change"
        );
        self.guard()
    }

    /// Makes the fixture a borderless window filling `rect` with its EDIT, without activating
    /// it or changing its Z-order, so pass-through clicks land harmlessly in the EDIT.
    pub(crate) fn cover(&mut self, rect: RECT) -> Result<()> {
        self.guard()?;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        let [left, top, desktop_width, desktop_height] = desktop_rect();
        ensure!(
            (1..=MAX_COVER).contains(&width)
                && (1..=MAX_COVER).contains(&height)
                && rect.left >= left
                && rect.top >= top
                && rect.right <= left + desktop_width
                && rect.bottom <= top + desktop_height,
            "cover must be at most {MAX_COVER} px square inside the virtual desktop"
        );
        request(
            self.fixture,
            COVER,
            WPARAM(usize::try_from(pack(rect.left, rect.top))?),
            LPARAM(isize::try_from(pack(width, height))?),
        )?;
        ensure!(
            self.fixture.rect()? == rect,
            "the fixture didn't take the cover placement"
        );
        self.covered = Some(rect);
        self.guard()
    }

    /// Puts the fixture back in its framed window. Sends no input, so cleanup may call it after
    /// an abort.
    pub(crate) fn uncover(&mut self) -> Result<()> {
        request(self.fixture, UNCOVER, WPARAM(0), LPARAM(0))?;
        self.covered = None;
        Ok(())
    }

    /// Covers the presence with the fixture (borderless, its EDIT filling it, without activation or
    /// a Z-order change) and returns the hover's enter point, the dock slot's center, and its
    /// leave point, beside the dock at the same height and beyond the whole overlay, so the panel
    /// can't reopen. `dock` and `overlay` are the
    /// observer's startup physical rects. Moves no pointer; the caller uncovers afterwards.
    pub(crate) fn hover_points(&mut self, dock: RECT, overlay: RECT) -> Result<[POINT; 2]> {
        self.guard()?;
        ensure!(
            overlay.left <= dock.left
                && overlay.top <= dock.top
                && overlay.right >= dock.right
                && overlay.bottom >= dock.bottom,
            "the overlay doesn't contain the dock"
        );
        let (cover, points) = hover_cover(dock, overlay, desktop_rect())
            .context("no cover inside the virtual desktop holds the presence and a leave point")?;
        self.cover(cover)?;
        Ok(points)
    }

    /// Binds the bound product's panel window, from the observer's startup record: one of the
    /// product's own GPUI windows, not its presence proxy. Cleared whenever the product changes.
    pub(crate) fn bind_panel(&mut self, hwnd: usize) -> Result<()> {
        let product = self.product()?;
        let panel = HWND(std::ptr::with_exposed_provenance_mut(hwnd));
        ensure!(
            panel != product.hwnd && pid(panel) == product.pid && class_name(panel) == GPUI_WINDOW,
            "the panel handle isn't one of the bound product's own GPUI windows"
        );
        self.panel = Some(panel);
        Ok(())
    }

    /// Whether the bound panel window is visible now, read fresh from its exact handle.
    pub(crate) fn panel_visible(&self) -> Result<bool> {
        self.guard()?;
        let panel = self.panel.context("no panel is bound to the session")?;
        ensure!(
            pid(panel) == self.product()?.pid,
            "the bound panel disappeared or changed owner"
        );
        // SAFETY: reads the visibility of the PID-checked panel handle; no content is read.
        Ok(unsafe { IsWindowVisible(panel) }.as_bool())
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

/// How far the leave point sits beyond the overlay, and at least how far beyond the dock.
const LEAVE_BEYOND_OVERLAY: i32 = 32;
const LEAVE_FROM_DOCK: i32 = 256;
/// The EDIT margin kept around the overlay and the leave point.
const COVER_MARGIN: i32 = 32;

/// A cover, at least 640 x 400 and at most [`MAX_COVER`] square inside the virtual desktop,
/// holding the overlay and the leave point with a margin, plus the enter point (the dock's
/// center) and the leave point: left of the dock at its center height, or right at a screen
/// edge, beyond both the overlay and [`LEAVE_FROM_DOCK`].
fn hover_cover(
    dock: RECT,
    overlay: RECT,
    [left, top, width, height]: [i32; 4],
) -> Option<(RECT, [POINT; 2])> {
    let center = POINT {
        x: i32::midpoint(dock.left, dock.right),
        y: i32::midpoint(dock.top, dock.bottom),
    };
    let sides = [
        overlay.left.min(dock.left - LEAVE_FROM_DOCK) - LEAVE_BEYOND_OVERLAY,
        overlay.right.max(dock.right + LEAVE_FROM_DOCK) + LEAVE_BEYOND_OVERLAY,
    ];
    sides.into_iter().find_map(|x| {
        let leave = POINT { x, y: center.y };
        let box_left = overlay.left.min(x) - COVER_MARGIN;
        let box_right = overlay.right.max(x + 1) + COVER_MARGIN;
        let box_top = overlay.top - COVER_MARGIN;
        let box_bottom = overlay.bottom + COVER_MARGIN;
        let cover_width = (box_right - box_left).max(640);
        let cover_height = (box_bottom - box_top).max(400);
        if cover_width > MAX_COVER.min(width) || cover_height > MAX_COVER.min(height) {
            return None;
        }
        let x = (i32::midpoint(box_left, box_right) - cover_width / 2)
            .clamp(left, left + width - cover_width);
        let y = (i32::midpoint(box_top, box_bottom) - cover_height / 2)
            .clamp(top, top + height - cover_height);
        let cover = RECT {
            left: x,
            top: y,
            right: x + cover_width,
            bottom: y + cover_height,
        };
        let inside = |point: POINT| {
            point.x >= cover.left
                && point.x < cover.right
                && point.y >= cover.top
                && point.y < cover.bottom
        };
        (inside(center) && inside(leave)).then_some((cover, [center, leave]))
    })
}

fn owner(window: HWND) -> Owner {
    Owner {
        window: window.0.addr(),
        pid: pid(window),
    }
}

fn click_inputs(dx: i32, dy: i32) -> [INPUT; 2] {
    let position = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    [
        mouse_input(position | MOUSEEVENTF_LEFTDOWN, dx, dy),
        mouse_input(position | MOUSEEVENTF_LEFTUP, dx, dy),
    ]
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                dwFlags: flags,
                dwExtraInfo: MARKER,
                ..Default::default()
            },
        },
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

#[cfg(test)]
mod tests {
    use super::{INPUT_MOUSE, MARKER, click_inputs, hover_cover};
    use windows::Win32::Foundation::{POINT, RECT};

    const DESKTOP: [i32; 4] = [0, 0, 2560, 1440];

    #[test]
    fn both_click_edges_pin_the_absolute_virtual_desktop_coordinates() {
        for (dx, dy) in [(12_345, 54_321), (0, 65_535), (65_535, 0)] {
            for (input, flags) in click_inputs(dx, dy).into_iter().zip([0xc003, 0xc005]) {
                assert_eq!(input.r#type, INPUT_MOUSE);
                // SAFETY: the asserted mouse type selects the initialized MOUSEINPUT member.
                let mouse = unsafe { input.Anonymous.mi };
                assert_eq!((mouse.dx, mouse.dy), (dx, dy));
                assert_eq!(mouse.dwFlags.0, flags);
                assert_eq!(mouse.dwExtraInfo, MARKER);
            }
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn assert_cover(cover: RECT, overlay: RECT, points: [POINT; 2]) {
        let [left, top, width, height] = DESKTOP;
        assert!(cover.left >= left && cover.top >= top);
        assert!(cover.right <= left + width && cover.bottom <= top + height);
        assert!((640..=1024).contains(&(cover.right - cover.left)));
        assert!((400..=1024).contains(&(cover.bottom - cover.top)));
        assert!(cover.left <= overlay.left && cover.top <= overlay.top);
        assert!(cover.right >= overlay.right && cover.bottom >= overlay.bottom);
        for point in points {
            assert!((cover.left..cover.right).contains(&point.x));
            assert!((cover.top..cover.bottom).contains(&point.y));
        }
    }

    #[test]
    fn a_bottom_dock_leaves_left_beyond_the_overlay_and_256_px() {
        let dock = rect(1224, 1388, 1336, 1436);
        let overlay = rect(1112, 1276, 1448, 1440);
        let (cover, [enter, leave]) = hover_cover(dock, overlay, DESKTOP).unwrap();
        assert_eq!(enter, POINT { x: 1280, y: 1412 });
        assert_eq!(leave.y, enter.y);
        assert!(leave.x < overlay.left && dock.left - leave.x >= 256);
        assert_cover(cover, overlay, [enter, leave]);
    }

    #[test]
    fn a_dock_at_the_left_edge_leaves_to_the_right() {
        let dock = rect(4, 600, 116, 648);
        let overlay = rect(0, 488, 228, 760);
        let (cover, [enter, leave]) = hover_cover(dock, overlay, DESKTOP).unwrap();
        assert_eq!(enter, POINT { x: 60, y: 624 });
        assert_eq!(leave.y, enter.y);
        assert!(leave.x >= overlay.right && leave.x - dock.right >= 256);
        assert_cover(cover, overlay, [enter, leave]);
    }

    #[test]
    fn an_overlay_too_wide_for_the_largest_cover_is_refused() {
        assert_eq!(
            hover_cover(rect(494, 26, 606, 74), rect(0, 0, 1100, 100), DESKTOP),
            None
        );
        assert_eq!(
            hover_cover(rect(0, 0, 10, 10), rect(0, 0, 10, 10), [0, 0, 0, 0]),
            None
        );
    }
}
