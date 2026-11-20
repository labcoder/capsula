//! Model requests on the signed-in account's plan: the transport that writes the request itself
//! under the coordinator's send lease, the Responses event stream and what its failures mean.
//! See docs/development/responses.md.

pub mod budget;
pub mod conn;
pub mod error;
pub(in crate::connections) mod framer;
pub mod sse;
#[cfg(all(windows, debug_assertions))]
pub(crate) mod text_observation;
pub mod wire;

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::ControlFlow;
use std::time::Duration;

use kumochi_protocol::Secret;
use serde_json::Value;
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use self::budget::Budget;
use self::conn::{Connect, ModelClient, ModelConn, Upload};
use self::error::{Failure, ProviderError};
use self::sse::{Progress, Reader, SseEvent};
use self::wire::{Body, Head, Media};
use crate::connections::http_client::MAX_BODY_BYTES;

/// The largest request body [guess].
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// From the grant to the request's last byte reaching the kernel, or to an early refusal being
/// read. It's never reset, and it stays under the coordinator's 15 s send deadline [guess].
pub const SEND_DEADLINE: Duration = Duration::from_secs(10);
/// The longest wait for the next bytes once the request is sent.
pub const IDLE: Duration = Duration::from_secs(60);
/// The longest read of an error body once the request is sent [guess].
pub const ERROR_BODY_DEADLINE: Duration = Duration::from_secs(10);
/// How long a completed stream may take to end its framing; after that its connection is closed
/// instead of reused [guess].
const DRAIN: Duration = Duration::from_secs(1);
/// How much of a body with no Content-Type is checked for the start of an event stream.
const SNIFF_BYTES: usize = 128;

/// A Responses request's JSON, checked before any credential or connection is touched.
pub struct ResponsesBody(Secret<Zeroizing<Vec<u8>>>);

impl ResponsesBody {
    /// The body for `request`.
    ///
    /// # Errors
    /// [`Failure::Request`] unless it sets `"store": false` and `"stream": true` and fits
    /// [`MAX_REQUEST_BYTES`].
    pub fn new(request: &Value) -> Result<Self, Failure> {
        if request["store"] != false || request["stream"] != true {
            return Err(Failure::Request);
        }
        let len = serialized_len(request);
        if len > MAX_REQUEST_BYTES {
            return Err(Failure::Request);
        }
        // Sized first, so the buffer never grows and leaves a copy behind.
        let mut bytes = Zeroizing::new(Vec::with_capacity(len));
        serde_json::to_writer(&mut *bytes, request).map_err(|_| Failure::Request)?;
        Ok(Self(Secret::new(bytes)))
    }
}

/// `value`'s length as JSON, counted without building it; `usize::MAX` if it can't be
/// serialized.
pub(crate) fn serialized_len(value: &impl serde::Serialize) -> usize {
    struct Counted(usize);

    impl std::io::Write for Counted {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counted = Counted(0);
    serde_json::to_writer(&mut counted, value).map_or(usize::MAX, |()| counted.0)
}

impl fmt::Debug for ResponsesBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponsesBody")
            .field("bytes", &self.0.expose().len())
            .finish()
    }
}

/// Where a stream's events go: each one in order, then, after each read, a wait until that
/// read's events are taken, which can stop the stream.
pub trait Events {
    /// The next event.
    fn event(&mut self, event: &SseEvent);

    /// Waits until the events so far are taken; `Break` stops reading, and the stream ends
    /// with [`Failure::Protocol`].
    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send;
}

impl<F: FnMut(&SseEvent)> Events for F {
    fn event(&mut self, event: &SseEvent) {
        self(event);
    }

    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send {
        std::future::ready(ControlFlow::Continue(()))
    }
}

/// A granted send's lease. The transport asks it right before the request's first write and
/// tells it once a write was attempted; a lease never told settles as unsent.
pub trait Lease {
    /// Whether the request may still be written: `false` once the picture it carries lost its
    /// grant, and then nothing is.
    fn may_write(&self) -> bool;

    /// A write of the request was attempted, so bytes of it may have left.
    fn wrote(&mut self);
}

/// A live check's send, which no coordinator leases.
impl Lease for () {
    fn may_write(&self) -> bool {
        true
    }

    fn wrote(&mut self) {}
}

/// One send's state. Fields drop in declaration order, so the connection always closes before
/// the lease settles.
struct Attempt<Io, L: Lease> {
    conn: Option<ModelConn<Io>>,
    lease: Option<L>,
    #[cfg(all(windows, debug_assertions))]
    stamp: Option<text_observation::Stamp>,
}

impl<Io, L: Lease> Drop for Attempt<Io, L> {
    /// A lease still held here is told of a write once one was attempted on its connection, as
    /// any attempted write may have sent bytes; otherwise it settles unsent.
    fn drop(&mut self) {
        if let Some(lease) = &mut self.lease
            && self.conn.as_ref().is_some_and(|conn| conn.may_have_written)
        {
            lease.wrote();
        }
        #[cfg(all(windows, debug_assertions))]
        if let Some(stamp) = self.stamp.take() {
            if let Some(conn) = self.conn.take() {
                stamp.close(conn);
            } else {
                stamp.ended(text_observation::End::NoIo);
            }
        }
    }
}

/// Sends `body` once `ask` grants a lease, and hands each event to `out` until
/// `response.completed`, which it hands out too, waiting after each read until `out` took them.
/// `ask` is the coordinator's gate for this one attempt: `None` means Rebuild, and the lease
/// settles when it drops. A retry is another call, so it asks again.
///
/// The lease settles once the request's last byte is flushed to the kernel, or once the
/// connection holding it is closed, and it's told [`Lease::wrote`] first once a write of the
/// request was attempted. A lease that may no longer write ends the send as
/// [`Failure::Rebuild`], with nothing written. Nothing is retried after a byte is written; an idle
/// connection that turns out stale before the write is replaced under the same lease.
///
/// # Errors
/// Why the request didn't complete; see [`Failure`].
///
/// # Panics
/// Never: a flushed upload always holds its connection.
pub async fn stream<C: Connect, L: Lease>(
    client: &ModelClient<C>,
    body: &ResponsesBody,
    bearer: &Secret<Zeroizing<String>>,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    out: impl Events,
) -> Result<(), Failure> {
    send(
        client,
        body,
        bearer,
        ask,
        budget,
        out,
        #[cfg(all(windows, debug_assertions))]
        None,
    )
    .await
}

#[cfg(all(windows, debug_assertions))]
pub(crate) async fn stream_observed<C: Connect, L: Lease>(
    client: &ModelClient<C>,
    body: &ResponsesBody,
    bearer: &Secret<Zeroizing<String>>,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    out: impl Events,
    stamp: Option<text_observation::Stamp>,
) -> Result<(), Failure> {
    send(client, body, bearer, ask, budget, out, stamp).await
}

async fn send<C: Connect, L: Lease>(
    client: &ModelClient<C>,
    body: &ResponsesBody,
    bearer: &Secret<Zeroizing<String>>,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    mut out: impl Events,
    #[cfg(all(windows, debug_assertions))] stamp: Option<text_observation::Stamp>,
) -> Result<(), Failure> {
    let request = wire::request(bearer.expose(), body.0.expose())?;
    let lease = ask.await.ok_or(Failure::Rebuild)?;
    let deadline = Instant::now() + SEND_DEADLINE;
    let mut attempt = Attempt {
        conn: None,
        lease: Some(lease),
        #[cfg(all(windows, debug_assertions))]
        stamp,
    };
    if !budget.reserve() {
        return Err(Failure::Spent);
    }
    #[cfg(all(windows, debug_assertions))]
    if let Some(stamp) = &attempt.stamp {
        stamp.reserved();
    }
    let upload = timeout_at(deadline, async {
        let io = match client.checkout() {
            Some(io) => io,
            None => client
                .connect
                .connect()
                .await
                .map_err(|_| Failure::Transport)?,
        };
        if !attempt.lease.as_ref().is_some_and(Lease::may_write) {
            return Err(Failure::Rebuild);
        }
        let conn = attempt.conn.insert(ModelConn::new(io));
        match conn.upload(&request).await? {
            Upload::Flushed => Ok(()),
            Upload::Refused(head) => Err(Failure::Provider(refusal(conn, head).await?)),
        }
    });
    upload.await.map_err(|_| Failure::Transport)??;
    if let Some(mut lease) = attempt.lease.take() {
        lease.wrote();
    }
    let conn = attempt
        .conn
        .as_mut()
        .expect("an upload that flushed holds its connection");
    let head = conn.head(IDLE).await?;
    if !(200..300).contains(&head.status) {
        let refused = timeout(ERROR_BODY_DEADLINE, refusal(conn, head)).await;
        return Err(Failure::Provider(refused.map_err(|_| Failure::Transport)??));
    }
    let mut events = Reader::default();
    let mut body = Body::new(head.framing);
    let mut progress = match head.media {
        Media::EventStream => Progress::More,
        Media::Absent => {
            let fed = events.feed(&sniff(conn, &mut body).await?, &mut |event| {
                out.event(event);
            })?;
            if out.taken().await.is_break() {
                return Err(Failure::Protocol);
            }
            fed
        }
        Media::Other => return Err(Failure::Protocol),
    };
    while progress == Progress::More {
        let length = conn
            .next(&mut body, IDLE)
            .await?
            .ok_or(Failure::Truncated)?;
        let fed = events.feed(&conn.buf[..length], &mut |event| out.event(event));
        conn.buf.drain(..length);
        progress = fed?;
        if out.taken().await.is_break() {
            return Err(Failure::Protocol);
        }
    }
    // `response.completed` isn't the body's end: reuse needs the framing read to its end.
    let drained = timeout(DRAIN, async {
        while let Some(length) = conn.next(&mut body, DRAIN).await? {
            conn.buf.drain(..length);
        }
        Ok::<_, Failure>(())
    })
    .await;
    if matches!(drained, Ok(Ok(())))
        && !head.close
        && conn.buf.is_empty()
        && let Some(conn) = attempt.conn.take()
    {
        client.put(conn.io);
        #[cfg(all(windows, debug_assertions))]
        if let Some(stamp) = attempt.stamp.take() {
            stamp.ended(text_observation::End::Pooled);
        }
    }
    Ok(())
}

/// The first data of a body whose head names no type, once its first 128 bytes show an event
/// stream: blank space, then `data:`, `event:` or a `:` comment.
async fn sniff<Io>(conn: &mut ModelConn<Io>, body: &mut Body) -> Result<Vec<u8>, Failure>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    const FIELDS: [&[u8]; 3] = [b"data:", b"event:", b":"];
    let mut prefix = Vec::new();
    loop {
        let window = &prefix[..prefix.len().min(SNIFF_BYTES)];
        let start = window.trim_ascii_start();
        if FIELDS.iter().any(|field| start.starts_with(field)) {
            return Ok(prefix);
        }
        if window.len() == SNIFF_BYTES || !FIELDS.iter().any(|field| field.starts_with(start)) {
            return Err(Failure::Protocol);
        }
        let length = conn.next(body, IDLE).await?.ok_or(Failure::Protocol)?;
        prefix.extend_from_slice(&conn.buf[..length]);
        conn.buf.drain(..length);
    }
}

/// A refusal's bounded body, read to the end of its framing.
async fn refusal<Io>(conn: &mut ModelConn<Io>, head: Head) -> Result<ProviderError, Failure>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if head.status < 400 {
        return Err(Failure::Protocol);
    }
    let mut body = Body::new(head.framing);
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(length) = conn.next(&mut body, IDLE).await? {
        if length > MAX_BODY_BYTES - bytes.len() {
            return Err(Failure::Protocol);
        }
        bytes.extend_from_slice(&conn.buf[..length]);
        conn.buf.drain(..length);
    }
    Ok(ProviderError::answered(head.status, &bytes))
}
