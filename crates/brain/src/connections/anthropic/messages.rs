//! One admitted Messages attempt, with bounded waits and no retry or connection fallback.

use std::ops::ControlFlow;
use std::time::Duration;

use kumochi_protocol::Secret;
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::connections::http_client::MAX_BODY_BYTES;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{ModelClient, ModelConn, Upload};
use crate::connections::model::sse::Progress;
use crate::connections::model::wire::{Body, Head, Media};
use crate::connections::model::{ERROR_BODY_DEADLINE, Events, IDLE, SEND_DEADLINE};

use super::error::{Failure, ProviderFailure};
use super::key::AnthropicKey;
pub use super::request::MessagesBody;
use super::sse_map::{Reader, StopReason};
use super::tls::{AnthropicConnect, HOST};

/// A consumer must take each read's events within this time.
pub const BACKPRESSURE: Duration = Duration::from_secs(60);
// shortcut: ten minutes without content, replace after approved live gap measurements
const CONTENT_IDLE: Duration = Duration::from_secs(600);

struct Attempt<Io, L> {
    conn: Option<ModelConn<Io>>,
    lease: Option<L>,
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
pub async fn stream<C: AnthropicConnect, L>(
    client: &ModelClient<C>,
    body: &MessagesBody,
    key: &Secret<AnthropicKey>,
    ask: impl Future<Output = Option<L>>,
    run_budget: &Budget,
    live_budget: &Budget,
    mut out: impl Events,
) -> Result<StopReason, Failure> {
    let request = request(key.expose().as_str(), body.bytes())?;
    let lease = ask.await.ok_or(Failure::Rebuild)?;
    if !run_budget.reserve() || !live_budget.reserve() {
        return Err(Failure::Spent);
    }
    let mut attempt = Attempt {
        conn: None,
        lease: Some(lease),
    };
    timeout(SEND_DEADLINE, async {
        let io = client
            .connect
            .connect()
            .await
            .map_err(|_| Failure::Transport)?;
        let conn = attempt.conn.insert(ModelConn::new(io));
        match conn.upload(request.expose()).await? {
            Upload::Flushed => Ok(()),
            Upload::Refused(head) => Err(Failure::Provider(refusal(conn, head).await?)),
        }
    })
    .await
    .map_err(|_| Failure::Transport)??;
    drop(attempt.lease.take());
    let conn = attempt
        .conn
        .as_mut()
        .expect("a flushed upload owns its connection");
    let mut content_by = Instant::now() + CONTENT_IDLE;
    let head = timeout_at(content_by, conn.head(IDLE))
        .await
        .map_err(|_| Failure::Transport)??;
    if head.status != 200 {
        return Err(Failure::Provider(refusal(conn, head).await?));
    }
    if head.media != Media::EventStream {
        return Err(Failure::Protocol);
    }
    let mut body = Body::new(head.framing);
    let mut reader = Reader::default();
    loop {
        let length = timeout_at(content_by, conn.next(&mut body, IDLE))
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
            content_by = Instant::now() + CONTENT_IDLE;
        }
        let capacity_by = Instant::now() + BACKPRESSURE;
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
    if key.is_empty() || key.len() > 2048 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
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

async fn refusal<Io>(conn: &mut ModelConn<Io>, head: Head) -> Result<ProviderFailure, Failure>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if head.status < 400 {
        return Err(Failure::Protocol);
    }
    timeout(ERROR_BODY_DEADLINE, async {
        let mut body = Body::new(head.framing);
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(length) = conn.next(&mut body, IDLE).await? {
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
