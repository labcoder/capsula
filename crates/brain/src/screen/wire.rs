use serde::{Deserialize, Serialize};

/// The largest reply, serialized.
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
    /// Trigger one failure without modifying the parent's environment.
    #[cfg(debug_assertions)]
    Fault(WorkerFault),
}

/// A dev-only, one-request failure fixture.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum WorkerFault {
    /// Stop answering.
    Hang,
    /// Exit abnormally.
    Crash,
    /// Write an invalid frame.
    Garbage,
    /// Panic after installing the location-only hook.
    Panic,
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
}

impl std::fmt::Debug for ReplyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pong => f.write_str("Pong"),
            Self::Fault(code) => write!(f, "Fault({code})"),
        }
    }
}

#[cfg(test)]
mod tests;
