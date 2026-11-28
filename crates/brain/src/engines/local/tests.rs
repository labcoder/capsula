//! The local engine against real sockets on 127.0.0.1:0, the only address a test may use.

use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::message::{
    ConnectionKind, Effort, ErrorKey, ModelChoice, ModelSlug, RunId, ThreadId, UserError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::{LocalEngine, deliver};
use crate::connections::local::Failure;
use crate::connections::local::endpoint::LoopbackEndpoint;
use crate::connections::model::budget::Budget;
use crate::context::{InputItem, Role, ToolSchema};
use crate::coordinator::{Input, LeaseId};
use crate::engines::{
    Answer, Asks, Engine, EngineFailure, Finish, RunRequest, SendGate, SendLease, TurnEvent,
};

const TEXT: &[u8] = include_bytes!("../../../fixtures/local/synthetic-llama-server-text.sse");
const EVENTS: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
/// How long a test waits to be sure something didn't happen.
const QUIET: Duration = Duration::from_millis(200);
/// The most a test waits, so a regression fails rather than hangs.
const BOUND: Duration = Duration::from_secs(10);
const MODEL: &str = "local-model";

fn on(connection: ConnectionKind, model: &str) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(model).expect("slug"),
        effort: Effort::Low,
        connection,
    }
}

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

fn request(input: Vec<InputItem>) -> RunRequest {
    let mut request = RunRequest::for_tests(RunId(7), ThreadId(2), "Be brief.");
    request.request.choice = Some(on(ConnectionKind::LocalChat, MODEL));
    request.request.input = input;
    request
}

async fn listener() -> (TcpListener, LoopbackEndpoint) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    (
        listener,
        LoopbackEndpoint::for_tests(&format!("127.0.0.1:{port}")),
    )
}

/// The request's head and its Content-Length bytes of body.
async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse().ok())
                .expect("a content length");
            if bytes.len() >= end + 4 + length {
                return bytes;
            }
        }
        let read = stream.read(&mut chunk).await.expect("read");
        assert_ne!(read, 0, "the request ended early");
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// One connection: reads the request, writes `stream` after the event-stream head, closes.
fn serve(listener: TcpListener, stream: &'static [u8]) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        socket
            .write_all(&[EVENTS.as_bytes(), stream].concat())
            .await
            .expect("write");
        request
    })
}

/// A gate the coordinator grants every ask of.
fn granting(request: &RunRequest) -> SendGate {
    let key = request.key;
    let asks = Asks::default();
    let (inputs_tx, mut inputs) = mpsc::unbounded_channel();
    let gate = SendGate::new(key, Arc::clone(&asks), inputs_tx.clone());
    tokio::spawn(async move {
        while let Some(input) = inputs.recv().await {
            if matches!(input, Input::SendReady { .. }) {
                let (_, answer) = asks.lock().expect("asks").pop().expect("an ask");
                let lease = SendLease::new(LeaseId { key, send: 1 }, inputs_tx.clone());
                answer.send(Answer::Grant(lease)).ok();
            }
        }
    });
    gate
}

fn engine(endpoint: LoopbackEndpoint) -> LocalEngine {
    LocalEngine::new(
        endpoint,
        ModelSlug::new(MODEL).expect("slug"),
        Budget::default(),
    )
}

/// Every event of a round, after waiting `late` before taking the first.
async fn played(
    engine: &LocalEngine,
    request: RunRequest,
    gate: SendGate,
    late: Duration,
) -> Vec<TurnEvent> {
    let (events_tx, mut events) = mpsc::channel(1);
    let round = tokio::spawn(engine.start(request, events_tx, gate));
    tokio::time::sleep(late).await;
    let mut seen = Vec::new();
    while let Some(event) = timeout(BOUND, events.recv()).await.expect("an event") {
        seen.push(event);
    }
    timeout(BOUND, round).await.expect("ended").expect("round");
    seen
}

fn text(events: &[TurnEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn failed(failure: Failure) -> TurnEvent {
    TurnEvent::Finished(Finish::Failed(EngineFailure::Local(failure)))
}

#[tokio::test]
async fn a_local_task_sends_exactly_its_text_and_streams_the_answer() {
    let (listener, endpoint) = listener().await;
    let server = serve(listener, TEXT);
    let request = request(vec![
        message(Role::User, "Hi"),
        message(Role::Assistant, "Hello"),
        message(Role::User, "Again"),
    ]);
    let gate = granting(&request);
    let events = played(&engine(endpoint), request, gate, Duration::ZERO).await;
    assert_eq!(text(&events), "Hello from loopback.");
    assert_eq!(events.last(), Some(&TurnEvent::Finished(Finish::Completed)));
    let sent = timeout(BOUND, server)
        .await
        .expect("served")
        .expect("server");
    let (_, json) = std::str::from_utf8(&sent)
        .expect("text")
        .split_once("\r\n\r\n")
        .expect("a head");
    assert_eq!(
        json,
        r#"{"model":"local-model","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi"},{"role":"assistant","content":"Hello"},{"role":"user","content":"Again"}],"stream":true}"#
    );
}

#[tokio::test]
async fn anything_but_plain_text_is_refused_before_any_connection() {
    let call = crate::engines::CallId::new("call_1").expect("call");
    let tool = ToolSchema {
        name: "draw",
        description: "",
        parameters: serde_json::Value::Null,
    };
    let rows: Vec<(&str, RunRequest)> = vec![
        (
            "developer",
            request(vec![
                message(Role::User, "Hi"),
                message(Role::Developer, "Summary"),
            ]),
        ),
        (
            "call",
            request(vec![
                message(Role::User, "Hi"),
                InputItem::FunctionCall {
                    call: call.clone(),
                    name: "draw".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ]),
        ),
        (
            "output",
            request(vec![
                message(Role::User, "Hi"),
                InputItem::FunctionOutput {
                    call,
                    output: "{}".to_owned(),
                },
            ]),
        ),
        ("tools", {
            let mut offered = request(vec![message(Role::User, "Hi")]);
            offered.request.tools = vec![tool];
            offered
        }),
    ];
    for (row, request) in rows {
        let (listener, endpoint) = listener().await;
        let gate = granting(&request);
        let events = played(&engine(endpoint), request, gate, Duration::ZERO).await;
        assert_eq!(events, vec![failed(Failure::Request)], "{row}");
        assert!(
            timeout(QUIET, listener.accept()).await.is_err(),
            "{row} connected"
        );
    }
}

/// A gate that asked would answer Rebuild, so `Request` shows the refusal came first.
#[tokio::test]
async fn a_request_off_the_engines_model_is_refused_before_it_asks() {
    for choice in [
        None,
        Some(on(ConnectionKind::ChatGpt, MODEL)),
        Some(on(ConnectionKind::LocalChat, "other-model")),
    ] {
        let (listener, endpoint) = listener().await;
        let mut request = request(vec![message(Role::User, "Hi")]);
        request.request.choice = choice.clone();
        let gate = SendGate::closed(request.key);
        let events = played(&engine(endpoint), request, gate, Duration::ZERO).await;
        assert_eq!(events, vec![failed(Failure::Request)], "{choice:?}");
        assert!(timeout(QUIET, listener.accept()).await.is_err());
    }
}

/// One event both hands out text and stops at the length limit; a consumer that takes nothing
/// for a while still gets the text before the failure.
#[tokio::test]
async fn text_beside_a_failure_arrives_before_it() {
    const LENGTH: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"length\"}]}\n\n";
    let (listener, endpoint) = listener().await;
    let server = serve(listener, LENGTH);
    let request = request(vec![message(Role::User, "Hi")]);
    let gate = granting(&request);
    let events = played(&engine(endpoint), request, gate, QUIET).await;
    assert_eq!(
        events,
        vec![
            TurnEvent::TextDelta("Hi".to_owned()),
            failed(Failure::Incomplete)
        ]
    );
    timeout(BOUND, server)
        .await
        .expect("served")
        .expect("server");
}

#[tokio::test]
async fn a_consumer_that_never_takes_gets_nothing_late() {
    let (events_tx, mut events) = mpsc::channel(1);
    events_tx
        .send(TurnEvent::TextDelta("earlier".to_owned()))
        .await
        .expect("queued");
    timeout(
        BOUND,
        deliver(&events_tx, "pending".to_owned(), Finish::Completed, QUIET),
    )
    .await
    .expect("delivery gave up in time");
    drop(events_tx);
    assert_eq!(
        events.recv().await,
        Some(TurnEvent::TextDelta("earlier".to_owned()))
    );
    assert_eq!(events.recv().await, None);
}

/// A read whose wait for room was cut, by its own cutoff or the round's deadline, already had
/// its wait: only the failure follows.
#[tokio::test]
async fn text_cut_by_its_own_wait_is_never_offered_again() {
    for failure in [Failure::Backpressure, Failure::Deadline] {
        let (events_tx, mut events) = mpsc::channel(2);
        let cut = Finish::Failed(EngineFailure::Local(failure));
        timeout(BOUND, deliver(&events_tx, "cut".to_owned(), cut, QUIET))
            .await
            .expect("delivered");
        drop(events_tx);
        assert_eq!(events.recv().await, Some(failed(failure)), "{failure:?}");
        assert_eq!(events.recv().await, None, "{failure:?}");
    }
}

/// A read's text whose room opens on the same poll as its cutoff stays unsent, so the round
/// ends as `Backpressure`.
#[tokio::test(start_paused = true)]
async fn room_that_opens_as_a_reads_wait_ends_takes_nothing() {
    let (events_tx, mut events) = mpsc::channel(1);
    events_tx
        .send(TurnEvent::TextDelta("earlier".to_owned()))
        .await
        .expect("queued");
    let sender = events_tx.clone();
    let wait = tokio::spawn(async move {
        let mut text = "late".to_owned();
        let mut out = super::Pending {
            text: &mut text,
            events: &sender,
        };
        let taken = crate::connections::local::taken_within(&mut out, QUIET).await;
        (taken, text)
    });
    tokio::task::yield_now().await;
    assert_eq!(
        events.try_recv(),
        Ok(TurnEvent::TextDelta("earlier".to_owned()))
    );
    tokio::time::advance(QUIET).await;
    let (taken, text) = wait.await.expect("wait");
    assert_eq!((taken, text.as_str()), (Err(Failure::Backpressure), "late"));
    drop(events_tx);
    assert_eq!(events.recv().await, None);
}

/// Room that opens on the same poll as the cutoff carries nothing.
#[tokio::test(start_paused = true)]
async fn room_that_opens_as_the_wait_ends_carries_nothing() {
    let (events_tx, mut events) = mpsc::channel(1);
    events_tx
        .send(TurnEvent::TextDelta("earlier".to_owned()))
        .await
        .expect("queued");
    let sender = events_tx.clone();
    let delivery = tokio::spawn(async move {
        deliver(&sender, "pending".to_owned(), Finish::Completed, QUIET).await;
    });
    tokio::task::yield_now().await;
    assert_eq!(
        events.try_recv(),
        Ok(TurnEvent::TextDelta("earlier".to_owned()))
    );
    tokio::time::advance(QUIET).await;
    delivery.await.expect("delivery");
    drop(events_tx);
    assert_eq!(events.recv().await, None);
}

/// Esc drops the round, which closes its connection mid-answer.
#[tokio::test]
async fn dropping_the_round_closes_its_connection() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        read_request(&mut socket).await;
        let first = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";
        socket
            .write_all(&[EVENTS.as_bytes(), first].concat())
            .await
            .expect("write");
        let mut rest = [0; 64];
        socket.read(&mut rest).await.expect("read")
    });
    let request = request(vec![message(Role::User, "Hi")]);
    let gate = granting(&request);
    let (events_tx, mut events) = mpsc::channel(1);
    let round = tokio::spawn(engine(endpoint).start(request, events_tx, gate));
    assert_eq!(
        timeout(BOUND, events.recv()).await.expect("text"),
        Some(TurnEvent::TextDelta("Hi".to_owned()))
    );
    round.abort();
    assert_eq!(
        timeout(BOUND, server)
            .await
            .expect("closed")
            .expect("server"),
        0
    );
}

#[test]
fn no_local_failure_offers_an_action() {
    for (failure, key) in [
        (Failure::Dormant, ErrorKey::LocalOff),
        (Failure::Endpoint, ErrorKey::LocalUnfinished),
        (Failure::Request, ErrorKey::RequestRefused),
        (Failure::Rebuild, ErrorKey::LocalUnfinished),
        (Failure::Spent, ErrorKey::LiveBudgetSpent),
        (Failure::Transport, ErrorKey::LocalUnfinished),
        (Failure::Withheld, ErrorKey::LocalUnfinished),
        (Failure::AuthRequired, ErrorKey::LocalRefused),
        (Failure::Refused { status: 500 }, ErrorKey::LocalRefused),
        (Failure::Provider, ErrorKey::LocalRefused),
        (Failure::Protocol, ErrorKey::LocalUnreadable),
        (Failure::Truncated, ErrorKey::LocalUnfinished),
        (Failure::Incomplete, ErrorKey::LocalIncomplete),
        (Failure::Backpressure, ErrorKey::LocalUnfinished),
        (Failure::Deadline, ErrorKey::LocalUnfinished),
    ] {
        assert_eq!(
            failure.user_error(),
            UserError::new(key, Vec::new(), Vec::new()).expect("valid"),
            "{failure:?}"
        );
    }
}
