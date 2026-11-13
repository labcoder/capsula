use kumochi_protocol::geometry::{DisplayRef, PhysicalRect};
use kumochi_protocol::message::RunId;
use serde::{Deserialize, Serialize};

use crate::supervise::WorkerLost;

pub(super) use super::elements::ElementTarget;
use super::elements::ElementTree;

/// The largest reply, serialized.
pub(super) const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// Why screen work did not finish. No foreign error text crosses this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScreenError {
    /// The worker died, broke its protocol or missed a deadline.
    #[error("screen worker lost")]
    WorkerLost,
    /// A lost worker wasn't seen to exit, so nothing replaces it and screen work stays refused.
    #[error("screen worker exit unconfirmed")]
    ExitUnconfirmed,
    /// The worker refused an operation with a numeric platform code.
    #[error("screen operation failed ({0})")]
    Fault(i32),
}

impl From<WorkerLost> for ScreenError {
    fn from(lost: WorkerLost) -> Self {
        match lost {
            WorkerLost::Lost => Self::WorkerLost,
            WorkerLost::ExitUnconfirmed => Self::ExitUnconfirmed,
        }
    }
}

/// A request to the isolated worker.
#[derive(Serialize, Deserialize)]
pub enum RequestBody {
    /// Check that the worker can serve work.
    Ping,
    /// Take one frame of `display` into the worker's buffer; only metadata comes back.
    Capture {
        /// The authority for this one capture.
        ticket: TicketWire,
        /// The physical display the hold was recognized on.
        display: DisplayRef,
        /// What must not appear in the frame.
        exclude: Exclusion,
    },
    /// Read control-view elements under the held capture.
    Elements {
        /// The committed frame whose authority is spent.
        capture: CaptureId,
        /// The top-level window to read.
        target: ElementTarget,
    },
    /// Trigger one failure without modifying the parent's environment.
    #[cfg(debug_assertions)]
    Fault(WorkerFault),
}

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

/// One capture's authority. Only B2a's ticket book mints it, from a recognized hold or an
/// Include send under a live exclusion lease; this module only carries and checks it.
#[derive(Clone, Serialize, Deserialize)]
pub struct TicketWire {
    run: RunId,
    capture: CaptureId,
    generation: u64,
    nonce: [u8; 16],
    ttl_ms: u32,
    shell_pid: u32,
}

impl TicketWire {
    // shortcut: B2a's `Tickets::mint` is the production caller; until then only tests mint.
    #[cfg_attr(not(test), expect(dead_code, reason = "B2a mints tickets"))]
    pub(crate) fn new(
        run: RunId,
        capture: CaptureId,
        generation: u64,
        nonce: [u8; 16],
        ttl_ms: u32,
    ) -> Self {
        Self {
            run,
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
            .field("run", &self.run)
            .field("capture", &self.capture)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// What the frame must leave out. Windows: the shell's lease (B3a) hides Kumochi's windows, so
/// this is a declaration, not proof of a live lease. macOS: the worker excludes Kumochi in the
/// `SCContentFilter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exclusion {
    /// Every Kumochi window.
    KumochiWindows,
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

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestBody(***)")
    }
}

/// The result of one complete worker reply.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyBody {
    /// The worker is responsive.
    Pong,
    /// A content-free failure code.
    Fault(i32),
    /// A complete frame is held in the worker.
    Captured(FrameMeta),
    /// A bounded, capture-scoped control-view tree.
    Elements(ElementTree),
    /// The requested operation was refused.
    Refused(Refusal),
}

impl std::fmt::Debug for ReplyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pong => f.write_str("Pong"),
            Self::Fault(code) => write!(f, "Fault({code})"),
            Self::Captured(meta) => write!(f, "Captured({meta:?})"),
            Self::Elements(tree) => write!(f, "Elements({tree:?})"),
            Self::Refused(refusal) => write!(f, "Refused({refusal:?})"),
        }
    }
}

#[cfg(test)]
mod tests;
