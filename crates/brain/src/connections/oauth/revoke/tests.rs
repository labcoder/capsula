use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use kumochi_protocol::Secret;
use tokio::time::Instant;
use zeroize::Zeroizing;

use super::{BUDGET, revoke};
use crate::connections::http_client::{Http, HttpError, MAX_BODY_BYTES, Request, Response};
use crate::connections::records::{Origin, Revocable};

const DISCOVERY: &str = r#"{"issuer":"https://auth.openai.com","revocation_endpoint":"https://auth.openai.com/api/accounts/oauth/revoke"}"#;

enum Reply {
    Now(Result<Response, HttpError>),
    /// Never answers, like a stalled connection.
    Hang,
}

/// A request as the fixture saw it: its URL, its fields and when it was made.
type Seen = (String, BTreeMap<String, String>, Instant);

/// Replays replies in order, keeping each request's URL, fields and when it was made.
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Seen>>,
}

impl Scripted {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.requests.lock().expect("requests"))
    }
}

impl Http for Scripted {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let fields = request
            .fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.expose().to_string()))
            .collect();
        self.requests.lock().expect("requests").push((
            request.endpoint.url().to_owned(),
            fields,
            Instant::now(),
        ));
        let reply = self.replies.lock().expect("replies").pop_front();
        async move {
            match reply.expect("a scripted reply for every request") {
                Reply::Now(result) => result,
                Reply::Hang => std::future::pending().await,
            }
        }
    }
}

fn status(code: u16, body: &[u8]) -> Reply {
    Reply::Now(Ok(Response {
        status: code,
        request_id: None,
        body: Secret::new(Zeroizing::new(body.to_vec())),
    }))
}

fn discovered() -> Reply {
    status(200, DISCOVERY.as_bytes())
}

fn entry(token: &str, origin: Origin) -> Secret<Revocable> {
    Secret::new(Revocable {
        client_id: Zeroizing::new(format!("client-{token}")),
        refresh_token: Zeroizing::new(token.to_owned()),
        added_at: 1,
        origin,
    })
}

fn tokens(entries: &[Secret<Revocable>]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.expose().refresh_token.to_string())
        .collect()
}

fn deadline() -> Instant {
    Instant::now() + BUDGET
}

#[tokio::test]
async fn an_empty_200_confirms_each_entry_the_account_first_with_the_documented_fields() {
    let http = Scripted::new([discovered(), status(200, b""), status(200, b"")]);
    let entries = vec![
        entry("exchange", Origin::Exchange),
        entry("account", Origin::Account),
    ];
    let confirmed = revoke(&http, entries, deadline()).await;
    assert_eq!(tokens(&confirmed), ["account", "exchange"]);
    let requests = http.requests();
    assert_eq!(
        requests[0].0,
        "https://auth.openai.com/.well-known/openid-configuration"
    );
    assert!(requests[0].1.is_empty());
    for ((url, fields, _), token) in requests[1..].iter().zip(["account", "exchange"]) {
        assert_eq!(url, "https://auth.openai.com/api/accounts/oauth/revoke");
        let expected: BTreeMap<String, String> = [
            ("token", token.to_owned()),
            ("token_type_hint", "refresh_token".to_owned()),
            ("client_id", format!("client-{token}")),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        assert_eq!(fields, &expected);
    }
    assert_eq!(requests.len(), 3);
}

#[tokio::test]
async fn only_an_empty_200_confirms_and_other_answers_are_not_tried_again() {
    let typed = br#"{"error":"invalid_request"}"#;
    for reply in [
        status(200, b"{}"),
        status(201, b""),
        status(204, b""),
        status(400, typed),
        status(401, b""),
        status(499, b""),
        Reply::Now(Err(HttpError::Redirect)),
        Reply::Now(Err(HttpError::BodyTooLarge)),
    ] {
        let http = Scripted::new([discovered(), reply]);
        let confirmed = revoke(&http, vec![entry("a", Origin::Exchange)], deadline()).await;
        assert!(confirmed.is_empty());
        assert_eq!(http.requests().len(), 2, "one attempt");
    }
}

#[tokio::test(start_paused = true)]
async fn a_transport_failure_or_5xx_is_tried_three_times_in_all_with_backoff() {
    let http = Scripted::new([
        discovered(),
        status(503, b""),
        Reply::Now(Err(HttpError::Transport)),
        status(200, b""),
    ]);
    let confirmed = revoke(&http, vec![entry("a", Origin::Exchange)], deadline()).await;
    assert_eq!(tokens(&confirmed), ["a"]);
    let requests = http.requests();
    let gaps: Vec<_> = requests
        .windows(2)
        .map(|pair| pair[1].2 - pair[0].2)
        .collect();
    assert_eq!(
        gaps,
        [
            Duration::ZERO,
            Duration::from_millis(500),
            Duration::from_secs(1)
        ]
    );

    let http = Scripted::new([
        discovered(),
        status(500, b""),
        Reply::Now(Err(HttpError::Timeout)),
        status(599, b""),
        status(200, b""),
    ]);
    let confirmed = revoke(&http, vec![entry("a", Origin::Exchange)], deadline()).await;
    assert!(confirmed.is_empty(), "a fourth attempt is never made");
    assert_eq!(http.requests().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn the_budget_ends_a_stalled_request_and_a_backoff_that_would_pass_it() {
    let start = Instant::now();
    let http = Scripted::new([discovered(), Reply::Hang]);
    let entries = vec![entry("a", Origin::Account), entry("b", Origin::Exchange)];
    assert!(revoke(&http, entries, start + BUDGET).await.is_empty());
    assert_eq!(
        Instant::now() - start,
        BUDGET,
        "it ends at the budget, not later"
    );
    assert_eq!(http.requests().len(), 2, "the next entry isn't tried");

    let start = Instant::now();
    let http = Scripted::new([discovered(), status(503, b""), status(503, b"")]);
    let short = start + Duration::from_millis(1_500);
    assert!(
        revoke(&http, vec![entry("a", Origin::Exchange)], short)
            .await
            .is_empty()
    );
    assert_eq!(
        http.requests().len(),
        3,
        "the 1 s backoff would end at the deadline"
    );
    assert_eq!(Instant::now() - start, Duration::from_millis(500));

    let start = Instant::now();
    let http = Scripted::new([Reply::Hang]);
    assert!(
        revoke(&http, vec![entry("a", Origin::Exchange)], start + BUDGET)
            .await
            .is_empty()
    );
    assert_eq!(Instant::now() - start, BUDGET);
}

#[tokio::test]
async fn a_bad_discovery_document_sends_no_token_anywhere() {
    let endpoint = |url: &str| {
        format!(r#"{{"issuer":"https://auth.openai.com","revocation_endpoint":"{url}"}}"#)
    };
    let oversized = format!(
        "{DISCOVERY}{}",
        " ".repeat(MAX_BODY_BYTES + 1 - DISCOVERY.len())
    );
    let documents = [
        (404, String::from(DISCOVERY)),
        (200, String::new()),
        (200, DISCOVERY[..DISCOVERY.len() - 1].to_owned()),
        (200, r#"{"issuer":"https://auth.openai.com"}"#.to_owned()),
        (200, r#"{"issuer":"https://evil.example","revocation_endpoint":"https://auth.openai.com/r"}"#.to_owned()),
        (200, r#"{"issuer":"https://auth.openai.com","revocation_endpoint":"https://auth.openai.com/a","revocation_endpoint":"https://auth.openai.com/b"}"#.to_owned()),
        (200, endpoint("http://auth.openai.com/revoke")),
        (200, endpoint("https://auth.openai.com.evil.example/revoke")),
        (200, endpoint("https://auth.openai.com:444/revoke")),
        (200, endpoint("https://user@auth.openai.com/revoke")),
        (200, endpoint(&format!("https://auth.openai.com/{}", "a".repeat(2_025)))),
        (200, oversized),
    ];
    for (code, body) in documents {
        let http = Scripted::new([status(code, body.as_bytes())]);
        let confirmed = revoke(&http, vec![entry("a", Origin::Exchange)], deadline()).await;
        assert!(confirmed.is_empty());
        assert_eq!(
            http.requests().len(),
            1,
            "no revocation request: {body:.80}"
        );
    }
    let longest = format!("https://auth.openai.com/{}", "a".repeat(2_024));
    assert_eq!(longest.len(), 2_048);
    let http = Scripted::new([status(200, endpoint(&longest).as_bytes()), status(200, b"")]);
    let confirmed = revoke(&http, vec![entry("a", Origin::Exchange)], deadline()).await;
    assert_eq!(
        tokens(&confirmed),
        ["a"],
        "a URL at the 2 KiB bound is used"
    );
}
