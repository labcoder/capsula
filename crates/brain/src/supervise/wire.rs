//! The worker pipes' framing. Both directions carry `Frame`s on the data pipe: one request id
//! throughout, chunks numbered from zero, the last one marked. A message is postcard-serialized
//! once, capped by its kind, and split into frames under the pipe's cap.

use std::io::{Read, Write};

use kumochi_protocol::frame::{self, FrameError, Pipe};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::WorkerLost;

/// Body bytes per frame: the data pipe's cap less room for the id, chunk number and flag.
pub(super) const CHUNK: usize = Pipe::Data.max_payload() - 64;

#[derive(Serialize, Deserialize)]
pub(crate) struct Frame {
    pub id: u64,
    pub chunk: u32,
    pub last: bool,
    pub body: Vec<u8>,
}

/// One message's chunks, in order, up to a cap.
pub(crate) struct Assembly {
    max: usize,
    next: u32,
    bytes: Vec<u8>,
}

impl Assembly {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            max,
            next: 0,
            bytes: Vec::new(),
        }
    }

    /// Adds the next chunk; the whole message once the last arrives. A gap, a repeat, an empty
    /// chunk or going over the cap is a protocol break.
    pub(crate) fn push(&mut self, chunk: Frame) -> Result<Option<Vec<u8>>, WorkerLost> {
        if chunk.chunk != self.next
            || chunk.body.is_empty()
            || chunk.body.len() > self.max - self.bytes.len()
        {
            return Err(WorkerLost);
        }
        self.next = self.next.checked_add(1).ok_or(WorkerLost)?;
        self.bytes.extend(chunk.body);
        if !chunk.last {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.bytes)))
    }
}

/// Writes `message`, a request or a reply, as one or more frames under `id`.
pub(crate) fn write_message<T: Serialize>(
    output: &mut impl Write,
    id: u64,
    message: &T,
    max: usize,
) -> Result<(), FrameError> {
    let bytes = frame::encode_payload(message)?;
    if bytes.len() > max {
        return Err(FrameError::TooLarge {
            len: bytes.len(),
            max,
        });
    }
    let count = bytes.len().div_ceil(CHUNK).max(1);
    for (number, body) in bytes.chunks(CHUNK).enumerate() {
        let chunk = Frame {
            id,
            chunk: u32::try_from(number).map_err(|_| FrameError::TooLarge {
                len: bytes.len(),
                max,
            })?,
            last: number + 1 == count,
            body: body.to_vec(),
        };
        frame::write(output, &chunk, Pipe::Data)?;
    }
    output.flush()?;
    Ok(())
}

/// The worker's side: the next whole request and its id, or `None` when the supervisor closed the
/// pipe between requests. Anything else that breaks the framing is an error, and the worker
/// should exit.
pub(crate) fn read_request<T: DeserializeOwned>(
    input: &mut impl Read,
    max: usize,
) -> Result<Option<(u64, T)>, FrameError> {
    let mut assembly = Assembly::new(max);
    let mut id = None;
    loop {
        let Some(chunk) = frame::read::<Frame>(input, Pipe::Data)? else {
            return if id.is_none() {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        };
        if *id.get_or_insert(chunk.id) != chunk.id {
            return Err(FrameError::Chunks);
        }
        let this = chunk.id;
        if let Some(bytes) = assembly.push(chunk).map_err(|_| FrameError::Chunks)? {
            return Ok(Some((this, frame::decode(&bytes)?)));
        }
    }
}
