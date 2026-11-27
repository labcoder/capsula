//! The transport against real sockets on 127.0.0.1:0, the only address a test may use.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::message::ModelSlug;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::request::{ChatMessage, Role};
use super::*;

const TEXT: &[u8] = include_bytes!("../../../fixtures/local/synthetic-llama-server-text.sse");
const EVENTS: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
/// How long a test waits to be sure something didn't happen.
const QUIET: Duration = Duration::from_millis(200);

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
    (listener, LoopbackEndpoint::for_tests(&format!("127.0.0.1:{port}")))
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
async fn a_text_round_sends_the_exact_request_with_no_credential_and_streams_its_text() {
    let (listener, endpoint) = listener().await;
    let server = serve(listener, events(TEXT));
    assert_eq!(run(endpoint).await, ("Hello from loopback.".to_owned(), Ok(())));
    let request = server.await.expect("server");
    assert_eq!(request, *body().request(endpoint));
    let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
    assert!(!head.contains("authorization") && !head.contains("cookie"), "{head}");
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
    server.await.expect("server");
    assert!(no_connection(&target).await);
}

#[tokio::test]
async fn a_refusal_is_its_status_alone_and_its_body_is_never_waited_for() {
    for (status, failure) in [
        ("401 Unauthorized", Failure::AuthRequired),
        ("403 Forbidden", Failure::AuthRequired),
        ("404 Not Found", Failure::Refused { status: 404 }),
        ("500 Internal Server Error", Failure::Refused { status: 500 }),
        ("204 No Content", Failure::Protocol),
    ] {
        let (listener, endpoint) = listener().await;
        let hold = Arc::new(tokio::sync::Notify::new());
        let release = Arc::clone(&hold);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_request(&mut stream).await;
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: 1000000\r\n\r\n{{\"error\":");
            stream.write_all(head.as_bytes()).await.expect("write");
            release.notified().await;
        });
        assert_eq!(run(endpoint).await, (String::new(), Err(failure)), "{status}");
        hold.notify_one();
        server.await.expect("server");
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
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            format!("{done:x}\r\n").as_bytes(),
            TEXT,
            b"\r\n",
        ]
        .concat(),
    ] {
        let (listener, endpoint) = listener().await;
        let server = serve(listener, response);
        assert_eq!(run(endpoint).await, ("Hello from loopback.".to_owned(), Err(Failure::Truncated)));
        server.await.expect("server");
    }
}

#[tokio::test]
async fn a_body_that_is_not_an_event_stream_is_refused() {
    let (listener, endpoint) = listener().await;
    let response = [b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".as_slice(), TEXT].concat();
    let server = serve(listener, response);
    assert_eq!(run(endpoint).await, (String::new(), Err(Failure::Protocol)));
    server.await.expect("server");
}

#[tokio::test]
async fn no_grant_or_no_budget_connects_to_nothing() {
    let (listener, endpoint) = listener().await;
    let budget = Budget::limited(Some(1));
    let refused = stream(endpoint, &body(), async { None::<()> }, &budget, |_: &str| {}).await;
    assert_eq!(refused, Err(Failure::Rebuild));
    assert_eq!(budget.spent(), 0);
    let spent = Budget::limited(Some(0));
    let refused = stream(endpoint, &body(), async { Some(()) }, &spent, |_: &str| {}).await;
    assert_eq!(refused, Err(Failure::Spent));
    assert!(no_connection(&listener).await);
}

#[tokio::test(start_paused = true)]
async fn a_grant_that_never_comes_ends_at_the_round_deadline_with_nothing_spent() {
    let endpoint = LoopbackEndpoint::for_tests("127.0.0.1:9");
    let budget = Budget::limited(Some(1));
    let ended = stream(endpoint, &body(), std::future::pending::<Option<()>>(), &budget, |_: &str| {}).await;
    assert_eq!(ended, Err(Failure::Deadline));
    assert_eq!(budget.spent(), 0);
}

struct Stalled;

impl Sink for Stalled {
    fn text(&mut self, _: &str) {}

    fn taken(&mut self) -> impl Future<Output = ControlFlow<()>> + Send {
        std::future::pending()
    }
}

#[tokio::test]
async fn text_that_is_never_taken_ends_the_round_by_its_deadline() {
    let (listener, endpoint) = listener().await;
    let server = serve(listener, events(TEXT));
    let ended = stream_by(endpoint, &body(), async { Some(()) }, &Budget::default(), Stalled, QUIET).await;
    assert_eq!(ended, Err(Failure::Deadline));
    server.await.expect("server");
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
async fn stopping_a_round_closes_its_connection_and_settles_its_lease_as_written() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        read_request(&mut stream).await;
        stream.write_all(EVENTS.as_bytes()).await.expect("write");
        let mut rest = [0; 16];
        stream.read(&mut rest).await.expect("read")
    });
    let wrote = Arc::new(AtomicBool::new(false));
    let probe = Probe(Arc::clone(&wrote));
    let (body, budget) = (body(), Budget::default());
    let round = stream(endpoint, &body, async { Some(probe) }, &budget, |_: &str| {});
    assert!(timeout(QUIET, round).await.is_err(), "the round waits for text");
    assert_eq!(server.await.expect("server"), 0, "the server sees the close");
    assert!(wrote.load(Ordering::SeqCst));
}
