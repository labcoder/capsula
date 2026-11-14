use kumochi_protocol::geometry::PhysicalRect;
use serde::{Deserialize, Serialize};

pub(super) use super::elements::ElementTarget;

/// The largest reply, serialized.
pub(super) const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// A dev-only, one-request failure fixture.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerFault {
    /// Stop answering.
    Hang,
    /// Exit abnormally.
    Crash,
    /// Write an invalid frame.
    Garbage,
    /// Panic after installing the location-only hook.
    Panic,
    /// End before serving anything, as a worker that can't start does.
    Exit,
}

/// A capture's brain-assigned number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureId(pub u64);

/// One capture's authority. Only the brain's hold client mints it, for a recognized hold; this
/// module only carries and checks it.
#[derive(Clone, Serialize, Deserialize)]
pub struct TicketWire {
    capture: CaptureId,
    generation: u64,
    nonce: [u8; 16],
    ttl_ms: u32,
    shell_pid: u32,
}

impl TicketWire {
    pub(crate) fn new(capture: CaptureId, generation: u64, nonce: [u8; 16], ttl_ms: u32) -> Self {
        Self {
            capture,
            generation,
            nonce,
            ttl_ms,
            shell_pid: 0,
        }
    }

    pub(super) fn capture(&self) -> CaptureId {
        self.capture
    }

    pub(super) fn for_shell(mut self, pid: u32) -> Self {
        self.shell_pid = pid;
        self
    }
    pub(super) fn shell_pid(&self) -> u32 {
        self.shell_pid
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn ttl_ms(&self) -> u32 {
        self.ttl_ms
    }
}

impl std::fmt::Debug for TicketWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketWire")
            .field("capture", &self.capture)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Why the worker refused a capture or element walk without a platform fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// The ticket's local validity ran out before a complete frame.
    Expired,
    /// This ticket's generation was already spent in this worker.
    Reused,
    /// A newer generation was already spent in this worker.
    Stale,
    /// The display no longer has the requested physical bounds.
    DisplayChanged,
    /// The requested bounds are empty, overflow or exceed the size cap.
    Invalid,
    /// Only Kumochi or the desktop is under the pointer, so there's no window to capture.
    NoWindow,
    /// The window is minimised.
    Minimized,
    /// The window is cloaked, such as on another virtual desktop.
    Cloaked,
    /// The window's process is elevated, or can't be opened to tell.
    Elevated,
    /// The window asks not to be captured (display affinity).
    Protected,
    /// The window is Kumochi's own.
    Excluded,
    /// The window's identity, frame or DPI changed, or it closed, before its frame was taken.
    WindowChanged,
    /// A drawn layered window is under the pointer and where a click lands can't be told.
    Ambiguous,
    /// The window's password masks aren't known to be complete, so its image isn't exported.
    Unmasked,
    /// The sealed image is over the JPEG cap, so it isn't exported.
    TooLarge,
}

/// A complete, valid frame's description; the pixels stay in the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// The ticket's capture.
    pub capture: CaptureId,
    /// The captured surface's physical bounds: the display, or a window's frame.
    pub display: PhysicalRect,
    /// The display's effective DPI.
    pub dpi: u32,
    /// From the request's receipt to the frame's arrival, in milliseconds.
    pub acquired_after_ms: u32,
}

#[cfg(test)]
mod tests;
