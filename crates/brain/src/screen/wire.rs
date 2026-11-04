use kumochi_protocol::frame::{self, Pipe};
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

pub(super) fn pong(id: u64) -> Result<Reply, frame::FrameError> {
    let encoded = frame::encode(&ReplyBody::Pong, Pipe::Data)?;
    Ok(Reply {
        id,
        chunk: 0,
        last: true,
        body: encoded[frame::HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
mod tests;
