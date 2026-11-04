//! Bridge ↔ brain messages: postcard in protocol's frame format, chunked under the data pipe's
//! 256 KiB cap in both directions.

use serde::{Deserialize, Serialize};

/// The most bytes of a body one frame carries, well under the data pipe's cap with its envelope.
pub(crate) const CHUNK: usize = 200 * 1024;

/// A call as the brain sees it: `m:n:<decimal>` or `m:s:<hex>`, at most 64 ASCII bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CallKey(pub(crate) String);

/// The methods the bridge relays, and `ping`, which it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Method {
    Initialize,
    ToolsList,
    ToolsCall,
    Ping,
}

/// Bridge to brain.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ToBrain {
    /// Part of a request's raw params JSON; the brain reassembles at most `MAX_INCOMING`.
    Request {
        key: CallKey,
        method: Method,
        chunk: Vec<u8>,
        last: bool,
    },
    /// `notifications/initialized`.
    Initialized,
}

/// Brain to bridge.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ToBridge {
    /// Sent once at connect: the launch's remaining deadline.
    Hello { remaining_ms: u64 },
    /// Part of the result or error JSON for `key`; `error` on the last chunk decides which.
    Reply {
        key: CallKey,
        chunk: Vec<u8>,
        last: bool,
        error: bool,
    },
}

/// `body` in chunks of at most [`CHUNK`] bytes, the last one marked; an empty body is one chunk.
pub(crate) fn chunks(body: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let count = body.len().div_ceil(CHUNK).max(1);
    (0..count).map(move |at| {
        let end = body.len().min((at + 1) * CHUNK);
        (&body[(at * CHUNK).min(end)..end], at + 1 == count)
    })
}

/// A body over its cap.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TooLarge;

/// Joins one body's chunks, keeping at most `cap` bytes; past it, the rest is dropped until the
/// last chunk.
pub(crate) struct Reassembly {
    buf: Vec<u8>,
    cap: usize,
    over: bool,
}

impl Reassembly {
    pub(crate) const fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            over: false,
        }
    }

    /// Adds the next chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if !self.over && self.buf.len() + chunk.len() > self.cap {
            self.over = true;
            self.buf = Vec::new();
        }
        if !self.over {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// After the last chunk: the whole body, or [`TooLarge`] if it passed the cap.
    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, TooLarge> {
        if std::mem::take(&mut self.over) {
            return Err(TooLarge);
        }
        Ok(std::mem::take(&mut self.buf))
    }
}
