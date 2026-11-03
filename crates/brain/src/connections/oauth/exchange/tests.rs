use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use kumochi_protocol::Secret;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{ExchangeError, OAuthCode, exchange};
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response};
use crate::connections::oauth::Authorization;
use crate::test_support::{Captured, SENTINEL};

struct Scripted {
    replies: Mutex<VecDeque<Result<Response, HttpError>>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from([Ok(Response {
                status,
                request_id: None,
                body: Secret::new(Zeroizing::new(body)),
            })])),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Http for Scripted {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        self.requests.lock().expect("requests").push(request);
        std::future::ready(
            self.replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("one scripted response"),
        )
    }
}

fn authorization() -> Authorization {
    Authorization {
        code: Secret::new(Zeroizing::new(SENTINEL.to_owned())),
        client_id: Secret::new(Zeroizing::new("issued-client".into())),
        nonce: Secret::new(Zeroizing::new("expected-nonce".into())),
        verifier: Secret::new(Zeroizing::new("v".repeat(43))),
        redirect: "http://127.0.0.1:1455/auth/callback".into(),
    }
}

fn tokens() -> Value {
    json!({"access_token":"fixture-access", "refresh_token":"fixture-refresh", "id_token":"fixture-id", "token_type":"Bearer", "scope":"openid chatgpt.tokens.use.direct", "expires_in":3600, "earliest_refresh_at":1234})
}

fn unusable(refresh_token: &str) -> ExchangeError {
    ExchangeError::Unusable {
        refresh_token: Some(Secret::new(Zeroizing::new(refresh_token.to_owned()))),
    }
}

#[tokio::test]
async fn a_missing_id_token_keeps_the_received_refresh_token_for_revocation() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("capture-control");
    let mut body = tokens();
    body.as_object_mut().expect("object").remove("id_token");
    body["refresh_token"] = json!(SENTINEL);
    let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
    let error = exchange(&fixture, &authorization()).await.unwrap_err();
    assert_eq!(error, unusable(SENTINEL));
    assert!(
        !format!("{error:?} {error}").contains(SENTINEL),
        "retained token redacts diagnostics"
    );
    assert!(
        captured.text().contains("capture-control"),
        "capture active"
    );
    assert!(
        !captured.text().contains(SENTINEL),
        "retained token never logged"
    );
}

#[tokio::test]
async fn malformed_sibling_fields_keep_the_received_refresh_token() {
    let mut cases = Vec::new();
    for (field, value) in [
        ("expires_in", json!("3600")),
        ("expires_in", json!(3600.5)),
        ("scope", json!(["openid"])),
        ("access_token", json!(5)),
        ("access_token", Value::Null),
    ] {
        let mut body = tokens();
        body[field] = value;
        cases.push(serde_json::to_vec(&body).expect("fixture"));
    }
    let mut duplicate = serde_json::to_string(&tokens()).expect("fixture");
    duplicate.pop();
    duplicate.push_str(",\"scope\":\"openid\"}");
    cases.push(duplicate.into_bytes());
    for body in cases {
        let fixture = Scripted::new(200, body);
        assert_eq!(
            exchange(&fixture, &authorization()).await.unwrap_err(),
            unusable("fixture-refresh")
        );
    }
}

#[tokio::test]
async fn zero_lifetime_is_already_expired_and_exact_limits_are_accepted() {
    let mut body = tokens();
    body["expires_in"] = json!(0);
    body["access_token"] = json!("t".repeat(16 * 1024));
    let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
    let mut authorization = authorization();
    authorization.client_id = Secret::new(Zeroizing::new("c".repeat(256)));
    let issued = exchange(&fixture, &authorization)
        .await
        .expect("exact field limits");
    assert_eq!(issued.access_token.expose().len(), 16 * 1024);
    assert_eq!(issued.expires_in, 0);
    assert_eq!(issued.expires_at(&FixedClock(100)), Some(100));
    let fixture = Scripted::new(
        400,
        serde_json::to_vec(&json!({"error":"e".repeat(256)})).expect("fixture"),
    );
    assert_eq!(
        exchange(&fixture, &authorization).await.unwrap_err(),
        ExchangeError::OAuth {
            status: 400,
            code: OAuthCode::Other
        }
    );
}

#[tokio::test]
async fn exchange_sends_the_exact_grant_and_retains_the_issued_tokens() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("capture-control");
    let fixture = Scripted::new(200, serde_json::to_vec(&tokens()).expect("fixture"));
    let authorization = authorization();
    let issued = exchange(&fixture, &authorization).await.expect("exchange");
    assert_eq!(issued.access_token.expose().as_str(), "fixture-access");
    assert_eq!(
        issued
            .refresh_token
            .as_ref()
            .expect("offline grant")
            .expose()
            .as_str(),
        "fixture-refresh"
    );
    assert_eq!(issued.id_token.expose().as_str(), "fixture-id");
    assert_eq!(
        issued.scope.expose().as_str(),
        "openid chatgpt.tokens.use.direct"
    );
    assert_eq!(issued.expires_at(&FixedClock(100)), Some(3700));
    assert_eq!(issued.expires_at(&FixedClock(u64::MAX)), None);
    let requests = fixture.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(
        matches!(requests[0].endpoint, Endpoint::Token),
        "credentials use only the token endpoint"
    );
    let actual: BTreeMap<_, _> = requests[0]
        .fields
        .iter()
        .map(|(key, value)| (*key, value.expose().as_str()))
        .collect();
    assert_eq!(
        actual,
        BTreeMap::from([
            ("grant_type", "authorization_code"),
            ("client_id", "issued-client"),
            ("code", SENTINEL),
            ("code_verifier", authorization.verifier.expose().as_str()),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("resource", "https://api.openai.com/v1"),
        ])
    );
    let debug = format!("{issued:?} {:?}", requests[0]);
    for value in [
        SENTINEL,
        "fixture-access",
        "fixture-refresh",
        "fixture-id",
        "issued-client",
    ] {
        assert!(!debug.contains(value), "credentials redacted");
        assert!(!captured.text().contains(value), "credentials never logged");
    }
    assert!(
        captured.text().contains("capture-control"),
        "capture active"
    );
}

#[tokio::test]
async fn a_partial_grant_without_a_refresh_token_is_representable() {
    let mut body = tokens();
    body.as_object_mut()
        .expect("object")
        .remove("refresh_token");
    body["scope"] = json!("openid profile email");
    let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
    let issued = exchange(&fixture, &authorization())
        .await
        .expect("partial grant");
    assert!(
        issued.refresh_token.is_none(),
        "absence isn't fabricated or refused"
    );
    assert_eq!(issued.access_token.expose().as_str(), "fixture-access");
    assert_eq!(issued.id_token.expose().as_str(), "fixture-id");
}

#[tokio::test]
async fn exchange_fields_and_records_are_bounded_by_bytes() {
    for (field, value) in [
        ("access_token", "x".repeat(16 * 1024 + 1)),
        ("id_token", "x".repeat(16 * 1024 + 1)),
        ("refresh_token", "x".repeat(8 * 1024 + 1)),
        ("scope", "é".repeat(513)),
        ("token_type", "x".repeat(257)),
    ] {
        let mut body = tokens();
        body[field] = json!(value);
        let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
        assert_eq!(
            exchange(&fixture, &authorization()).await.unwrap_err(),
            unusable(
                body["refresh_token"]
                    .as_str()
                    .expect("fixture refresh token")
            ),
            "oversized {field}"
        );
    }
    let fixture = Scripted::new(200, vec![b' '; 48 * 1024 + 1]);
    assert_eq!(
        exchange(&fixture, &authorization()).await.unwrap_err(),
        ExchangeError::Bounds
    );
    let mut body = tokens();
    body["padding"] = json!("x".repeat(48 * 1024));
    let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
    assert_eq!(
        exchange(&fixture, &authorization()).await.unwrap_err(),
        unusable("fixture-refresh"),
        "a parsed oversized record retains its revocable token"
    );
    let mut body = tokens();
    body["expires_in"] = json!(u64::MAX);
    let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
    assert_eq!(
        exchange(&fixture, &authorization())
            .await
            .expect("clipped lifetime")
            .expires_in,
        86400
    );
}

#[tokio::test]
async fn incomplete_and_non_bearer_responses_fail_without_echoing_provider_text() {
    for field in [
        "access_token",
        "id_token",
        "scope",
        "token_type",
        "expires_in",
    ] {
        let mut body = tokens();
        body.as_object_mut().expect("object").remove(field);
        let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
        assert_eq!(
            exchange(&fixture, &authorization()).await.unwrap_err(),
            unusable("fixture-refresh"),
            "missing {field}"
        );
    }
    for (field, value) in [
        ("access_token", ""),
        ("id_token", ""),
        ("refresh_token", ""),
        ("scope", ""),
        ("token_type", SENTINEL),
    ] {
        let mut body = tokens();
        body[field] = json!(value);
        let fixture = Scripted::new(200, serde_json::to_vec(&body).expect("fixture"));
        let error = exchange(&fixture, &authorization()).await.unwrap_err();
        assert_eq!(
            error,
            unusable(
                body["refresh_token"]
                    .as_str()
                    .expect("fixture refresh token")
            )
        );
        assert!(
            !format!("{error:?} {error}").contains(SENTINEL),
            "provider text discarded"
        );
    }
}

#[tokio::test]
async fn only_typed_oauth_4xx_responses_report_a_conclusive_refusal() {
    for (status, body, expected) in [
        (
            400,
            json!({"error":"invalid_grant"}),
            ExchangeError::OAuth {
                status: 400,
                code: OAuthCode::InvalidGrant,
            },
        ),
        (
            401,
            json!({"error":{"code":"invalid_client"}}),
            ExchangeError::Response { status: 401 },
        ),
        (
            401,
            json!({"error":"invalid_client"}),
            ExchangeError::OAuth {
                status: 401,
                code: OAuthCode::InvalidClient,
            },
        ),
        (
            403,
            json!({"error":SENTINEL}),
            ExchangeError::OAuth {
                status: 403,
                code: OAuthCode::Other,
            },
        ),
        (400, json!({"error":"x".repeat(257)}), ExchangeError::Bounds),
        (
            400,
            json!({"error":""}),
            ExchangeError::Response { status: 400 },
        ),
        (
            400,
            json!({"detail":SENTINEL}),
            ExchangeError::Response { status: 400 },
        ),
        (
            503,
            json!({"error":"invalid_grant"}),
            ExchangeError::Response { status: 503 },
        ),
        (201, tokens(), unusable("fixture-refresh")),
    ] {
        let fixture = Scripted::new(status, serde_json::to_vec(&body).expect("fixture"));
        assert_eq!(
            exchange(&fixture, &authorization()).await.unwrap_err(),
            expected
        );
    }
    let fixture = Scripted::new(200, br#"{"access_token":"truncated"#.to_vec());
    assert_eq!(
        exchange(&fixture, &authorization()).await.unwrap_err(),
        ExchangeError::Response { status: 200 }
    );
}

#[tokio::test]
async fn transport_failure_remains_uncertain() {
    let fixture = Scripted {
        replies: Mutex::new(VecDeque::from([Err(HttpError::Timeout)])),
        requests: Mutex::new(Vec::new()),
    };
    assert_eq!(
        exchange(&fixture, &authorization()).await.unwrap_err(),
        ExchangeError::Http(HttpError::Timeout)
    );
}

#[tokio::test]
async fn invalid_local_inputs_are_refused_before_any_http_request() {
    let mut cases = Vec::new();
    let mut bad = authorization();
    bad.client_id = Secret::new(Zeroizing::new("dynamic_agent_client".into()));
    cases.push(bad);
    let mut bad = authorization();
    bad.code = Secret::new(Zeroizing::new("x".repeat(16 * 1024 + 1)));
    cases.push(bad);
    let mut bad = authorization();
    bad.verifier = Secret::new(Zeroizing::new("short".into()));
    cases.push(bad);
    let mut bad = authorization();
    bad.verifier = Secret::new(Zeroizing::new(format!("{}/", "v".repeat(42))));
    cases.push(bad);
    for redirect in [
        "https://127.0.0.1:1455/auth/callback",
        "http://localhost:1455/auth/callback",
        "http://127.0.0.1:1455/wrong",
        "http://127.0.0.1:1455/auth/callback?code=extra",
        "http://127.0.0.1/auth/callback",
        "http://user@127.0.0.1:1455/auth/callback",
        "http://:pass@127.0.0.1:1455/auth/callback",
        "http://127.0.0.1:1455/auth/callback#fragment",
    ] {
        let mut bad = authorization();
        bad.redirect = redirect.into();
        cases.push(bad);
    }
    for bad in cases {
        let fixture = Scripted::new(200, Vec::new());
        assert_eq!(
            exchange(&fixture, &bad).await.unwrap_err(),
            ExchangeError::Request
        );
        assert!(
            fixture.requests.lock().expect("requests").is_empty(),
            "no credentials dispatched"
        );
    }
}
