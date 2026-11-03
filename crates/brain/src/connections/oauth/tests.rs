use std::cell::RefCell;
use std::net::SocketAddr;
use std::time::Duration;

use kumochi_protocol::Secret;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use url::Url;
use zeroize::Zeroizing;

use super::callback::{self, Event};
use super::{Intent, LaunchTarget, Launcher, SignIn, SignInError};
use crate::connections::records::{Account, Tokens};
use crate::test_support::{Captured, SENTINEL};

type Launch = (String, Vec<String>);

#[derive(Default)]
struct RecordingLauncher(RefCell<Option<Launch>>);

impl Launcher for RecordingLauncher {
    fn launch(&self, target: &LaunchTarget) -> Result<(), SignInError> {
        let url = Url::parse(target.url()).expect("authorization URL");
        let redirect = url
            .query_pairs()
            .find(|(k, _)| k == "redirect_uri")
            .expect("redirect")
            .1;
        let redirect = Url::parse(&redirect).expect("loopback redirect");
        let address: SocketAddr = format!("127.0.0.1:{}", redirect.port().expect("port"))
            .parse()
            .expect("address");
        let _bound =
            std::net::TcpStream::connect(address).expect("listener is bound before launch");
        self.0.replace(Some((
            target.url().to_owned(),
            target.arguments().map(str::to_owned).to_vec(),
        )));
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
    .expect("reply deadline")
}

#[test]
fn callback_is_bound_to_attempt_and_registration() {
    let address = "127.0.0.1:1455".parse().expect("address");
    let redirect = "http://127.0.0.1:1455/auth/callback";
    let parse =
        |query: &str, client| callback::parse(&request(address, query), redirect, "state", client);
    let (code, client) =
        parse("state=state&code=one-use&client_id=oaiapp_issued", None).expect("issued client");
    assert_eq!(code.as_str(), "one-use");
    assert_eq!(client.as_str(), "oaiapp_issued");
    let (code, client) =
        parse("state=state&code=returning", Some("saved-client")).expect("saved client");
    assert_eq!(code.as_str(), "returning");
    assert_eq!(client.as_str(), "saved-client");
    assert_eq!(
        parse(
            "state=state&code=c&client_id=saved-client",
            Some("saved-client")
        )
        .expect("matching client")
        .1
        .as_str(),
        "saved-client"
    );
    for query in [
        "state=state&code=c",
        "state=state&code=c&client_id=",
        "state=state&code=c&client_id=dynamic_agent_client",
        "state=state&code=&client_id=oaiapp_issued",
        "state=state&client_id=oaiapp_issued",
        "state=state&code=c&client_id=oaiapp_issued&%73tate=state",
        "state=state&code=c&client_id=oaiapp_issued&extra=a&extra=b",
        "state=state&code=c&client_id=oaiapp_issued#fragment",
    ] {
        assert_eq!(
            parse(query, None),
            Err(SignInError::InvalidCallback),
            "invalid query"
        );
    }
    assert_eq!(
        parse(
            "state=state&code=c&client_id=different",
            Some("saved-client")
        ),
        Err(SignInError::InvalidCallback)
    );
    assert_eq!(
        parse("state=old&error=access_denied", None),
        Err(SignInError::InvalidCallback)
    );
    assert_eq!(
        parse("state=state&error=access_denied", None),
        Err(SignInError::Declined)
    );
    assert_eq!(
        parse("state=state&error=untrusted-provider-text", None),
        Err(SignInError::Authorization)
    );
    let long_client = "c".repeat(257);
    assert_eq!(
        parse(&format!("state=state&code=c&client_id={long_client}"), None),
        Err(SignInError::InvalidCallback)
    );
}

#[tokio::test]
async fn stale_and_invalid_callbacks_do_not_consume_pending_attempt() {
    let logs = Captured::default();
    let _logging = logs.install();
    let (attempt, address, mut seen) = attempt().await;
    let query = format!(
        "state={}&code={SENTINEL}&client_id=oaiapp_issued",
        attempt.state.expose().as_str()
    );
    let valid = request(address, &query);
    let invalid = [
        valid.replace("/auth/callback?", "/wrong?"),
        valid.replace("Host: 127.0.0.1", "Host: localhost"),
        valid.replace(&format!("Host: {address}"), "Host: 127.0.0.1:1"),
        valid.replace(attempt.state.expose().as_str(), "stale-state"),
        request(address, &format!("{query}&state=duplicate")),
        valid.replace("\r\n\r\n", &format!("\r\nHost: {address}\r\n\r\n")),
        valid.replace("GET ", "POST "),
        valid.replace(
            "/auth/callback?",
            &format!("http://{address}/auth/callback?"),
        ),
    ];
    let result = tokio::spawn(attempt.receive());
    for rejected in invalid {
        let response = send(address, &rejected).await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "invalid request rejected"
        );
        assert!(!response.contains(SENTINEL), "code never reflected");
        assert_eq!(seen.recv().await, Some(Event::Reading));
        assert_eq!(seen.recv().await, Some(Event::Rejected));
        assert!(
            !result.is_finished(),
            "stale request left the attempt alive"
        );
    }
    let response = send(address, &valid).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "valid callback accepted"
    );
    assert!(!response.contains(SENTINEL), "code never reflected");
    let authorization = result.await.expect("task").expect("valid callback");
    assert_eq!(authorization.code.expose().as_str(), SENTINEL);
    assert_eq!(authorization.client_id.expose().as_str(), "oaiapp_issued");
    assert_eq!(
        authorization.redirect,
        format!("http://{address}/auth/callback")
    );
    assert!(
        !format!("{authorization:?}").contains(SENTINEL),
        "callback data redacted"
    );
    assert!(
        !logs.text().contains(SENTINEL),
        "callback code never logged"
    );
    assert!(
        TcpStream::connect(address).await.is_err(),
        "one-shot listener closed"
    );
}

#[tokio::test]
async fn oversized_header_is_rejected_before_a_valid_callback() {
    let (attempt, address, mut seen) = attempt().await;
    let state = attempt.state.expose().to_string();
    let oversized = request(
        address,
        &format!(
            "state={state}&code=oversized&client_id=issued&padding={}",
            "x".repeat(8192)
        ),
    );
    assert!(
        oversized.len() > 8192,
        "otherwise-valid header exceeds the cap"
    );
    let task = tokio::spawn(attempt.receive());
    let mut socket = TcpStream::connect(address).await.expect("oversized client");
    socket
        .write_all(oversized.as_bytes())
        .await
        .expect("send oversized header");
    assert_eq!(seen.recv().await, Some(Event::Reading));
    assert_eq!(seen.recv().await, Some(Event::Rejected));
    let valid = request(
        address,
        &format!("state={state}&code=valid&client_id=issued"),
    );
    assert!(
        send(address, &valid)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "attempt remains usable"
    );
    assert_eq!(
        task.await
            .expect("task")
            .expect("callback")
            .code
            .expose()
            .as_str(),
        "valid"
    );
}

#[tokio::test]
async fn exactly_eight_kib_header_is_accepted() {
    let (attempt, address, _seen) = attempt().await;
    let query = format!(
        "state={}&code=c&client_id=issued",
        attempt.state.expose().as_str()
    );
    let prefix = request(address, &query).trim_end_matches("\r\n").to_owned();
    let suffix = "\r\nX-Padding: ";
    let padding = "x".repeat(8192 - prefix.len() - suffix.len() - 4);
    let request = format!("{prefix}{suffix}{padding}\r\n\r\n");
    assert_eq!(request.len(), 8192);
    let task = tokio::spawn(attempt.receive());
    assert!(
        send(address, &request)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "header at limit accepted"
    );
    assert_eq!(
        task.await
            .expect("task")
            .expect("callback")
            .code
            .expose()
            .as_str(),
        "c"
    );
}

#[tokio::test]
async fn slow_partial_header_has_an_absolute_two_second_limit() {
    let (attempt, address, mut seen) = attempt().await;
    let state = attempt.state.expose().to_string();
    let task = tokio::spawn(attempt.receive());
    let mut socket = TcpStream::connect(address).await.expect("slow client");
    socket
        .write_all(b"GET /auth/callback?")
        .await
        .expect("partial header");
    assert_eq!(seen.recv().await, Some(Event::Reading));
    let read_started = tokio::time::Instant::now();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    socket
        .write_all(b"state=")
        .await
        .expect("more partial data");
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(1100)).await;
    assert_eq!(seen.recv().await, Some(Event::ReadTimedOut));
    assert!(
        tokio::time::Instant::now() - read_started < Duration::from_millis(2200),
        "read timed out on the advanced clock, not a renewed per-chunk deadline"
    );
    tokio::time::resume();
    assert_eq!(seen.recv().await, Some(Event::Rejected));
    assert!(
        read_reply(&mut socket)
            .await
            .starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "slow client rejected"
    );
    let valid = request(address, &format!("state={state}&code=c&client_id=issued"));
    assert!(
        send(address, &valid)
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"),
        "attempt remains usable"
    );
    assert_eq!(
        task.await
            .expect("task")
            .expect("callback")
            .code
            .expose()
            .as_str(),
        "c"
    );
}

#[tokio::test]
async fn idle_preconnect_then_valid_callback_completes_inside_attempt_deadline() {
    let (attempt, address, mut seen) = attempt().await;
    let deadline = attempt.deadline;
    let valid = request(
        address,
        &format!(
            "state={}&code=c&client_id=issued",
            attempt.state.expose().as_str()
        ),
    );
    let task = tokio::spawn(attempt.receive());
    let mut idle = TcpStream::connect(address).await.expect("idle preconnect");
    assert_eq!(seen.recv().await, Some(Event::Reading));
    let read_started = tokio::time::Instant::now();
    let mut callback = TcpStream::connect(address).await.expect("queued callback");
    callback
        .write_all(valid.as_bytes())
        .await
        .expect("send valid callback behind idle socket");
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(2100)).await;
    assert_eq!(seen.recv().await, Some(Event::ReadTimedOut));
    assert!(
        tokio::time::Instant::now() - read_started < Duration::from_millis(2200),
        "idle read timed out on the advanced clock"
    );
    tokio::time::resume();
    assert_eq!(seen.recv().await, Some(Event::Rejected));
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
        task.await
            .expect("task")
            .expect("callback")
            .code
            .expose()
            .as_str(),
        "c"
    );
    assert!(
        tokio::time::Instant::now() < deadline,
        "finished within attempt deadline"
    );
}

#[tokio::test]
async fn expired_attempt_neither_launches_nor_accepts_a_queued_valid_callback() {
    let (attempt, address, _seen) = attempt().await;
    let valid = request(
        address,
        &format!(
            "state={}&code=c&client_id=issued",
            attempt.state.expose().as_str()
        ),
    );
    let mut queued = TcpStream::connect(address)
        .await
        .expect("queued connection");
    queued
        .write_all(valid.as_bytes())
        .await
        .expect("queued callback");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::time::resume();
    let launcher = RecordingLauncher::default();
    assert_eq!(attempt.open_browser(&launcher), Err(SignInError::Expired));
    assert!(
        launcher.0.borrow().is_none(),
        "expired attempt never launched"
    );
    assert_eq!(attempt.receive().await.unwrap_err(), SignInError::Expired);
    assert!(
        TcpStream::connect(address).await.is_err(),
        "expired listener closed"
    );
}

#[tokio::test]
async fn five_minute_deadline_also_bounds_an_active_slow_client() {
    let (attempt, address, mut seen) = attempt().await;
    let task = tokio::spawn(attempt.receive());
    let _idle = TcpStream::connect(address).await.expect("idle connection");
    assert_eq!(seen.recv().await, Some(Event::Reading));
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::time::resume();
    assert_eq!(task.await.expect("task").unwrap_err(), SignInError::Expired);
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
        }),
    };
    let attempt = SignIn::bind("urn:uuid:synthetic-host", Some(&saved), Intent::SignIn)
        .await
        .expect("bind");
    let launcher = RecordingLauncher::default();
    attempt.open_browser(&launcher).expect("injected launch");
    let (url, arguments) = launcher.0.into_inner().expect("launch recorded");
    let tokens = saved.tokens.as_ref().expect("saved tokens");
    for value in [
        tokens.access_token.as_str(),
        tokens.refresh_token.as_str(),
        tokens.id_token.as_ref().expect("ID token").as_str(),
        "id_token_hint",
        "login_hint",
        "saved-email@example.invalid",
    ] {
        assert!(!url.contains(value), "sensitive value absent from URL");
        assert!(
            arguments.iter().all(|a| !a.contains(value)),
            "sensitive value absent from launch arguments"
        );
    }
    assert_eq!(arguments, vec![url.clone()]);
    let url = Url::parse(&url).expect("URL");
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(
        query.get("client_id").map(std::convert::AsRef::as_ref),
        Some("oaiapp_saved")
    );
    assert!(
        !query.contains_key("agent_name_hint"),
        "returning registration omits agent hint"
    );
    assert!(
        !query.contains_key("prompt"),
        "ordinary sign-in doesn't force consent"
    );
    for secret in [
        attempt.state.expose().as_str(),
        attempt.nonce.expose().as_str(),
        attempt.verifier.expose().as_str(),
        "oaiapp_saved",
        "synthetic-host",
    ] {
        assert!(
            !format!("{attempt:?} {:?}", attempt.target).contains(secret),
            "attempt and target redact sensitive fields"
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
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(query["client_id"], "dynamic_agent_client");
    assert_eq!(
        query["agent_name_hint"],
        kumochi_protocol::constants::DISPLAY_NAME
    );
    assert_eq!(query["prompt"], "consent");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(
        query["code_challenge"],
        super::pkce::challenge(first.verifier.expose())
    );
    assert_eq!(query["redirect_uri"], first.redirect);
}

#[test]
fn browser_crate_path_and_log_facade_do_not_leak_into_tracing() {
    let captured = Captured::default();
    let _logging = captured.install();
    let target = LaunchTarget(Secret::new(
        Url::parse(&format!("https://example.invalid/{SENTINEL}")).expect("sentinel URL"),
    ));
    let mut options = webbrowser::BrowserOptions::default();
    options.with_dry_run(true);
    super::launch_browser(&target, &options).expect("real webbrowser path, dry run only");
    assert!(
        !log::log_enabled!(target: "webbrowser", log::Level::Debug),
        "no log-to-tracing bridge may expose browser commands"
    );
    log::debug!(target: "webbrowser", "background spawn: {}", target.url());
    tracing::info!("capture-control");
    assert!(
        captured.text().contains("capture-control"),
        "capture is active"
    );
    assert!(
        !captured.text().contains(SENTINEL),
        "browser URL never logged"
    );
}

#[test]
fn callback_pages_are_localized_nonreflecting_and_not_cached() {
    for (page, expected) in [
        (callback::Page::Received, "Sign-in response received."),
        (
            callback::Page::Rejected,
            "This sign-in response was rejected.",
        ),
    ] {
        let response = callback::response(page);
        let (header, body) = response.split_once("\r\n\r\n").expect("HTTP response");
        assert!(body.starts_with(expected), "Fluent message rendered");
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
