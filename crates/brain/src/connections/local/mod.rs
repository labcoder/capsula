//! A model server on this machine that speaks `OpenAI`'s Chat Completions: one streamed text request
//! to a loopback port, with no credential, over the model transport the other routes share. No
//! user can reach it while [`LOCAL_CHAT`] is off. See docs/development/engines.md, "The local
//! Chat Completions transport".
//!
//! [`LOCAL_CHAT`]: kumochi_protocol::constants::LOCAL_CHAT

pub mod endpoint;
pub(super) mod reader;
pub mod request;

#[cfg(test)]
mod tests;

use std::ops::ControlFlow;
use std::time::Duration;

use kumochi_protocol::message::{ErrorKey, UserError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until, timeout_at};

use self::endpoint::LoopbackEndpoint;
use self::reader::Reader;
use self::request::ChatBody;
use crate::config::tuning::ModelTuning;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{ModelConn, Upload};
use crate::connections::model::wire::{Body, Media};
use crate::connections::model::{Lease, error};

/// From asking for the grant to the stream's last byte; trickled bytes never extend it [guess].
pub const ROUND_DEADLINE: Duration = Duration::from_secs(120);
/// The longest wait for the local server to accept the connection [guess].
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(3);

/// Why a local round didn't complete. It carries no server text, only a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// Local connections are switched off in this build; nothing was sent.
    #[error("local connections are switched off")]
    Dormant,
    /// The configured endpoint isn't a loopback address and port; nothing was sent.
    #[error("the local endpoint isn't a loopback address and port")]
    Endpoint,
    /// The request is empty or too large; nothing was sent.
    #[error("the request body was refused")]
    Request,
    /// The coordinator answered Rebuild, or is gone; nothing was sent.
    #[error("the request must be built again")]
    Rebuild,
    /// The live-check request budget is spent; nothing was sent.
    #[error("the request budget is spent")]
    Spent,
    /// The connection failed or stalled. Once a byte was written the request may have arrived,
    /// so it's never sent again on its own.
    #[error("the local connection failed")]
    Transport,
    /// The send's lease stopped admitting writes after one may have written.
    #[error("the send's lease stopped its writes")]
    Withheld,
    /// The server asks for authentication, which a local connection never sends.
    #[error("the local server asks for authentication")]
    AuthRequired,
    /// The server refused the request with this status.
    #[error("the local server refused the request (status {status})")]
    Refused {
        /// The HTTP status.
        status: u16,
    },
    /// The server reported an error inside the stream.
    #[error("the local server reported an error")]
    Provider,
    /// Bytes that aren't a text-only Chat Completions stream, a redirect, or a cap passed.
    #[error("the local stream couldn't be read")]
    Protocol,
    /// The stream ended before `[DONE]`, or the body before its framing did.
    #[error("the local stream ended early")]
    Truncated,
    /// The model stopped at its length limit.
    #[error("the local answer ended incomplete")]
    Incomplete,
    /// The text wasn't taken in time.
    #[error("the local answer wasn't taken in time")]
    Backpressure,
    /// The round passed [`ROUND_DEADLINE`].
    #[error("the local round ran past its deadline")]
    Deadline,
}

impl Failure {
    /// What the user is told. No local failure offers a sign-in, a cloud recovery or a Retry,
    /// which would resend under whatever route the thread has by then.
    #[must_use]
    pub fn user_error(self) -> UserError {
        match self {
            Self::Dormant => ErrorKey::LocalOff,
            Self::Request => ErrorKey::RequestRefused,
            Self::Spent => ErrorKey::LiveBudgetSpent,
            Self::AuthRequired | Self::Refused { .. } | Self::Provider => ErrorKey::LocalRefused,
            Self::Incomplete => ErrorKey::LocalIncomplete,
            Self::Protocol => ErrorKey::LocalUnreadable,
            Self::Endpoint
            | Self::Rebuild
            | Self::Transport
            | Self::Withheld
            | Self::Truncated
            | Self::Backpressure
            | Self::Deadline => ErrorKey::LocalUnfinished,
        }
        .into()
    }
}

impl From<error::Failure> for Failure {
    fn from(failure: error::Failure) -> Self {
        match failure {
            error::Failure::Rebuild => Self::Rebuild,
            error::Failure::Withheld => Self::Withheld,
            error::Failure::Transport => Self::Transport,
            error::Failure::Truncated => Self::Truncated,
            _ => Self::Protocol,
        }
    }
}

/// Where a round's text goes: each piece in order, then, after each read, a wait until that
/// read's text is taken, which can stop the round.
pub trait Sink {
    /// The next piece of text.
    fn text(&mut self, text: &str);

    /// Waits until the text so far is taken, handing nothing on once `by` has passed; `Break`
    /// ends the round with [`Failure::Protocol`].
    fn taken(&mut self, by: Instant) -> impl Future<Output = ControlFlow<()>> + Send;
}

impl<F: FnMut(&str)> Sink for F {
    fn text(&mut self, text: &str) {
        self(text);
    }

    fn taken(&mut self, _: Instant) -> impl Future<Output = ControlFlow<()>> + Send {
        std::future::ready(ControlFlow::Continue(()))
    }
}

/// One send's state. Fields drop in declaration order, so the connection closes before the lease
/// settles.
struct Attempt<L: Lease> {
    conn: Option<ModelConn<TcpStream>>,
    lease: Option<L>,
}

impl<L: Lease> Drop for Attempt<L> {
    /// A lease still held here is told of a write once one was attempted; otherwise it settles
    /// unsent.
    fn drop(&mut self) {
        if let Some(lease) = &mut self.lease
            && self.conn.as_ref().is_some_and(|conn| conn.may_have_written)
        {
            lease.wrote();
        }
    }
}

/// Sends `body` to `endpoint` once `ask` grants a lease, and hands its text to `out`. One
/// absolute [`ROUND_DEADLINE`] covers the grant, the connection, the upload, the response and
/// every wait on `out`. Dropping the future closes the connection before the lease settles;
/// nothing is retried or resent.
///
/// # Errors
/// Why the round didn't complete; see [`Failure`]. Text handed out before a failure stays
/// handed out, and never counts as a completed answer.
pub async fn stream<L: Lease>(
    endpoint: LoopbackEndpoint,
    body: &ChatBody,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    out: impl Sink,
) -> Result<(), Failure> {
    stream_by(endpoint, body, ask, budget, out, ROUND_DEADLINE).await
}

async fn stream_by<L: Lease>(
    endpoint: LoopbackEndpoint,
    body: &ChatBody,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    out: impl Sink,
    deadline: Duration,
) -> Result<(), Failure> {
    let by = Instant::now() + deadline;
    // The deadline is polled first, so nothing ready after it passes can still run.
    tokio::select! {
        biased;
        () = sleep_until(by) => Err(Failure::Deadline),
        ended = round(endpoint, body, ask, budget, out, by) => ended,
    }
}

async fn round<L: Lease>(
    endpoint: LoopbackEndpoint,
    body: &ChatBody,
    ask: impl Future<Output = Option<L>>,
    budget: &Budget,
    mut out: impl Sink,
    by: Instant,
) -> Result<(), Failure> {
    let request = body.request(endpoint);
    let lease = ask.await.ok_or(Failure::Rebuild)?;
    if Instant::now() >= by {
        return Err(Failure::Deadline);
    }
    if !budget.reserve() {
        return Err(Failure::Spent);
    }
    let tuning = ModelTuning::default();
    let mut attempt = Attempt {
        conn: None,
        lease: Some(lease),
    };
    let send_by = (Instant::now() + tuning.send).min(by);
    timeout_at(send_by, async {
        let connect_by = Instant::now() + CONNECT_DEADLINE;
        let io = timeout_at(connect_by, endpoint.connect())
            .await
            .map_err(|_| Failure::Transport)??;
        // A connection that completes as its cap passes is still too late.
        if Instant::now() >= connect_by {
            return Err(Failure::Transport);
        }
        let conn = attempt.conn.insert(ModelConn::new(io));
        match upload(conn, &request, attempt.lease.as_ref(), send_by).await? {
            Upload::Flushed => Ok(()),
            Upload::Refused(head) => Err(refused(head.status)),
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
        .expect("a flushed upload holds its connection");
    let head = conn.head(tuning.idle).await?;
    if head.status != 200 {
        return Err(refused(head.status));
    }
    if head.media != Media::EventStream {
        return Err(Failure::Protocol);
    }
    let mut body = Body::new(head.framing);
    let mut reader = Reader::default();
    while let Some(length) = conn.next(&mut body, tuning.idle).await? {
        let mut emitted = false;
        let fed = reader.feed(&conn.buf[..length], &mut |text| {
            emitted = true;
            out.text(text);
        });
        conn.buf.drain(..length);
        // Text parsed past the round's deadline is cut with it, even beside a parse failure.
        if Instant::now() >= by {
            return Err(Failure::Deadline);
        }
        // The sink already holds this read's text, so a failure in it needs no wait.
        fed?;
        if emitted {
            taken_within(&mut out, tuning.backpressure, by).await?;
        }
    }
    reader.finish()
}

/// Waits for `out` to take its text within `wait`, and never past the round's `deadline`; the
/// sink hands nothing on past the earlier cutoff, even when room opens on the same poll.
pub(crate) async fn taken_within(
    out: &mut impl Sink,
    wait: Duration,
    deadline: Instant,
) -> Result<(), Failure> {
    let by = (Instant::now() + wait).min(deadline);
    match timeout_at(by, out.taken(by)).await {
        Err(_) if by == deadline => Err(Failure::Deadline),
        Err(_) => Err(Failure::Backpressure),
        Ok(ControlFlow::Break(())) => Err(Failure::Protocol),
        Ok(ControlFlow::Continue(())) => Ok(()),
    }
}

/// Writes `request`, each write admitted by `lease` and only before `send_by`, which a socket
/// that turns writable late can't outrun. A write refused by the cutoff is `Transport`, never a
/// retryable `Rebuild`.
async fn upload<Io: AsyncRead + AsyncWrite + Unpin, L: Lease>(
    conn: &mut ModelConn<Io>,
    request: &[u8],
    lease: Option<&L>,
    send_by: Instant,
) -> Result<Upload, Failure> {
    let admit = || {
        (Instant::now() < send_by)
            .then(|| lease.and_then(Lease::admit))
            .flatten()
    };
    match conn.upload(request, admit).await {
        Err(error::Failure::Rebuild | error::Failure::Withheld) if Instant::now() >= send_by => {
            Err(Failure::Transport)
        }
        uploaded => uploaded.map_err(Failure::from),
    }
}

/// What a final head other than 200 means. Its body is never read: only the status counts.
// shortcut: a refusal head with no length, chunking or `Connection: close` fails the shared head
// parser as Protocol before its status is seen; check against each server's recording in 8b.
fn refused(status: u16) -> Failure {
    match status {
        401 | 403 => Failure::AuthRequired,
        400.. => Failure::Refused { status },
        _ => Failure::Protocol,
    }
}
