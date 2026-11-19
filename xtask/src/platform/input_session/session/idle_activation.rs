//! The idle opener's native bootstrap: one guarded, tagged fixture EDIT click, decided step by
//! step by the pure `Activation`.

use std::io::{Write as _, stdout};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GUITHREADINFO, GWL_EXSTYLE, GetAncestor, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowLongPtrW, IsWindowVisible, WS_EX_TOPMOST, WindowFromPoint,
};

use super::super::activation::{Idle, Look, missed_hit};
use super::super::fixture::{
    ABORT, ARMED_INPUT, FINALIZE_IDLE, FRAME, FRAME_ORIGIN, PREPARE_IDLE, READY, frame_name,
};
use super::super::gate::Gate;
use super::super::{
    Receipt, class_name, desktop_ours, desktop_rect, foreground_process_name, last_input,
    mouse_and_keyboard_only, normalized, property, request, size, unpack,
};
use super::{FRESH, Session, click_inputs, mouse_input, owner};

/// How long the idle activation waits for its own move or click to land.
const SETTLE: Duration = Duration::from_millis(250);

impl Session {
    /// The single-use idle activation; the fixture's armed input stamp once it publishes Ready.
    pub(super) fn activate(&mut self, end: Instant, away: Duration) -> Result<u32> {
        ensure!(
            self.gate() == Gate::Waiting,
            "the session was already armed"
        );
        let away_ms = WPARAM(usize::try_from(away.as_millis())?);
        let idle = loop {
            ensure!(Instant::now() < end, "no idle window before the deadline");
            self.fixture.validate()?;
            ensure!(
                property(self.fixture.hwnd, ABORT) == 0,
                "fixture guard aborted before the idle arm"
            );
            ensure!(
                property(self.fixture.hwnd, READY) == 0,
                "Ready was pressed; this task opens only on idle"
            );
            let input = last_input()?;
            // SAFETY: reads the system tick count only.
            let now = unsafe { GetTickCount() };
            if super::super::gate::away(input, now, away) {
                break Idle {
                    last_input: input,
                    now,
                    away,
                    desktop: desktop_ours(),
                    in_time: Instant::now() < end,
                };
            }
            sleep(Duration::from_secs(1));
        };
        let point = self.edit_center()?;
        let mut cursor = POINT::default();
        // SAFETY: cursor is a writable output; nothing moves.
        unsafe { GetCursorPos(&raw mut cursor) }?;
        self.original_cursor = Some(cursor);
        self.activation.prepare(idle, point)?;
        let clicked = request(self.fixture, PREPARE_IDLE, away_ms, LPARAM(0))
            .and_then(|_| {
                let mut out = stdout().lock();
                writeln!(out, "activation_native_guard=armed")?;
                out.flush()?;
                Ok(())
            })
            .and_then(|()| self.click_edit(end, point));
        if clicked.is_err() {
            self.activation.abort();
        }
        let finalized = request(
            self.fixture,
            FINALIZE_IDLE,
            WPARAM(usize::from(clicked.is_ok())),
            LPARAM(0),
        );
        if let Err(error) = clicked {
            let context = format!("activation cleanup finalized={}", finalized.is_ok());
            return Err(error.context(context));
        }
        finalized?;
        ensure!(
            property(self.fixture.hwnd, READY) != 0 && property(self.fixture.hwnd, ABORT) == 0,
            "the fixture didn't publish its activation Ready"
        );
        Ok(u32::try_from(property(self.fixture.hwnd, ARMED_INPUT))?)
    }

    /// One tagged move and down/up batch at `point` on the topmost fixture's EDIT, each only
    /// after a fresh look, then its own receipt and the fixture root foreground.
    fn click_edit(&mut self, end: Instant, point: POINT) -> Result<()> {
        let [left, top, width, height] = desktop_rect();
        let (Some(dx), Some(dy)) = (
            normalized(point.x, left, width),
            normalized(point.y, top, height),
        ) else {
            bail!("the EDIT point is outside the virtual desktop");
        };
        let input_size = i32::try_from(size::<INPUT>())?;
        let to_point = [mouse_input(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            dx,
            dy,
        )];
        let click = click_inputs(dx, dy);
        mouse_and_keyboard_only()?;
        let snapshot = Instant::now();
        let look = self.look(end, point)?;
        let missed = self.activation.misses_edit(look);
        if let Err(error) = self.activation.may_move(look) {
            return Err(if missed {
                error.context(self.missed_edit(look))
            } else {
                error
            });
        }
        ensure!(
            snapshot.elapsed() <= FRESH,
            "activation move target went stale"
        );
        // SAFETY: one tagged move onto the fixture EDIT the fresh look just found at the point.
        let moved = unsafe { SendInput(&to_point, input_size) };
        self.pointer_moved |= moved == 1;
        ensure!(moved == 1, "SendInput refused the activation move");
        self.settle(end, point, |look, _| look.cursor == point)?;
        mouse_and_keyboard_only()?;
        let snapshot = Instant::now();
        let look = self.look(end, point)?;
        self.activation.may_click(look)?;
        ensure!(
            snapshot.elapsed() <= FRESH,
            "activation click target went stale"
        );
        // SAFETY: one tagged down/up batch with the pointer just found on the fixture EDIT.
        let accepted = usize::try_from(unsafe { SendInput(&click, input_size) })?;
        if let Err(error) = self.activation.clicked(accepted) {
            let release = if accepted == 1 {
                self.release_owned_down(end, point, input_size)
            } else {
                "nothing_acknowledged_down"
            };
            let context = format!("accepted={accepted} release={release}");
            return Err(error.context(context));
        }
        self.settle(end, point, |look, receipt| {
            receipt.count != 0 && look.foreign.is_none() && look.capture.is_none()
        })?;
        // Activation hands the front over asynchronously, so the root may arrive a little later.
        let waited = Instant::now();
        let until = (waited + SETTLE).min(end);
        let (look, receipt, foreground) = loop {
            let snapshot = Instant::now();
            let mut look = self.look(end, point)?;
            let receipt = self.fixture.receipt()?;
            // SAFETY: reads the foreground HWND only.
            let foreground = owner(unsafe { GetForegroundWindow() });
            look.age = snapshot.elapsed();
            if Instant::now() >= until || !self.activation.awaits_root(look, foreground) {
                break (look, receipt, foreground);
            }
            sleep(Duration::from_millis(1));
        };
        self.root_wait = waited.elapsed();
        self.activation
            .confirm(look, receipt, foreground)
            .with_context(|| format!("root_wait_ms={}", self.root_wait.as_millis()))
    }

    /// Who owns `point`, where the pointer is, and whether anything foreign is active.
    fn look(&self, end: Instant, point: POINT) -> Result<Look> {
        let taken = Instant::now();
        self.fixture.validate()?;
        let mut info = GUITHREADINFO {
            cbSize: size::<GUITHREADINFO>(),
            ..Default::default()
        };
        // SAFETY: thread zero queries the foreground thread and info has its documented writable size.
        unsafe { GetGUIThreadInfo(0, &raw mut info) }?;
        let held = (1..256).any(|key| {
            // SAFETY: reads a down state only; no key identity is kept.
            (unsafe { GetAsyncKeyState(key) }) < 0
        });
        let mut cursor = POINT::default();
        // SAFETY: cursor is a writable output; nothing moves.
        unsafe { GetCursorPos(&raw mut cursor) }?;
        // SAFETY: resolves ownership at the point without sending input.
        let at_point = owner(unsafe { WindowFromPoint(point) });
        Ok(Look {
            foreign: if held {
                Some("held_input")
            } else if property(self.fixture.hwnd, ABORT) != 0 {
                Some("guard_aborted")
            } else if !info.hwndMenuOwner.is_invalid() {
                Some("foreground_menu")
            } else if !info.hwndMoveSize.is_invalid() {
                Some("foreground_move_size")
            } else {
                None
            },
            desktop: desktop_ours(),
            in_time: Instant::now() < end,
            at_point,
            cursor,
            capture: (!info.hwndCapture.is_invalid()).then(|| owner(info.hwndCapture)),
            age: taken.elapsed(),
        })
    }

    /// Fixed facts about the window `look` hit instead of the EDIT, read at once without another
    /// hit test: no title, text, handle or coordinate.
    fn missed_edit(&self, look: Look) -> String {
        let hit = HWND(std::ptr::with_exposed_provenance_mut(look.at_point.window));
        let root = self.fixture.hwnd;
        // SAFETY: reads the hit's top-level ancestor; a stale or null handle yields null.
        let hit_root = unsafe { GetAncestor(hit, GA_ROOT) };
        // SAFETY: reads only the fixture root's visibility.
        let visible = unsafe { IsWindowVisible(root) }.as_bool();
        // SAFETY: reads only the fixture root's extended style.
        let style = unsafe { GetWindowLongPtrW(root, GWL_EXSTYLE) };
        let topmost = isize::try_from(WS_EX_TOPMOST.0).is_ok_and(|bit| style & bit != 0);
        let [x, y] = unpack(u64::try_from(property(root, FRAME_ORIGIN)).unwrap_or_default());
        let [_, _, width, height] = FRAME;
        let asked = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };
        format!(
            "hit_class={:?} hit_process={:?} hit={} root_visible={visible} root_topmost={topmost} \
             root_at_frame={} frame={}",
            (!hit.is_invalid()).then(|| class_name(hit)),
            foreground_process_name(look.at_point.pid).ok(),
            missed_hit(look.at_point, hit_root.0.addr(), owner(root)),
            self.fixture.rect().is_ok_and(|rect| rect == asked),
            frame_name([x, y]),
        )
    }

    /// Polls every millisecond until `landed`, for at most [`SETTLE`] and never past `end`,
    /// ending at the first native stop or desktop change.
    fn settle(
        &self,
        end: Instant,
        point: POINT,
        landed: impl Fn(Look, Receipt) -> bool,
    ) -> Result<()> {
        let until = (Instant::now() + SETTLE).min(end);
        loop {
            ensure!(
                property(self.fixture.hwnd, ABORT) == 0,
                "input arrived during the fixture activation; no retry"
            );
            let look = self.look(end, point)?;
            ensure!(
                look.desktop,
                "the input desktop changed during the fixture activation"
            );
            if landed(look, self.fixture.receipt()?) {
                return Ok(());
            }
            ensure!(
                Instant::now() < until,
                "the fixture activation's own input didn't land in time"
            );
            sleep(Duration::from_millis(1));
        }
    }

    /// Releases a half-acknowledged activation down only toward the still-owned EDIT.
    fn release_owned_down(&self, end: Instant, point: POINT, input_size: i32) -> &'static str {
        let snapshot = Instant::now();
        match self.look(end, point) {
            Ok(look) if self.activation.may_release(look) && snapshot.elapsed() <= FRESH => {
                let up = [mouse_input(MOUSEEVENTF_LEFTUP, 0, 0)];
                // SAFETY: releases only the acknowledged down, toward the fixture's own EDIT.
                let accepted = unsafe { SendInput(&up, input_size) };
                if accepted == 1 {
                    "released"
                } else {
                    "incomplete_refused"
                }
            }
            _ => "incomplete_no_owned_target",
        }
    }
}
