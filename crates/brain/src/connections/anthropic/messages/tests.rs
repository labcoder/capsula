//! Synthetic HTTP peers, polled in memory: no socket, credential store or provider calls.

use super::*;
use crate::connections::model::conn::Connect;
use serde_json::json;
use std::future::{Future, poll_fn, ready};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const END: &[u8] = b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent synthetic peer fault and hold switches"
)]
struct State {
    connect: bool,
    connects: usize,
    hold_write: bool,
    hold_flush: bool,
    fail_write: bool,
    flushed: bool,
    early: bool,
    response: Vec<u8>,
    read: usize,
    eof: bool,
    written: Vec<u8>,
    settled: usize,
    flushes: Vec<usize>,
    closes: Vec<usize>,
    events: Vec<String>,
    block_out: bool,
    close_out: bool,
}

type Shared = Arc<Mutex<State>>;

struct Io(Shared);

impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().expect("peer state");
        if !state.flushed && !state.early {
            return Poll::Pending;
        }
        let length = buf.remaining().min(state.response.len() - state.read);
        if length == 0 && !state.eof {
            return Poll::Pending;
        }
        buf.put_slice(&state.response[state.read..state.read + length]);
        state.read += length;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().expect("peer state");
        if state.hold_write {
            return Poll::Pending;
        }
        if state.fail_write {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        state.written.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().expect("peer state");
        if state.hold_flush {
            return Poll::Pending;
        }
        let settled = state.settled;
        state.flushes.push(settled);
        state.flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        let mut state = self.0.lock().expect("peer state");
        let settled = state.settled;
        state.closes.push(settled);
    }
}

struct Peer(Shared);

impl Connect for Peer {
    type Io = Io;
    fn connect(&self) -> impl Future<Output = io::Result<Io>> + Send {
        self.0.lock().expect("peer state").connects += 1;
        poll_fn(|_| {
            if self.0.lock().expect("peer state").connect {
                Poll::Ready(Ok(Io(Arc::clone(&self.0))))
            } else {
                Poll::Pending
            }
        })
    }
}

impl AnthropicConnect for Peer {}

struct Lease(Shared);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.lock().expect("peer state").settled += 1;
    }
}

struct Sink(Shared);
impl Events for Sink {
    fn event(&mut self, event: &crate::connections::model::sse::SseEvent) {
        self.0
            .lock()
            .expect("peer state")
            .events
            .push(event.kind().to_owned());
    }
    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send {
        poll_fn(|_| {
            let state = self.0.lock().expect("peer state");
            if state.close_out {
                Poll::Ready(ControlFlow::Break(()))
            } else if state.block_out {
                Poll::Pending
            } else {
                Poll::Ready(ControlFlow::Continue(()))
            }
        })
    }
}

struct Harness {
    state: Shared,
    client: ModelClient<Peer>,
    body: MessagesBody,
    key: Secret<AnthropicKey>,
}

impl Harness {
    fn new(response: Vec<u8>) -> Self {
        let state = Arc::new(Mutex::new(State {
            connect: true,
            response,
            ..State::default()
        }));
        Self {
            client: ModelClient::new(Peer(Arc::clone(&state))),
            state,
            body: MessagesBody::new(&json!({"model":"claude-sonnet-5-5", "messages":[{"role":"user","content":"SYNTHETIC_PROMPT"}], "max_tokens":2048,"stream":true,"system":[{"type":"text","text":"synthetic","cache_control":{"type":"ephemeral"}}]})).expect("synthetic request"),
            key: AnthropicKey::parse(Secret::new(Zeroizing::new(b"sk-ant-api-SYNTHETIC_KEY".to_vec()))).expect("synthetic key"),
        }
    }

    fn call<'a>(
        &'a self,
        run: &'a Budget,
        live: &'a Budget,
    ) -> impl Future<Output = Result<StopReason, Failure>> + 'a {
        tokio::task::unconstrained(stream(
            &self.client,
            &self.body,
            &self.key,
            ready(Some(Lease(Arc::clone(&self.state)))),
            run,
            live,
            Sink(Arc::clone(&self.state)),
        ))
    }
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn chunk(bytes: &[u8]) -> Vec<u8> {
    let mut framed = format!("{:x}\r\n", bytes.len()).into_bytes();
    framed.extend_from_slice(bytes);
    framed.extend_from_slice(b"\r\n");
    framed
}

#[tokio::test]
async fn fixed_wire_flush_and_explicit_retries_spend_shared_budgets() {
    let harness = Harness::new(response(200, END));
    let run = Budget::limited(Some(2));
    let live = Budget::limited(Some(3));
    for attempt in 1..=2 {
        assert_eq!(
            harness
                .call(&run, &live)
                .await
                .map_err(|error| format!("{error:?}")),
            Ok(StopReason::EndTurn)
        );
        let mut state = harness.state.lock().expect("peer state");
        assert_eq!(state.connects, attempt);
        assert_eq!(state.flushes, (0..attempt).collect::<Vec<_>>());
        assert_eq!(state.closes, (1..=attempt).collect::<Vec<_>>());
        let split = state
            .written
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("HTTP head")
            + 4;
        let head = std::str::from_utf8(&state.written[..split]).expect("ASCII head");
        assert_eq!(
            head,
            format!(
                "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nAuthorization: Bearer sk-ant-api-SYNTHETIC_KEY\r\nanthropic-version: 2023-06-01\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nAccept-Encoding: identity\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                harness.body.bytes().len()
            )
        );
        assert_eq!(&state.written[split..], harness.body.bytes());
        state.written.clear();
        state.read = 0;
        state.flushed = false;
    }
    assert!(matches!(
        harness.call(&run, &live).await,
        Err(Failure::Spent)
    ));
    assert_eq!(harness.state.lock().expect("peer state").connects, 2);
    assert!(
        live.reserve(),
        "spent run does not consume another live unit"
    );
    assert!(!live.reserve());
}

#[tokio::test]
async fn zero_caps_refusal_and_failed_upload_never_refund_or_retry() {
    for (run_cap, live_cap) in [(0, 1), (1, 0)] {
        let harness = Harness::new(response(200, END));
        assert!(matches!(
            harness
                .call(
                    &Budget::limited(Some(run_cap)),
                    &Budget::limited(Some(live_cap))
                )
                .await,
            Err(Failure::Spent)
        ));
        assert_eq!(harness.state.lock().expect("peer state").connects, 0);
    }
    let run = Budget::limited(Some(1));
    let live = Budget::limited(Some(1));
    let harness = Harness::new(response(200, END));
    harness.state.lock().expect("peer state").fail_write = true;
    assert!(matches!(
        harness.call(&run, &live).await,
        Err(Failure::Transport)
    ));
    assert!(!run.reserve());
    assert!(!live.reserve());
    let state = harness.state.lock().expect("peer state");
    assert_eq!(state.connects, 1);
    assert_eq!(state.closes, [0]);
    assert_eq!(state.settled, 1);
}

#[tokio::test]
async fn live_cap_is_shared_across_runs_and_admission_refusal_sends_nothing() {
    let harness = Harness::new(response(200, END));
    let live = Budget::limited(Some(1));
    let first = Budget::limited(Some(1));
    let second = Budget::limited(Some(1));
    assert_eq!(
        harness
            .call(&first, &live)
            .await
            .map_err(|error| format!("{error:?}")),
        Ok(StopReason::EndTurn)
    );
    assert!(matches!(
        harness.call(&second, &live).await,
        Err(Failure::Spent)
    ));
    assert_eq!(harness.state.lock().expect("peer state").connects, 1);
    assert!(
        !second.reserve(),
        "a live refusal conservatively spends the run reservation"
    );
    let untouched = Budget::limited(Some(1));
    let result = stream(
        &harness.client,
        &harness.body,
        &harness.key,
        ready(None::<Lease>),
        &untouched,
        &live,
        Sink(Arc::clone(&harness.state)),
    )
    .await;
    assert!(matches!(result, Err(Failure::Rebuild)));
    assert!(untouched.reserve());
    assert_eq!(harness.state.lock().expect("peer state").connects, 1);
}

#[tokio::test]
async fn dropping_a_pending_admission_touches_neither_socket_nor_budgets() {
    let harness = Harness::new(response(200, END));
    let run = Budget::limited(Some(1));
    let live = Budget::limited(Some(1));
    let mut call = Box::pin(stream(
        &harness.client,
        &harness.body,
        &harness.key,
        std::future::pending::<Option<Lease>>(),
        &run,
        &live,
        Sink(Arc::clone(&harness.state)),
    ));
    assert!(poll(call.as_mut()).is_pending());
    drop(call);
    let state = harness.state.lock().expect("peer state");
    assert_eq!(state.connects, 0);
    assert_eq!(state.settled, 0);
    assert!(run.reserve());
    assert!(live.reserve());
}

#[test]
fn header_controls_are_refused_before_request_assembly() {
    for key in ["", "bad\r\nkey", "bad key", "bad\u{7f}key", "badékey"] {
        assert!(matches!(request(key, b"{}"), Err(Failure::Credential)));
    }
    assert!(matches!(
        request(&"x".repeat(2049), b"{}"),
        Err(Failure::Credential)
    ));
    let exact = request(&"x".repeat(2048), b"{}").expect("inclusive key bound");
    assert!(exact.expose().ends_with(b"\r\n\r\n{}"));
}

#[tokio::test]
async fn refusal_body_bound_is_inclusive_without_forwarding_body_bytes() {
    for length in [1024 * 1024, 1024 * 1024 + 1] {
        let harness = Harness::new(response(429, &vec![b'x'; length]));
        let result = harness.call(&Budget::default(), &Budget::default()).await;
        if length == 1024 * 1024 {
            let Err(Failure::Provider(error)) = result else {
                panic!("bounded refusal");
            };
            assert_eq!(error.kind(), super::super::error::Kind::RateLimited);
        } else {
            assert!(matches!(result, Err(Failure::Protocol)));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn connect_write_and_flush_deadlines_close_before_settling() {
    for held in 0..3 {
        let harness = Harness::new(Vec::new());
        {
            let mut state = harness.state.lock().expect("peer state");
            state.connect = held != 0;
            state.hold_write = held == 1;
            state.hold_flush = held == 2;
        }
        let run = Budget::limited(Some(1));
        let live = Budget::limited(Some(1));
        let mut call = Box::pin(harness.call(&run, &live));
        assert!(poll(call.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(matches!(
            poll(call.as_mut()),
            Poll::Ready(Err(Failure::Transport))
        ));
        let state = harness.state.lock().expect("peer state");
        assert_eq!(state.closes, if held == 0 { vec![] } else { vec![0] });
        assert_eq!(state.settled, 1);
        assert!(!run.reserve());
        assert!(!live.reserve());
    }
}

#[tokio::test]
async fn dropping_each_wait_closes_its_connection_and_keeps_budget_spent() {
    for held in 0..8 {
        let harness = Harness::new(if held == 4 {
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
        } else if held == 5 {
            response(200, END)
        } else if held >= 6 {
            b"HTTP/1.1 429 Limited\r\nContent-Length: 20\r\n\r\n".to_vec()
        } else {
            Vec::new()
        });
        {
            let mut state = harness.state.lock().expect("peer state");
            state.connect = held != 0;
            state.hold_write = held == 1;
            state.hold_flush = held == 2;
            state.block_out = held == 5;
            state.early = held == 6;
        }
        let run = Budget::limited(Some(1));
        let live = Budget::limited(Some(1));
        let mut call = Box::pin(harness.call(&run, &live));
        assert!(poll(call.as_mut()).is_pending(), "wait {held}");
        drop(call);
        let state = harness.state.lock().expect("peer state");
        assert_eq!(
            state.closes,
            if held == 0 {
                vec![]
            } else {
                vec![usize::from(held >= 3 && held != 6)]
            }
        );
        assert_eq!(state.settled, 1);
        assert!(!run.reserve());
        assert!(!live.reserve());
    }
}

#[tokio::test(start_paused = true)]
async fn head_body_and_refusal_reads_end_on_deadline_or_eof() {
    for (prefix, seconds) in [
        (Vec::new(), 60),
        (
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 20\r\n\r\n"
                .to_vec(),
            60,
        ),
        (
            b"HTTP/1.1 429 Limited\r\nContent-Length: 20\r\n\r\n".to_vec(),
            10,
        ),
    ] {
        for eof in [false, true] {
            let harness = Harness::new(prefix.clone());
            harness.state.lock().expect("peer state").eof = eof;
            let run = Budget::default();
            let live = Budget::default();
            let mut call = Box::pin(harness.call(&run, &live));
            if !eof {
                assert!(poll(call.as_mut()).is_pending());
                tokio::time::advance(Duration::from_secs(seconds)).await;
            }
            let expected = if eof { "Truncated" } else { "Transport" };
            let Poll::Ready(Err(error)) = poll(call.as_mut()) else {
                panic!("read must end");
            };
            assert_eq!(format!("{error:?}"), expected);
            assert_eq!(harness.state.lock().expect("peer state").closes, [1]);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn backpressure_is_bounded_and_receiver_close_ends_it() {
    for close in [false, true] {
        let harness = Harness::new(response(200, END));
        harness.state.lock().expect("peer state").block_out = true;
        let run = Budget::default();
        let live = Budget::default();
        let mut call = Box::pin(harness.call(&run, &live));
        assert!(poll(call.as_mut()).is_pending());
        if close {
            harness.state.lock().expect("peer state").close_out = true;
        } else {
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert!(matches!(
            (close, poll(call.as_mut())),
            (true, Poll::Ready(Err(Failure::Protocol)))
                | (false, Poll::Ready(Err(Failure::BackpressureTimeout)))
        ));
        assert_eq!(harness.state.lock().expect("peer state").closes, [1]);
    }
}

#[tokio::test(start_paused = true)]
async fn pings_reset_byte_idle_but_cannot_keep_an_empty_stream_alive() {
    let harness = Harness::new(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec(),
    );
    let run = Budget::default();
    let live = Budget::default();
    let mut call = Box::pin(harness.call(&run, &live));
    assert!(poll(call.as_mut()).is_pending());
    for _ in 0..11 {
        tokio::time::advance(Duration::from_secs(50)).await;
        harness
            .state
            .lock()
            .expect("peer state")
            .response
            .extend(chunk(b"data: {\"type\":\"ping\"}\n\n"));
        assert!(poll(call.as_mut()).is_pending());
    }
    tokio::time::advance(Duration::from_secs(50)).await;
    assert!(matches!(
        poll(call.as_mut()),
        Poll::Ready(Err(Failure::Transport))
    ));
    assert_eq!(
        harness.state.lock().expect("peer state").events,
        Vec::<String>::new()
    );
}

#[tokio::test(start_paused = true)]
async fn real_content_resets_content_idle_without_a_total_stream_deadline() {
    let harness = Harness::new(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec(),
    );
    let run = Budget::default();
    let live = Budget::default();
    let mut call = Box::pin(harness.call(&run, &live));
    assert!(poll(call.as_mut()).is_pending());
    for _ in 0..3 {
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(50)).await;
            harness
                .state
                .lock()
                .expect("peer state")
                .response
                .extend(chunk(b"data: {\"type\":\"ping\"}\n\n"));
            assert!(poll(call.as_mut()).is_pending());
        }
        tokio::time::advance(Duration::from_secs(50)).await;
        harness.state.lock().expect("peer state").response.extend(chunk(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"synthetic\"}}\n\n"));
        assert!(poll(call.as_mut()).is_pending());
    }
    harness
        .state
        .lock()
        .expect("peer state")
        .response
        .extend(chunk(END));
    let Poll::Ready(result) = poll(call.as_mut()) else {
        panic!("terminal ends the stream");
    };
    assert_eq!(
        result.map_err(|error| format!("{error:?}")),
        Ok(StopReason::EndTurn)
    );
    assert_eq!(
        harness.state.lock().expect("peer state").events,
        [
            "content_block_delta",
            "content_block_delta",
            "content_block_delta",
            "message_delta",
            "message_stop"
        ]
    );
}

#[tokio::test]
async fn early_provider_refusal_closes_before_lease_and_recovery_stays_closed() {
    let captured = crate::test_support::Captured::default();
    let _capturing = captured.install();
    let harness = Harness::new(response(
        429,
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"SYNTHETIC_PROVIDER"}}"#,
    ));
    harness.state.lock().expect("peer state").early = true;
    let result = harness.call(&Budget::default(), &Budget::default()).await;
    let Err(Failure::Provider(error)) = result else {
        panic!("expected provider refusal");
    };
    assert_eq!(error.kind(), super::super::error::Kind::RateLimited);
    assert_eq!(
        error.user_error().key(),
        kumochi_protocol::message::ErrorKey::AnthropicRateLimited
    );
    let state = harness.state.lock().expect("peer state");
    assert_eq!(state.written, Vec::<u8>::new());
    assert_eq!(state.closes, [0]);
    assert_eq!(state.settled, 1);
    let shown = format!("{error:?} {error}");
    assert!(!shown.contains("SYNTHETIC_PROVIDER"));
    assert!(!shown.contains(format!("{:?}", b"SYNTHETIC_PROVIDER").trim_matches(['[', ']'])));
    let logged = captured.text();
    for marker in ["SYNTHETIC_PROVIDER", "SYNTHETIC_PROMPT", "SYNTHETIC_KEY"] {
        assert!(!logged.contains(marker));
        assert!(!logged.contains(format!("{:?}", marker.as_bytes()).trim_matches(['[', ']'])));
    }
}
