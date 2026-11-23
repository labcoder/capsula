//! One admitted Messages attempt, with bounded waits and no retry or connection fallback.

use std::ops::ControlFlow;
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::ANTHROPIC_KEY_BYTES;
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::config::tuning::ModelTuning;
use crate::connections::http_client::MAX_BODY_BYTES;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{ModelClient, ModelConn, Upload};
use crate::connections::model::sse::Progress;
use crate::connections::model::wire::{Body, Head, Media};
use crate::connections::model::{Events, Lease};

use super::error::{Failure, ProviderFailure};
use super::key::AnthropicKey;
pub use super::request::MessagesBody;
use super::sse_map::{Capture, Reader, StopReason};
use super::tls::{AnthropicConnect, HOST};

/// A consumer must take each read's events within this time.
pub const BACKPRESSURE: Duration = Duration::from_secs(60);
// shortcut: ten minutes without content, replace after approved live gap measurements
pub(crate) const CONTENT_IDLE: Duration = Duration::from_secs(600);

/// One send's state. Fields drop in declaration order, so the connection closes before the lease
/// settles.
struct Attempt<Io, L: Lease> {
    conn: Option<ModelConn<Io>>,
    lease: Option<L>,
}

impl<Io, L: Lease> Drop for Attempt<Io, L> {
    /// A lease still held here is told of a write once one was attempted on its connection;
    /// otherwise it settles unsent.
    fn drop(&mut self) {
        if let Some(lease) = &mut self.lease
            && self.conn.as_ref().is_some_and(|conn| conn.may_have_written)
        {
            lease.wrote();
        }
    }
}

/// Send one validated request using the caller's key and shared run/live budgets.
/// The connection closes before an unsettled lease drops, including cancellation.
/// No budget reservation is refunded and no attempt is retried automatically.
///
/// # Errors
/// Closed admission, budget, transport, protocol, provider or deadline failures.
///
/// ```compile_fail
/// use kumochi_brain::connections::{anthropic::tls::AnthropicConnect, model::conn::Tls};
/// fn anthropic<C: AnthropicConnect>() {}
/// anthropic::<Tls>();
/// ```
///
/// # Panics
/// Never: a flushed upload owns its connection.
#[expect(
    clippy::too_many_arguments,
    reason = "the capture choice comes from the stream's mapper, before the stream starts"
)]
pub async fn stream<C: AnthropicConnect, L: Lease>(
    client: &ModelClient<C>,
    body: &MessagesBody,
    key: &Secret<AnthropicKey>,
    ask: impl Future<Output = Option<L>>,
    run_budget: &Budget,
    live_budget: &Budget,
    capture: Capture,
    mut out: impl Events,
) -> Result<StopReason, Failure> {
    let request = request(key.expose().as_str(), body.bytes())?;
    let lease = ask.await.ok_or(Failure::Rebuild)?;
    // A picture goes only under the coordinator's grant for it, which each write rechecks.
    if body.pictured() && !lease.carries_picture() {
        return Err(Failure::Request);
    }
    if !run_budget.reserve() || !live_budget.reserve() {
        return Err(Failure::Spent);
    }
    let mut attempt = Attempt {
        conn: None,
        lease: Some(lease),
    };
    let tuning = client.tuning;
    timeout(tuning.send, async {
        let io = client
            .connect
            .connect()
            .await
            .map_err(|_| Failure::Transport)?;
        let lease = &attempt.lease;
        let conn = attempt.conn.insert(ModelConn::new(io));
        match conn
            .upload(request.expose(), || lease.as_ref().and_then(Lease::admit))
            .await?
        {
            Upload::Flushed => Ok(()),
            Upload::Refused(head) => Err(Failure::Provider(refusal(conn, head, tuning).await?)),
        }
    })
    .await
    .map_err(|_| Failure::Transport)??;
    if let Some(mut lease) = attempt.lease.take() {
        lease.wrote();
    }
    let conn = attempt
        .conn
        .as_mut()
        .expect("a flushed upload owns its connection");
    let mut content_by = Instant::now() + tuning.content_idle;
    let head = timeout_at(content_by, conn.head(tuning.idle))
        .await
        .map_err(|_| Failure::Transport)??;
    if head.status != 200 {
        return Err(Failure::Provider(refusal(conn, head, tuning).await?));
    }
    if head.media != Media::EventStream {
        return Err(Failure::Protocol);
    }
    let mut body = Body::new(head.framing);
    let mut reader = Reader::new(capture);
    loop {
        let length = timeout_at(content_by, conn.next(&mut body, tuning.idle))
            .await
            .map_err(|_| Failure::Transport)??;
        let Some(length) = length else {
            return reader.finish();
        };
        let mut content = false;
        let fed = reader.feed(&conn.buf[..length], &mut |event| {
            content = true;
            out.event(event);
        });
        conn.buf.drain(..length);
        if content {
            content_by = Instant::now() + tuning.content_idle;
        }
        let capacity_by = Instant::now() + tuning.backpressure;
        // shortcut: a stalled delivery can replace Exhausted/Refused with BackpressureTimeout; revisit in A3b.
        let taken = timeout_at(content_by.min(capacity_by), out.taken())
            .await
            .map_err(|_| {
                if content_by <= capacity_by {
                    Failure::Transport
                } else {
                    Failure::BackpressureTimeout
                }
            })?;
        if taken == ControlFlow::Break(()) {
            return Err(Failure::Protocol);
        }
        let progress = fed?;
        if progress == Progress::Completed {
            return reader.finish();
        }
    }
}

fn request(key: &str, body: &[u8]) -> Result<Secret<Zeroizing<Vec<u8>>>, Failure> {
    if !ANTHROPIC_KEY_BYTES.contains(&key.len()) || !key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(Failure::Credential);
    }
    let head = format!("POST /v1/messages HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer ");
    let length = body.len().to_string();
    let parts: [&[u8]; 6] = [
        head.as_bytes(),
        key.as_bytes(),
        b"\r\nanthropic-version: 2023-06-01\r\nContent-Type: application/json\r\n\
          Accept: text/event-stream\r\nAccept-Encoding: identity\r\n\
          Connection: close\r\nContent-Length: ",
        length.as_bytes(),
        b"\r\n\r\n",
        body,
    ];
    let mut bytes = Zeroizing::new(Vec::with_capacity(
        parts.iter().map(|part| part.len()).sum(),
    ));
    for part in parts {
        bytes.extend_from_slice(part);
    }
    Ok(Secret::new(bytes))
}

async fn refusal<Io>(
    conn: &mut ModelConn<Io>,
    head: Head,
    tuning: ModelTuning,
) -> Result<ProviderFailure, Failure>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if head.status < 400 {
        return Err(Failure::Protocol);
    }
    // The status alone decides a 401, so a cut body can't hide it; the body would only travel.
    if head.status == 401 {
        return ProviderFailure::answered(401, Secret::new(Zeroizing::new(Vec::new())));
    }
    timeout(tuning.error_body, async {
        let mut body = Body::new(head.framing);
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(length) = conn.next(&mut body, tuning.idle).await? {
            if length > MAX_BODY_BYTES - bytes.len() {
                return Err(Failure::Protocol);
            }
            bytes.extend_from_slice(&conn.buf[..length]);
            conn.buf.drain(..length);
        }
        ProviderFailure::answered(head.status, Secret::new(bytes))
    })
    .await
    .map_err(|_| Failure::Transport)?
}

#[cfg(test)]
mod tests;
