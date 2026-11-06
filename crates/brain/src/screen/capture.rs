//! The worker's capture operation: ticket checks, the platform seam and the frame's validity.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::DisplayRef;

use super::elements::{
    ElementReader, ElementTree, MaskSink, OpenElements, TopWindow, WalkJob, Walked, intersects,
};
use super::frame::{RawFrame, display_edges};
use super::mask::MaskSet;
use super::wire::{CaptureId, ElementTarget, Exclusion, FrameMeta, Refusal, TicketWire};

/// Why a screen operation failed or requires worker retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// An expected refusal, such as expiry or a changed display.
    Refused(Refusal),
    /// A numeric platform code.
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

/// One display capture. Windows: Windows Graphics Capture. macOS: `ScreenCaptureKit`'s
/// `SCScreenshotManager` with Kumochi excluded from the content filter.
pub(crate) trait Capturer {
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

struct Held {
    capture: CaptureId,
    expires: Instant,
    walk_by: Instant,
    foreground: Option<TopWindow>,
    spent: bool,
}

/// The capture state one worker owns on its serve thread.
pub(super) struct Screen {
    book: TicketBook,
    frame: RawFrame,
    capturer: Option<Box<dyn Capturer>>,
    held: Option<Held>,
    masks: MaskSink,
    reader: Option<Result<Box<dyn ElementReader>, i32>>,
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
        }
    }
}

impl Screen {
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
        self.frame.invalidate();
        self.held = None;
        self.masks = Arc::new(Mutex::new(MaskSet::new(ticket.capture())));
        self.book.spend(ticket.generation())?;
        let expires = received + Duration::from_millis(ticket.ttl_ms().into());
        display_edges(display)?;
        if now() >= expires {
            return Err(Refusal::Expired.into());
        }
        let capturer = match &mut self.capturer {
            Some(capturer) => capturer,
            None => self.capturer.insert(open.0().map_err(Failure::Fault)?),
        };
        let foreground = self
            .reader
            .get_or_insert_with(open.1)
            .as_mut()
            .ok()
            .and_then(|reader| reader.foreground());
        let acquired = capturer.capture_into(display, exclude, expires, &mut self.frame)?;
        if acquired.arrived >= expires || now() >= expires {
            self.frame.invalidate();
            return Err(Refusal::Expired.into());
        }
        let meta = FrameMeta {
            capture: ticket.capture(),
            display: display.0,
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
        });
        Ok(meta)
    }

    pub(super) fn elements(
        &mut self,
        capture: CaptureId,
        target: ElementTarget,
        now: &dyn Fn() -> Instant,
    ) -> Result<Walked, Failure> {
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
        let reader = match self.reader.as_mut().ok_or(Failure::Fault(0))? {
            Ok(reader) => reader,
            Err(code) => {
                ElementTree::unavailable(capture, super::elements::Unavailable::Busy)
                    .finish(&self.masks);
                return Err(Failure::Retire(*code));
            }
        };
        let display = self.frame.meta().ok_or(Failure::Fault(0))?.display;
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
            let retire = !tree.finish(&self.masks);
            if retire {
                tree = ElementTree::unavailable(capture, super::elements::Unavailable::Busy);
            }
            return Ok(Walked { tree, retire });
        };
        let job = WalkJob {
            target,
            capture,
            display,
            until: (start + Duration::from_millis(120)).min(expires),
            masks: Arc::clone(&self.masks),
        };
        reader.walk(job)
    }

    #[cfg(test)]
    pub(super) fn frame(&self) -> &RawFrame {
        &self.frame
    }
}

#[cfg(test)]
#[path = "capture/tests.rs"]
mod tests;
