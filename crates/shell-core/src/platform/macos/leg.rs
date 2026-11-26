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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, FrameError, HEADER_LEN, Pipe};
use kumochi_protocol::message::ControlToShell;
use kumochi_session::{
    Channel, ENVELOPE_LEN, MAX_PLAINTEXT, MAX_RECORD, Opener, SECRET_LEN, Sealer, Session,
    SessionError, ShellAwaitsReply,
};
use zeroize::{Zeroize as _, Zeroizing};

use super::child::poll;
use super::pair::ShellEnds;

/// Both legs' shared state: once refused, neither leg reads, delivers or writes again.
struct Pair {
    refused: AtomicBool,
    /// Clones of both legs' sockets, so a refusal can shut both down.
    ends: [UnixStream; 2],
    /// Both legs' incoming directions, here so a refusal can wipe both.
    readers: [Mutex<Reader>; 2],
}

impl Pair {
    fn refused(&self) -> bool {
        self.refused.load(Ordering::SeqCst)
    }

    /// Latches the refusal, shuts down both legs so every blocked call returns, and wipes both
    /// readers.
    fn refuse(&self, error: impl Into<io::Error>) -> io::Error {
        self.refused.store(true, Ordering::SeqCst);
        for end in &self.ends {
            // A leg that's already shut down has nothing left to wake.
            let _shut = end.shutdown(Shutdown::Both);
        }
        for reader in &self.readers {
            wipe_unless_held(reader);
        }
        error.into()
    }
}

/// Wipes a reader now, unless a call holds it; that call wipes it as it lets go.
fn wipe_unless_held(reader: &Mutex<Reader>) {
    match reader.try_lock() {
        Ok(mut reader) => reader.wipe(),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().wipe(),
        Err(TryLockError::WouldBlock) => {}
    }
}

fn session_error(error: SessionError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn pair_refused() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the pair already refused")
}

/// `TimedOut` once `by` has passed, so no transfer starts after its deadline.
fn expired(by: Option<Instant>) -> io::Result<()> {
    if by.is_some_and(|by| Instant::now() >= by) {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(())
}

/// Waits until `stream` is ready for `events`, until `by` when bounded. Returns once it's ready,
/// the wait was interrupted or `by` passed; the caller tries again and sees which.
fn ready(stream: &UnixStream, events: libc::c_short, by: Option<Instant>) -> io::Result<()> {
    let Some(by) = by else {
        return poll(stream, events, None);
    };
    let left = by.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    poll(stream, events, Some(left))
}

/// One read of what's there, waiting for bytes until `by` when bounded. The sockets are
/// nonblocking, so no transfer ever waits inside the kernel past a deadline.
fn read_by(stream: &UnixStream, buf: &mut [u8], by: Option<Instant>) -> io::Result<usize> {
    loop {
        expired(by)?;
        match (&*stream).read(buf) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                ready(stream, libc::POLLIN, by)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// One write of what fits, waiting for room until `by` when bounded. Darwin restarts a blocking
/// send's timeout on every wake and ignores `MSG_DONTWAIT` on Unix streams, so the sockets are
/// nonblocking and the wait is a poll on what's left of the deadline.
fn write_by(stream: &UnixStream, bytes: &[u8], by: Option<Instant>) -> io::Result<usize> {
    loop {
        expired(by)?;
        match (&*stream).write(bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                ready(stream, libc::POLLOUT, by)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn leg_aborted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "this leg was aborted")
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

impl Reader {
    fn new(opener: Opener) -> Self {
        Self {
            opener: Some(opener),
            record: Zeroizing::new(vec![0; ENVELOPE_LEN + MAX_RECORD]),
            filled: 0,
            announced: None,
            start: 0,
            end: 0,
            retained: Zeroizing::new(Vec::new()),
            retained_at: 0,
        }
    }

    /// Wipes every byte this reader holds, plaintext and ciphertext alike, and its opener.
    fn wipe(&mut self) {
        self.opener = None;
        self.record.as_mut_slice().zeroize();
        (self.filled, self.announced, self.start, self.end) = (0, None, 0, 0);
        self.retained.zeroize();
        self.retained_at = 0;
    }
}

/// One leg of the pair. Reads and writes may run on different threads at once.
pub(crate) struct PipeClient {
    stream: UnixStream,
    pipe: Pipe,
    writer: Mutex<Sealer>,
    /// How long each read or write call may take as a whole; None waits without a bound.
    timeout: Mutex<Option<Duration>>,
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

impl Drop for PipeClient {
    /// The pair outlives a dropped leg while the other leg lives, so the leg wipes its reader.
    fn drop(&mut self) {
        locked(self.reader()).wipe();
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PipeClient {
    fn new(stream: UnixStream, pipe: Pipe, sealer: Sealer, pair: Arc<Pair>) -> Self {
        Self {
            stream,
            pipe,
            writer: Mutex::new(sealer),
            timeout: Mutex::new(None),
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

    fn reader(&self) -> &Mutex<Reader> {
        &self.pair.readers[match self.pipe {
            Pipe::Control => 0,
            Pipe::Data => 1,
        }]
    }

    /// After a call lets go of the reader: a refusal or abort that came while it held the lock
    /// couldn't wipe it, so the call does.
    fn wipe_if_aborted(&self) {
        if self.is_aborted() {
            locked(self.reader()).wipe();
        }
    }

    /// Ends this leg: blocked calls return, later ones fail, and its reader is wiped.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // Already shut down when the pair refused.
        let _shut = self.stream.shutdown(Shutdown::Both);
        wipe_unless_held(self.reader());
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst) || self.pair.refused()
    }

    /// Bounds each later read and write call as a whole, however many socket transfers it takes;
    /// None waits without a bound.
    pub(crate) fn set_timeout(&self, timeout: Option<Duration>) {
        *locked(&self.timeout) = timeout;
    }

    /// When the call starting now must end, if it's bounded.
    fn call_deadline(&self) -> Option<Instant> {
        locked(&self.timeout).map(|timeout| Instant::now() + timeout)
    }

    /// Reads the next control frame by `by`, decodes it, and keeps its bytes so the next reader
    /// gets the same frame first. For the connector's look at the first frame.
    pub(crate) fn peek_control(&self, by: Instant) -> Result<Option<ControlToShell>, FrameError> {
        let mut io = By(self, by);
        let mut header = [0; HEADER_LEN];
        match io.read(&mut header[..1])? {
            0 => return Ok(None),
            _ => io.read_exact(&mut header[1..])?,
        }
        let mut bytes =
            Zeroizing::new(vec![0; HEADER_LEN + frame::payload_len(header, self.pipe)?]);
        bytes[..HEADER_LEN].copy_from_slice(&header);
        io.read_exact(&mut bytes[HEADER_LEN..])?;
        let message = frame::decode(&bytes[HEADER_LEN..])?;
        let mut reader = locked(self.reader());
        (reader.retained, reader.retained_at) = (bytes, 0);
        drop(reader);
        self.wipe_if_aborted();
        Ok(Some(message))
    }
}

/// Reads that all end by one instant.
struct By<'a>(&'a PipeClient, Instant);

impl Read for By<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.1.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.0.set_timeout(Some(left));
        self.0.read(buf)
    }
}

impl Read for &PipeClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.read_held(buf);
        self.wipe_if_aborted();
        result
    }
}

impl PipeClient {
    /// One read with the reader held.
    fn read_held(&self, buf: &mut [u8]) -> io::Result<usize> {
        let client = self;
        let mut reader = locked(client.reader());
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
        let by = client.call_deadline();
        loop {
            if client.is_aborted() {
                return Err(if client.pair.refused() {
                    pair_refused()
                } else {
                    leg_aborted()
                });
            }
            if *retained_at < retained.len() {
                let n = buf.len().min(retained.len() - *retained_at);
                buf[..n].copy_from_slice(&retained[*retained_at..*retained_at + n]);
                retained[*retained_at..*retained_at + n].zeroize();
                *retained_at += n;
                if *retained_at == retained.len() {
                    retained.zeroize();
                    *retained_at = 0;
                }
                return Ok(n);
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
                // A timeout keeps the partial record, so the next read resumes it.
                let n = match read_by(&client.stream, &mut record[*filled..need], by) {
                    // Refused or aborted while this read waited: report that, not an end.
                    _ if client.is_aborted() => continue,
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
        if client.pair.refused() {
            return Err(pair_refused());
        }
        if client.is_aborted() {
            return Err(leg_aborted());
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
        let by = client.call_deadline();
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
                let n = match write_by(&client.stream, rest, by) {
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

fn end(ends: &ShellEnds, leg: Pipe) -> &UnixStream {
    match leg {
        Pipe::Control => &ends.control,
        Pipe::Data => &ends.data,
    }
}

/// Writes all of `bytes` by `by`.
fn send(stream: &UnixStream, mut bytes: &[u8], by: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        match write_by(stream, bytes, Some(by))? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => bytes = &bytes[n..],
        }
    }
    Ok(())
}

/// Fills `buf` by `by`.
fn fill(stream: &UnixStream, mut buf: &mut [u8], by: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        match read_by(stream, buf, Some(by))? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => buf = &mut std::mem::take(&mut buf)[n..],
        }
    }
    Ok(())
}

/// One handshake message, its length taken from the envelope by the state that expects it.
fn message(
    stream: &UnixStream,
    by: Instant,
    len: impl FnOnce([u8; ENVELOPE_LEN]) -> Result<usize, SessionError>,
) -> io::Result<Vec<u8>> {
    let mut envelope = [0; ENVELOPE_LEN];
    fill(stream, &mut envelope, by)?;
    let mut message = vec![0; len(envelope).map_err(session_error)?];
    fill(stream, &mut message, by)?;
    Ok(message)
}

/// The initiator's side over the brain child's pair, all of it by `by`: control init and
/// Finished, data init and Finished, then Bind, and the legs only once `BindAck` verifies. The
/// legs start unbounded; the caller sets its own bounds.
///
/// # Errors
/// `TimedOut` past `by`, `InvalidData` for a session refusal, or the transport's error.
pub(crate) fn initiate(
    ends: ShellEnds,
    secret: &[u8; SECRET_LEN],
    channel: Channel,
    by: Instant,
) -> io::Result<(PipeClient, PipeClient)> {
    for end in [&ends.control, &ends.data] {
        end.set_nonblocking(true)?;
    }
    let (mut shell, init) = ShellAwaitsReply::start(secret, channel).map_err(session_error)?;
    send(end(&ends, init.leg), &init.bytes, by)?;
    let reply = message(&ends.control, by, |envelope| shell.record_len(envelope))?;
    let (shell, out) = shell.receive(&reply).map_err(session_error)?;
    for out in &out {
        send(end(&ends, out.leg), &out.bytes, by)?;
    }
    let reply = message(&ends.data, by, |envelope| shell.record_len(envelope))?;
    let (shell, out) = shell.receive(&reply).map_err(session_error)?;
    for out in &out {
        send(end(&ends, out.leg), &out.bytes, by)?;
    }
    let ack = message(&ends.control, by, |envelope| shell.record_len(envelope))?;
    let Session { control, data } = shell.receive(&ack).map_err(session_error)?;
    if Instant::now() >= by {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let pair = Arc::new(Pair {
        refused: AtomicBool::new(false),
        ends: [ends.control.try_clone()?, ends.data.try_clone()?],
        readers: [
            Mutex::new(Reader::new(control.opener)),
            Mutex::new(Reader::new(data.opener)),
        ],
    });
    Ok((
        PipeClient::new(
            ends.control,
            Pipe::Control,
            control.sealer,
            Arc::clone(&pair),
        ),
        PipeClient::new(ends.data, Pipe::Data, data.sealer, pair),
    ))
}

#[cfg(test)]
#[path = "leg_tests.rs"]
mod tests;

#[cfg(all(test, debug_assertions))]
#[path = "leg_credit_tests.rs"]
mod credit_tests;
