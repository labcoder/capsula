//! The worker's capture operation: ticket checks, the platform seam and the frame's validity.

use std::time::{Duration, Instant};

use kumochi_protocol::geometry::DisplayRef;

use super::frame::{RawFrame, display_edges};
use super::wire::{Exclusion, FrameMeta, Refusal, TicketWire};

/// Why the platform took no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// An expected refusal, such as expiry or a changed display.
    Refused(Refusal),
    /// A numeric platform code.
    Fault(i32),
    /// The capture may still be running or the device is lost, so the worker must exit after
    /// answering.
    #[cfg_attr(
        not(any(test, windows)),
        expect(dead_code, reason = "only the Windows adapter retires")
    )]
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

/// The capture state one worker owns on its serve thread.
#[derive(Default)]
pub(super) struct Screen {
    book: TicketBook,
    frame: RawFrame,
    capturer: Option<Box<dyn Capturer>>,
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
        open: Open,
    ) -> Result<FrameMeta, Failure> {
        self.frame.invalidate();
        self.book.spend(ticket.generation())?;
        let expires = received + Duration::from_millis(ticket.ttl_ms().into());
        display_edges(display)?;
        if now() >= expires {
            return Err(Refusal::Expired.into());
        }
        let capturer = match &mut self.capturer {
            Some(capturer) => capturer,
            None => self.capturer.insert(open().map_err(Failure::Fault)?),
        };
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
        Ok(meta)
    }

    #[cfg(test)]
    pub(super) fn frame(&self) -> &RawFrame {
        &self.frame
    }
}

#[cfg(test)]
#[path = "capture/tests.rs"]
mod tests;
