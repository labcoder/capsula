use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use kumochi_protocol::Secret;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;
use zeroize::Zeroizing;

use super::callback::{self, CallbackError, Event};
use super::{Authorization, Intent, LaunchError, LaunchTarget, Launcher, SignIn, SignInError};
use crate::connections::records::{Account, Tokens};
use crate::test_support::{Captured, SENTINEL};

#[derive(Default)]
struct RecordingLauncher(RefCell<Option<String>>);

impl Launcher for RecordingLauncher {
    fn launch(&self, target: &LaunchTarget) -> Result<(), LaunchError> {
        let url = Url::parse(target.url()).expect("authorization URL");
        let redirect = url
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .expect("redirect")
            .1;
        let redirect = Url::parse(&redirect).expect("loopback redirect");
        let address: SocketAddr = format!("127.0.0.1:{}", redirect.port().expect("bound port"))
            .parse()
            .expect("numeric address");
        let _connected =
            std::net::TcpStream::connect(address).expect("listener bound before launch");
        self.0.replace(Some(target.url().to_owned()));
        Ok(())
    }
}

async fn attempt() -> (SignIn, SocketAddr, mpsc::UnboundedReceiver<Event>) {
    let mut attempt = SignIn::bind("urn:uuid:synthetic-host", None, Intent::SignIn)
        .await
        .expect("bind");
    let address = attempt.listener.local_addr().expect("bound address");
    let (events, seen) = mpsc::unbounded_channel();
    attempt.events = Some(events);
    (attempt, address, seen)
}

fn request(address: SocketAddr, query: &str) -> String {
    format!("GET /auth/callback?{query} HTTP/1.1\r\nHost: {address}\r\n\r\n")
}

fn valid(attempt: &SignIn, address: SocketAddr) -> String {
    request(
        address,
        &format!(
            "state={}&code=test-code&client_id=issued",
            attempt.state.expose().as_str()
        ),
    )
}

async fn result(
    task: JoinHandle<Result<Authorization, SignInError>>,
) -> Result<Authorization, SignInError> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("receive watchdog")
        .expect("receive task")
}

async fn event(seen: &mut mpsc::UnboundedReceiver<Event>, expected: Event) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("event watchdog"),
        Some(expected)
    );
}

async fn send(address: SocketAddr, request: &str) -> String {
    let mut socket = TcpStream::connect(address).await.expect("connect loopback");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("send callback");
    read_reply(&mut socket).await
}

async fn read_reply(socket: &mut TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut reader = BufReader::new(socket);
        let mut response = String::new();
        let mut length = None;
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).await.expect("read header"),
                0,
                "complete reply header"
            );
            response.push_str(&line);
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.trim().parse::<usize>().expect("byte length"));
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; length.expect("content length")];
        reader
            .read_exact(&mut body)
            .await
            .expect("complete reply body");
        response.push_str(std::str::from_utf8(&body).expect("UTF-8 reply"));
        response
    })
    .await
    .expect("reply watchdog")
}

async fn pending(mut future: Pin<&mut impl Future>) {
    poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "future must still be pending"
        );
        Poll::Ready(())
    })
    .await;
}

#[test]
fn accept_errors_are_classified_per_connection_or_fatal() {
    use std::io::ErrorKind;
    let actual = [
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::Interrupted,
        ErrorKind::PermissionDenied,
    ]
    .map(callback::retry_accept);
    assert_eq!(actual, [true, true, true, false]);
}

#[test]
fn callback_is_bound_to_attempt_and_registration() {
    let address = "127.0.0.1:1455".parse().expect("address");
    let parse = |query: &str, client| {
        callback::parse(
            &request(address, query),
            "http://127.0.0.1:1455/auth/callback",
            "state",
            client,
        )
    };
    for (query, saved, expected) in [
        ("state=state&code=one-use&client_id=issued", None, "issued"),
        ("state=state&code=one-use", Some("saved"), "saved"),
        (
            "state=state&code=one-use&client_id=saved",
            Some("saved"),
            "saved",
        ),
    ] {
        let (code, client) = parse(query, saved).expect("bound client");
        assert_eq!((code.as_str(), client.as_str()), ("one-use", expected));
    }
    for query in [
        "state=old&error=access_denied",
        "state=state&code=c&client_id=issued&%73tate=state",
        "state=state&code=c&client_id=issued&extra=a&extra=b",
        "state=state&code=c&client_id=issued#fragment",
    ] {
        assert_eq!(parse(query, None), Err(CallbackError::Rejected));
    }
    assert_eq!(
        parse("state=state&error=access_denied", None),
        Err(CallbackError::Terminal(SignInError::Declined))
    );
    assert_eq!(
        parse("state=state&error=provider-text", None),
        Err(CallbackError::Terminal(SignInError::Authorization))
    );
}

fn incomplete_cases() -> Vec<(String, Option<&'static str>)> {
    vec![
        ("state=state&client_id=issued".into(), None),
        ("state=state&code=&client_id=issued".into(), None),
        ("state=state&code=c".into(), None),
        ("state=state&code=c&client_id=".into(), None),
        (
            "state=state&code=c&client_id=dynamic_agent_client".into(),
            None,
        ),
        (
            "state=state&code=c&client_id=different".into(),
            Some("saved"),
        ),
        (
            format!("state=state&code=c&client_id={}", "x".repeat(257)),
            None,
        ),
    ]
}

#[test]
fn every_state_matching_incomplete_result_is_terminal() {
    let address = "127.0.0.1:1455".parse().expect("address");
    let actual: Vec<_> = incomplete_cases()
        .iter()
        .map(|(query, saved)| {
            callback::parse(
                &request(address, query),
                "http://127.0.0.1:1455/auth/callback",
                "state",
                *saved,
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![Err(CallbackError::Terminal(SignInError::SignInFailed)); 7]
    );
}

#[tokio::test]
async fn incomplete_callbacks_end_receive_and_serve_the_failure_page() {
    for (query, saved) in incomplete_cases() {
        let (mut attempt, address, _seen) = attempt().await;
        let query = query.replace(
            "state=state",
            &format!("state={}", attempt.state.expose().as_str()),
        );
        attempt.saved_client = saved.map(|s| Secret::new(Zeroizing::new(s.to_owned())));
        let task = tokio::spawn(attempt.receive());
        let reply = send(address, &request(address, &query)).await;
        assert!(
            reply.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "failed callback status"
        );
        assert!(
            reply.ends_with("Sign-in failed. Return to the app and start sign-in again."),
            "fixed failure page"
        );
        assert_eq!(result(task).await.unwrap_err(), SignInError::SignInFailed);
    }
}

#[tokio::test]
async fn stale_and_invalid_callbacks_do_not_consume_pending_attempt() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("capture-control");
    let (attempt, address, mut seen) = attempt().await;
    let valid = valid(&attempt, address).replace("test-code", SENTINEL);
    let invalid = [
        valid.replace("/auth/callback?", "/wrong?"),
        valid.replace("Host: 127.0.0.1", "Host: localhost"),
        valid.replace(&format!("Host: {address}"), "Host: 127.0.0.1:1"),
        valid.replace(attempt.state.expose().as_str(), "stale-state"),
        valid.replace("&code=", "&state=duplicate&code="),
        valid.replace("\r\n\r\n", &format!("\r\nHost: {address}\r\n\r\n")),
        valid.replace("GET ", "POST "),
        valid.replace(
            "/auth/callback?",
            &format!("http://{address}/auth/callback?"),
        ),
        valid.replacen("\r\n", "\n", 1),
        valid.replace("Host:", " Host:"),
        valid.replace("/auth/callback?", "/auth/%63allback?"),
        valid.replace(&format!("Host: {address}\r\n"), ""),
        valid.replace("HTTP/1.1", "HTTP/1.0"),
        valid.replace("Host:", "Host :"),
    ];
    let task = tokio::spawn(attempt.receive());
    for request in invalid {
        assert!(
            send(address, &request)
                .await
                .starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "invalid callback rejected"
        );
        event(&mut seen, Event::Reading).await;
        event(&mut seen, Event::Rejected).await;
    }
    let reply = send(address, &valid.replace("Host:", "host:")).await;
    assert!(
        reply.starts_with("HTTP/1.1 200 OK\r\n"),
        "lowercase host accepted after stale requests"
    );
    assert!(!reply.contains(SENTINEL), "reply never reflects code");
    let authorization = result(task).await.expect("valid callback");
    assert_eq!(authorization.code.expose().as_str(), SENTINEL);
    assert_eq!(authorization.client_id.expose().as_str(), "issued");
    assert_eq!(
        authorization.redirect,
        format!("http://{address}/auth/callback")
    );
    assert!(
        !format!("{authorization:?}").contains(SENTINEL),
        "code redacted"
    );
    assert!(
        captured.text().contains("capture-control"),
        "capture is active"
    );
    assert!(
        !captured.text().contains(SENTINEL),
        "authorization code never logged"
    );
    assert!(
        TcpStream::connect(address).await.is_err(),
        "listener closes after success"
    );
}

#[tokio::test]
async fn exact_32_kib_header_is_accepted_and_one_more_byte_gets_400() {
    for (length, status) in [(32768, "200 OK"), (32769, "400 Bad Request")] {
        let (attempt, address, _seen) = attempt().await;
        let valid = valid(&attempt, address);
        let prefix = valid.trim_end_matches("\r\n");
        let padding = "x".repeat(length - prefix.len() - "\r\nCookie: \r\n\r\n".len());
        let request = format!("{prefix}\r\nCookie: {padding}\r\n\r\n");
        assert_eq!(request.len(), length);
        let task = tokio::spawn(attempt.receive());
        assert!(
            send(address, &request)
                .await
                .starts_with(&format!("HTTP/1.1 {status}\r\n")),
            "exact boundary status"
        );
        if length == 32769 {
            assert!(
                send(address, &valid)
                    .await
                    .starts_with("HTTP/1.1 200 OK\r\n"),
                "oversize didn't consume attempt"
            );
        }
        assert_eq!(
            result(task)
                .await
                .expect("valid callback")
                .code
                .expose()
                .as_str(),
            "test-code"
        );
    }
}

#[tokio::test]
async fn a_twenty_kib_cookie_does_not_break_sign_in() {
    let (attempt, address, _seen) = attempt().await;
    let request = valid(&attempt, address).replace(
        "\r\n\r\n",
        &format!("\r\nCookie: local={}\r\n\r\n", "x".repeat(20 * 1024)),
    );
    let task = tokio::spawn(attempt.receive());
    assert!(
        send(address, &request)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "browser cookie accepted"
    );
    assert_eq!(
        result(task).await.expect("callback").code.expose().as_str(),
        "test-code"
    );
}

#[tokio::test]
async fn reset_preconnect_before_receive_does_not_end_the_attempt() {
    let (attempt, address, _seen) = attempt().await;
    let valid = valid(&attempt, address);
    let socket = TcpSocket::new_v4().expect("IPv4 socket");
    #[expect(
        deprecated,
        reason = "zero linger makes this fixture's close abortive without waiting"
    )]
    socket
        .set_linger(Some(Duration::ZERO))
        .expect("abortive close");
    drop(socket.connect(address).await.expect("preconnect"));
    let task = tokio::spawn(attempt.receive());
    assert!(
        send(address, &valid)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "valid callback after reset"
    );
    assert_eq!(
        result(task)
            .await
            .expect("callback after reset")
            .code
            .expose()
            .as_str(),
        "test-code"
    );
}

#[tokio::test(start_paused = true)]
async fn slow_partial_header_has_an_absolute_two_second_limit() {
    let (mut client, mut server) = tokio::io::duplex(64);
    let started = tokio::time::Instant::now();
    let mut read = Box::pin(callback::read_header(&mut server));
    client.write_all(b"GET ").await.expect("first chunk");
    pending(read.as_mut()).await;
    tokio::time::advance(Duration::from_millis(1500)).await;
    client.write_all(b"/auth/").await.expect("second chunk");
    pending(read.as_mut()).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    assert_eq!(read.await, Err(CallbackError::Rejected));
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_millis(2100),
        "no auto-advance to a renewed deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn fragmented_header_can_finish_before_the_absolute_deadline() {
    let (mut client, mut server) = tokio::io::duplex(64);
    let mut read = Box::pin(callback::read_header(&mut server));
    client.write_all(b"GET ").await.expect("first chunk");
    pending(read.as_mut()).await;
    tokio::time::advance(Duration::from_millis(1500)).await;
    client
        .write_all(b"/ HTTP/1.1\r\n\r\n")
        .await
        .expect("header remainder");
    assert_eq!(
        read.await.expect("complete header").as_str(),
        "GET / HTTP/1.1\r\n\r\n"
    );
}

#[tokio::test]
async fn idle_preconnect_then_valid_callback_completes_inside_attempt_deadline() {
    let (attempt, address, mut seen) = attempt().await;
    let valid = valid(&attempt, address);
    let task = tokio::spawn(attempt.receive());
    let mut idle = TcpStream::connect(address).await.expect("idle preconnect");
    event(&mut seen, Event::Reading).await;
    let mut callback = TcpStream::connect(address).await.expect("queued callback");
    callback
        .write_all(valid.as_bytes())
        .await
        .expect("queued header");
    event(&mut seen, Event::Rejected).await;
    assert!(
        read_reply(&mut idle)
            .await
            .starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "idle socket rejected"
    );
    assert!(
        read_reply(&mut callback)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "queued callback accepted"
    );
    assert_eq!(
        result(task).await.expect("callback").code.expose().as_str(),
        "test-code"
    );
}

#[tokio::test(start_paused = true)]
async fn expired_attempt_neither_launches_nor_receives() {
    let (attempt, _address, _seen) = attempt().await;
    tokio::time::advance(Duration::from_secs(300)).await;
    let launcher = RecordingLauncher::default();
    assert_eq!(attempt.open_browser(&launcher), Err(LaunchError::Expired));
    assert!(
        launcher.0.borrow().is_none(),
        "expired attempt never launched"
    );
    assert_eq!(attempt.receive().await.unwrap_err(), SignInError::Expired);
}

#[tokio::test(start_paused = true)]
async fn pending_attempt_expires_after_five_minutes() {
    let (attempt, _address, _seen) = attempt().await;
    let mut receive = Box::pin(attempt.receive());
    pending(receive.as_mut()).await;
    tokio::time::advance(Duration::from_secs(300)).await;
    assert_eq!(receive.await.unwrap_err(), SignInError::Expired);
}

#[tokio::test]
async fn returning_sign_in_never_hands_saved_tokens_or_hints_to_the_launcher() {
    let saved = Account {
        client_id: Zeroizing::new("oaiapp_saved".into()),
        subject: Some(Zeroizing::new("saved-subject".into())),
        email: Some(Zeroizing::new("saved-email@example.invalid".into())),
        plan_enabled: true,
        tokens: Some(Tokens {
            access_token: Zeroizing::new("synthetic-access-token-B1".into()),
            refresh_token: Zeroizing::new("synthetic-refresh-token-B1".into()),
            id_token: Some(Zeroizing::new("synthetic-id-token-B1".into())),
            scope: Zeroizing::new("openid".into()),
            expires_at: 1,
            pending_identity_validation: false,
            refresh_sent: false,
            connection: 0,
        }),
    };
    let attempt = SignIn::bind("urn:uuid:synthetic-host", Some(&saved), Intent::SignIn)
        .await
        .expect("bind");
    let launcher = RecordingLauncher::default();
    attempt.open_browser(&launcher).expect("injected launch");
    let url = launcher.0.into_inner().expect("launch recorded");
    for value in [
        "synthetic-access-token-B1",
        "synthetic-refresh-token-B1",
        "synthetic-id-token-B1",
        "id_token_hint",
        "login_hint",
        "saved-email@example.invalid",
    ] {
        assert!(
            !url.contains(value),
            "sensitive value absent from launch URL"
        );
    }
    let url = Url::parse(&url).expect("URL");
    let query: BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "oaiapp_saved");
    assert!(
        !query.contains_key("agent_name_hint"),
        "no returning agent hint"
    );
    assert!(!query.contains_key("prompt"), "no forced consent");
    for secret in [
        attempt.state.expose().as_str(),
        attempt.nonce.expose().as_str(),
        attempt.verifier.expose().as_str(),
        "oaiapp_saved",
        "synthetic-host",
    ] {
        assert!(
            !format!("{attempt:?} {:?}", attempt.target).contains(secret),
            "attempt and launch target redact fields"
        );
    }
}

#[tokio::test]
async fn new_attempts_have_independent_secrets_and_explicit_consent_only_when_requested() {
    let first = SignIn::bind("urn:uuid:host", None, Intent::EnablePlan)
        .await
        .expect("bind");
    let second = SignIn::bind("urn:uuid:host", None, Intent::SignIn)
        .await
        .expect("bind");
    let values = [
        &first.state,
        &first.nonce,
        &first.verifier,
        &second.state,
        &second.nonce,
        &second.verifier,
    ];
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value.expose().len(), 43);
        for other in &values[index + 1..] {
            assert_ne!(value.expose(), other.expose());
        }
    }
    let url = Url::parse(first.target.url()).expect("URL");
    let mut query: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        query.get("code_challenge"),
        Some(&super::pkce::challenge(first.verifier.expose()))
    );
    for key in ["state", "nonce", "code_challenge"] {
        assert!(query.remove(key).is_some(), "random field present");
    }
    let expected: BTreeMap<_, _> = [
        ("client_id", "dynamic_agent_client"),
        ("agent_name_hint", kumochi_protocol::constants::DISPLAY_NAME),
        ("ext_agent_host_id", "urn:uuid:host"),
        ("response_type", "code"),
        ("redirect_uri", first.redirect.as_str()),
        (
            "scope",
            "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
        ),
        ("resource", "https://api.openai.com/v1"),
        ("code_challenge_method", "S256"),
        ("prompt", "consent"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(query, expected);
}

#[test]
fn callback_pages_are_localized_nonreflecting_and_not_cached() {
    for (page, expected) in [
        (
            callback::Page::Received,
            "Sign-in response received. You can close this tab and return to the app.",
        ),
        (
            callback::Page::Rejected,
            "This sign-in response was rejected. Return to the app and try again.",
        ),
        (
            callback::Page::Failed,
            "Sign-in failed. Return to the app and start sign-in again.",
        ),
    ] {
        let response = callback::response(page);
        let (header, body) = response.split_once("\r\n\r\n").expect("HTTP response");
        assert_eq!(body, expected);
        assert!(
            header.contains(&format!("Content-Length: {}\r\n", body.len())),
            "correct byte length"
        );
        assert!(
            header.contains("Cache-Control: no-store"),
            "no callback cached"
        );
        assert!(
            header.contains("Referrer-Policy: no-referrer"),
            "no callback referrer"
        );
    }
}
