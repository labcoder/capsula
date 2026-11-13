//! The worker pipes' framing. Both directions carry `Frame`s on the data pipe: one request id
//! throughout, chunks numbered from zero, the last one marked. A message is postcard-serialized
//! once, capped by its kind, and split into frames under the pipe's cap.

use std::fmt;
use std::io::{self, Read, Write};

use kumochi_protocol::frame::{self, FrameError, HEADER_LEN, Pipe};
use postcard::ser_flavors::Size;
use serde::de::{self, DeserializeOwned, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::{Zeroize as _, Zeroizing};

use super::WorkerLost;

#[cfg(test)]
mod tests;

/// Room in a frame for its id, chunk number, flag and body length.
const FIELDS: usize = 64;
/// Body bytes per frame: the data pipe's cap less room for the id, chunk number and flag.
pub(super) const CHUNK: usize = Pipe::Data.max_payload() - FIELDS;

#[derive(Serialize, Deserialize)]
pub(crate) struct Frame<B = Vec<u8>> {
    pub id: u64,
    pub chunk: u32,
    pub last: bool,
    pub body: B,
}

/// A [`Zeroed`] frame's body: user content, zeroed when it's dropped, wherever that happens.
/// It's read as one byte string into a buffer of exactly its length, so a frame that fails to
/// decode leaves no partly filled copy.
pub(crate) struct ZeroedBody(Vec<u8>);

#[cfg(test)]
thread_local! {
    /// The length and capacity each [`ZeroedBody`] dropped on this thread had once zeroed.
    pub(super) static SCRUBBED: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl Drop for ZeroedBody {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        SCRUBBED.with_borrow_mut(|scrubbed| scrubbed.push((self.0.len(), self.0.capacity())));
    }
}

impl AsRef<[u8]> for ZeroedBody {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ZeroedBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Exact;

        impl Visitor<'_> for Exact {
            type Value = ZeroedBody;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a frame's body")
            }

            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                let mut body = Vec::with_capacity(bytes.len());
                body.extend_from_slice(bytes);
                Ok(ZeroedBody(body))
            }
        }

        deserializer.deserialize_bytes(Exact)
    }
}

/// A [`Frame`] whose body is borrowed; it serializes to the same bytes.
#[derive(Serialize)]
struct FrameRef<'a> {
    id: u64,
    chunk: u32,
    last: bool,
    body: &'a [u8],
}

/// How a kind's replies are read and put back together. [`Assembly`] is the plain way;
/// [`Zeroed`] is for replies that can carry the user's content.
pub(crate) trait Reassembly: Send + 'static {
    /// What a frame's body is held in until it's pushed or dropped.
    type Body: AsRef<[u8]> + DeserializeOwned + Send + 'static;

    fn new(max: usize) -> Self;

    /// The next frame, `None` when the worker closed its pipe between frames.
    ///
    /// # Errors
    /// Fails as [`frame::read`] does.
    fn read(reader: &mut impl Read) -> Result<Option<Frame<Self::Body>>, FrameError>;

    /// Adds the next chunk; once the last arrives, `decode` reads the whole message.
    ///
    /// # Errors
    /// A gap, a repeat, an empty chunk or going over the cap is a protocol break.
    fn push<R>(
        &mut self,
        chunk: Frame<Self::Body>,
        decode: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, WorkerLost>;
}

/// Each message's chunks, in order, up to a cap per message.
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
            return Err(WorkerLost::Lost);
        }
        self.next = self.next.checked_add(1).ok_or(WorkerLost::Lost)?;
        self.bytes.extend(chunk.body);
        if !chunk.last {
            return Ok(None);
        }
        self.next = 0;
        Ok(Some(std::mem::take(&mut self.bytes)))
    }
}

impl Reassembly for Assembly {
    type Body = Vec<u8>;

    fn new(max: usize) -> Self {
        Self::new(max)
    }

    fn read(reader: &mut impl Read) -> Result<Option<Frame>, FrameError> {
        frame::read(reader, Pipe::Data)
    }

    fn push<R>(
        &mut self,
        chunk: Frame,
        decode: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, WorkerLost> {
        Ok(Self::push(self, chunk)?.map(|bytes| decode(&bytes)))
    }
}

/// Reassembly that leaves no copy of a reply behind: each frame is read into a zeroing buffer of
/// its own length, each chunk's body is a [`ZeroedBody`], and a message of several chunks is
/// put together in one zeroing buffer allocated at the cap, so it never grows. A message of one
/// chunk is decoded from that chunk.
pub(crate) struct Zeroed {
    max: usize,
    next: u32,
    bytes: Option<Zeroizing<Vec<u8>>>,
}

impl Reassembly for Zeroed {
    type Body = ZeroedBody;

    fn new(max: usize) -> Self {
        Self {
            max,
            next: 0,
            bytes: None,
        }
    }

    fn read(reader: &mut impl Read) -> Result<Option<Frame<ZeroedBody>>, FrameError> {
        let mut header = [0; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match reader.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(FrameError::Truncated),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut payload = Zeroizing::new(vec![0; frame::payload_len(header, Pipe::Data)?]);
        reader
            .read_exact(&mut payload)
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => FrameError::Truncated,
                _ => FrameError::Io(e),
            })?;
        frame::decode(&payload).map(Some)
    }

    fn push<R>(
        &mut self,
        chunk: Frame<ZeroedBody>,
        decode: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, WorkerLost> {
        let body = chunk.body.as_ref();
        let held = self.bytes.as_ref().map_or(0, |bytes| bytes.len());
        if chunk.chunk != self.next || body.is_empty() || body.len() > self.max - held {
            return Err(WorkerLost::Lost);
        }
        self.next = self.next.checked_add(1).ok_or(WorkerLost::Lost)?;
        if chunk.last && self.bytes.is_none() {
            self.next = 0;
            return Ok(Some(decode(body)));
        }
        let max = self.max;
        let bytes = self
            .bytes
            .get_or_insert_with(|| Zeroizing::new(Vec::with_capacity(max)));
        bytes.extend_from_slice(body);
        if !chunk.last {
            return Ok(None);
        }
        self.next = 0;
        let decoded = decode(bytes);
        self.bytes = None;
        Ok(Some(decoded))
    }
}

/// Writes `message`, a request or a reply, as one or more frames under `id`. A message can carry
/// an image, so its serialized copies are zeroed once written.
pub(crate) fn write_message<T: Serialize>(
    output: &mut impl Write,
    id: u64,
    message: &T,
    max: usize,
) -> Result<(), FrameError> {
    let bytes = Zeroizing::new(frame::encode_payload(message)?);
    let mut framed = Zeroizing::new(Vec::new());
    if bytes.len() > max {
        return Err(FrameError::TooLarge {
            len: bytes.len(),
            max,
        });
    }
    let count = bytes.len().div_ceil(CHUNK).max(1);
    for (number, body) in bytes.chunks(CHUNK).enumerate() {
        let mut chunk = Frame {
            id,
            chunk: u32::try_from(number).map_err(|_| FrameError::TooLarge {
                len: bytes.len(),
                max,
            })?,
            last: number + 1 == count,
            body: body.to_vec(),
        };
        let encoded = frame::encode_into(&chunk, Pipe::Data, &mut framed);
        chunk.body.zeroize();
        encoded?;
        output.write_all(&framed)?;
    }
    output.flush()?;
    Ok(())
}

/// Writes `message` as [`write_message`] does, with no copy left behind by growth: the message
/// is sized first and serialized into one zeroing buffer of that size, and its frames are encoded
/// one after another into one zeroing buffer that already fits the largest. `output` should be
/// unbuffered, so no copy lands in a buffer this process doesn't zero.
pub(crate) fn write_sized<T: Serialize>(
    output: &mut impl Write,
    id: u64,
    message: &T,
    max: usize,
) -> Result<(), FrameError> {
    let len = postcard::serialize_with_flavor::<T, Size, usize>(message, Size::default())?;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut payload = Zeroizing::new(vec![0; len]);
    let written = postcard::to_slice(message, &mut payload)?.len();
    debug_assert_eq!(written, len, "the sized length");
    let mut framed = Zeroizing::new(Vec::with_capacity(HEADER_LEN + len.min(CHUNK) + FIELDS));
    let capacity = framed.capacity();
    let count = len.div_ceil(CHUNK).max(1);
    for (number, body) in payload.chunks(CHUNK).enumerate() {
        let chunk = FrameRef {
            id,
            chunk: u32::try_from(number).map_err(|_| FrameError::TooLarge { len, max })?,
            last: number + 1 == count,
            body,
        };
        frame::encode_into(&chunk, Pipe::Data, &mut framed)?;
        debug_assert_eq!(
            framed.capacity(),
            capacity,
            "a frame never grows its buffer"
        );
        output.write_all(&framed)?;
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
