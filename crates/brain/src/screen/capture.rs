//! The worker's capture operation: ticket checks, the platform seam and the frame's validity.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{DisplayRef, PhysicalPoint, PhysicalRect, WindowRef};

use super::context::ScreenContext;
use super::elements::{
    ElementReader, ElementTree, MaskSink, OpenElements, TopWindow, WalkJob, Walked, intersects,
};
use super::encode::overview_within;
use super::frame::{RawFrame, display_edges};
use super::mask::{Completeness, MaskSet};
use super::sanitize::sanitize;
use super::wire::{CaptureId, ElementTarget, Exclusion, FrameMeta, Refusal, TicketWire};

/// Why a screen operation failed or requires worker retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// An expected refusal, such as expiry or a changed display.
    Refused(Refusal),
    /// A content-free numeric fault code.
    Fault(i32),
    /// The platform state can't safely be reused, so the worker must exit after answering.
    Retire(i32),
}

impl From<Refusal> for Failure {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// What the platform reports about a frame it copied into the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Acquired {
    /// When the frame arrived, before the copy.
    pub(crate) arrived: Instant,
    /// The display's effective DPI.
    pub(crate) dpi: u32,
}

/// The window a hold points at, as the walk resolved it. Its capture and its element walk use
/// this same window.
pub(crate) struct Target {
    /// The root window, its process and its frame: the captured surface's physical bounds.
    pub(crate) top: TopWindow,
    /// The platform's second identity word, so a reused handle reads as another window: the
    /// creating thread on Windows.
    pub(crate) instance: u32,
    /// Its DPI when resolved.
    pub(crate) dpi: u32,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("dpi", &self.dpi)
            .finish_non_exhaustive()
    }
}

/// One display or window capture. Windows: Windows Graphics Capture. macOS: `ScreenCaptureKit`'s
/// `SCScreenshotManager`, with Kumochi excluded from a display's content filter and
/// `desktopIndependentWindow` for a window.
pub(crate) trait Capturer {
    /// The window a click at `point` would land on, Kumochi's own aside, after the checks that
    /// refuse it before any pixels. `shell` is the connected shell's process.
    ///
    /// # Errors
    /// `NoWindow` when only Kumochi or the desktop is there, `Ambiguous` when a drawn layered
    /// window is there and where a click lands can't be told, or a preflight refusal.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the hold's capture resolves the pointer's window")
    )]
    fn pointer_window(&mut self, _point: PhysicalPoint, _shell: u32) -> Result<Target, Refusal> {
        Err(Refusal::NoWindow)
    }

    /// Fills `frame` through [`RawFrame::begin`] with one frame of `target` alone, which
    /// leaves out whatever is stacked above it. It starts nothing at or after `until`.
    ///
    /// # Errors
    /// `WindowChanged` when the window's identity, frame or DPI moved or it closed, another
    /// refusal, a numeric fault, or `Retire` when it can't show its capture stopped.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the hold's capture resolves the pointer's window")
    )]
    fn capture_window(
        &mut self,
        _target: &Target,
        _until: Instant,
        _frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        Err(Failure::Fault(NO_WINDOW_CAPTURE))
    }
    /// Fills `frame` through [`RawFrame::begin`] with one frame of `display`. It starts nothing
    /// at or after `until`, and stops waiting there.
    ///
    /// # Errors
    /// A refusal, a numeric fault, or `Retire` when it can't show its capture stopped.
    fn capture_into(
        &mut self,
        display: &DisplayRef,
        exclude: Exclusion,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure>;
}

/// Opens the platform capturer on the first capture.
pub(crate) type Open = fn() -> Result<Box<dyn Capturer>, i32>;

/// Generations spent by this worker. A respawn starts empty: the brain's single-use tickets
/// (B2a) guard across workers.
#[derive(Debug, Default)]
struct TicketBook {
    high_water: Option<u64>,
}

impl TicketBook {
    fn spend(&mut self, generation: u64) -> Result<(), Refusal> {
        match self.high_water {
            Some(spent) if generation == spent => Err(Refusal::Reused),
            Some(spent) if generation < spent => Err(Refusal::Stale),
            _ => {
                self.high_water = Some(generation);
                Ok(())
            }
        }
    }
}

/// The interval in which screen text still describes a newly arrived frame (guess).
const WALK_WINDOW: Duration = Duration::from_millis(250);

/// `E_NOTIMPL`, for a platform with no window capture yet.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the dormant window seam")
)]
const NO_WINDOW_CAPTURE: i32 = 0x8000_4001_u32.cast_signed();
const MISSING_ELEMENT_READER: i32 = -1;
const MISSING_FRAME_META: i32 = -2;

struct Held {
    capture: CaptureId,
    expires: Instant,
    walk_by: Instant,
    foreground: Option<TopWindow>,
    spent: bool,
    /// The window its one walk read, which a seal requires to be the captured one.
    walked: Option<(WindowRef, u32)>,
    /// The captured window as its capture resolved it, which a seal checks again.
    target: Option<Target>,
}

/// The capture state one worker owns on its serve thread.
pub(super) struct Screen {
    book: TicketBook,
    pub(super) frame: RawFrame,
    capturer: Option<Box<dyn Capturer>>,
    held: Option<Held>,
    masks: MaskSink,
    pub(super) reader: Option<Result<Box<dyn ElementReader>, i32>>,
    /// Whether a window is still the one its capture resolved: the platform's check, a fake in
    /// tests.
    pub(super) unchanged: fn(&Target) -> bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            book: TicketBook::default(),
            frame: RawFrame::default(),
            capturer: None,
            held: None,
            masks: Arc::new(Mutex::new(MaskSet::new(CaptureId(0)))),
            reader: None,
            unchanged: crate::platform::window_unchanged,
        }
    }
}

impl Screen {
    pub(super) fn admit(
        &mut self,
        ticket: &TicketWire,
        received: Instant,
        now: &dyn Fn() -> Instant,
    ) -> Result<Instant, Failure> {
        self.frame.invalidate();
        self.held = None;
        self.masks = Arc::new(Mutex::new(MaskSet::new(ticket.capture())));
        self.book.spend(ticket.generation())?;
        let expires = received + Duration::from_millis(ticket.ttl_ms().into());
        if now() >= expires {
            return Err(Refusal::Expired.into());
        }
        Ok(expires)
    }

    /// Commits the frame of `surface`: a display, or the window the walk resolved.
    pub(super) fn acquired(
        &mut self,
        ticket: &TicketWire,
        surface: PhysicalRect,
        received: Instant,
        expires: Instant,
        now: Instant,
        result: (Acquired, Option<TopWindow>),
    ) -> Result<FrameMeta, Failure> {
        let (acquired, foreground) = result;
        if acquired.arrived >= expires || now >= expires {
            self.frame.invalidate();
            return Err(Refusal::Expired.into());
        }
        let meta = FrameMeta {
            capture: ticket.capture(),
            display: surface,
            dpi: acquired.dpi,
            acquired_after_ms: u32::try_from(
                acquired
                    .arrived
                    .saturating_duration_since(received)
                    .as_millis(),
            )
            .unwrap_or(u32::MAX),
        };
        self.frame.commit(meta)?;
        self.held = Some(Held {
            capture: meta.capture,
            expires,
            walk_by: acquired.arrived + WALK_WINDOW,
            foreground,
            spent: false,
            walked: None,
            target: None,
        });
        Ok(meta)
    }

    /// Keeps the captured window's identity for the seal's check.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the dormant alternate loop captures windows")
    )]
    pub(super) fn pin(&mut self, target: Target) {
        if let Some(held) = &mut self.held {
            held.target = Some(target);
        }
    }

    /// How far the held capture's masks are known to cover its window.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the dormant alternate loop reports walks")
    )]
    pub(super) fn completeness(&self) -> Completeness {
        self.masks.lock().map_or(
            Completeness::Unknown(super::mask::Unavailable::Busy),
            |masks| masks.completeness(),
        )
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the dormant alternate loop disposes frames")
    )]
    pub(super) fn clear(&mut self) {
        self.held = None;
        self.frame.release();
        self.masks = Arc::new(Mutex::new(MaskSet::new(CaptureId(0))));
    }

    /// Hands out the held frame's one image, masked and encoded under `cap` JPEG bytes, and
    /// releases the frame whatever the outcome. Only a walk of the captured window that found
    /// every password mask seals, and only while that window is still the one captured.
    pub(super) fn seal(
        &mut self,
        capture: CaptureId,
        cap: usize,
    ) -> Result<ScreenContext, Failure> {
        let sealed = self.sealed(capture, cap);
        self.clear();
        sealed
    }

    fn sealed(&mut self, capture: CaptureId, cap: usize) -> Result<ScreenContext, Failure> {
        let held = self
            .held
            .as_ref()
            .filter(|held| held.capture == capture)
            .ok_or(Refusal::Stale)?;
        let captured = held.foreground.as_ref().map(|top| (top.window, top.pid));
        if captured.is_none() || held.walked != captured {
            return Err(Refusal::Unmasked.into());
        }
        let sink = Arc::clone(&self.masks);
        let masks = sink.lock().map_err(|_| Refusal::Unmasked)?;
        if masks.completeness() != Completeness::Complete {
            return Err(Refusal::Unmasked.into());
        }
        if !held.target.as_ref().is_some_and(self.unchanged) {
            return Err(Refusal::WindowChanged.into());
        }
        let clean = sanitize(&mut self.frame, &masks)?;
        overview_within(&clean, cap).map(ScreenContext::new)
    }

    /// Takes one frame under `ticket`, received at `received`. Its validity is local: it runs
    /// from receipt, so the brain still checks its own deadline on the reply.
    pub(super) fn capture(
        &mut self,
        ticket: &TicketWire,
        display: &DisplayRef,
        exclude: Exclusion,
        received: Instant,
        now: &dyn Fn() -> Instant,
        open: (Open, OpenElements),
    ) -> Result<FrameMeta, Failure> {
        let expires = self.admit(ticket, received, now)?;
        display_edges(display)?;
        let capturer = match &mut self.capturer {
            Some(capturer) => capturer,
            None => self.capturer.insert(open.0().map_err(Failure::Fault)?),
        };
        if self.reader.as_ref().is_some_and(Result::is_err) {
            self.reader = None;
        }
        let foreground = self
            .reader
            // shortcut: no listener until A6b2's actor; the legacy walk waits for its own reply.
            .get_or_insert_with(|| open.1(Box::new(|| true)))
            .as_mut()
            .ok()
            .and_then(|reader| {
                reader.set_shell_pid(ticket.shell_pid());
                reader.foreground()
            });
        let acquired = capturer.capture_into(display, exclude, expires, &mut self.frame)?;
        self.acquired(
            ticket,
            display.0,
            received,
            expires,
            now(),
            (acquired, foreground),
        )
    }

    pub(super) fn walk_job(
        &mut self,
        capture: CaptureId,
        target: ElementTarget,
        now: &dyn Fn() -> Instant,
    ) -> Result<Result<WalkJob, Walked>, Failure> {
        let Some(held) = &mut self.held else {
            return Err(Refusal::Stale.into());
        };
        if held.capture != capture {
            return Err(Refusal::Stale.into());
        }
        if std::mem::replace(&mut held.spent, true) {
            return Err(Refusal::Reused.into());
        }
        if let ElementTarget::Window { window, pid } = target
            && (window.0 == 0 || pid == 0)
        {
            return Err(Refusal::Invalid.into());
        }
        let start = now();
        let expires = held.expires.min(held.walk_by);
        if start >= expires {
            return Err(Refusal::Expired.into());
        }
        match self
            .reader
            .as_mut()
            .ok_or(Failure::Fault(MISSING_ELEMENT_READER))?
        {
            Ok(_) => {}
            Err(code) => {
                if !super::elements::finish_unavailable(
                    &self.masks,
                    super::elements::Unavailable::Busy,
                ) {
                    return Err(Failure::Retire(*code));
                }
                return Err(Failure::Fault(*code));
            }
        }
        let display = self
            .frame
            .meta()
            .ok_or(Failure::Fault(MISSING_FRAME_META))?
            .display;
        let target = match target {
            ElementTarget::Foreground => held
                .foreground
                .as_ref()
                .filter(|top| intersects(top.rect, display))
                .map(|top| ElementTarget::Window {
                    window: top.window,
                    pid: top.pid,
                }),
            window @ ElementTarget::Window { .. } => Some(window),
        };
        if let Some(ElementTarget::Window { window, pid }) = target {
            held.walked = Some((window, pid));
        }
        let Some(target) = target else {
            let mut tree =
                ElementTree::unavailable(capture, super::elements::Unavailable::NoWindow);
            let retire = !super::elements::finish_unavailable(
                &self.masks,
                super::elements::Unavailable::NoWindow,
            );
            if retire {
                tree = ElementTree::unavailable(capture, super::elements::Unavailable::Busy);
            }
            return Ok(Err(Walked { tree, retire }));
        };
        let job = WalkJob {
            target,
            capture,
            display,
            until: (start + Duration::from_millis(120)).min(expires),
            masks: Arc::clone(&self.masks),
        };
        Ok(Ok(job))
    }

    pub(super) fn elements(
        &mut self,
        capture: CaptureId,
        target: ElementTarget,
        now: &dyn Fn() -> Instant,
    ) -> Result<Walked, Failure> {
        match self.walk_job(capture, target, now)? {
            Ok(job) => self
                .reader
                .as_mut()
                .expect("validated")
                .as_mut()
                .map_err(|code| Failure::Fault(*code))?
                .walk(job),
            Err(walked) => Ok(walked),
        }
    }

    #[cfg(test)]
    pub(super) fn frame(&self) -> &RawFrame {
        &self.frame
    }
}

#[cfg(test)]
#[path = "capture/tests.rs"]
mod tests;
