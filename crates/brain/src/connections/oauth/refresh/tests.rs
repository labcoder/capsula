use std::future::Future;
use std::sync::Mutex;

use kumochi_protocol::Secret;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{DEADLINE, refresh};
use crate::connections::error::Recovery;
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response};
use crate::connections::oauth::exchange::{ExchangeError, OAuthCode};
use crate::connections::records::{MAX_ID_BYTES, MAX_REFRESH_BYTES};

struct Scripted {
    reply: Mutex<Option<Response>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            reply: Mutex::new(Some(Response {
                status,
                request_id: None,
                body: Secret::new(Zeroizing::new(body)),
            })),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn sent(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}

impl Http for Scripted {
    fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.requests.lock().expect("requests").push(request);
        std::future::ready(Ok(self
            .reply
            .lock()
            .expect("reply")
            .take()
            .expect("one scripted reply")))
    }
}

/// Never answers, so only the deadline ends the request.
struct Silent;

impl Http for Silent {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> + Send {
        std::future::pending()
    }
}

fn rotated() -> Value {
    json!({"access_token":"access-new", "refresh_token":"refresh-new", "id_token":"id-new", "token_type":"Bearer", "scope":"openid chatgpt.tokens.use.direct", "expires_in":3600})
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("fixture")
}

fn text(secret: Option<&Secret<Zeroizing<String>>>) -> Option<&str> {
    secret.map(|secret| secret.expose().as_str())
}

#[tokio::test]
async fn a_refresh_sends_the_documented_form_to_the_token_endpoint() {
    let http = Scripted::new(200, bytes(&rotated()));
    let set = refresh(&http, "client-1", "refresh-old")
        .await
        .expect("rotated");
    let requests = http.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(matches!(requests[0].endpoint, Endpoint::Token));
    let fields: Vec<(&str, &str)> = requests[0]
        .fields
        .iter()
        .map(|(key, value)| (*key, value.expose().as_str()))
        .collect();
    assert_eq!(
        fields,
        [
            ("grant_type", "refresh_token"),
            ("client_id", "client-1"),
            ("refresh_token", "refresh-old"),
            ("resource", "https://api.openai.com/v1"),
        ]
    );
    assert_eq!(set.access_token.expose().as_str(), "access-new");
    assert_eq!(text(set.refresh_token.as_ref()), Some("refresh-new"));
    assert_eq!(text(set.id_token.as_ref()), Some("id-new"));
    assert_eq!(set.expires_in, 3600);
}

#[tokio::test]
async fn a_refresh_may_leave_out_its_identity_and_refresh_tokens() {
    let mut body = rotated();
    let fields = body.as_object_mut().expect("object");
    fields.remove("id_token");
    fields.remove("refresh_token");
    let set = refresh(&Scripted::new(200, bytes(&body)), "client-1", "refresh-old")
        .await
        .expect("rotated");
    assert_eq!(text(set.id_token.as_ref()), None);
    assert_eq!(text(set.refresh_token.as_ref()), None);
}

#[tokio::test]
async fn inputs_are_bounded_before_anything_is_sent() {
    let (client, token) = ("c".repeat(MAX_ID_BYTES), "r".repeat(MAX_REFRESH_BYTES));
    let http = Scripted::new(200, bytes(&rotated()));
    refresh(&http, &client, &token)
        .await
        .expect("at the limits");
    let over = |n: usize, c: &str| c.repeat(n + 1);
    for (client, token) in [
        (String::new(), token.clone()),
        (over(MAX_ID_BYTES, "c"), token.clone()),
        (client.clone(), String::new()),
        (client.clone(), over(MAX_REFRESH_BYTES, "r")),
    ] {
        let http = Scripted::new(200, bytes(&rotated()));
        assert_eq!(
            refresh(&http, &client, &token).await.unwrap_err(),
            ExchangeError::Request
        );
        assert_eq!(http.sent(), 0, "{} {}", client.len(), token.len());
    }
}

#[tokio::test(start_paused = true)]
async fn the_token_request_gives_up_at_its_deadline() {
    let started = tokio::time::Instant::now();
    assert_eq!(
        refresh(&Silent, "client-1", "refresh-old")
            .await
            .unwrap_err(),
        ExchangeError::Http(HttpError::Timeout)
    );
    assert_eq!(started.elapsed(), DEADLINE);
    assert_eq!(DEADLINE.as_secs(), 30);
}

#[tokio::test]
async fn a_malformed_answer_is_never_a_rotated_set() {
    let unusable = || ExchangeError::Unusable {
        refresh_token: Some(Secret::new(Zeroizing::new("refresh-new".to_owned()))),
    };
    let mut mistyped = rotated();
    mistyped["expires_in"] = json!("3600");
    let mut duplicate = serde_json::to_string(&rotated()).expect("fixture");
    duplicate.pop();
    duplicate.push_str(",\"scope\":\"openid\"}");
    let whole = bytes(&rotated());
    for (body, expected) in [
        (Vec::new(), ExchangeError::Response { status: 200 }),
        (
            whole[..whole.len() / 2].to_vec(),
            ExchangeError::Response { status: 200 },
        ),
        (bytes(&mistyped), unusable()),
        (duplicate.into_bytes(), unusable()),
    ] {
        let result = refresh(&Scripted::new(200, body), "client-1", "refresh-old").await;
        assert_eq!(result.unwrap_err(), expected);
    }
}

/// The ledger's codes, and what the spike's table did that the ledger leaves out.
#[tokio::test]
async fn each_ledger_code_maps_to_its_recovery() {
    let cases = [
        ("invalid_grant", 400, Recovery::SignInAgain),
        ("invalid_refresh_token", 400, Recovery::SignInAgain),
        ("token_expired", 401, Recovery::SignInAgain),
        ("refresh_token_expired", 400, Recovery::SignInAgain),
        ("refresh_token_invalidated", 400, Recovery::SignInAgain),
        ("refresh_token_reused", 400, Recovery::SignInAgain),
        ("invalid_client", 401, Recovery::FixRequest),
        (
            "subscription_sharing_user_not_eligible",
            403,
            Recovery::Unavailable,
        ),
        (
            "subscription_sharing_usage_unavailable",
            400,
            Recovery::RetryLater,
        ),
        (
            "subscription_sharing_user_unavailable",
            400,
            Recovery::RetryLater,
        ),
        (
            "chatpass_v2_scope_not_authorized",
            403,
            Recovery::FixRequest,
        ),
        (
            "chatpass_v2_invalid_authorization_context",
            403,
            Recovery::FixRequest,
        ),
        // OSC-223 gap 9: an unlisted refusal offers a new sign-in, never a dead end.
        ("rate_limit_error", 400, Recovery::SignInAgain),
        ("unlisted_code", 401, Recovery::SignInAgain),
        ("server_error", 429, Recovery::RetryLater),
    ];
    for (code, status, expected) in cases {
        let body = bytes(&json!({ "error": code }));
        let result = refresh(&Scripted::new(status, body), "client-1", "refresh-old").await;
        let recovery = match result {
            Err(ExchangeError::OAuth { status, code }) => code.recovery(status),
            other => panic!("{code}: {other:?}"),
        };
        assert_eq!(recovery, expected, "{code}");
    }
    // An empty code isn't a typed refusal, so issuance stays unknown.
    let body = bytes(&json!({ "error": "" }));
    let result = refresh(&Scripted::new(400, body), "client-1", "refresh-old").await;
    assert_eq!(result.unwrap_err(), ExchangeError::Response { status: 400 });
    // The plan endpoints share this map, and send their codes with a 5xx.
    for status in [500, 503] {
        assert_eq!(OAuthCode::Other.recovery(status), Recovery::RetryLater);
    }
}
