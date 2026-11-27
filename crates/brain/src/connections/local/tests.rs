//! The transport against real sockets on 127.0.0.1:0, the only address a test may use.

use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use kumochi_protocol::message::ModelSlug;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::request::{ChatMessage, Role};
use super::*;

const TEXT: &[u8] = include_bytes!("../../../fixtures/local/synthetic-llama-server-text.sse");
const EVENTS: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
/// How long a test waits to be sure something didn't happen.
const QUIET: Duration = Duration::from_millis(200);
/// The most a test waits for its server, so a regression fails rather than hangs.
const BOUND: Duration = Duration::from_secs(10);

fn body() -> ChatBody {
    ChatBody::new(
        &ModelSlug::new("local-model").expect("slug"),
        &[ChatMessage {
            role: Role::User,
            text: "Hi",
        }],
    )
    .expect("body")
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

/// One connection: reads the request, writes \`response\`, closes. Returns the request.
fn serve(listener: TcpListener, response: Vec<u8>) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut stream).await;
        stream.write_all(&response).await.expect("write");
        request
    })
}

async fn join<T>(server: JoinHandle<T>) -> T {
    timeout(BOUND, server)
        .await
        .expect("the server finished")
        .expect("server")
}

fn events(stream: &[u8]) -> Vec<u8> {
    [EVENTS.as_bytes(), stream].concat()
}

async fn run(endpoint: LoopbackEndpoint) -> (String, Result<(), Failure>) {
    let mut text = String::new();
    let end = stream(
        endpoint,
        &body(),
        async { Some(()) },
        &Budget::default(),
        |delta: &str| text.push_str(delta),
    )
    .await;
    (text, end)
}

async fn no_connection(listener: &TcpListener) -> bool {
    timeout(QUIET, listener.accept()).await.is_err()
}

#[tokio::test]
async fn a_text_round_sends_one_uncredentialed_post_and_streams_its_text() {
    let (listener, endpoint) = listener().await;
    let server = serve(listener, events(TEXT));
    assert_eq!(
        run(endpoint).await,
        ("Hello from loopback.".to_owned(), Ok(()))
    );
    let request = String::from_utf8(join(server).await).expect("text");
    let (head, json) = request.split_once("\r\n\r\n").expect("a head");
    let mut lines = head.split("\r\n");
    assert_eq!(lines.next(), Some("POST /v1/chat/completions HTTP/1.1"));
    let mut names: Vec<String> = lines
        .map(|line| {
            line.split_once(": ")
                .expect("a field")
                .0
                .to_ascii_lowercase()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "accept",
            "accept-encoding",
            "connection",
            "content-length",
            "content-type",
            "host"
        ]
    );
    assert!(head.contains(&format!("\r\nHost: {}\r\n", endpoint.authority())));
    let json: serde_json::Value = serde_json::from_str(json).expect("json");
    assert_eq!(
        json,
        serde_json::json!({"model": "local-model", "messages": [{"role": "user", "content": "Hi"}], "stream": true})
    );
}

#[tokio::test]
async fn a_redirect_is_refused_and_never_followed() {
    let (target, target_endpoint) = listener().await;
    let (listener, endpoint) = listener().await;
    let location = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/v1/chat/completions\r\nContent-Length: 0\r\n\r\n",
        target_endpoint.authority()
    );
    let server = serve(listener, location.into_bytes());
    assert_eq!(run(endpoint).await, (String::new(), Err(Failure::Protocol)));
    join(server).await;
    assert!(no_connection(&target).await);
}

#[tokio::test]
async fn a_refusal_is_its_status_alone_and_its_body_is_never_waited_for() {
    for (status, failure) in [
        ("401 Unauthorized", Failure::AuthRequired),
        ("403 Forbidden", Failure::AuthRequired),
        ("404 Not Found", Failure::Refused { status: 404 }),
        (
            "500 Internal Server Error",
            Failure::Refused { status: 500 },
        ),
        ("204 No Content", Failure::Protocol),
    ] {
        let (listener, endpoint) = listener().await;
        let hold = Arc::new(Notify::new());
        let release = Arc::clone(&hold);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_request(&mut stream).await;
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: 1000000\r\n\r\n{{\"error\":");
            stream.write_all(head.as_bytes()).await.expect("write");
            release.notified().await;
        });
        assert_eq!(
            run(endpoint).await,
            (String::new(), Err(failure)),
            "{status}"
        );
        hold.notify_one();
        join(server).await;
    }
}

#[tokio::test]
async fn a_body_that_ends_short_of_its_framing_is_truncated() {
    let done = TEXT.len();
    for response in [
        [
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: ".as_slice(),
            (done + 10).to_string().as_bytes(),
            b"\r\n\r\n",
            TEXT,
        ]
        .concat(),
        [
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
                .as_slice(),
            format!("{done:x}\r\n").as_bytes(),
            TEXT,
            b"\r\n",
        ]
        .concat(),
    ] {
        let (listener, endpoint) = listener().await;
        let server = serve(listener, response);
        assert_eq!(
            run(endpoint).await,
            ("Hello from loopback.".to_owned(), Err(Failure::Truncated))
        );
        join(server).await;
    }
}

#[tokio::test]
async fn a_body_that_is_not_an_event_stream_is_refused() {
    let (listener, endpoint) = listener().await;
    let response = [
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
            .as_slice(),
        TEXT,
    ]
    .concat();
    let server = serve(listener, response);
    assert_eq!(run(endpoint).await, (String::new(), Err(Failure::Protocol)));
    join(server).await;
}

#[tokio::test]
async fn no_grant_or_no_budget_connects_to_nothing() {
    let (listener, endpoint) = listener().await;
    let budget = Budget::limited(Some(1));
    let refused = stream(
        endpoint,
        &body(),
        async { None::<()> },
        &budget,
        |_: &str| {},
    )
    .await;
    assert_eq!(refused, Err(Failure::Rebuild));
    assert_eq!(budget.spent(), 0);
    let spent = Budget::limited(Some(0));
    let refused = stream(endpoint, &body(), async { Some(()) }, &spent, |_: &str| {}).await;
    assert_eq!(refused, Err(Failure::Spent));
    assert!(no_connection(&listener).await);
}

/// A lease that refuses every write it's asked to admit.
struct Closed;

impl Lease for Closed {
    type Write = ();

    fn admit(&self) -> Option<()> {
        None
    }

    fn wrote(&mut self) {}
}

#[tokio::test]
async fn a_lease_that_admits_no_write_sends_no_byte() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.expect("read");
        bytes
    });
    let ended = stream(
        endpoint,
        &body(),
        async { Some(Closed) },
        &Budget::default(),
        |_: &str| {},
    )
    .await;
    assert_eq!(ended, Err(Failure::Rebuild));
    assert_eq!(join(server).await, b"");
}

#[tokio::test(start_paused = true)]
async fn a_grant_ready_only_after_the_deadline_spends_nothing() {
    let (_listener, endpoint) = listener().await;
    let budget = Budget::limited(Some(1));
    let (grant, granted) = oneshot::channel::<()>();
    let (body, ask) = (body(), async { granted.await.ok() });
    let mut round = std::pin::pin!(stream(endpoint, &body, ask, &budget, |_: &str| {}));
    poll_fn(|cx| {
        assert!(round.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::advance(ROUND_DEADLINE + Duration::from_secs(1)).await;
    grant.send(()).expect("the round waits for its grant");
    assert_eq!(round.await, Err(Failure::Deadline));
    assert_eq!(budget.spent(), 0);
}

/// A sink that never takes its text, and says when it's been asked to.
struct Stalled(Arc<AtomicBool>);

impl Sink for Stalled {
    fn text(&mut self, _: &str) {}

    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send {
        self.0.store(true, Ordering::SeqCst);
        std::future::pending()
    }
}

#[tokio::test]
async fn text_that_is_never_taken_ends_the_round_by_its_deadline() {
    let (listener, endpoint) = listener().await;
    let server = serve(listener, events(TEXT));
    let waited = Arc::new(AtomicBool::new(false));
    let sink = Stalled(Arc::clone(&waited));
    let ended = stream_by(
        endpoint,
        &body(),
        async { Some(()) },
        &Budget::default(),
        sink,
        BOUND / 4,
    )
    .await;
    assert_eq!(ended, Err(Failure::Deadline));
    assert!(
        waited.load(Ordering::SeqCst),
        "the round waited on the sink"
    );
    join(server).await;
}

#[tokio::test]
async fn a_server_error_beside_untaken_text_is_still_the_server_error() {
    let (listener, endpoint) = listener().await;
    let stream_bytes = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\ndata: {\"error\":{\"message\":\"x\"}}\n\n";
    let server = serve(listener, events(stream_bytes));
    let waited = Arc::new(AtomicBool::new(false));
    let sink = Stalled(Arc::clone(&waited));
    let ended = timeout(
        BOUND,
        stream(
            endpoint,
            &body(),
            async { Some(()) },
            &Budget::default(),
            sink,
        ),
    )
    .await;
    assert_eq!(ended.expect("no wait on the sink"), Err(Failure::Provider));
    assert!(!waited.load(Ordering::SeqCst));
    join(server).await;
}

struct Probe(Arc<AtomicBool>);

impl Lease for Probe {
    type Write = ();

    fn admit(&self) -> Option<()> {
        Some(())
    }

    fn wrote(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn stopping_a_round_closes_its_connection_after_its_request_was_written() {
    let (listener, endpoint) = listener().await;
    let (answered, heard) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        read_request(&mut stream).await;
        stream.write_all(EVENTS.as_bytes()).await.expect("write");
        answered.send(()).expect("the round is waiting");
        let mut rest = [0; 16];
        // A close with the head still unread arrives as a reset on Windows.
        match stream.read(&mut rest).await {
            Ok(read) => read == 0,
            Err(error) => error.kind() == std::io::ErrorKind::ConnectionReset,
        }
    });
    let wrote = Arc::new(AtomicBool::new(false));
    let probe = Probe(Arc::clone(&wrote));
    let (body, budget) = (body(), Budget::default());
    let round = stream(
        endpoint,
        &body,
        async { Some(probe) },
        &budget,
        |_: &str| {},
    );
    tokio::select! {
        ended = round => panic!("the round ended on its own: {ended:?}"),
        heard = heard => heard.expect("the server answered"),
    }
    assert!(join(server).await, "the server sees the close");
    assert!(wrote.load(Ordering::SeqCst));
}
