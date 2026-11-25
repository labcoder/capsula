//! macOS's leg of the shell–brain pair: the link's `PipeClient`, carried over one Unix stream through
//! the session crate's record owners. Reads resume across a timeout; writes take one whole frame at
//! a time, so a frame cut off part way is never resumed. Both legs share the pair: a record refusal
//! latches it and shuts down both, so a reader blocked on the other leg wakes. [`initiate`] runs the
//! initiator's handshake and returns the two legs. See `docs/development/ipc.md`.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, FrameError, HEADER_LEN, Pipe};
use kumochi_protocol::message::ControlToShell;
use kumochi_session::{
    Channel, ENVELOPE_LEN, MAX_PLAINTEXT, MAX_RECORD, Opener, SECRET_LEN, Sealer, Session,
    SessionError, ShellAwaitsReply,
};
use zeroize::{Zeroize as _, Zeroizing};

use super::pair::ShellEnds;

/// Both legs' shared state: once refused, neither leg reads, delivers or writes again.
#[derive(Debug)]
struct Pair {
    refused: AtomicBool,
    /// Clones of both legs' sockets, so a refusal can shut both down.
    ends: [UnixStream; 2],
}

impl Pair {
    fn refused(&self) -> bool {
        self.refused.load(Ordering::SeqCst)
    }

    /// Latches the refusal, then shuts down both legs so every blocked call returns.
    fn refuse(&self, error: impl Into<io::Error>) -> io::Error {
        self.refused.store(true, Ordering::SeqCst);
        for end in &self.ends {
            // A leg that's already shut down has nothing left to wake.
            let _shut = end.shutdown(Shutdown::Both);
        }
        error.into()
    }
}

fn session_error(error: SessionError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn pair_refused() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the pair already refused")
}

/// One leg's incoming direction: at most one record, its ciphertext while it arrives, then its
/// plaintext until it's read. A frame the connector looked at first is read again before it.
struct Reader {
    /// None once the stream has ended between frames.
    opener: Option<Opener>,
    record: Zeroizing<Vec<u8>>,
    filled: usize,
    announced: Option<usize>,
    start: usize,
    end: usize,
    retained: Zeroizing<Vec<u8>>,
    retained_at: usize,
}

/// One leg of the pair. Reads and writes may run on different threads at once.
pub(crate) struct PipeClient {
    stream: UnixStream,
    pipe: Pipe,
    reader: Mutex<Reader>,
    writer: Mutex<Sealer>,
    aborted: AtomicBool,
    pair: Arc<Pair>,
}

impl std::fmt::Debug for PipeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeClient")
            .field("pipe", &self.pipe)
            .field("aborted", &self.is_aborted())
            .finish_non_exhaustive()
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PipeClient {
    fn new(
        stream: UnixStream,
        pipe: Pipe,
        opener: Opener,
        sealer: Sealer,
        pair: Arc<Pair>,
    ) -> Self {
        Self {
            stream,
            pipe,
            reader: Mutex::new(Reader {
                opener: Some(opener),
                record: Zeroizing::new(vec![0; ENVELOPE_LEN + MAX_RECORD]),
                filled: 0,
                announced: None,
                start: 0,
                end: 0,
                retained: Zeroizing::new(Vec::new()),
                retained_at: 0,
            }),
            writer: Mutex::new(sealer),
            aborted: AtomicBool::new(false),
            pair,
        }
    }

    /// The pair has no named pipes to open.
    pub(crate) fn open(_: &str) -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// The pair's peer is the shell's own child, not a pipe server.
    #[expect(
        clippy::unused_self,
        reason = "the link calls it on any platform's PipeClient"
    )]
    pub(crate) fn server_image(&self) -> io::Result<PathBuf> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Ends this leg: blocked calls return, and later ones fail.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // Already shut down when the pair refused.
        let _shut = self.stream.shutdown(Shutdown::Both);
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst) || self.pair.refused()
    }

    /// Bounds each later read and write; None waits without a bound. A bound the socket refuses
    /// ends the leg rather than leaving a wait unbounded.
    pub(crate) fn set_timeout(&self, timeout: Option<Duration>) {
        if self.stream.set_read_timeout(timeout).is_err()
            || self.stream.set_write_timeout(timeout).is_err()
        {
            self.abort();
        }
    }

    /// Reads the next control frame, decodes it, and keeps its bytes so the next reader gets the
    /// same frame first. For the connector's look at the first frame.
    pub(crate) fn peek_control(&self) -> Result<Option<ControlToShell>, FrameError> {
        let mut header = [0; HEADER_LEN];
        match (&mut &*self).read(&mut header[..1])? {
            0 => return Ok(None),
            _ => (&mut &*self).read_exact(&mut header[1..])?,
        }
        let mut bytes =
            Zeroizing::new(vec![0; HEADER_LEN + frame::payload_len(header, self.pipe)?]);
        bytes[..HEADER_LEN].copy_from_slice(&header);
        (&mut &*self).read_exact(&mut bytes[HEADER_LEN..])?;
        let message = frame::decode(&bytes[HEADER_LEN..])?;
        let mut reader = locked(&self.reader);
        (reader.retained, reader.retained_at) = (bytes, 0);
        Ok(Some(message))
    }
}

impl Read for &PipeClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let client = *self;
        let mut reader = locked(&client.reader);
        let Reader {
            opener,
            record,
            filled,
            announced,
            start,
            end,
            retained,
            retained_at,
        } = &mut *reader;
        if *retained_at < retained.len() {
            let n = buf.len().min(retained.len() - *retained_at);
            buf[..n].copy_from_slice(&retained[*retained_at..*retained_at + n]);
            *retained_at += n;
            if *retained_at == retained.len() {
                retained.zeroize();
                *retained_at = 0;
            }
            return Ok(n);
        }
        loop {
            if client.pair.refused() {
                record[*start..*end].zeroize();
                *start = *end;
                return Err(pair_refused());
            }
            if buf.is_empty() {
                return Ok(0);
            }
            if start < end {
                let n = buf.len().min(*end - *start);
                buf[..n].copy_from_slice(&record[*start..*start + n]);
                record[*start..*start + n].zeroize();
                *start += n;
                return Ok(n);
            }
            let Some(owner) = opener.as_mut() else {
                return Ok(0);
            };
            let need = ENVELOPE_LEN + announced.unwrap_or(0);
            if *filled < need {
                let n = match (&client.stream).read(&mut record[*filled..need]) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    // A timeout keeps the partial record, so the next read resumes it.
                    result => result?,
                };
                if n == 0 {
                    let owner = opener.take().expect("checked above");
                    return match (owner.end(), *filled) {
                        (Ok(()), 0) => Ok(0),
                        (Ok(()), _) => {
                            Err(client.pair.refuse(session_error(SessionError::Truncated)))
                        }
                        (Err(error), _) => Err(client.pair.refuse(session_error(error))),
                    };
                }
                *filled += n;
            } else if announced.take().is_some() {
                let delivery = owner
                    .open(&record[ENVELOPE_LEN..need])
                    .map_err(|error| client.pair.refuse(session_error(error)))?;
                let bytes = delivery.bytes();
                record[..bytes.len()].copy_from_slice(bytes);
                (*start, *end) = (0, bytes.len());
                *filled = 0;
            } else {
                let len = owner
                    .record_len([record[0], record[1]])
                    .map_err(|error| client.pair.refuse(session_error(error)))?;
                *announced = Some(len);
            }
        }
    }
}

impl Write for &PipeClient {
    /// Takes exactly one frame, header included, and writes it whole as maximally sliced records.
    /// Anything that stops it part way refuses the pair, since a frame is never resumed.
    fn write(&mut self, frame_bytes: &[u8]) -> io::Result<usize> {
        let client = *self;
        if client.is_aborted() {
            return Err(pair_refused());
        }
        let declared = frame_bytes
            .first_chunk::<HEADER_LEN>()
            .and_then(|header| frame::payload_len(*header, client.pipe).ok());
        if declared.is_none() || declared != frame_bytes.len().checked_sub(HEADER_LEN) {
            return Err(client.pair.refuse(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the bytes are not one frame for this leg",
            )));
        }
        let mut sealer = locked(&client.writer);
        for record in frame_bytes.chunks(MAX_PLAINTEXT) {
            sealer
                .seal(record)
                .map_err(|error| client.pair.refuse(session_error(error)))?;
            loop {
                let rest = sealer
                    .unwritten()
                    .map_err(|error| client.pair.refuse(session_error(error)))?;
                if rest.is_empty() {
                    break;
                }
                let n = match (&client.stream).write(rest) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(client.pair.refuse(error)),
                    Ok(0) => return Err(client.pair.refuse(io::ErrorKind::WriteZero)),
                    Ok(n) => n,
                };
                sealer
                    .advance(n)
                    .map_err(|error| client.pair.refuse(session_error(error)))?;
            }
        }
        Ok(frame_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Sets both sockets' bounds to what's left of `by`.
fn bound(ends: &ShellEnds, by: Instant) -> io::Result<()> {
    let left = by.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    for end in [&ends.control, &ends.data] {
        end.set_read_timeout(Some(left))?;
        end.set_write_timeout(Some(left))?;
    }
    Ok(())
}

fn send(ends: &ShellEnds, leg: Pipe, bytes: &[u8], by: Instant) -> io::Result<()> {
    bound(ends, by)?;
    match leg {
        Pipe::Control => (&ends.control).write_all(bytes),
        Pipe::Data => (&ends.data).write_all(bytes),
    }
}

/// One handshake message, its length taken from the envelope by the state that expects it.
fn message(
    ends: &ShellEnds,
    leg: Pipe,
    by: Instant,
    len: impl FnOnce([u8; ENVELOPE_LEN]) -> Result<usize, SessionError>,
) -> io::Result<Vec<u8>> {
    let mut stream = match leg {
        Pipe::Control => &ends.control,
        Pipe::Data => &ends.data,
    };
    bound(ends, by)?;
    let mut envelope = [0; ENVELOPE_LEN];
    stream.read_exact(&mut envelope)?;
    let mut message = vec![0; len(envelope).map_err(session_error)?];
    bound(ends, by)?;
    stream.read_exact(&mut message)?;
    Ok(message)
}

/// The initiator's side over the brain child's pair, all of it by `by`: control init and
/// Finished, data init and Finished, then Bind, and the legs only once `BindAck` verifies. The
/// sockets keep their last bounds; the caller sets its own.
///
/// # Errors
/// `TimedOut` past `by`, `InvalidData` for a session refusal, or the transport's error.
pub(crate) fn initiate(
    ends: ShellEnds,
    secret: &[u8; SECRET_LEN],
    channel: Channel,
    by: Instant,
) -> io::Result<(PipeClient, PipeClient)> {
    let (mut shell, init) = ShellAwaitsReply::start(secret, channel).map_err(session_error)?;
    send(&ends, init.leg, &init.bytes, by)?;
    let reply = message(&ends, Pipe::Control, by, |envelope| {
        shell.record_len(envelope)
    })?;
    let (shell, out) = shell.receive(&reply).map_err(session_error)?;
    for out in &out {
        send(&ends, out.leg, &out.bytes, by)?;
    }
    let reply = message(&ends, Pipe::Data, by, |envelope| shell.record_len(envelope))?;
    let (shell, out) = shell.receive(&reply).map_err(session_error)?;
    for out in &out {
        send(&ends, out.leg, &out.bytes, by)?;
    }
    let ack = message(&ends, Pipe::Control, by, |envelope| {
        shell.record_len(envelope)
    })?;
    let Session { control, data } = shell.receive(&ack).map_err(session_error)?;
    if Instant::now() >= by {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let pair = Arc::new(Pair {
        refused: AtomicBool::new(false),
        ends: [ends.control.try_clone()?, ends.data.try_clone()?],
    });
    Ok((
        PipeClient::new(
            ends.control,
            Pipe::Control,
            control.opener,
            control.sealer,
            Arc::clone(&pair),
        ),
        PipeClient::new(ends.data, Pipe::Data, data.opener, data.sealer, pair),
    ))
}

#[cfg(test)]
#[path = "leg_tests.rs"]
mod tests;
