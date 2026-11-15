//! The worker's capture operation: ticket checks, the platform seam and the frame's validity.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

use super::context::ScreenContext;
use super::elements::{
    ElementReader, ElementTree, MaskSink, TopWindow, WalkJob, Walked, intersects,
};
use super::encode::overview_inked;
use super::frame::RawFrame;
use super::mask::MaskSet;
use super::sanitize::sanitize;
use super::wire::{CaptureId, ElementTarget, FrameMeta, Refusal, TicketWire};

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

/// The window a hold points at, as the z-order walk from the pointer resolved it. Its capture
/// uses this same window.
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
    fn pointer_window(&mut self, _point: PhysicalPoint, _shell: u32) -> Result<Target, Refusal> {
        Err(Refusal::NoWindow)
    }

    /// Fills `frame` through [`RawFrame::begin`] with one frame of `target` alone, which
    /// leaves out whatever is stacked above it. It starts nothing at or after `until`.
    ///
    /// # Errors
    /// `WindowChanged` when the window's identity, frame or DPI moved or it closed, another
    /// refusal, a numeric fault, or `Retire` when it can't show its capture stopped.
    fn capture_window(
        &mut self,
        _target: &Target,
        _until: Instant,
        _frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        Err(Failure::Fault(NO_WINDOW_CAPTURE))
    }

    /// Fills `frame` through [`RawFrame::begin`] with one frame of the monitor nearest `point`,
    /// pointer included, drawn at least one compositor redraw after `excluded`: the
    /// performance-counter stamp taken once Kumochi's windows were kept out of capture. It
    /// starts nothing at or after `until`, and returns the monitor's physical bounds.
    ///
    /// # Errors
    /// `Expired` for a zero stamp or when no frame drawn late enough arrived by `until`,
    /// `DisplayChanged` when the monitor's geometry moved, a numeric fault, or `Retire` when it
    /// can't show its capture stopped.
    fn capture_display(
        &mut self,
        _point: PhysicalPoint,
        _excluded: u64,
        _until: Instant,
        _frame: &mut RawFrame,
    ) -> Result<(Acquired, PhysicalRect), Failure> {
        Err(Failure::Fault(NO_WINDOW_CAPTURE))
    }
}

/// Generations spent by this worker. A respawn starts empty: the hold client's rising ticket
/// numbers guard across workers.
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
const NO_WINDOW_CAPTURE: i32 = 0x8000_4001_u32.cast_signed();
const MISSING_ELEMENT_READER: i32 = -1;
const MISSING_FRAME_META: i32 = -2;

/// What a held frame shows, which its seal checks again.
#[derive(Debug)]
pub(super) enum Shown {
    /// The window as its capture resolved it.
    Window(Target),
    /// The monitor's physical bounds and effective DPI when it was captured.
    Display { bounds: PhysicalRect, dpi: u32 },
}

struct Held {
    capture: CaptureId,
    expires: Instant,
    walk_by: Instant,
    foreground: Option<TopWindow>,
    spent: bool,
    shown: Option<Shown>,
    /// The pointer's path to paint on the image, in physical pixels.
    ink: Vec<PhysicalPoint>,
}

/// The capture state one worker owns on its serve thread.
pub(super) struct Screen {
    book: TicketBook,
    pub(super) frame: RawFrame,
    held: Option<Held>,
    masks: MaskSink,
    pub(super) reader: Option<Result<Box<dyn ElementReader>, i32>>,
    /// Whether a window is still the one its capture resolved: the platform's check, a fake in
    /// tests.
    pub(super) unchanged: fn(&Target) -> bool,
    /// The bounds and DPI of the monitor at a display's bounds now: the platform's read, a fake
    /// in tests.
    pub(super) display_at: fn(PhysicalRect) -> Option<(PhysicalRect, u32)>,
    /// How long until a ticket's deadline stamp: the platform's counter, a fake in tests.
    pub(super) left_until: fn(u64) -> Option<Duration>,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            book: TicketBook::default(),
            frame: RawFrame::default(),
            held: None,
            masks: Arc::new(Mutex::new(MaskSet::new(CaptureId(0)))),
            reader: None,
            unchanged: crate::platform::window_unchanged,
            display_at: crate::platform::display_at,
            left_until: crate::platform::qpc_left,
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
        // Now is read before the counter, so the brain's deadline can only come sooner here.
        let at = now();
        let left = (self.left_until)(ticket.deadline_qpc()).ok_or(Refusal::Expired)?;
        let expires = at
            .checked_add(left)
            .map_or(expires, |deadline| deadline.min(expires));
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
            shown: None,
            ink: Vec::new(),
        });
        Ok(meta)
    }

    /// Keeps what the frame shows for the seal's check, and the path the seal paints.
    pub(super) fn pin(&mut self, shown: Shown, ink: Vec<PhysicalPoint>) {
        if let Some(held) = &mut self.held {
            held.shown = Some(shown);
            held.ink = ink;
        }
    }

    pub(super) fn clear(&mut self) {
        self.held = None;
        self.frame.release();
        self.masks = Arc::new(Mutex::new(MaskSet::new(CaptureId(0))));
    }

    /// Hands out the held frame's one image as captured, with no mask and its path painted,
    /// encoded under `cap` JPEG bytes, and releases the frame whatever the outcome. It seals only
    /// while the captured window is still the one captured, or the captured display still has the
    /// same bounds and DPI.
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
        let (unchanged, display_at) = (self.unchanged, self.display_at);
        let held = self
            .held
            .as_mut()
            .filter(|held| held.capture == capture)
            .ok_or(Refusal::Stale)?;
        match &held.shown {
            Some(Shown::Window(target)) if unchanged(target) => {}
            Some(Shown::Display { bounds, dpi })
                if display_at(*bounds) == Some((*bounds, *dpi)) => {}
            Some(Shown::Display { .. }) => return Err(Refusal::DisplayChanged.into()),
            _ => return Err(Refusal::WindowChanged.into()),
        }
        let ink = std::mem::take(&mut held.ink);
        let clean = sanitize(&mut self.frame, &MaskSet::new(capture))?;
        let (encoded, inked) = overview_inked(&clean, cap, &ink)?;
        Ok(ScreenContext::new(encoded).with_ink(inked))
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

    #[cfg(test)]
    pub(super) fn frame(&self) -> &RawFrame {
        &self.frame
    }
}

#[cfg(test)]
#[path = "capture/tests.rs"]
mod tests;
