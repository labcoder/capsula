//! One leg's records over tokio I/O. [`RecordReader`] opens records into the frame bytes the
//! existing frame readers take; [`RecordWriter`] sends one whole frame at a time, so a frame cut
//! off part way is never resumed. Both share the pair cell: every refusal sets it before the error
//! is returned, and once it is set neither leg reads, delivers or writes again. See
//! `docs/development/session.md`.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_session::{ENVELOPE_LEN, MAX_PLAINTEXT, MAX_RECORD, Opener, Sealer, SessionError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use zeroize::{Zeroize, Zeroizing};

/// Why an adapter refused. The pair cell is set before any of these reaches the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    /// The session refused a record, or the stream ended inside a record or frame.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// An adapter of this pair has already refused.
    #[error("the pair already refused")]
    PairRefused,
    /// The writer's previous frame was cut off before its last byte.
    #[error("an earlier frame on this leg was not finished")]
    Unfinished,
    /// The bytes handed to the writer are not exactly one frame within the leg's cap.
    #[error("the bytes are not one frame for this leg")]
    NotOneFrame,
}

/// Sets the pair cell, then returns the refusal as the error the caller sees.
fn refuse(pair: &watch::Sender<bool>, refusal: impl Into<Refusal>) -> io::Error {
    latch_refusal(pair);
    io::Error::new(io::ErrorKind::InvalidData, refusal.into())
}

fn latch_refusal(pair: &watch::Sender<bool>) {
    pair.send_if_modified(|refused| !std::mem::replace(refused, true));
}

struct FrameWrite<'a> {
    pair: &'a watch::Sender<bool>,
    armed: bool,
}

impl Drop for FrameWrite<'_> {
    fn drop(&mut self) {
        if self.armed {
            latch_refusal(self.pair);
        }
    }
}

/// One leg's incoming direction. Holds at most one record: its ciphertext while it arrives, then
/// its plaintext until the caller has read it.
pub(crate) struct RecordReader<R> {
    io: R,
    /// None once the stream has ended between frames.
    opener: Option<Opener>,
    pair: watch::Sender<bool>,
    record: Zeroizing<Vec<u8>>,
    filled: usize,
    announced: Option<usize>,
    start: usize,
    end: usize,
}

impl<R> RecordReader<R> {
    pub(crate) fn new(io: R, opener: Opener, pair: watch::Sender<bool>) -> Self {
        Self {
            io,
            opener: Some(opener),
            pair,
            record: Zeroizing::new(vec![0; ENVELOPE_LEN + MAX_RECORD]),
            filled: 0,
            announced: None,
            start: 0,
            end: 0,
        }
    }

    /// The transport ended: clean only between frames.
    fn end_of_stream(&mut self) -> io::Result<()> {
        let opener = self
            .opener
            .take()
            .expect("read only while the opener is held");
        match (opener.end(), self.filled) {
            (Ok(()), 0) => Ok(()),
            (Ok(()), _) => Err(refuse(&self.pair, SessionError::Truncated)),
            (Err(error), _) => Err(refuse(&self.pair, error)),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for RecordReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if *this.pair.borrow() {
                this.record[this.start..this.end].zeroize();
                this.start = this.end;
                return Poll::Ready(Err(refuse(&this.pair, Refusal::PairRefused)));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            if this.start < this.end {
                let taken = this.start..this.start + buf.remaining().min(this.end - this.start);
                this.start = taken.end;
                buf.put_slice(&this.record[taken.clone()]);
                this.record[taken].zeroize();
                return Poll::Ready(Ok(()));
            }
            let Some(opener) = this.opener.as_mut() else {
                return Poll::Ready(Ok(()));
            };
            let need = ENVELOPE_LEN + this.announced.unwrap_or(0);
            if this.filled < need {
                let mut read = ReadBuf::new(&mut this.record[this.filled..need]);
                match ready!(Pin::new(&mut this.io).poll_read(cx, &mut read)) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                }
                let n = read.filled().len();
                if n == 0 {
                    return Poll::Ready(this.end_of_stream());
                }
                this.filled += n;
            } else if this.announced.take().is_some() {
                let delivery = opener
                    .open(&this.record[ENVELOPE_LEN..need])
                    .map_err(|error| refuse(&this.pair, error))?;
                let bytes = delivery.bytes();
                this.record[..bytes.len()].copy_from_slice(bytes);
                (this.start, this.end) = (0, bytes.len());
                this.filled = 0;
            } else {
                let envelope = [this.record[0], this.record[1]];
                let len = opener
                    .record_len(envelope)
                    .map_err(|error| refuse(&this.pair, error))?;
                this.announced = Some(len);
            }
        }
    }
}

impl<R> fmt::Debug for RecordReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordReader")
            .field("opener", &self.opener)
            .field("record_bytes", &self.filled)
            .field("buffered", &(self.end - self.start))
            .finish_non_exhaustive()
    }
}

/// One leg's outgoing direction. Writes only whole frames, one sealed record at a time.
pub(crate) struct RecordWriter<W> {
    io: W,
    sealer: Sealer,
    pipe: Pipe,
    pair: watch::Sender<bool>,
    /// Set until a frame finishes, so even an ordinary transport error bars reuse.
    unfinished: bool,
}

impl<W> RecordWriter<W> {
    pub(crate) fn new(io: W, sealer: Sealer, pipe: Pipe, pair: watch::Sender<bool>) -> Self {
        Self {
            io,
            sealer,
            pipe,
            pair,
            unfinished: false,
        }
    }
}

impl<W: AsyncWrite + Unpin> RecordWriter<W> {
    /// Writes one frame, header included, as maximally sliced records. Dropping a pending frame
    /// latches refusal even if neither direction is polled again.
    ///
    /// # Errors
    /// A transport error, returned as it came; or a [`Refusal`], after the pair cell is set.
    pub(crate) async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let Self {
            io,
            sealer,
            pipe,
            pair,
            unfinished,
        } = self;
        if *pair.borrow() {
            return Err(refuse(pair, Refusal::PairRefused));
        }
        if *unfinished {
            return Err(refuse(pair, Refusal::Unfinished));
        }
        let declared = frame
            .first_chunk::<HEADER_LEN>()
            .and_then(|header| frame::payload_len(*header, *pipe).ok());
        if declared.is_none() || declared != frame.len().checked_sub(HEADER_LEN) {
            return Err(refuse(pair, Refusal::NotOneFrame));
        }
        *unfinished = true;
        let mut guard = FrameWrite { pair, armed: true };
        for record in frame.chunks(MAX_PLAINTEXT) {
            sealer.seal(record).map_err(|error| refuse(pair, error))?;
            let result = std::future::poll_fn(|cx| -> Poll<io::Result<()>> {
                loop {
                    if *pair.borrow() {
                        return Poll::Ready(Err(refuse(pair, Refusal::PairRefused)));
                    }
                    let rest = sealer.unwritten().map_err(|error| refuse(pair, error))?;
                    if rest.is_empty() {
                        return Poll::Ready(Ok(()));
                    }
                    let n = match ready!(Pin::new(&mut *io).poll_write(cx, rest)) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                    if n == 0 {
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                    }
                    sealer.advance(n).map_err(|error| refuse(pair, error))?;
                }
            })
            .await;
            if result.is_err() {
                guard.armed = false;
                return result;
            }
        }
        guard.armed = false;
        *unfinished = false;
        Ok(())
    }
}

impl<W> fmt::Debug for RecordWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordWriter")
            .field("sealer", &self.sealer)
            .field("pipe", &self.pipe)
            .field("unfinished", &self.unfinished)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
