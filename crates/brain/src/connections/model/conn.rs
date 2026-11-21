//! The model connection: TLS over TCP to the one model host, a read buffer that bounds the
//! wire, and a pool of one idle connection.

use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker, ready};
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::error::Failure;
use super::sse::MAX_STREAM_BYTES;
use super::wire::{self, Body, Head, MAX_INTERIM_HEADS, Step};
use crate::connections::http_client::{self, HttpError};

const HOST: &str = "api.openai.com";
/// The most bytes one response may put on the wire: the stream's cap and its framing.
pub(super) const MAX_WIRE_BYTES: usize = MAX_STREAM_BYTES + 1024 * 1024;
const READ_BYTES: usize = 16 * 1024;
/// An idle connection older than this is closed rather than reused [guess].
pub(super) const POOL_IDLE: Duration = Duration::from_secs(50);

/// Opens a connection to the model host.
pub trait Connect: Send + Sync {
    /// The connection.
    type Io: AsyncRead + AsyncWrite + Unpin + Send;

    /// Connects, handshake included.
    fn connect(&self) -> impl Future<Output = io::Result<Self::Io>> + Send;
}

/// TLS to `api.openai.com:443` with the OS roots, offering only HTTP/1.1, with no early data.
pub struct Tls(TlsConnector);

impl Tls {
    /// The connector, with the account client's trust roots.
    ///
    /// # Errors
    /// As [`http_client::build`].
    pub fn build() -> Result<Self, HttpError> {
        let mut config = http_client::client_config(crate::platform::trust_roots())?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self(TlsConnector::from(Arc::new(config))))
    }
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tls")
    }
}

impl Connect for Tls {
    type Io = TlsStream<TcpStream>;

    async fn connect(&self) -> io::Result<Self::Io> {
        let tcp = TcpStream::connect((HOST, 443)).await?;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(HOST).map_err(io::Error::other)?;
        self.0.connect(name, tcp).await
    }
}

/// Model connections through `C`, keeping one idle connection for the next request.
pub struct ModelClient<C: Connect> {
    pub(in crate::connections) connect: C,
    idle: Mutex<Option<(C::Io, Instant)>>,
}

impl<C: Connect> ModelClient<C> {
    /// A client that connects through `connect`.
    pub fn new(connect: C) -> Self {
        Self {
            connect,
            idle: Mutex::new(None),
        }
    }

    /// The idle connection, when it's fresh and nothing has arrived on it, such as a close.
    pub(super) fn checkout(&self) -> Option<C::Io> {
        let (mut io, since) = self.lock().take()?;
        (since.elapsed() < POOL_IDLE && quiet(&mut io)).then_some(io)
    }

    /// Keeps `io`, which finished a response with nothing left over, for the next request.
    pub(super) fn put(&self, io: C::Io) {
        *self.lock() = Some((io, Instant::now()));
    }

    /// Pools `io` as if a response had just finished on it.
    #[cfg(test)]
    pub(crate) fn pool_for_tests(&self, io: C::Io) {
        self.put(io);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(C::Io, Instant)>> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<C: Connect> fmt::Debug for ModelClient<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ModelClient")
    }
}

/// Whether a read would wait: an idle connection that has seen bytes, an end or an error is stale.
fn quiet(io: &mut (impl AsyncRead + Unpin)) -> bool {
    let mut byte = [0];
    let mut buf = ReadBuf::new(&mut byte);
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(io).poll_read(&mut cx, &mut buf).is_pending()
}

/// How the upload ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connections) enum Upload {
    /// Every request byte was written and flushed.
    Flushed,
    /// A final head other than a success arrived first, so writing stopped.
    Refused(Head),
}

/// One request's use of a connection.
pub(in crate::connections) struct ModelConn<Io> {
    pub(super) io: Io,
    /// Response bytes read but not yet consumed.
    pub(in crate::connections) buf: Vec<u8>,
    wire: usize,
    /// Interim heads read so far.
    interim: usize,
    /// A success head that arrived while the request was still uploading.
    answer: Option<Head>,
    /// A write of the request was attempted, so bytes of it may have left, even if no write
    /// returned a count: TLS can write a record and then fail in the same call.
    pub(in crate::connections) may_have_written: bool,
}

impl<Io> fmt::Debug for ModelConn<Io> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelConn")
            .field("buffered", &self.buf.len())
            .field("wire", &self.wire)
            .finish_non_exhaustive()
    }
}

impl<Io: AsyncRead + AsyncWrite + Unpin> ModelConn<Io> {
    pub(in crate::connections) fn new(io: Io) -> Self {
        Self {
            io,
            buf: Vec::new(),
            wire: 0,
            interim: 0,
            answer: None,
            may_have_written: false,
        }
    }

    /// Writes and flushes `request`. Interim heads that arrive meanwhile are skipped and a
    /// success head is kept for [`Self::head`], so writing goes on; any other final head stops it
    /// for good. `admit` is asked before each poll of the writer, a write or the flush, and its
    /// admission is held for that one poll. A refusal ends the upload with no further poll: as
    /// [`Failure::Rebuild`] before any poll was admitted, with nothing written, and as
    /// [`Failure::Withheld`] after one was, since an admitted poll may have written even when it
    /// returned `Pending`, a partial count or an error.
    pub(in crate::connections) async fn upload<W>(
        &mut self,
        request: &[u8],
        admit: impl Fn() -> Option<W>,
    ) -> Result<Upload, Failure> {
        let mut written = 0;
        poll_fn(|cx| {
            while self.answer.is_none() {
                let mut chunk = [0; 1024];
                let mut early = ReadBuf::new(&mut chunk);
                match Pin::new(&mut self.io).poll_read(cx, &mut early) {
                    Poll::Ready(Ok(())) if !early.filled().is_empty() => {
                        self.buf.extend_from_slice(early.filled());
                        self.count(early.filled().len())?;
                        match self.final_head()? {
                            Some(head) if (200..300).contains(&head.status) => {
                                self.answer = Some(head);
                            }
                            Some(head) => return Poll::Ready(Ok(Upload::Refused(head))),
                            None => {}
                        }
                    }
                    Poll::Ready(_) => return Poll::Ready(Err(Failure::Transport)),
                    Poll::Pending => break,
                }
            }
            loop {
                let Some(_admitted) = admit() else {
                    return Poll::Ready(Err(if self.may_have_written {
                        Failure::Withheld
                    } else {
                        Failure::Rebuild
                    }));
                };
                self.may_have_written = true;
                if written == request.len() {
                    return Pin::new(&mut self.io).poll_flush(cx).map(|flushed| {
                        flushed
                            .map(|()| Upload::Flushed)
                            .map_err(|_| Failure::Transport)
                    });
                }
                match ready!(Pin::new(&mut self.io).poll_write(cx, &request[written..])) {
                    Ok(0) | Err(_) => return Poll::Ready(Err(Failure::Transport)),
                    Ok(count) => written += count,
                }
            }
        })
        .await
    }

    /// The final head, past any interim ones; each read waits at most `idle`.
    pub(in crate::connections) async fn head(&mut self, idle: Duration) -> Result<Head, Failure> {
        if let Some(head) = self.answer.take() {
            return Ok(head);
        }
        loop {
            if let Some(head) = self.final_head()? {
                return Ok(head);
            }
            if self.fill(idle).await? {
                return Err(Failure::Truncated);
            }
        }
    }

    /// The final head at the front of `buf`, past the interim ones, or `None` while it's
    /// incomplete.
    fn final_head(&mut self) -> Result<Option<Head>, Failure> {
        while let Some((head, length)) = wire::head(&self.buf)? {
            self.buf.drain(..length);
            if head.status >= 200 {
                return Ok(Some(head));
            }
            self.interim += 1;
            if self.interim > MAX_INTERIM_HEADS {
                return Err(Failure::Protocol);
            }
        }
        Ok(None)
    }

    /// The length of the body's next data at the front of `buf`, or `None` at its end.
    pub(in crate::connections) async fn next(
        &mut self,
        body: &mut Body,
        idle: Duration,
    ) -> Result<Option<usize>, Failure> {
        loop {
            match body.step(&self.buf)? {
                Step::Data(length) => return Ok(Some(length)),
                Step::Framing(length) => {
                    self.buf.drain(..length);
                }
                Step::More => {
                    if self.fill(idle).await? {
                        body.end()?;
                    }
                }
                Step::Done => return Ok(None),
            }
        }
    }

    /// Reads more within `idle`; `true` when the connection has ended.
    async fn fill(&mut self, idle: Duration) -> Result<bool, Failure> {
        let start = self.buf.len();
        self.buf.resize(start + READ_BYTES, 0);
        let read = timeout(idle, self.io.read(&mut self.buf[start..])).await;
        let read = read.ok().and_then(Result::ok);
        self.buf.truncate(start + read.unwrap_or(0));
        let read = read.ok_or(Failure::Transport)?;
        self.count(read)?;
        Ok(read == 0)
    }

    /// Counts `read` more bytes against the response's wire cap.
    fn count(&mut self, read: usize) -> Result<(), Failure> {
        self.wire = self
            .wire
            .checked_add(read)
            .filter(|wire| *wire <= MAX_WIRE_BYTES)
            .ok_or(Failure::Protocol)?;
        Ok(())
    }
}
