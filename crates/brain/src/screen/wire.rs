use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::geometry::{DisplayRef, PhysicalRect};
use kumochi_protocol::message::RunId;
use serde::{Deserialize, Serialize};

pub(super) const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// Why screen work did not finish. No foreign error text crosses this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScreenError {
    /// The worker died, broke its protocol or missed a deadline.
    #[error("screen worker lost")]
    WorkerLost,
    /// The worker refused an operation with a numeric platform code.
    #[error("screen operation failed ({0})")]
    Fault(i32),
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
        }
    }

    pub(super) fn capture(&self) -> CaptureId {
        self.capture
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

/// Why the worker took no frame, for a reason that isn't a platform fault.
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
}

/// A complete, valid frame's description; the pixels stay in the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// The ticket's capture.
    pub capture: CaptureId,
    /// The display's physical bounds, as captured.
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
    /// No frame was taken.
    Refused(Refusal),
}

impl std::fmt::Debug for ReplyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pong => f.write_str("Pong"),
            Self::Fault(code) => write!(f, "Fault({code})"),
            Self::Captured(meta) => write!(f, "Captured({meta:?})"),
            Self::Refused(refusal) => write!(f, "Refused({refusal:?})"),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    pub id: u64,
    pub body: RequestBody,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Reply {
    pub id: u64,
    pub chunk: u32,
    pub last: bool,
    pub body: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Assembly {
    next: u32,
    bytes: Vec<u8>,
}

impl Assembly {
    pub(super) fn push(&mut self, reply: Reply) -> Result<Option<Vec<u8>>, ScreenError> {
        if reply.chunk != self.next
            || reply.body.is_empty()
            || reply.body.len() > MAX_REPLY_BYTES - self.bytes.len()
        {
            return Err(ScreenError::WorkerLost);
        }
        self.next = self.next.checked_add(1).ok_or(ScreenError::WorkerLost)?;
        self.bytes.extend(reply.body);
        if !reply.last {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.bytes)))
    }
}

/// One single-chunk reply; every reply the worker sends today fits one frame.
pub(super) fn reply(id: u64, body: &ReplyBody) -> Result<Reply, frame::FrameError> {
    let encoded = frame::encode(body, Pipe::Data)?;
    Ok(Reply {
        id,
        chunk: 0,
        last: true,
        body: encoded[frame::HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
mod tests;
