//! The exchange, offline: a scripted server on in-memory pipes, polled by hand so each step's
//! order is exact. A valve under the client's connection holds or fails its writes and flushes,
//! and records how many leases had settled at each flush and at the close.

use std::future::Future;
use std::io;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker, ready};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::RunId;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};
use zeroize::Zeroizing;

use super::budget::Budget;
use super::conn::{Connect, MAX_WIRE_BYTES, ModelClient, POOL_IDLE};
use super::error::{Code, Failure, Param, ProviderError, RejectionOrigin};
use super::sse::SseEvent;
use super::{Events, ResponsesBody, stream, wire};
use crate::connections::http_client::MAX_BODY_BYTES;
use crate::coordinator::{PictureGrant, PictureWrite};
use crate::test_support::Captured;

/// The coordinator's side, as the tests play it: an ask names its attempt, a grant carries a
/// lease, and dropping the lease reports `Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SendKey {
    run: RunId,
    round: u32,
    attempt: u64,
}

#[derive(Debug)]
struct SendReady {
    key: SendKey,
    answer: oneshot::Sender<Admit>,
}

#[derive(Debug)]
enum Admit {
    Grant(SendLease),
    Rebuild,
}

#[derive(Debug)]
struct SendLease {
    settled: Settled,
    picture: PictureGrant,
    to: mpsc::UnboundedSender<Settled>,
}

impl super::Lease for SendLease {
    type Write = PictureWrite;

    fn admit(&self) -> Option<PictureWrite> {
        self.picture.reserve_write()
    }

    fn wrote(&mut self) {
        self.settled.unsent = false;
    }
}

impl Drop for SendLease {
    fn drop(&mut self) {
        self.to.send(self.settled).ok();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settled {
    key: SendKey,
    send: u64,
    /// Told that no write of the request was attempted.
    unsent: bool,
}

/// The ask the transport awaits: `None` for Rebuild, a closed gate or a dropped answer.
async fn ask(gate: &mpsc::Sender<SendReady>, key: SendKey) -> Option<SendLease> {
    let (answer, granted) = oneshot::channel();
    gate.send(SendReady { key, answer }).await.ok()?;
    match granted.await.ok()? {
        Admit::Grant(lease) => Some(lease),
        Admit::Rebuild => None,
    }
}

const BEARER: &str = "PLANTED-BEARER";
const TEXT: &[u8] = include_bytes!("../../../fixtures/responses/text.sse");
const UNSUPPORTED: &[u8] =
    include_bytes!("../../../fixtures/responses/subscription_sharing_unsupported_capability.json");
const DETAIL: &[u8] = include_bytes!("../../../fixtures/responses/detail.json");

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the valve does to the client's writes.
#[derive(Debug, Default)]
struct Valve {
    /// Writes and flushes wait.
    hold: bool,
    /// Flushes wait; writes pass.
    hold_flush: bool,
    /// Writes fail once this many bytes have passed.
    fail_after: Option<usize>,
    /// The first write passes this many bytes to the peer and still fails, as TLS can when it
    /// writes a record and then fails on the next.
    pass_then_fail: Option<usize>,
    /// What the peer has already sent when the connection opens.
    greeting: Option<Vec<u8>>,
    written: usize,
    /// Closed the first time the client polls a read.
    revoke_on_read: Option<PictureGrant>,
    /// Write polls the client made.
    write_polls: usize,
}

/// Leases settled when each flush completed and when each connection closed.
#[derive(Debug, Default)]
struct Seen {
    flushes: Vec<usize>,
    closes: Vec<usize>,
    #[cfg(all(windows, debug_assertions))]
    closed_at: Vec<u64>,
}

#[derive(Clone)]
struct Shared {
    valve: Arc<Mutex<Valve>>,
    seen: Arc<Mutex<Seen>>,
    settled: Arc<Mutex<mpsc::UnboundedReceiver<Settled>>>,
}

impl Shared {
    fn settled_count(&self) -> usize {
        lock(&self.settled).len()
    }
}

/// The client's end of a pipe, under the valve.
struct Gated {
    io: DuplexStream,
    shared: Shared,
}

impl AsyncRead for Gated {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(picture) = lock(&self.shared.valve).revoke_on_read.take() {
            picture.close();
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Gated {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut valve = lock(&this.shared.valve);
        valve.write_polls += 1;
        if valve.hold {
            return Poll::Pending;
        }
        if let Some(passed) = valve.pass_then_fail.take() {
            let mut cx = Context::from_waker(Waker::noop());
            let sent = Pin::new(&mut this.io).poll_write(&mut cx, &buf[..passed]);
            assert!(matches!(sent, Poll::Ready(Ok(count)) if count == passed));
            valve.written += passed;
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        let buf = match valve.fail_after {
            Some(limit) if valve.written >= limit => {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            Some(limit) => &buf[..buf.len().min(limit - valve.written)],
            None => buf,
        };
        let written = ready!(Pin::new(&mut this.io).poll_write(cx, buf))?;
        valve.written += written;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        {
            let valve = lock(&this.shared.valve);
            if valve.hold || valve.hold_flush {
                return Poll::Pending;
            }
        }
        ready!(Pin::new(&mut this.io).poll_flush(cx))?;
        let settled = this.shared.settled_count();
        lock(&this.shared.seen).flushes.push(settled);
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl Drop for Gated {
    fn drop(&mut self) {
        let settled = self.shared.settled_count();
        let mut seen = lock(&self.shared.seen);
        seen.closes.push(settled);
        #[cfg(all(windows, debug_assertions))]
        seen.closed_at.push(crate::platform::qpc_now().unwrap());
    }
}

/// Opens pipes once `open` says so, handing each server end to the test.
struct Pipes {
    capacity: usize,
    open: watch::Receiver<bool>,
    servers: mpsc::UnboundedSender<DuplexStream>,
    shared: Shared,
}

impl Connect for Pipes {
    type Io = Gated;

    async fn connect(&self) -> io::Result<Gated> {
        let mut open = self.open.clone();
        if open.wait_for(|open| *open).await.is_err() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let (io, mut server) = tokio::io::duplex(self.capacity);
        if let Some(greeting) = lock(&self.shared.valve).greeting.take() {
            reply(&mut server, &greeting);
        }
        self.servers.send(server).map_err(io::Error::other)?;
        Ok(Gated {
            io,
            shared: self.shared.clone(),
        })
    }
}

type Call<'a> = Pin<Box<dyn Future<Output = (Result<(), Failure>, Vec<String>)> + 'a>>;

struct Harness<C: Connect = Pipes> {
    client: ModelClient<C>,
    servers: Mutex<mpsc::UnboundedReceiver<DuplexStream>>,
    gate: mpsc::Sender<SendReady>,
    asks: Mutex<mpsc::Receiver<SendReady>>,
    settled: mpsc::UnboundedSender<Settled>,
    open: watch::Sender<bool>,
    shared: Shared,
    budget: Budget,
    bearer: Secret<Zeroizing<String>>,
}

fn key(attempt: u64) -> SendKey {
    SendKey {
        run: RunId(7),
        round: 1,
        attempt,
    }
}

impl Harness {
    fn new(capacity: usize) -> Self {
        Self::budgeted(capacity, None)
    }

    fn budgeted(capacity: usize, limit: Option<u32>) -> Self {
        Self::with(capacity, limit, |pipes| pipes)
    }
}

impl<C: Connect> Harness<C> {
    fn with(capacity: usize, limit: Option<u32>, wrap: impl FnOnce(Pipes) -> C) -> Self {
        let (servers_tx, servers) = mpsc::unbounded_channel();
        let (gate, asks) = mpsc::channel(4);
        let (settled, settled_rx) = mpsc::unbounded_channel();
        let (open, open_rx) = watch::channel(true);
        let shared = Shared {
            valve: Arc::default(),
            seen: Arc::default(),
            settled: Arc::new(Mutex::new(settled_rx)),
        };
        Self {
            client: ModelClient::new(wrap(Pipes {
                capacity,
                open: open_rx,
                servers: servers_tx,
                shared: shared.clone(),
            })),
            servers: Mutex::new(servers),
            gate,
            asks: Mutex::new(asks),
            settled,
            open,
            shared,
            budget: Budget::limited(limit),
            bearer: Secret::new(Zeroizing::new(BEARER.to_owned())),
        }
    }

    fn call<'a>(&'a self, body: &'a ResponsesBody, attempt: u64) -> Call<'a> {
        // Unconstrained, so tokio's cooperative budget never makes a hand poll return early.
        Box::pin(tokio::task::unconstrained(async move {
            let mut kinds = Vec::new();
            let on_event = |event: &super::sse::SseEvent| kinds.push(event.kind().to_owned());
            let result = stream(
                &self.client,
                body,
                &self.bearer,
                ask(&self.gate, key(attempt)),
                &self.budget,
                on_event,
            )
            .await;
            (result, kinds)
        }))
    }

    /// Answers the waiting ask with grant `send`.
    fn grant(&self, send: u64) {
        drop(self.grant_revocable(send));
    }

    /// Answers the waiting ask with grant `send`, whose picture grant the test may close.
    fn grant_revocable(&self, send: u64) -> PictureGrant {
        let ready = lock(&self.asks).try_recv().expect("a send asked");
        let picture = PictureGrant::open_for_tests();
        let lease = SendLease {
            settled: Settled {
                key: ready.key,
                send,
                unsent: true,
            },
            picture: picture.clone(),
            to: self.settled.clone(),
        };
        ready
            .answer
            .send(Admit::Grant(lease))
            .expect("the send still waits");
        picture
    }

    fn settled(&self) -> Vec<Settled> {
        let mut settled = lock(&self.shared.settled);
        std::iter::from_fn(|| settled.try_recv().ok()).collect()
    }

    fn accept(&self) -> DuplexStream {
        lock(&self.servers)
            .try_recv()
            .expect("a connection was opened")
    }

    fn accepts(&self) -> usize {
        let mut servers = lock(&self.servers);
        std::iter::from_fn(|| servers.try_recv().ok()).count()
    }

    fn seen(&self) -> (Vec<usize>, Vec<usize>) {
        let seen = lock(&self.shared.seen);
        (seen.flushes.clone(), seen.closes.clone())
    }

    fn valve(&self, change: impl FnOnce(&mut Valve)) {
        change(&mut lock(&self.shared.valve));
    }
}

fn poll<T>(call: &mut Pin<Box<dyn Future<Output = T> + '_>>) -> Poll<T> {
    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// What the server end has received so far, and whether the client has closed it.
fn received(server: &mut DuplexStream) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        let mut chunk = [0; 4096];
        let mut buf = ReadBuf::new(&mut chunk);
        match Pin::new(&mut *server).poll_read(&mut cx, &mut buf) {
            Poll::Ready(Ok(())) if buf.filled().is_empty() => return (bytes, true),
            Poll::Ready(Ok(())) => bytes.extend_from_slice(buf.filled()),
            Poll::Ready(Err(_)) => return (bytes, true),
            Poll::Pending => return (bytes, false),
        }
    }
}

fn reply(server: &mut DuplexStream, mut bytes: &[u8]) {
    let mut cx = Context::from_waker(Waker::noop());
    while !bytes.is_empty() {
        match Pin::new(&mut *server).poll_write(&mut cx, bytes) {
            Poll::Ready(Ok(written)) => bytes = &bytes[written..],
            other => panic!("the reply didn't fit the pipe: {other:?}"),
        }
    }
}

/// A chunked event-stream response carrying `events` in one chunk.
fn sse(events: &[u8]) -> Vec<u8> {
    let mut bytes =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
    bytes.extend(format!("{:x}\r\n", events.len()).into_bytes());
    bytes.extend_from_slice(events);
    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
    bytes
}

fn refused(status: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn body(prompt: &str) -> ResponsesBody {
    ResponsesBody::new(&json!({"model": "m", "store": false, "stream": true, "input": prompt}))
        .expect("a valid body")
}

fn request_length<C: Connect>(harness: &Harness<C>, body: &ResponsesBody) -> usize {
    wire::request(harness.bearer.expose(), body.0.expose())
        .expect("a valid bearer")
        .len()
}

const TEXT_KINDS: [&str; 4] = [
    "response.created",
    "response.output_text.delta",
    "response.output_text.delta",
    "response.completed",
];

/// Runs one request through to a completed stream on the server end it's given, or a new one.
fn complete(
    harness: &Harness,
    body: &ResponsesBody,
    send: u64,
    server: Option<DuplexStream>,
) -> DuplexStream {
    let mut call = harness.call(body, send);
    assert!(poll(&mut call).is_pending());
    harness.grant(send);
    assert!(poll(&mut call).is_pending());
    let mut server = server.unwrap_or_else(|| harness.accept());
    let (request, closed) = received(&mut server);
    assert_eq!(
        (request.len(), closed),
        (request_length(harness, body), false)
    );
    assert!(request.starts_with(b"POST /v1/responses HTTP/1.1\r\n"));
    assert!(request.ends_with(body.0.expose()));
    reply(&mut server, &sse(TEXT));
    let Poll::Ready((result, kinds)) = poll(&mut call) else {
        panic!("the stream didn't end");
    };
    assert_eq!(
        (result, kinds),
        (Ok(()), TEXT_KINDS.map(str::to_owned).to_vec())
    );
    server
}

#[tokio::test]
async fn no_settle_while_the_final_bytes_wait_on_a_gated_flush() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold_flush = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    // Every byte reached the peer, but the flush hasn't completed, so nothing settles.
    assert_eq!(
        received(&mut server).0.len(),
        request_length(&harness, &body)
    );
    assert_eq!(harness.settled(), []);
    harness.valve(|valve| valve.hold_flush = false);
    assert!(poll(&mut call).is_pending());
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(1),
            send: 1,
            unsent: false,
        }]
    );
    // The flush finished before the lease settled.
    assert_eq!(harness.seen(), (vec![0], vec![]));
}

#[tokio::test(start_paused = true)]
async fn a_blocked_write_is_stopped_by_the_send_deadline() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    tokio::time::advance(
        super::SEND_DEADLINE
            .checked_sub(Duration::from_millis(1))
            .expect("over 1 ms"),
    )
    .await;
    assert!(poll(&mut call).is_pending());
    assert_eq!(harness.settled(), []);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    // The connection closed with nothing settled, then the lease settled.
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(1),
            send: 1,
            unsent: false,
        }]
    );
    assert_eq!(received(&mut server), (Vec::new(), true));
}

#[tokio::test(start_paused = true)]
async fn a_slow_connect_and_a_slow_upload_share_one_deadline() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.open.send_replace(false);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_secs(6)).await;
    harness.valve(|valve| valve.hold = true);
    harness.open.send_replace(true);
    assert!(poll(&mut call).is_pending());
    // Kept open, so only the deadline can end the upload.
    let mut server = harness.accept();
    // Four more seconds is ten since the grant: the upload doesn't get a deadline of its own.
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(harness.settled().len(), 1);
    assert_eq!(received(&mut server), (Vec::new(), true));
}

#[tokio::test]
async fn dropping_the_round_mid_write_closes_then_settles() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    drop(call);
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(1),
            send: 1,
            unsent: false,
        }]
    );
    assert_eq!(received(&mut server), (Vec::new(), true));
}

#[tokio::test]
async fn dropping_the_round_after_a_sent_prefix_closes_then_settles() {
    let harness = Harness::new(4096);
    let body = body(&"p".repeat(64 * 1024));
    let request = wire::request(harness.bearer.expose(), body.0.expose()).expect("a valid bearer");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    assert_eq!(harness.settled(), []);
    drop(call);
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(harness.settled().len(), 1);
    // The pipe took the first 4096 bytes; nothing after them was written, and then it closed.
    assert_eq!(received(&mut server), (request[..4096].to_vec(), true));
}

#[tokio::test]
async fn an_interim_head_mid_upload_keeps_the_upload_going() {
    let harness = Harness::new(4096);
    let body = body(&"p".repeat(64 * 1024));
    let request = wire::request(harness.bearer.expose(), body.0.expose()).expect("a valid bearer");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    let (mut got, _) = received(&mut server);
    assert_eq!(got.len(), 4096);
    reply(
        &mut server,
        b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n",
    );
    while got.len() < request.len() {
        assert!(poll(&mut call).is_pending());
        let (more, closed) = received(&mut server);
        assert!(
            !more.is_empty() && !closed,
            "the upload stopped at {}",
            got.len()
        );
        got.extend(more);
    }
    assert_eq!(got, *request);
    assert_eq!(harness.settled().len(), 1);
    reply(&mut server, &sse(TEXT));
    let Poll::Ready((result, kinds)) = poll(&mut call) else {
        panic!("the stream didn't end");
    };
    assert_eq!(
        (result, kinds),
        (Ok(()), TEXT_KINDS.map(str::to_owned).to_vec())
    );
    assert_eq!(harness.seen().0, vec![0]);
}

#[tokio::test]
async fn a_success_head_while_the_flush_is_held_is_the_answer() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold_flush = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    assert_eq!(
        received(&mut server).0.len(),
        request_length(&harness, &body)
    );
    reply(&mut server, &sse(TEXT));
    assert!(poll(&mut call).is_pending());
    assert_eq!(harness.settled(), []);
    harness.valve(|valve| valve.hold_flush = false);
    let Poll::Ready((result, kinds)) = poll(&mut call) else {
        panic!("the stream didn't end");
    };
    assert_eq!(
        (result, kinds),
        (Ok(()), TEXT_KINDS.map(str::to_owned).to_vec())
    );
    assert_eq!(harness.seen().0, vec![0]);
    assert_eq!(harness.settled().len(), 1);
}

#[tokio::test]
async fn an_early_400_stops_the_upload_without_the_server_closing() {
    let harness = Harness::new(4096);
    let body = body(&"p".repeat(64 * 1024));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    let (part, _) = received(&mut server);
    assert!(part.starts_with(b"POST "));
    reply(&mut server, &refused(400, UNSUPPORTED));
    let Poll::Ready((result, _)) = poll(&mut call) else {
        panic!("the refusal wasn't read");
    };
    assert_eq!(
        result,
        Err(Failure::Provider(ProviderError {
            status: 400,
            code: Some(Code::Unsupported),
            param: Some(Param::Tools),
            origin: RejectionOrigin::Unproven,
        }))
    );
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(
        harness.settled(),
        settled_once(false),
        "a write was attempted"
    );
    // At most what was already in the pipe follows; then the close. The request never finished.
    let (rest, closed) = received(&mut server);
    assert!(closed);
    assert!(part.len() + rest.len() < request_length(&harness, &body));
}

/// The one settlement of attempt 1's grant.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the lease settled unsent"
)]
fn settled_once(unsent: bool) -> [Settled; 1] {
    [Settled {
        key: key(1),
        send: 1,
        unsent,
    }]
}

#[tokio::test]
async fn a_spent_budget_settles_unsent() {
    let harness = Harness::budgeted(1 << 20, Some(0));
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Spent), _))
    ));
    assert_eq!(harness.settled(), settled_once(true));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_while_connecting_settles_unsent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.open.send_replace(false);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(super::SEND_DEADLINE).await;
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    assert_eq!(harness.accepts(), 0);
    assert_eq!(harness.settled(), settled_once(true));
}

#[tokio::test]
async fn a_final_head_waiting_before_any_write_settles_unsent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.greeting = Some(refused(400, UNSUPPORTED)));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((
            Err(Failure::Provider(ProviderError {
                status: 400,
                origin: RejectionOrigin::Unproven,
                ..
            })),
            _
        ))
    ));
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(harness.settled(), settled_once(true));
}

#[tokio::test]
async fn a_picture_grant_revoked_before_the_first_write_writes_nothing_and_settles_unsent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant_revocable(1).close();
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Rebuild), _))
    ));
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(harness.settled(), settled_once(true));
}

#[tokio::test]
async fn a_picture_grant_revoked_inside_upload_before_the_first_write_writes_nothing() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(1);
    harness.valve(|valve| valve.revoke_on_read = Some(picture.clone()));
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Rebuild), _))
    ));
    assert!(!picture.is_open(), "revoked from the early read");
    assert_eq!(lock(&harness.shared.valve).write_polls, 0);
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(harness.settled(), settled_once(true));
}

#[tokio::test]
async fn an_open_picture_grant_writes_the_exact_request_once_and_releases_each_admission() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    let (request, closed) = received(&mut server);
    assert_eq!(
        (request.len(), closed),
        (request_length(&harness, &body), false)
    );
    assert!(request.starts_with(b"POST /v1/responses HTTP/1.1\r\n"));
    assert!(request.ends_with(body.0.expose()));
    assert!(
        picture.reserve_write().is_some(),
        "no admission outlives its poll"
    );
    reply(&mut server, &sse(TEXT));
    let Poll::Ready((result, _)) = poll(&mut call) else {
        panic!("the stream didn't end");
    };
    assert_eq!(result, Ok(()));
    assert_eq!(lock(&harness.shared.valve).write_polls, 1);
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_picture_grant_closed_before_a_pooled_connection_writes_nothing_and_drops_it() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut server = complete(&harness, &body, 1, None);
    harness.settled();
    let polls = lock(&harness.shared.valve).write_polls;
    let mut call = harness.call(&body, 2);
    assert!(poll(&mut call).is_pending());
    harness.grant_revocable(2).close();
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Rebuild), _))
    ));
    assert_eq!(lock(&harness.shared.valve).write_polls, polls);
    assert_eq!(
        harness.accepts(),
        0,
        "the pooled connection was the one used"
    );
    assert_eq!(received(&mut server), (Vec::new(), true));
    assert!(harness.client.checkout().is_none());
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(2),
            send: 2,
            unsent: true,
        }]
    );
}

#[tokio::test]
async fn a_picture_grant_closed_while_a_stale_pooled_connection_is_replaced_writes_nothing() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    drop(complete(&harness, &body, 1, None));
    harness.settled();
    let polls = lock(&harness.shared.valve).write_polls;
    harness.open.send_replace(false);
    let mut call = harness.call(&body, 2);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(2);
    assert!(
        poll(&mut call).is_pending(),
        "the replacement still connects"
    );
    picture.close();
    harness.open.send_replace(true);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Rebuild), _))
    ));
    assert_eq!(lock(&harness.shared.valve).write_polls, polls);
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(2),
            send: 2,
            unsent: true,
        }]
    );
}

#[tokio::test]
async fn a_picture_grant_closed_after_a_pending_write_polls_no_more_and_never_settles_unsent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(1);
    assert!(poll(&mut call).is_pending());
    assert_eq!(lock(&harness.shared.valve).write_polls, 1);
    picture.close();
    harness.valve(|valve| valve.hold = false);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Withheld), _))
    ));
    assert_eq!(lock(&harness.shared.valve).write_polls, 1);
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_picture_grant_closed_after_a_partial_write_polls_no_more_and_closes_the_connection() {
    let harness = Harness::new(16);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(1);
    assert!(poll(&mut call).is_pending());
    assert_eq!(
        lock(&harness.shared.valve).write_polls,
        2,
        "16 bytes, then full"
    );
    let mut server = harness.accept();
    let (sent, closed) = received(&mut server);
    assert_eq!((sent.len(), closed), (16, false));
    picture.close();
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Withheld), _))
    ));
    assert_eq!(lock(&harness.shared.valve).write_polls, 2);
    assert_eq!(received(&mut server), (Vec::new(), true));
    assert!(harness.client.checkout().is_none());
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_picture_grant_closed_while_the_flush_waits_flushes_no_more() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold_flush = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    let picture = harness.grant_revocable(1);
    assert!(poll(&mut call).is_pending());
    picture.close();
    harness.valve(|valve| valve.hold_flush = false);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Withheld), _))
    ));
    assert_eq!(harness.seen().0, Vec::<usize>::new(), "no flush completed");
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_first_write_that_fails_with_nothing_passed_still_counts_as_maybe_sent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.fail_after = Some(0));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    assert_eq!(received(&mut harness.accept()), (Vec::new(), true));
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_first_write_that_passes_bytes_and_fails_never_settles_unsent() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let request = wire::request(harness.bearer.expose(), body.0.expose()).expect("a valid bearer");
    harness.valve(|valve| valve.pass_then_fail = Some(16));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    assert_eq!(
        received(&mut harness.accept()),
        (request[..16].to_vec(), true)
    );
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test]
async fn a_flushed_request_answered_500_settles_as_written() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    assert_eq!(
        received(&mut server).0.len(),
        request_length(&harness, &body)
    );
    assert_eq!(harness.settled(), settled_once(false));
    reply(&mut server, &refused(500, DETAIL));
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Provider(ProviderError { status: 500, .. })), _))
    ));
    assert_eq!(harness.settled(), []);
}

#[tokio::test]
async fn a_round_dropped_while_its_flush_waits_settles_as_written() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.hold_flush = true);
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    assert_eq!(
        received(&mut harness.accept()).0.len(),
        request_length(&harness, &body)
    );
    drop(call);
    assert_eq!(harness.settled(), settled_once(false));
}

#[tokio::test(start_paused = true)]
async fn a_trickled_early_refusal_settles_at_the_earlier_budget() {
    for (head_at_s, body_ms) in [(9, 1000), (1, 3000)] {
        let mut harness = Harness::new(4096);
        harness.client.tuning.error_body = Duration::from_secs(3);
        let payload = body(&"p".repeat(64 * 1024));
        let mut call = harness.call(&payload, 1);
        assert!(poll(&mut call).is_pending());
        harness.grant(1);
        assert!(poll(&mut call).is_pending());
        let mut server = harness.accept();
        received(&mut server);
        tokio::time::advance(Duration::from_secs(head_at_s)).await;
        reply(
            &mut server,
            b"HTTP/1.1 429 Slow\r\nContent-Length: 50\r\n\r\n{\"error\":",
        );
        assert!(poll(&mut call).is_pending());
        tokio::time::advance(Duration::from_millis(body_ms - 1)).await;
        reply(&mut server, b" {");
        assert!(poll(&mut call).is_pending());
        assert_eq!(harness.settled(), []);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            poll(&mut call),
            Poll::Ready((Err(Failure::Transport), _))
        ));
        assert_eq!(harness.seen(), (vec![], vec![0]));
        assert_eq!(harness.settled(), settled_once(false));
    }
}

#[tokio::test]
async fn a_write_error_after_part_of_the_request_is_not_replayed() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.fail_after = Some(40));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    let mut server = harness.accept();
    assert_eq!(received(&mut server).0.len(), 40);
    assert_eq!(harness.accepts(), 0);
    assert_eq!(harness.seen(), (vec![], vec![0]));
    assert_eq!(harness.settled().len(), 1);
}

#[tokio::test]
async fn a_connection_closed_after_a_partial_send_is_not_replayed() {
    let harness = Harness::new(4096);
    let body = body(&"p".repeat(64 * 1024));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    drop(server);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    assert_eq!(harness.accepts(), 0);
    assert_eq!(harness.settled().len(), 1);
}

#[tokio::test]
async fn two_completed_requests_reuse_one_connection_with_two_grants() {
    let harness = Harness::budgeted(1 << 20, Some(2));
    let body = body("hello");
    let server = complete(&harness, &body, 1, None);
    complete(&harness, &body, 2, Some(server));
    assert_eq!(harness.accepts(), 0);
    assert_eq!(
        harness.settled(),
        [
            Settled {
                key: key(1),
                send: 1,
                unsent: false,
            },
            Settled {
                key: key(2),
                send: 2,
                unsent: false,
            }
        ]
    );
    assert!(!harness.budget.reserve());
    // Each flush came before its lease settled.
    assert_eq!(harness.seen(), (vec![0, 1], vec![]));
}

#[tokio::test(start_paused = true)]
async fn a_pooled_connection_is_reused_until_it_has_idled_its_limit() {
    for (idle, reused) in [
        (
            POOL_IDLE
                .checked_sub(Duration::from_millis(1))
                .expect("the pool limit is over 1 ms"),
            true,
        ),
        (POOL_IDLE, false),
    ] {
        let harness = Harness::budgeted(1 << 20, Some(2));
        let body = body("hello");
        // The first server end stays open and quiet, so only the connection's age decides.
        let first = complete(&harness, &body, 1, None);
        tokio::time::advance(idle).await;
        if reused {
            complete(&harness, &body, 2, Some(first));
            assert_eq!(harness.accepts(), 0);
        } else {
            complete(&harness, &body, 2, None);
            drop(first);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_response_may_put_exactly_the_wire_cap_on_the_wire_and_no_more() {
    let head = |length: usize| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\n\r\n"
        )
    };
    for (extra, pooled) in [(0, true), (1, false)] {
        let harness = Harness::new(MAX_WIRE_BYTES + 4096);
        let body = body("hello");
        let mut call = harness.call(&body, 1);
        assert!(poll(&mut call).is_pending());
        harness.grant(1);
        assert!(poll(&mut call).is_pending());
        let mut server = harness.accept();
        received(&mut server);
        let length = MAX_WIRE_BYTES - head(MAX_WIRE_BYTES).len() + extra;
        let mut response = head(length).into_bytes();
        response.extend_from_slice(TEXT);
        // Bytes after response.completed are read only to finish the framing for reuse.
        response.resize(response.len() + length - TEXT.len(), b'x');
        assert_eq!(response.len(), MAX_WIRE_BYTES + extra);
        reply(&mut server, &response);
        let Poll::Ready((result, kinds)) = poll(&mut call) else {
            panic!("the stream didn't end");
        };
        assert_eq!(
            (result, kinds),
            (Ok(()), TEXT_KINDS.map(str::to_owned).to_vec())
        );
        assert_eq!(harness.client.checkout().is_some(), pooled, "{extra} over");
    }
}

#[tokio::test]
async fn a_stale_pooled_connection_is_replaced_under_the_same_lease() {
    let harness = Harness::budgeted(1 << 20, Some(2));
    let body = body("hello");
    drop(complete(&harness, &body, 1, None));
    assert_eq!(harness.settled().len(), 1);
    complete(&harness, &body, 2, None);
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(2),
            send: 2,
            unsent: false,
        }]
    );
    // One unit for each request, none for the replacement.
    assert!(!harness.budget.reserve());
    // Counted after the first settlement was read: the stale close and the second flush both
    // came before the second lease settled.
    assert_eq!(harness.seen(), (vec![0, 0], vec![0]));
}

#[tokio::test]
async fn cancel_during_the_stale_requeue_sends_nothing_later() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    drop(complete(&harness, &body, 1, None));
    harness.settled();
    harness.open.send_replace(false);
    let mut call = harness.call(&body, 2);
    assert!(poll(&mut call).is_pending());
    harness.grant(2);
    assert!(poll(&mut call).is_pending());
    drop(call);
    assert_eq!(harness.accepts(), 0);
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(2),
            send: 2,
            unsent: true,
        }]
    );
}

#[tokio::test]
async fn leftover_bytes_or_a_close_keep_the_connection_out_of_the_pool() {
    for (tail, head) in [
        (
            &b"HTTP/1.1 200 OK\r\n"[..],
            &b"Transfer-Encoding: chunked\r\n"[..],
        ),
        (b"", b"Transfer-Encoding: chunked\r\nConnection: close\r\n"),
    ] {
        let harness = Harness::new(1 << 20);
        let body = body("hello");
        let mut call = harness.call(&body, 1);
        assert!(poll(&mut call).is_pending());
        harness.grant(1);
        assert!(poll(&mut call).is_pending());
        let mut server = harness.accept();
        received(&mut server);
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n".to_vec();
        response.extend_from_slice(head);
        response.extend(format!("\r\n{:x}\r\n", TEXT.len()).into_bytes());
        response.extend_from_slice(TEXT);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        response.extend_from_slice(tail);
        reply(&mut server, &response);
        assert!(matches!(poll(&mut call), Poll::Ready((Ok(()), _))));
        assert_eq!(
            harness.seen().1,
            [1],
            "closed after it settled, never pooled"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_completed_stream_whose_framing_stalls_is_closed_not_pooled() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    let mut response = sse(TEXT);
    response.truncate(response.len() - 5);
    reply(&mut server, &response);
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(poll(&mut call), Poll::Ready((Ok(()), _))));
    assert_eq!(harness.seen().1, [1]);
}

#[tokio::test(start_paused = true)]
async fn the_tuned_drain_deadline_closes_only_the_unfinished_framing() {
    let mut harness = Harness::new(1 << 20);
    harness.client.tuning.drain = Duration::from_millis(250);
    let payload = body("hello");
    let mut call = harness.call(&payload, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    let mut response = sse(TEXT);
    response.truncate(response.len() - 5);
    reply(&mut server, &response);
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_millis(249)).await;
    reply(&mut server, b"1\r\nx\r\n");
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(poll(&mut call), Poll::Ready((Ok(()), _))));
    assert_eq!(harness.seen().1, [1]);
}

#[tokio::test(start_paused = true)]
async fn an_idle_stream_ends_after_its_idle_limit() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    reply(
        &mut server,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(
        super::IDLE
            .checked_sub(Duration::from_millis(1))
            .expect("over 1 ms"),
    )
    .await;
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
}

#[tokio::test(start_paused = true)]
async fn tuned_waits_end_an_idle_stream_and_retire_a_pooled_connection_sooner() {
    let tuning = crate::config::tuning::ModelTuning {
        idle: Duration::from_secs(15),
        pool_idle: Duration::from_secs(10),
        ..crate::config::tuning::ModelTuning::default()
    };
    let mut harness = Harness::new(1 << 20);
    harness.client.tuning = tuning;
    let payload = body("hello");
    let mut call = harness.call(&payload, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    reply(
        &mut server,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_millis(14_999)).await;
    assert!(poll(&mut call).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));

    for (idle, reused) in [
        (Duration::from_millis(9_999), true),
        (Duration::from_secs(10), false),
    ] {
        let mut harness = Harness::budgeted(1 << 20, Some(2));
        harness.client.tuning = tuning;
        let body = body("hello");
        let first = complete(&harness, &body, 1, None);
        tokio::time::advance(idle).await;
        if reused {
            complete(&harness, &body, 2, Some(first));
            assert_eq!(harness.accepts(), 0);
        } else {
            complete(&harness, &body, 2, None);
            drop(first);
        }
    }
}

/// The result of one request answered with `response` once it's sent.
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test helper whose result is the request's"
)]
fn answered(response: &[u8]) -> Result<(), Failure> {
    let harness = Harness::new(4 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    reply(&mut server, response);
    drop(server);
    match poll(&mut call) {
        Poll::Ready((result, _)) => result,
        Poll::Pending => panic!("the answer wasn't read"),
    }
}

#[tokio::test]
async fn answers_that_arent_an_event_stream_fail_closed() {
    let mut exact = b"[".to_vec();
    exact.extend(vec![b' '; MAX_BODY_BYTES - 2]);
    exact.push(b']');
    let mut over = exact.clone();
    over.push(b' ');
    let cases: [(Vec<u8>, Result<(), Failure>); 7] = [
        (
            refused(401, DETAIL),
            Err(Failure::Provider(ProviderError { status: 401, code: None, param: None, origin: RejectionOrigin::AfterUpload })),
        ),
        (
            refused(400, &exact),
            Err(Failure::Provider(ProviderError { status: 400, code: None, param: None, origin: RejectionOrigin::AfterUpload })),
        ),
        (refused(400, &over), Err(Failure::Protocol)),
        (refused(302, b""), Err(Failure::Protocol)),
        (b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_vec(), Err(Failure::Protocol)),
        (b"HTTP/1.1 101 Switching\r\n\r\n".to_vec(), Err(Failure::Protocol)),
        (b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\n\r\ndata: {}\n".to_vec(), Err(Failure::Truncated)),
    ];
    for (response, expected) in cases {
        assert_eq!(
            answered(&response),
            expected,
            "{}",
            String::from_utf8_lossy(&response[..response.len().min(40)])
        );
    }
    let mut interim =
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n".to_vec();
    interim.extend(sse(TEXT));
    assert_eq!(answered(&interim), Ok(()));
    assert_eq!(
        answered(&sse(&TEXT[..TEXT.len() - 1])),
        Err(Failure::Truncated)
    );
}

/// A length-framed 200 carrying `body`, with `media` as its Content-Type when there is one.
fn typed(media: Option<&str>, body: &[u8]) -> Vec<u8> {
    let media = media.map(|media| format!("Content-Type: {media}\r\n"));
    let mut bytes = format!(
        "HTTP/1.1 200 OK\r\n{}Content-Length: {}\r\n\r\n",
        media.unwrap_or_default(),
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

#[tokio::test]
async fn a_missing_type_is_sniffed_across_reads_and_every_event_is_read() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    let (first, rest) = TEXT.split_at(3);
    let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n".to_vec();
    bytes.extend_from_slice(first);
    reply(&mut server, &bytes);
    // "eve" could still start an event stream, so the transport waits for more.
    assert!(poll(&mut call).is_pending());
    let mut bytes = format!("\r\n{:x}\r\n", rest.len()).into_bytes();
    bytes.extend_from_slice(rest);
    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
    reply(&mut server, &bytes);
    let Poll::Ready((result, kinds)) = poll(&mut call) else {
        panic!("the stream didn't end");
    };
    assert_eq!(
        (result, kinds),
        (Ok(()), TEXT_KINDS.map(str::to_owned).to_vec())
    );
}

#[tokio::test]
async fn a_missing_type_needs_an_event_stream_in_its_first_128_bytes() {
    let mut within = vec![b'\n'; 122];
    within.extend_from_slice(TEXT);
    let mut past = vec![b'\n'; 123];
    past.extend_from_slice(TEXT);
    let cases = [
        (typed(None, TEXT), Ok(())),
        (typed(None, &within), Ok(())),
        (typed(None, &past), Err(Failure::Protocol)),
        (
            typed(None, b"{\"type\":\"response.created\"}"),
            Err(Failure::Protocol),
        ),
        (typed(None, b""), Err(Failure::Protocol)),
        (typed(Some("text/plain"), TEXT), Err(Failure::Protocol)),
    ];
    for (index, (response, expected)) in cases.into_iter().enumerate() {
        assert_eq!(answered(&response), expected, "case {index}");
    }
    // The decision comes at the 128th byte, without waiting for more of the body.
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n80\r\n".to_vec();
    bytes.extend_from_slice(&past[..128]);
    reply(&mut server, &bytes);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Protocol), _))
    ));
}

#[tokio::test]
async fn an_encoded_answer_is_refused_before_any_reader() {
    for (status, body) in [(200, TEXT), (400, UNSUPPORTED)] {
        let mut response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        assert_eq!(answered(&response), Err(Failure::Protocol), "{status}");
    }
}

#[tokio::test]
async fn four_interim_heads_pass_and_a_fifth_is_refused() {
    for (count, expected) in [(4, Ok(())), (5, Err(Failure::Protocol))] {
        let mut response = b"HTTP/1.1 100 Continue\r\n\r\n".repeat(count);
        response.extend(sse(TEXT));
        assert_eq!(answered(&response), expected, "{count} interim heads");
    }
}

#[test]
fn a_body_may_be_exactly_the_request_limit() {
    let sized =
        |length: usize| json!({"store": false, "stream": true, "input": "x".repeat(length)});
    let base = serde_json::to_vec(&sized(0)).expect("json").len();
    let exact = super::MAX_REQUEST_BYTES - base;
    assert!(ResponsesBody::new(&sized(exact)).is_ok());
    assert!(matches!(
        ResponsesBody::new(&sized(exact + 1)),
        Err(Failure::Request)
    ));
}

#[tokio::test]
async fn rebuild_a_closed_gate_or_a_dropped_answer_sends_nothing() {
    for answer in 0..3 {
        let harness = Harness::budgeted(1 << 20, Some(1));
        let body = body("hello");
        let mut call = harness.call(&body, 1);
        assert!(poll(&mut call).is_pending());
        {
            let mut asks = lock(&harness.asks);
            let ready = asks.try_recv().expect("asked");
            match answer {
                0 => ready.answer.send(Admit::Rebuild).expect("waiting"),
                1 => drop(ready),
                _ => asks.close(),
            }
        }
        assert!(
            matches!(poll(&mut call), Poll::Ready((Err(Failure::Rebuild), _))),
            "answer {answer}"
        );
        assert_eq!(harness.accepts(), 0);
        assert!(harness.budget.reserve(), "no unit taken");
    }
}

#[tokio::test]
async fn a_retry_after_a_failed_send_asks_again_on_a_new_connection() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    harness.valve(|valve| valve.fail_after = Some(40));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    drop(call);
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(1),
            send: 1,
            unsent: false,
        }]
    );
    drop(harness.accept());
    harness.valve(|valve| valve.fail_after = None);
    // The retry is its own attempt: it asks, waits for its own grant, and sends on a new connection.
    complete(&harness, &body, 2, None);
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(2),
            send: 2,
            unsent: false,
        }]
    );
}

#[tokio::test]
async fn a_retry_spends_its_own_unit_and_the_next_is_refused() {
    let harness = Harness::budgeted(1 << 20, Some(2));
    let body = body("hello");
    harness.valve(|valve| valve.fail_after = Some(40));
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Transport), _))
    ));
    drop(call);
    drop(harness.accept());
    harness.valve(|valve| valve.fail_after = None);
    complete(&harness, &body, 2, None);
    let mut call = harness.call(&body, 3);
    assert!(poll(&mut call).is_pending());
    harness.grant(3);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Spent), _))
    ));
    assert_eq!(harness.accepts(), 0);
}

#[tokio::test]
async fn a_spent_budget_settles_and_connects_nothing() {
    let harness = Harness::budgeted(1 << 20, Some(0));
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Spent), _))
    ));
    assert_eq!(harness.accepts(), 0);
    assert_eq!(harness.settled().len(), 1);
}

#[cfg(all(windows, debug_assertions))]
#[tokio::test]
async fn observation_counts_the_reserved_failed_connect_not_a_denied_or_spent_ask() {
    use super::text_observation::{self, Stamp};

    struct NeverConnect;
    impl Connect for NeverConnect {
        type Io = DuplexStream;

        fn connect(&self) -> impl Future<Output = io::Result<DuplexStream>> + Send {
            std::future::ready(Err(io::ErrorKind::NotConnected.into()))
        }
    }

    let request = crate::engines::RunRequest::for_tests(
        RunId(7),
        kumochi_protocol::message::ThreadId(2),
        "words",
    );
    let client = ModelClient::new(NeverConnect);
    let payload = body("words");
    let bearer = Secret::new(Zeroizing::new(BEARER.to_owned()));
    let pid = std::process::id();
    for (budget, last, kinds) in [
        (
            1,
            Failure::Spent,
            &["ready", "reserve", "no_io", "final"][..],
        ),
        (2, Failure::Transport, &["ready"][..]),
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&seen);
        let observation =
            text_observation::fixture(1, move |line| lock(&out).push(line.to_owned()));
        let budget = Budget::limited(Some(budget));
        for (grant, expected) in [
            (None, Failure::Rebuild),
            (Some(()), Failure::Transport),
            (Some(()), last),
        ] {
            assert_eq!(
                super::stream_observed(
                    &client,
                    &payload,
                    &bearer,
                    std::future::ready(grant),
                    &budget,
                    |_: &SseEvent| {},
                    Some(Stamp::new(&request, &observation)),
                )
                .await,
                Err(expected)
            );
        }
        observation.finish();
        let records = lock(&seen);
        assert_eq!(
            records
                .iter()
                .map(|line| line.split(' ').next().unwrap())
                .collect::<Vec<_>>(),
            kinds
        );
        if kinds.len() == 4 {
            assert!(records[2].starts_with(&format!("no_io {pid} 1 ")));
            assert!(records[3].starts_with(&format!("final {pid} 1 0 ")));
        }
    }
}

#[cfg(all(windows, debug_assertions))]
#[tokio::test]
async fn observation_distinguishes_cancellation_after_owned_io_drop_from_pool_reuse() {
    use super::text_observation::{self, Stamp};

    let harness = Harness::budgeted(1 << 20, Some(2));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&seen);
    let (observation, release) =
        text_observation::stalled_fixture(2, move |line| lock(&out).push(line.to_owned()));
    let request = crate::engines::RunRequest::for_tests(
        RunId(7),
        kumochi_protocol::message::ThreadId(2),
        "words",
    );
    let payload = body("words");
    for (serial, complete) in [(1, false), (2, true)] {
        harness.valve(|valve| valve.hold_flush = !complete);
        let mut call: Pin<Box<dyn Future<Output = Result<(), Failure>>>> =
            Box::pin(tokio::task::unconstrained(super::stream_observed(
                &harness.client,
                &payload,
                &harness.bearer,
                ask(&harness.gate, key(serial)),
                &harness.budget,
                |_: &SseEvent| {},
                Some(Stamp::new(&request, &observation)),
            )));
        assert!(poll(&mut call).is_pending());
        harness.grant(serial);
        assert!(poll(&mut call).is_pending());
        let mut server = harness.accept();
        assert_eq!(harness.shared.settled_count(), usize::from(complete));
        if complete {
            reply(&mut server, &sse(TEXT));
            assert_eq!(poll(&mut call), Poll::Ready(Ok(())));
        }
        drop(call);
        assert_eq!(received(&mut server).1, !complete);
        assert_eq!(
            harness.settled(),
            [Settled {
                key: key(serial),
                send: serial,
                unsent: false
            }]
        );
    }
    assert_eq!(lock(&seen).len(), 1, "the sink is still stalled");
    release.send(()).unwrap();
    observation.finish();
    let seen = lock(&seen);
    let observed = seen
        .iter()
        .map(|line| line.split(' ').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        ["ready", "reserve", "closed", "reserve", "pooled", "final"]
    );
    let fields = seen[2].split(' ').collect::<Vec<_>>();
    let ["closed", pid, "1", before, after] = fields.as_slice() else {
        panic!("an owned close needs both QPC samples");
    };
    assert_eq!(pid.parse::<u32>().unwrap(), std::process::id());
    let io = lock(&harness.shared.seen);
    let [closed] = io.closed_at.as_slice() else {
        panic!("the cancelled connection dropped; the pooled one did not");
    };
    assert!((before.parse().unwrap()..=after.parse().unwrap()).contains(closed));
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn observation_records_only_fixed_http_classes_and_refuses_a_final_after_overflow() {
    use super::text_observation;
    use crate::connections::http_client::Endpoint;

    let pid = std::process::id();
    for calls in [64, 65] {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&lines);
        let observation = text_observation::fixture(1, move |line| {
            let (fields, timestamp) = line.rsplit_once(' ').unwrap();
            assert!(timestamp.parse::<u64>().unwrap() > 0);
            lock(&out).push(fields.to_owned());
        });
        let models = Endpoint::Models(Secret::new(Zeroizing::new("PLANTED-ACCOUNT".into())));
        for call in 1..=calls {
            let endpoint = if call == 1 { &models } else { &Endpoint::Token };
            observation.auxiliary(endpoint);
        }
        assert_eq!(observation.buffered_for_tests(), 64);
        observation.finish();
        let mut expected = vec![format!("ready {pid} 1")];
        if calls == 64 {
            expected.push(format!("aux {pid} 1 5"));
            expected.extend((2..=64).map(|ordinal| format!("aux {pid} {ordinal} 1")));
            expected.push(format!("final {pid} 0 64"));
        }
        assert_eq!(*lock(&lines), expected);
    }
}

#[tokio::test]
async fn a_bad_body_or_bearer_never_asks() {
    for value in [
        json!({"store": true, "stream": true}),
        json!({"stream": true}),
        json!({"store": false, "stream": false}),
        json!({"store": false, "stream": true, "input": "x".repeat(super::MAX_REQUEST_BYTES)}),
    ] {
        assert!(matches!(ResponsesBody::new(&value), Err(Failure::Request)));
    }
    let mut harness = Harness::new(1 << 20);
    harness.bearer = Secret::new(Zeroizing::new("bad\r\nbearer".to_owned()));
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(matches!(
        poll(&mut call),
        Poll::Ready((Err(Failure::Credential), _))
    ));
    assert!(lock(&harness.asks).try_recv().is_err());
}

#[tokio::test]
async fn nothing_planted_reaches_debug_or_the_log() {
    let captured = Captured::default();
    let _capturing = captured.install();
    let harness = Harness::new(1 << 20);
    let body = body("PLANTED-PROMPT");
    complete(&harness, &body, 1, None);
    let leaked = |text: &str| {
        ["PLANTED", "Bearer"]
            .iter()
            .any(|marker| text.contains(marker))
    };
    assert_eq!(
        format!("{body:?}"),
        format!("ResponsesBody {{ bytes: {} }}", body.0.expose().len())
    );
    let failures = [
        answered(&refused(400, UNSUPPORTED)),
        answered(&refused(401, DETAIL)),
        answered(b"HTTP/1.1 200 OK\r\n\r\n"),
    ];
    for failure in failures {
        let failure = failure.expect_err("a failure");
        assert!(!leaked(&format!("{failure:?} {failure}")));
    }
    assert!(!leaked(&format!("{:?}", harness.client)));
    assert!(!leaked(&captured.text()), "the log holds no planted text");
}

/// TLS over the gated pipes; once the handshake is done the valve holds every write beneath
/// rustls, so the request can only wait in rustls's own buffer.
struct TlsPipes {
    pipes: Pipes,
    connector: tokio_rustls::TlsConnector,
}

impl Connect for TlsPipes {
    type Io = tokio_rustls::client::TlsStream<Gated>;

    async fn connect(&self) -> io::Result<Self::Io> {
        let gated = self.pipes.connect().await?;
        let name =
            rustls::pki_types::ServerName::try_from("localhost").map_err(io::Error::other)?;
        let tls = self.connector.connect(name, gated).await?;
        lock(&self.pipes.shared.valve).hold = true;
        Ok(tls)
    }
}

/// A client config trusting a fresh CA, and a server config for its "localhost" leaf.
fn tls_configs() -> (rustls::ClientConfig, rustls::ServerConfig) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca = CertificateParams::new(Vec::new()).expect("CA parameters");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let root = ca.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = KeyPair::generate().expect("leaf key");
    let leaf = CertificateParams::new(vec!["localhost".into()])
        .expect("leaf parameters")
        .signed_by(&leaf_key, &Issuer::from_params(&ca, &ca_key))
        .expect("CA-signed leaf");
    let provider = || Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root.der().clone()).expect("the CA as a root");
    let mut client = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"http/1.1".to_vec()];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into());
    let server = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring protocols")
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], key)
        .expect("the leaf and its key");
    (client, server)
}

#[tokio::test]
async fn under_tls_bytes_held_in_rustls_count_as_not_sent() {
    let (client, server) = tls_configs();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let harness = Harness::with(1 << 20, None, |pipes| TlsPipes { pipes, connector });
    let body = body("hello");
    let mut call = harness.call(&body, 1);
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let mut accept: Pin<Box<dyn Future<Output = io::Result<_>>>> = Box::pin(
        tokio::task::unconstrained(acceptor.accept(harness.accept())),
    );
    let mut tls = None;
    for _ in 0..16 {
        if let Poll::Ready(accepted) = poll(&mut accept) {
            tls = Some(accepted.expect("the handshake"));
            break;
        }
        assert!(poll(&mut call).is_pending());
    }
    let mut tls = tls.expect("the handshake finished");
    assert!(poll(&mut call).is_pending());
    // The request is written into rustls, whose records wait on the held socket: not settled.
    let mut read: Pin<Box<dyn Future<Output = _>>> = Box::pin(async {
        let mut request = vec![0; request_length(&harness, &body)];
        tokio::io::AsyncReadExt::read_exact(&mut tls, &mut request)
            .await
            .map(|_| request)
    });
    assert!(poll(&mut read).is_pending());
    assert_eq!(harness.settled(), []);
    harness.valve(|valve| valve.hold = false);
    assert!(poll(&mut call).is_pending());
    assert_eq!(
        harness.settled(),
        [Settled {
            key: key(1),
            send: 1,
            unsent: false,
        }]
    );
    let Poll::Ready(request) = poll(&mut read) else {
        panic!("the request didn't arrive");
    };
    assert!(
        request
            .expect("the whole request")
            .ends_with(body.0.expose())
    );
    let (flushes, closes) = harness.seen();
    assert_eq!((flushes.last(), closes), (Some(&0), vec![]));
}

#[tokio::test]
#[ignore = "a measurement for a quiet Rez: cargo test -p kumochi-brain --release measure_ -- --ignored --nocapture"]
async fn measure_the_exchange_against_the_reader_and_a_handshake() {
    use std::time::Instant;

    use super::sse::{Progress, Reader};

    let mut events = Vec::new();
    let mut sequence = 0;
    while events.len() < 1024 * 1024 - 512 {
        sequence += 1;
        let event = json!({"type": "response.output_text.delta", "sequence_number": sequence,
            "item_id": "msg_measure", "output_index": 0, "content_index": 0, "delta": " a word or two"});
        events.extend(format!("data: {event}\n\n").into_bytes());
    }
    events.extend_from_slice(b"data: {\"type\":\"response.completed\"}\n\n");
    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
    for chunk in events.chunks(16 * 1024) {
        response.extend(format!("{:x}\r\n", chunk.len()).into_bytes());
        response.extend_from_slice(chunk);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"0\r\n\r\n");
    let rounds: u32 = 50;
    let (mut exchange, mut reader) = (Duration::ZERO, Duration::ZERO);
    for _ in 0..rounds {
        let harness = Harness::new(4 << 20);
        let body = body("hello");
        let mut call = harness.call(&body, 1);
        assert!(poll(&mut call).is_pending());
        harness.grant(1);
        assert!(poll(&mut call).is_pending());
        let mut server = harness.accept();
        received(&mut server);
        reply(&mut server, &response);
        let started = Instant::now();
        assert!(matches!(poll(&mut call), Poll::Ready((Ok(()), _))));
        exchange += started.elapsed();
        let started = Instant::now();
        let mut alone = Reader::default();
        for chunk in events.chunks(16 * 1024) {
            if alone.feed(chunk, &mut |_| {}) == Ok(Progress::Completed) {
                break;
            }
        }
        reader += started.elapsed();
    }
    let mib = f64::from(rounds) * f64::from(u32::try_from(events.len()).expect("about 1 MiB"))
        / (1024.0 * 1024.0);
    for (name, took) in [("exchange", exchange), ("reader alone", reader)] {
        eprintln!(
            "{name}: {:.0} MiB/s over {} events",
            mib / took.as_secs_f64(),
            sequence + 1
        );
    }
    let (client, server) = tls_configs();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a DNS name");
    let mut handshakes = Duration::ZERO;
    for _ in 0..rounds {
        let (near, far) = tokio::io::duplex(1 << 16);
        let started = Instant::now();
        let (connected, accepted) =
            tokio::join!(connector.connect(name.clone(), near), acceptor.accept(far));
        handshakes += started.elapsed();
        connected.expect("client handshake");
        accepted.expect("server handshake");
    }
    eprintln!(
        "TLS handshake over in-memory pipes, both ends' CPU and no network: {:.0} us",
        handshakes.as_secs_f64() * 1e6 / f64::from(rounds)
    );
}

/// Takes nothing: stops at the first wait, as the engine does when a read's events are refused.
struct Refuses<'a>(&'a mut Vec<String>);

impl Events for Refuses<'_> {
    fn event(&mut self, event: &SseEvent) {
        self.0.push(event.kind().to_owned());
    }

    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send {
        std::future::ready(ControlFlow::Break(()))
    }
}

#[tokio::test]
async fn a_refused_read_stops_the_stream_and_closes_its_connection() {
    let harness = Harness::new(1 << 20);
    let body = body("hello");
    let mut kinds = Vec::new();
    let mut call: Pin<Box<dyn Future<Output = Result<(), Failure>> + '_>> =
        Box::pin(tokio::task::unconstrained(stream(
            &harness.client,
            &body,
            &harness.bearer,
            ask(&harness.gate, key(1)),
            &harness.budget,
            Refuses(&mut kinds),
        )));
    assert!(poll(&mut call).is_pending());
    harness.grant(1);
    assert!(poll(&mut call).is_pending());
    let mut server = harness.accept();
    received(&mut server);
    let first = TEXT
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .expect("the fixture has an event")
        + 2;
    let mut bytes =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
    bytes.extend(format!("{first:x}\r\n").into_bytes());
    bytes.extend_from_slice(&TEXT[..first]);
    bytes.extend_from_slice(b"\r\n");
    reply(&mut server, &bytes);
    assert_eq!(poll(&mut call), Poll::Ready(Err(Failure::Protocol)));
    drop(call);
    assert_eq!(kinds, ["response.created"]);
    assert_eq!(received(&mut server), (Vec::new(), true));
}
