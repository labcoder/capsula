//! Offline scripted replies for the typed GitHub API.

use std::{
    collections::VecDeque,
    future::Future,
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use kumochi_protocol::{
    Secret,
    message::{FailReason, RepoName},
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{
    ApiError, TokenError, TokenReply, TokenWire, installations, poll_device_token,
    refresh_user_token, repositories, request_device_code, token_response, user,
};
use crate::connections::{
    github::{ClientId, DeviceCodeValue, GitHubEndpoint, GitHubToken, Page, RefreshToken},
    http_client::{Endpoint, Http, HttpError, Request, Response, ResponseMeta},
};
use crate::test_support::{Captured, SENTINEL};

struct Script {
    replies: Mutex<VecDeque<Result<Response, HttpError>>>,
    requests: Mutex<Vec<Request>>,
}

impl Script {
    fn new(reply: Result<Response, HttpError>) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from([reply])),
            requests: Mutex::new(Vec::new()),
        }
    }
    fn json(value: &Value) -> Self {
        Self::bytes(serde_json::to_vec(value).unwrap())
    }
    fn bytes(body: Vec<u8>) -> Self {
        Self::new(Ok(response(200, body, ResponseMeta::default())))
    }
}

impl Http for Script {
    fn execute(&self, request: Request) -> impl Future<Output = Result<Response, HttpError>> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("one scripted response per request");
        std::future::ready(reply)
    }
}

fn response(status: u16, body: Vec<u8>, meta: ResponseMeta) -> Response {
    Response {
        status,
        request_id: Some(SENTINEL.to_owned()),
        meta,
        body: Secret::new(Zeroizing::new(body)),
    }
}
fn token() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new("ghu_fake".to_owned())).unwrap())
}
fn code() -> Secret<DeviceCodeValue> {
    Secret::new(DeviceCodeValue::new(Zeroizing::new("device-fake".to_owned())).unwrap())
}
fn refresh() -> Secret<RefreshToken> {
    Secret::new(RefreshToken::new(Zeroizing::new("ghr_fake".to_owned())).unwrap())
}
fn device() -> Value {
    json!({"device_code":"device-fake", "user_code":"ABCD-EFGH", "verification_uri":"https://github.com/login/device", "expires_in":900, "interval":5})
}
fn bearer() -> Value {
    json!({"access_token":"ghu_fake", "token_type":"bearer", "refresh_token":"ghr_fake", "expires_in":28800, "refresh_token_expires_in":15_897_600})
}

#[derive(Clone, Copy, Debug)]
enum Call {
    Device,
    Token,
    Refresh,
    User,
    Installations,
    Repositories,
}
const CALLS: [Call; 6] = [
    Call::Device,
    Call::Token,
    Call::Refresh,
    Call::User,
    Call::Installations,
    Call::Repositories,
];

fn valid(call: Call) -> Value {
    match call {
        Call::Device => device(),
        Call::Token => bearer(),
        Call::Refresh => {
            let mut value = bearer();
            value["refresh_token"] = json!("ghr_rotated");
            value
        }
        Call::User => json!({"id":1,"login":"owner"}),
        Call::Installations => json!({"total_count":1,"installations":[{"id":42}]}),
        Call::Repositories => {
            json!({"total_count":1,"repositories":[{"full_name":"owner/repo","id":7}]})
        }
    }
}
async fn invoke(http: &impl Http, call: Call) -> Result<(), ApiError> {
    match call {
        Call::Device => request_device_code(http, "client-fake").await.map(|_| ()),
        Call::Token => poll_device_token(http, "client-fake", &code())
            .await
            .map(|_| ()),
        Call::Refresh => refresh_user_token(http, "client-fake", &refresh())
            .await
            .map(|_| ()),
        Call::User => user(http, &token()).await.map(|_| ()),
        Call::Installations => installations(http, &token(), Page::new(2).unwrap())
            .await
            .map(|_| ()),
        Call::Repositories => repositories(http, &token(), 42, Page::new(2).unwrap())
            .await
            .map(|_| ()),
    }
}
fn cap(call: Call) -> usize {
    match call {
        Call::Device | Call::Token | Call::Refresh => 4096,
        Call::User => 65_536,
        _ => 1_048_576,
    }
}

#[tokio::test]
async fn each_call_builds_its_checked_endpoint_and_returns_exact_values() {
    let http = Script::json(&device());
    let reply = request_device_code(&http, "client-fake").await.unwrap();
    assert_eq!(reply.code.expose().as_str(), "device-fake");
    assert_eq!(reply.user_code.as_str(), "ABCD-EFGH");
    assert_eq!((reply.expires_in_s, reply.interval_s), (900, 5));
    {
        let requests = http.requests.lock().unwrap();
        let Endpoint::GitHub(GitHubEndpoint::DeviceCode { client_id }) = &requests[0].endpoint
        else {
            panic!("typed device endpoint")
        };
        assert_eq!(client_id.as_str(), "client-fake");
    }
    let http = Script::json(&bearer());
    let TokenReply::Token(reply) = poll_device_token(&http, "client-fake", &code())
        .await
        .unwrap()
    else {
        panic!("token reply")
    };
    assert_eq!(reply.token.expose().as_str(), "ghu_fake");
    assert_eq!(reply.refresh.expose().as_str(), "ghr_fake");
    {
        let requests = http.requests.lock().unwrap();
        let Endpoint::GitHub(GitHubEndpoint::AccessToken { client_id, code }) =
            &requests[0].endpoint
        else {
            panic!("typed polling endpoint")
        };
        assert_eq!(client_id.as_str(), "client-fake");
        assert_eq!(code.expose().as_str(), "device-fake");
    }
    let http = Script::json(&valid(Call::User));
    let reply = user(&http, &token()).await.unwrap();
    assert_eq!(reply.id, 1);
    assert_eq!(reply.login.as_str(), "owner");
    let Endpoint::GitHub(GitHubEndpoint::User { token }) =
        &http.requests.lock().unwrap()[0].endpoint
    else {
        panic!("typed user endpoint")
    };
    assert_eq!(token.expose().as_str(), "ghu_fake");
}

#[tokio::test]
async fn page_calls_keep_the_requested_page_and_do_not_aggregate() {
    let http = Script::json(&valid(Call::Installations));
    let reply = installations(&http, &token(), Page::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(reply.installations, vec![42]);
    assert_eq!(reply.total_count, 1);
    {
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].fields.len(), 0);
        let Endpoint::GitHub(GitHubEndpoint::Installations { token, page }) = &requests[0].endpoint
        else {
            panic!("typed installations endpoint")
        };
        assert_eq!((token.expose().as_str(), page.0), ("ghu_fake", 2));
    }
    let http = Script::json(&valid(Call::Repositories));
    let reply = repositories(&http, &token(), 42, Page::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(
        reply.repositories,
        vec![(
            RepoName::new("owner/repo").unwrap(),
            NonZeroU64::new(7).unwrap()
        )]
    );
    assert_eq!(reply.total_count, 1);
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].fields.len(), 0);
    let Endpoint::GitHub(GitHubEndpoint::Repositories {
        token,
        installation,
        page,
    }) = &requests[0].endpoint
    else {
        panic!("typed repositories endpoint")
    };
    assert_eq!(
        (token.expose().as_str(), *installation, page.0),
        ("ghu_fake", 42, 2)
    );
}

#[tokio::test]
async fn local_input_refusals_never_dispatch() {
    for bad in ["", " x", "x ", "x\n", "é", &"x".repeat(256)] {
        let http = Script::json(&device());
        assert_eq!(
            request_device_code(&http, bad).await.unwrap_err(),
            ApiError::Http(HttpError::Request)
        );
        assert_eq!(
            poll_device_token(&http, bad, &code()).await.unwrap_err(),
            ApiError::Http(HttpError::Request)
        );
        assert_eq!(
            refresh_user_token(&http, bad, &refresh())
                .await
                .unwrap_err(),
            ApiError::Http(HttpError::Request)
        );
        assert_eq!(http.requests.lock().unwrap().len(), 0);
    }
    let http = Script::json(&valid(Call::Repositories));
    assert_eq!(
        repositories(&http, &token(), 0, Page::new(1).unwrap())
            .await
            .unwrap_err(),
        ApiError::Http(HttpError::Request)
    );
    assert_eq!(http.requests.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn every_parser_enforces_its_entry_byte_cap_with_scripted_http() {
    for call in CALLS {
        let cap = cap(call);
        for length in [cap, cap + 1] {
            let mut body = serde_json::to_vec(&valid(call)).unwrap();
            body.resize(length, b' ');
            let reply = invoke(&Script::bytes(body), call).await;
            if length == cap {
                assert_eq!(reply, Ok(()), "{call:?}");
            } else {
                assert_eq!(reply, Err(ApiError::Response), "{call:?}");
            }
        }
    }
}

#[tokio::test]
async fn every_parser_refuses_bad_roots_and_token_parsers_also_refuse_unknown_fields() {
    for call in CALLS {
        for bad in ["", "{", "null", "[]", "42", "true", "\"text\""] {
            assert_eq!(
                invoke(&Script::bytes(bad.as_bytes().to_vec()), call).await,
                Err(ApiError::Response),
                "{call:?}"
            );
        }
        assert_eq!(
            invoke(&Script::bytes(vec![0xff]), call).await,
            Err(ApiError::Response)
        );
        let mut trailing = serde_json::to_vec(&valid(call)).unwrap();
        trailing.push(b'x');
        assert_eq!(
            invoke(&Script::bytes(trailing), call).await,
            Err(ApiError::Response),
            "{call:?}"
        );
        let mut extra = valid(call);
        extra["unknown"] = json!({"message":SENTINEL,"ignored":[1,2,3]});
        assert_eq!(
            invoke(&Script::json(&extra), call).await,
            if matches!(call, Call::Token | Call::Refresh) {
                Err(ApiError::Response)
            } else {
                Ok(())
            },
            "{call:?}"
        );
    }
}

#[tokio::test]
async fn required_and_duplicate_known_fields_are_refused() {
    use std::fmt::Write;
    let required = [
        (
            Call::Device,
            vec!["device_code", "user_code", "verification_uri", "expires_in"],
        ),
        (
            Call::Token,
            vec![
                "access_token",
                "token_type",
                "refresh_token",
                "expires_in",
                "refresh_token_expires_in",
            ],
        ),
        (
            Call::Refresh,
            vec![
                "access_token",
                "token_type",
                "refresh_token",
                "expires_in",
                "refresh_token_expires_in",
            ],
        ),
        (Call::User, vec!["id", "login"]),
        (Call::Installations, vec!["total_count", "installations"]),
        (Call::Repositories, vec!["total_count", "repositories"]),
    ];
    for (call, fields) in required {
        for field in fields {
            let mut missing = valid(call);
            missing.as_object_mut().unwrap().remove(field);
            assert_eq!(
                invoke(&Script::json(&missing), call).await,
                Err(ApiError::Response),
                "{call:?} {field}"
            );
            let original = valid(call);
            let mut duplicate = serde_json::to_string(&original).unwrap();
            duplicate.pop();
            write!(duplicate, ",{field:?}:{} }}", original[field]).unwrap();
            assert_eq!(
                invoke(&Script::bytes(duplicate.into_bytes()), call).await,
                Err(ApiError::Response),
                "{call:?} {field}"
            );
            let wrong_types = match &original[field] {
                Value::Array(_) => {
                    vec![Value::Null, json!({}), json!(true), json!(1), json!("text")]
                }
                Value::Number(_) => vec![
                    Value::Null,
                    json!([]),
                    json!({}),
                    json!(true),
                    json!("text"),
                ],
                Value::String(_) => vec![Value::Null, json!([]), json!({}), json!(true), json!(1)],
                _ => panic!("fixture field grammar"),
            };
            for bad in wrong_types {
                let mut wrong = valid(call);
                wrong[field] = bad;
                assert_eq!(
                    invoke(&Script::json(&wrong), call).await,
                    Err(ApiError::Response),
                    "{call:?} {field}"
                );
            }
        }
    }
}

#[tokio::test]
async fn device_lifetimes_are_positive_bounded_and_only_missing_interval_defaults() {
    for (lifetime, interval) in [(1, 1), (900, 900)] {
        let mut value = device();
        value["expires_in"] = json!(lifetime);
        value["interval"] = json!(interval);
        let reply = request_device_code(&Script::json(&value), "client")
            .await
            .unwrap();
        assert_eq!((reply.expires_in_s, reply.interval_s), (lifetime, interval));
    }
    for field in ["expires_in", "interval"] {
        for bad in [
            json!(0),
            json!(901),
            json!(-1),
            json!(1.5),
            json!(65536),
            json!("5"),
            Value::Null,
        ] {
            let mut value = device();
            value[field] = bad;
            assert_eq!(
                request_device_code(&Script::json(&value), "client")
                    .await
                    .unwrap_err(),
                ApiError::Response
            );
        }
    }
    let mut value = device();
    value.as_object_mut().unwrap().remove("interval");
    assert_eq!(
        request_device_code(&Script::json(&value), "client")
            .await
            .unwrap()
            .interval_s,
        5
    );
    let mut duplicate = serde_json::to_string(&device()).unwrap();
    duplicate.pop();
    duplicate.push_str(",\"interval\":5}");
    assert_eq!(
        request_device_code(&Script::bytes(duplicate.into_bytes()), "client")
            .await
            .unwrap_err(),
        ApiError::Response
    );
}

#[tokio::test]
async fn device_codes_and_verification_uri_are_checked_without_normalizing() {
    let mut value = device();
    value["device_code"] = json!(" ".repeat(255));
    value["user_code"] = json!("A".repeat(16));
    let reply = request_device_code(&Script::json(&value), "client")
        .await
        .unwrap();
    assert_eq!(reply.code.expose().as_str(), " ".repeat(255));
    assert_eq!(reply.user_code.as_str(), "A".repeat(16));
    for (field, bad) in [
        ("device_code", String::new()),
        ("device_code", "x".repeat(256)),
        ("device_code", "x\n".to_owned()),
        ("device_code", "é".to_owned()),
        ("user_code", "A".repeat(17)),
        ("user_code", String::new()),
        ("user_code", "abcd".to_owned()),
        ("user_code", "A B".to_owned()),
        (
            "verification_uri",
            "https://github.com/login/device/".to_owned(),
        ),
        (
            "verification_uri",
            "http://github.com/login/device".to_owned(),
        ),
        (
            "verification_uri",
            "https://example.com/login/device".to_owned(),
        ),
    ] {
        let mut value = device();
        value[field] = json!(bad);
        assert_eq!(
            request_device_code(&Script::json(&value), "client")
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
}

#[tokio::test]
async fn token_reply_maps_only_closed_codes_and_checks_the_bearer() {
    for (wire, expected) in [
        ("authorization_pending", TokenError::AuthorizationPending),
        ("slow_down", TokenError::SlowDown),
        ("expired_token", TokenError::ExpiredToken),
        ("access_denied", TokenError::AccessDenied),
        ("device_flow_disabled", TokenError::DeviceFlowDisabled),
        ("incorrect_device_code", TokenError::IncorrectDeviceCode),
        ("unsupported_grant_type", TokenError::UnsupportedGrantType),
        (
            "incorrect_client_credentials",
            TokenError::IncorrectClientCredentials,
        ),
        (SENTINEL, TokenError::Other),
    ] {
        let reply = poll_device_token(&Script::json(&json!({"error":wire})), "client", &code())
            .await
            .unwrap();
        let TokenReply::Error {
            code: kind,
            interval_s: None,
        } = reply
        else {
            panic!("closed polling refusal")
        };
        assert_eq!(kind, expected);
    }
    let exact = format!("ghu_{}", "x".repeat(251));
    let mut value = bearer();
    value["access_token"] = json!(exact);
    let TokenReply::Token(reply) = poll_device_token(&Script::json(&value), "client", &code())
        .await
        .unwrap()
    else {
        panic!("checked bearer")
    };
    assert_eq!(reply.token.expose().as_str(), exact);
    for bad in [
        String::new(),
        "ghu_".to_owned(),
        "gho_fake".to_owned(),
        "ghu_a b".to_owned(),
        "ghu_x\n".to_owned(),
        "ghu_é".to_owned(),
        format!("ghu_{}", "x".repeat(252)),
    ] {
        let mut value = bearer();
        value["access_token"] = json!(bad);
        assert_eq!(
            poll_device_token(&Script::json(&value), "client", &code())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    for bad in [
        json!({"access_token":"ghu_fake","token_type":"other"}),
        json!({"access_token":"ghu_fake","token_type":"bearer","error":"slow_down"}),
        json!({"error":1}),
        json!({"error":null}),
        json!({"error":{"authorization_pending":null}}),
        json!({"token_type":"bearer"}),
    ] {
        assert_eq!(
            poll_device_token(&Script::json(&bad), "client", &code())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
}

#[tokio::test]
async fn only_complete_bounded_expiring_credentials_are_accepted() {
    for call in [Call::Token, Call::Refresh] {
        assert_eq!(
            invoke(
                &Script::json(&json!({"access_token":"ghu_fake","token_type":"bearer"})),
                call
            )
            .await,
            Err(ApiError::Response)
        );
        for (field, values) in [
            ("expires_in", vec![0, 86_401, u64::MAX]),
            ("refresh_token_expires_in", vec![0, 31_622_401, u64::MAX]),
        ] {
            for value in values {
                let mut body = valid(call);
                body[field] = json!(value);
                assert_eq!(
                    invoke(&Script::json(&body), call).await,
                    Err(ApiError::Response)
                );
            }
        }
        for value in [
            "",
            "ghr_",
            "ghu_fake",
            "ghr_a b",
            "ghr_x\n",
            "ghr_é",
            &format!("ghr_{}", "x".repeat(252)),
        ] {
            let mut body = valid(call);
            body["refresh_token"] = json!(value);
            assert_eq!(
                invoke(&Script::json(&body), call).await,
                Err(ApiError::Response)
            );
        }
        for field in ["refresh_token", "expires_in", "refresh_token_expires_in"] {
            for value in [Value::Null, json!([]), json!({}), json!(true)] {
                let mut body = valid(call);
                body[field] = value;
                assert_eq!(
                    invoke(&Script::json(&body), call).await,
                    Err(ApiError::Response)
                );
            }
        }
        for scope in [None, Some("")] {
            let mut body = valid(call);
            body["refresh_token"] = json!(format!("ghr_{}", "x".repeat(251)));
            if let Some(scope) = scope {
                body["scope"] = json!(scope);
            }
            assert_eq!(invoke(&Script::json(&body), call).await, Ok(()));
        }
    }
}

#[tokio::test]
async fn returned_lifetimes_allow_shorter_longer_equal_and_refresh_first_expiries() {
    for (access, refresh, access_end, refresh_end) in [
        (1, 1, 1001, 1001),
        (1, 2, 1001, 1002),
        (123, 7, 1123, 1007),
        (28_799, 15_897_599, 29_799, 15_898_599),
        (28_801, 15_897_601, 29_801, 15_898_601),
        (86_400, 31_622_400, 87_400, 31_623_400),
        (86_400, 1, 87_400, 1001),
        (1, 31_622_400, 1001, 31_623_400),
    ] {
        for call in [Call::Token, Call::Refresh] {
            let mut value = valid(call);
            value["expires_in"] = json!(access);
            value["refresh_token_expires_in"] = json!(refresh);
            let raw: TokenWire = serde_json::from_value(value.clone()).unwrap();
            let TokenReply::Token(tokens) = raw.reply(1000).unwrap() else {
                panic!("bounded expiring pair");
            };
            assert_eq!(tokens.expires_at_s, access_end);
            assert_eq!(tokens.refresh_expires_at_s, refresh_end);
            assert_eq!(invoke(&Script::json(&value), call).await, Ok(()));
        }
    }
}

#[tokio::test]
async fn lifetime_integer_overflow_is_refused_by_both_token_requests() {
    for call in [Call::Token, Call::Refresh] {
        for field in ["expires_in", "refresh_token_expires_in"] {
            let value = valid(call);
            let body = serde_json::to_string(&value).unwrap().replace(
                &format!("\"{field}\":{}", value[field]),
                &format!("\"{field}\":18446744073709551616"),
            );
            assert_eq!(
                invoke(&Script::bytes(body.into_bytes()), call).await,
                Err(ApiError::Response)
            );
        }
    }
}

#[tokio::test]
async fn token_shapes_reject_mixed_errors_polling_advice_scope_and_unknown_fields() {
    for call in [Call::Token, Call::Refresh] {
        for (field, value) in [
            ("token_type", json!("Bearer")),
            ("token_type", json!("other")),
            ("scope", json!("repo")),
            ("scope", json!(" ")),
            ("scope", Value::Null),
            ("scope", json!([])),
            ("scope", json!({})),
            ("scope", json!(false)),
            ("scope", json!(0)),
            ("interval", json!(5)),
            ("error", json!("slow_down")),
            ("error_description", json!(SENTINEL)),
            ("error_uri", json!(SENTINEL)),
            ("unknown", json!(SENTINEL)),
        ] {
            let mut body = valid(call);
            body[field] = value;
            assert_eq!(
                invoke(&Script::json(&body), call).await,
                Err(ApiError::Response),
                "{field}"
            );
        }
    }
    for (field, value) in bearer().as_object().unwrap() {
        let mut body = json!({"error":"slow_down"});
        body[field] = value.clone();
        assert_eq!(
            invoke(&Script::json(&body), Call::Token).await,
            Err(ApiError::Response),
            "{field}"
        );
        assert_eq!(
            invoke(&Script::json(&body), Call::Refresh).await,
            Err(ApiError::Response),
            "{field}"
        );
    }
}

#[tokio::test]
async fn expiry_uses_request_start_rounded_down_not_completion_or_provider_clock() {
    struct ClockHttp(Mutex<SystemTime>);
    impl Http for ClockHttp {
        fn execute(&self, _request: Request) -> impl Future<Output = Result<Response, HttpError>> {
            *self.0.lock().unwrap() += Duration::from_secs(60);
            std::future::ready(Ok(response(
                200,
                serde_json::to_vec(&bearer()).unwrap(),
                ResponseMeta {
                    date_s: Some(u64::MAX),
                    ..ResponseMeta::default()
                },
            )))
        }
    }
    for endpoint in [
        GitHubEndpoint::AccessToken {
            client_id: ClientId::new("client").unwrap(),
            code: code(),
        },
        GitHubEndpoint::RefreshToken {
            client_id: ClientId::new("client").unwrap(),
            refresh: refresh(),
        },
    ] {
        let http = ClockHttp(Mutex::new(
            UNIX_EPOCH + Duration::new(1_700_000_000, 999_999_999),
        ));
        let (response, started_at_s) = token_response(&http, endpoint, || *http.0.lock().unwrap())
            .await
            .unwrap();
        assert_eq!(started_at_s, 1_700_000_000);
        assert_eq!(
            *http.0.lock().unwrap(),
            UNIX_EPOCH + Duration::new(1_700_000_060, 999_999_999)
        );
        let raw: TokenWire = super::parse(&response, 200, 4096).unwrap();
        let TokenReply::Token(tokens) = raw.reply(started_at_s).unwrap() else {
            panic!("expiring credentials")
        };
        assert_eq!(tokens.token.expose().as_str(), "ghu_fake");
        assert_eq!(tokens.refresh.expose().as_str(), "ghr_fake");
        assert_eq!(tokens.expires_at_s, 1_700_028_800);
        assert_eq!(tokens.refresh_expires_at_s, 1_715_897_600);
    }
    let http = Script::json(&bearer());
    assert_eq!(
        token_response(
            &http,
            GitHubEndpoint::RefreshToken {
                client_id: ClientId::new("client").unwrap(),
                refresh: refresh()
            },
            || UNIX_EPOCH - Duration::from_secs(1)
        )
        .await
        .unwrap_err(),
        ApiError::Response
    );
    assert_eq!(http.requests.lock().unwrap().len(), 0);
}

#[test]
fn expiry_addition_refuses_each_overflow_and_accepts_the_last_representable_pair() {
    for start in [u64::MAX - 15_897_600 + 1, u64::MAX - 28_800 + 1, u64::MAX] {
        let raw: TokenWire = serde_json::from_value(bearer()).unwrap();
        assert_eq!(raw.reply(start).unwrap_err(), ApiError::Response);
    }
    let raw: TokenWire = serde_json::from_value(bearer()).unwrap();
    let TokenReply::Token(tokens) = raw.reply(u64::MAX - 15_897_600).unwrap() else {
        panic!("last expiring pair")
    };
    assert_eq!(tokens.expires_at_s, u64::MAX - 15_868_800);
    assert_eq!(tokens.refresh_expires_at_s, u64::MAX);
}

#[test]
fn each_returned_upper_bound_checks_its_own_expiry_addition() {
    for (access, refresh, start) in [
        (86_400, 1, u64::MAX - 86_400 + 1),
        (1, 31_622_400, u64::MAX - 31_622_400 + 1),
    ] {
        let mut value = bearer();
        value["expires_in"] = json!(access);
        value["refresh_token_expires_in"] = json!(refresh);
        let raw: TokenWire = serde_json::from_value(value).unwrap();
        assert_eq!(raw.reply(start).unwrap_err(), ApiError::Response);
    }
    let mut value = bearer();
    value["expires_in"] = json!(86_400);
    value["refresh_token_expires_in"] = json!(31_622_400);
    let raw: TokenWire = serde_json::from_value(value).unwrap();
    let TokenReply::Token(tokens) = raw.reply(u64::MAX - 31_622_400).unwrap() else {
        panic!("last bounded expiring pair");
    };
    assert_eq!(tokens.expires_at_s, u64::MAX - 31_536_000);
    assert_eq!(tokens.refresh_expires_at_s, u64::MAX);
}

#[tokio::test]
async fn refresh_rotates_once_and_provider_errors_never_poll_or_retry() {
    let mut value = bearer();
    value["access_token"] = json!("ghu_rotated");
    value["refresh_token"] = json!("ghr_rotated");
    let http = Script::json(&value);
    let reply = refresh_user_token(&http, "client", &refresh())
        .await
        .unwrap();
    assert_eq!(reply.token.expose().as_str(), "ghu_rotated");
    assert_eq!(reply.refresh.expose().as_str(), "ghr_rotated");
    {
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].fields.len(), 0);
        let Endpoint::GitHub(GitHubEndpoint::RefreshToken { client_id, refresh }) =
            &requests[0].endpoint
        else {
            panic!("typed refresh endpoint")
        };
        assert_eq!(client_id.as_str(), "client");
        assert_eq!(refresh.expose().as_str(), "ghr_fake");
    }
    for status in [200, 400] {
        for code in [
            "authorization_pending",
            "slow_down",
            "bad_refresh_token",
            "incorrect_client_credentials",
            SENTINEL,
        ] {
            let http = Script::new(Ok(response(
                status,
                serde_json::to_vec(&json!({"error":code,"error_description":SENTINEL,"error_uri":SENTINEL,"interval":5})).unwrap(),
                ResponseMeta { retry_after_s: Some(u64::MAX), retry_after_present: true, ..ResponseMeta::default() },
            )));
            http.replies.lock().unwrap().push_back(Ok(response(
                200,
                serde_json::to_vec(&value).unwrap(),
                ResponseMeta::default(),
            )));
            let error = refresh_user_token(&http, "client", &refresh())
                .await
                .unwrap_err();
            assert_eq!(
                error,
                ApiError::Refused {
                    status,
                    meta: ResponseMeta {
                        retry_after_s: Some(3600),
                        retry_after_present: true,
                        ..ResponseMeta::default()
                    }
                }
            );
            assert_redacted(&format!("{error:?} {error}"));
            assert_eq!(http.requests.lock().unwrap().len(), 1);
            assert_eq!(http.replies.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn refresh_rejects_a_reused_refresh_credential_even_with_a_changed_access_token() {
    let original =
        Secret::new(RefreshToken::new(Zeroizing::new(format!("ghr_{SENTINEL}"))).unwrap());
    let mut value = bearer();
    value["access_token"] = json!("ghu_rotated");
    value["refresh_token"] = json!(format!("ghr_{SENTINEL}"));
    let http = Script::json(&value);
    http.replies.lock().unwrap().push_back(Ok(response(
        200,
        serde_json::to_vec(&valid(Call::Refresh)).unwrap(),
        ResponseMeta::default(),
    )));
    let error = refresh_user_token(&http, "client", &original)
        .await
        .unwrap_err();
    assert_eq!(error, ApiError::Response);
    assert_redacted(&format!("{error:?} {error}"));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
    assert_eq!(http.replies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn optional_token_fields_reject_duplicates_and_invalid_expiry_types() {
    use std::fmt::Write;
    for (field, value) in [
        ("refresh_token", json!("ghr_fake")),
        ("expires_in", json!(28800)),
        ("refresh_token_expires_in", json!(15_897_600)),
        ("scope", json!("")),
    ] {
        let mut original = bearer();
        original[field] = value.clone();
        let mut duplicate = serde_json::to_string(&original).unwrap();
        duplicate.pop();
        write!(duplicate, ",{field:?}:{value}}}").unwrap();
        assert_eq!(
            poll_device_token(&Script::bytes(duplicate.into_bytes()), "client", &code())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    assert_eq!(
        poll_device_token(
            &Script::bytes(br#"{"error":"authorization_pending","error":"slow_down"}"#.to_vec()),
            "client",
            &code()
        )
        .await
        .unwrap_err(),
        ApiError::Response
    );
    for field in ["expires_in", "refresh_token_expires_in"] {
        for value in [json!(-1), json!(1.5), json!("28800")] {
            let mut reply = bearer();
            reply[field] = value;
            assert_eq!(
                poll_device_token(&Script::json(&reply), "client", &code())
                    .await
                    .unwrap_err(),
                ApiError::Response
            );
        }
    }
}

#[tokio::test]
async fn user_requires_a_positive_id_and_a_checked_login() {
    let exact = json!({"id":u64::MAX,"login":"a".repeat(39)});
    let reply = user(&Script::json(&exact), &token()).await.unwrap();
    assert_eq!(reply.id, u64::MAX);
    assert_eq!(reply.login.as_str(), "a".repeat(39));
    for id in [json!(0), json!(-1), json!(1.5), json!("1")] {
        assert_eq!(
            user(&Script::json(&json!({"id":id,"login":"owner"})), &token())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    for login in ["", "-owner", "a b", "é", &"a".repeat(40)] {
        assert_eq!(
            user(&Script::json(&json!({"id":1,"login":login})), &token())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
}

#[tokio::test]
async fn list_pages_accept_empty_and_exactly_one_hundred_rows_but_not_one_over() {
    for count in [0_u64, 100, 101] {
        let ids: Vec<_> = (1..=count).map(|id| json!({"id":id})).collect();
        let http = Script::json(&json!({"total_count":count,"installations":ids}));
        let result = installations(&http, &token(), Page::new(1).unwrap()).await;
        if count == 101 {
            assert_eq!(result.unwrap_err(), ApiError::Response);
        } else {
            let page = result.unwrap();
            assert_eq!(page.total_count, count);
            assert_eq!(page.installations, (1..=count).collect::<Vec<_>>());
        }
        let names: Vec<_> = (1..=count)
            .map(|id| json!({"full_name":format!("owner/r{id}"),"id":id}))
            .collect();
        let http = Script::json(&json!({"total_count":count,"repositories":names}));
        let result = repositories(&http, &token(), 1, Page::new(1).unwrap()).await;
        if count == 101 {
            assert_eq!(result.unwrap_err(), ApiError::Response);
        } else {
            let page = result.unwrap();
            assert_eq!(page.total_count, count);
            assert_eq!(
                page.repositories,
                (1..=count)
                    .map(|id| (
                        RepoName::new(&format!("owner/r{id}")).unwrap(),
                        NonZeroU64::new(id).unwrap()
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn list_totals_reject_negative_fractional_and_overflowing_numbers() {
    for call in [Call::Installations, Call::Repositories] {
        for bad in [json!(-1), json!(1.5)] {
            let mut value = valid(call);
            value["total_count"] = bad;
            assert_eq!(
                invoke(&Script::json(&value), call).await,
                Err(ApiError::Response)
            );
        }
        let overflow = serde_json::to_string(&valid(call))
            .unwrap()
            .replace("\"total_count\":1", "\"total_count\":18446744073709551616");
        assert_eq!(
            invoke(&Script::bytes(overflow.into_bytes()), call).await,
            Err(ApiError::Response),
            "{call:?}"
        );
    }
}

#[tokio::test]
async fn installation_rows_require_positive_typed_ids_and_reject_duplicate_fields() {
    for row in [
        Value::Null,
        json!(1),
        json!([]),
        json!({}),
        json!({"id":0}),
        json!({"id":-1}),
        json!({"id":1.5}),
        json!({"id":"1"}),
    ] {
        let http = Script::json(&json!({"total_count":1,"installations":[row]}));
        assert_eq!(
            installations(&http, &token(), Page::new(1).unwrap())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    let http = Script::bytes(br#"{"total_count":1,"installations":[{"id":1,"id":2}]}"#.to_vec());
    assert_eq!(
        installations(&http, &token(), Page::new(1).unwrap())
            .await
            .unwrap_err(),
        ApiError::Response
    );
    let http = Script::json(
        &json!({"total_count":u64::MAX,"installations":[{"id":u64::MAX,"unknown":SENTINEL}]}),
    );
    let page = installations(&http, &token(), Page::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(page.total_count, u64::MAX);
    assert_eq!(page.installations, vec![u64::MAX]);
}

#[tokio::test]
async fn repository_rows_require_checked_names_and_reject_duplicate_fields() {
    let exact = format!("{}/{}", "a".repeat(39), "b".repeat(100));
    let http = Script::json(
        &json!({"total_count":1,"repositories":[{"full_name":exact,"id":7,"unknown":SENTINEL}]}),
    );
    let page = repositories(&http, &token(), 1, Page::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(
        page.repositories,
        vec![(RepoName::new(&exact).unwrap(), NonZeroU64::new(7).unwrap())]
    );
    for bad in [
        String::new(),
        "owner".to_owned(),
        "/repo".to_owned(),
        "owner/".to_owned(),
        "owner/.".to_owned(),
        "owner/..".to_owned(),
        "owner/a/b".to_owned(),
        "owner/é".to_owned(),
        format!("{}/repo", "a".repeat(40)),
        format!("owner/{}", "b".repeat(101)),
    ] {
        let http =
            Script::json(&json!({"total_count":1,"repositories":[{"full_name":bad,"id":7}]}));
        assert_eq!(
            repositories(&http, &token(), 1, Page::new(1).unwrap())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    for row in [
        Value::Null,
        json!(1),
        json!([]),
        json!({}),
        json!({"full_name":null}),
        json!({"full_name":true}),
        json!({"full_name":1}),
        json!({"full_name":"owner/repo"}),
        json!({"full_name":"owner/repo","id":null}),
        json!({"full_name":"owner/repo","id":0}),
        json!({"full_name":"owner/repo","id":-7}),
        json!({"full_name":"owner/repo","id":"7"}),
        json!({"full_name":"owner/repo","id":7.5}),
        json!({"full_name":"owner/repo","id":[7]}),
    ] {
        let http = Script::json(&json!({"total_count":1,"repositories":[row]}));
        assert_eq!(
            repositories(&http, &token(), 1, Page::new(1).unwrap())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    let http = Script::bytes(
        br#"{"total_count":1,"repositories":[{"full_name":"owner/one","id":7,"full_name":"owner/two"}]}"#
            .to_vec(),
    );
    assert_eq!(
        repositories(&http, &token(), 1, Page::new(1).unwrap())
            .await
            .unwrap_err(),
        ApiError::Response
    );
}

#[test]
fn definite_classification_keeps_transport_uncertainty_and_the_full_status_table() {
    for kind in [
        HttpError::Connect,
        HttpError::TrustStoreUnavailable,
        HttpError::Configuration,
        HttpError::Endpoint,
        HttpError::Request,
    ] {
        assert_eq!(
            ApiError::Http(kind).definite_reason(),
            Some(FailReason::Network)
        );
    }
    for kind in [
        HttpError::Timeout,
        HttpError::Transport,
        HttpError::Redirect,
        HttpError::BodyTooLarge,
    ] {
        assert_eq!(ApiError::Http(kind).definite_reason(), None);
    }
    for (status, expected) in [
        (400, FailReason::Rejected),
        (401, FailReason::NotConnected),
        (402, FailReason::Rejected),
        (403, FailReason::PermissionDenied),
        (404, FailReason::NotFound),
        (405, FailReason::Rejected),
        (409, FailReason::Rejected),
        (410, FailReason::NotFound),
        (418, FailReason::Rejected),
        (422, FailReason::InvalidIssue),
        (429, FailReason::RateLimited),
        (451, FailReason::Rejected),
        (499, FailReason::Rejected),
    ] {
        assert_eq!(
            ApiError::Refused {
                status,
                meta: ResponseMeta::default()
            }
            .definite_reason(),
            Some(expected)
        );
    }
    for status in [
        0, 199, 200, 201, 202, 204, 206, 300, 301, 302, 307, 308, 399, 500, 503, 599, 600,
    ] {
        assert_eq!(
            ApiError::Refused {
                status,
                meta: ResponseMeta::default()
            }
            .definite_reason(),
            None
        );
    }
    assert_eq!(ApiError::Response.definite_reason(), None);
}

#[tokio::test]
async fn all_calls_preserve_closed_transport_errors_and_refused_status_metadata() {
    for call in CALLS {
        for kind in [
            HttpError::TrustStoreUnavailable,
            HttpError::Configuration,
            HttpError::Endpoint,
            HttpError::Request,
            HttpError::Redirect,
            HttpError::Timeout,
            HttpError::Connect,
            HttpError::Transport,
            HttpError::BodyTooLarge,
        ] {
            assert_eq!(
                invoke(&Script::new(Err(kind)), call).await,
                Err(ApiError::Http(kind)),
                "{call:?}"
            );
        }
        let meta = ResponseMeta {
            date_s: Some(77),
            retry_after_s: Some(60),
            retry_after_present: true,
            rate_remaining: Some(0),
            rate_reset_s: Some(137),
        };
        for status in [201, 301, 400, 401, 403, 429, 500] {
            let http = Script::new(Ok(response(status, SENTINEL.as_bytes().to_vec(), meta)));
            let expected = match (call, status) {
                (Call::Token, 400) => ApiError::Response,
                _ => ApiError::Refused { status, meta },
            };
            assert_eq!(invoke(&http, call).await, Err(expected), "{call:?}");
        }
        for status in [403, 429] {
            let http = Script::new(Ok(response(status, vec![b' '; cap(call) + 1], meta)));
            assert_eq!(
                invoke(&http, call).await,
                Err(ApiError::Refused { status, meta }),
                "an oversized {status} body stays definite for {call:?}"
            );
        }
    }
}

#[tokio::test]
async fn retry_advice_is_capped_at_one_hour_and_unusable_advice_stays_absent() {
    for call in CALLS {
        for seconds in [3600, 3601, u64::MAX] {
            let meta = ResponseMeta {
                retry_after_s: Some(seconds),
                retry_after_present: true,
                date_s: Some(77),
                rate_remaining: Some(0),
                rate_reset_s: Some(137),
            };
            let http = Script::new(Ok(response(429, b"{}".to_vec(), meta)));
            assert_eq!(
                invoke(&http, call).await,
                Err(ApiError::Refused {
                    status: 429,
                    meta: ResponseMeta {
                        retry_after_s: Some(3600),
                        ..meta
                    }
                })
            );
        }
        let meta = ResponseMeta {
            retry_after_present: true,
            ..ResponseMeta::default()
        };
        let error = invoke(&Script::new(Ok(response(403, b"{}".to_vec(), meta))), call)
            .await
            .unwrap_err();
        assert_eq!(error, ApiError::Refused { status: 403, meta });
        assert_eq!(error.definite_reason(), Some(FailReason::RateLimited));
    }
    assert_eq!(
        ApiError::Refused {
            status: 403,
            meta: ResponseMeta {
                rate_remaining: Some(0),
                ..ResponseMeta::default()
            }
        }
        .definite_reason(),
        Some(FailReason::RateLimited)
    );
    assert_eq!(
        ApiError::Refused {
            status: 403,
            meta: ResponseMeta {
                rate_reset_s: Some(100),
                rate_remaining: Some(1),
                ..ResponseMeta::default()
            }
        }
        .definite_reason(),
        Some(FailReason::PermissionDenied)
    );
}

struct Held(Arc<AtomicUsize>);
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Http for Held {
    async fn execute(&self, _: Request) -> Result<Response, HttpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let _active = Active(Arc::clone(&self.0));
        std::future::pending().await
    }
}
async fn pending_once<T>(mut future: std::pin::Pin<&mut impl Future<Output = T>>) {
    let state = std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
    assert!(matches!(state, Poll::Pending));
}

#[tokio::test(start_paused = true)]
async fn every_injected_http_future_hits_its_fifteen_second_deadline_and_is_dropped() {
    for call in CALLS {
        let active = Arc::new(AtomicUsize::new(0));
        let http = Held(Arc::clone(&active));
        let future = invoke(&http, call);
        tokio::pin!(future);
        pending_once(future.as_mut()).await;
        assert_eq!(active.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(14)).await;
        pending_once(future.as_mut()).await;
        assert_eq!(active.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        let state = std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
        assert_eq!(state, Poll::Ready(Err(ApiError::Http(HttpError::Timeout))));
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_each_api_future_drops_its_injected_request_without_a_background_task() {
    for call in CALLS {
        let active = Arc::new(AtomicUsize::new(0));
        let http = Held(Arc::clone(&active));
        {
            let future = invoke(&http, call);
            tokio::pin!(future);
            pending_once(future.as_mut()).await;
            assert_eq!(active.load(Ordering::SeqCst), 1);
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

fn assert_redacted(text: &str) {
    for marker in [SENTINEL.to_owned(), SENTINEL.to_ascii_uppercase()] {
        assert!(!text.contains(&marker));
        let bytes = format!("{:?}", marker.as_bytes());
        assert!(!text.contains(&bytes));
        assert!(!text.contains(bytes.trim_start_matches('[').trim_end_matches(']')));
    }
}

#[tokio::test]
async fn outermost_success_carriers_redact_credentials_user_codes_logins_and_repositories() {
    let mut value = device();
    value["device_code"] = json!(SENTINEL);
    value["user_code"] = json!(SENTINEL.to_ascii_uppercase());
    let reply = request_device_code(&Script::json(&value), "client")
        .await
        .unwrap();
    let cloned = reply.clone();
    assert_eq!(reply.code.expose().as_str(), SENTINEL);
    assert_eq!(reply.user_code.as_str(), SENTINEL.to_ascii_uppercase());
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(reply)));
    assert_eq!(cloned.code.expose().as_str(), SENTINEL);
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(cloned)));
    let mut value = bearer();
    value["access_token"] = json!(format!("ghu_{SENTINEL}"));
    value["refresh_token"] = json!(format!("ghr_{SENTINEL}"));
    let reply = poll_device_token(&Script::json(&value), "client", &code())
        .await
        .unwrap();
    let cloned = reply.clone();
    let TokenReply::Token(bearer) = &reply else {
        panic!("sentinel bearer")
    };
    assert_eq!(bearer.token.expose().as_str(), format!("ghu_{SENTINEL}"));
    assert_eq!(bearer.refresh.expose().as_str(), format!("ghr_{SENTINEL}"));
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(bearer.clone())));
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(reply)));
    let TokenReply::Token(cloned) = cloned else {
        panic!("cloned sentinel credentials")
    };
    assert_eq!(cloned.token.expose().as_str(), format!("ghu_{SENTINEL}"));
    assert_eq!(cloned.refresh.expose().as_str(), format!("ghr_{SENTINEL}"));
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(cloned)));
    let tokens = refresh_user_token(&Script::json(&value), "client", &refresh())
        .await
        .unwrap();
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(tokens.clone())));
    assert_eq!(tokens.token.expose().as_str(), format!("ghu_{SENTINEL}"));
    assert_eq!(tokens.refresh.expose().as_str(), format!("ghr_{SENTINEL}"));
    let reply = user(&Script::json(&json!({"id":1,"login":SENTINEL})), &token())
        .await
        .unwrap();
    assert_eq!(reply.login.as_str(), SENTINEL);
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(reply)));
    let value = json!({"total_count":1,"repositories":[{"full_name":format!("{SENTINEL}/{SENTINEL}"),"id":7}]});
    let reply = repositories(&Script::json(&value), &token(), 1, Page::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(
        reply.repositories,
        vec![(
            RepoName::new(&format!("{SENTINEL}/{SENTINEL}")).unwrap(),
            NonZeroU64::new(7).unwrap()
        )]
    );
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(reply)));
    let reply = installations(
        &Script::json(
            &json!({"total_count":1,"installations":[{"id":9_876_543_210_987_654_321_u64}]}),
        ),
        &token(),
        Page::new(1).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(reply.installations, vec![9_876_543_210_987_654_321]);
    let printed = format!("{:?}", Ok::<_, ApiError>(reply));
    assert_redacted(&printed);
    assert!(!printed.contains("9876543210987654321"));
}

#[tokio::test]
async fn foreign_reply_text_and_request_ids_never_enter_errors_debug_or_logs() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("p2b1-capture-control");
    for call in CALLS {
        let error = invoke(&Script::bytes(SENTINEL.as_bytes().to_vec()), call)
            .await
            .unwrap_err();
        assert_eq!(error, ApiError::Response);
        assert_redacted(&format!("{error:?} {error}"));
        let http = Script::new(Ok(response(
            403,
            SENTINEL.as_bytes().to_vec(),
            ResponseMeta::default(),
        )));
        let error = invoke(&http, call).await.unwrap_err();
        assert_eq!(
            error,
            ApiError::Refused {
                status: 403,
                meta: ResponseMeta::default()
            }
        );
        assert_redacted(&format!("{error:?} {error}"));
    }
    let reply = poll_device_token(
        &Script::json(&json!({"error":SENTINEL,"error_description":SENTINEL})),
        "client",
        &code(),
    )
    .await
    .unwrap();
    let TokenReply::Error {
        code: error,
        interval_s: None,
    } = reply
    else {
        panic!("closed provider code")
    };
    assert_eq!(error, TokenError::Other);
    assert_redacted(&format!("{error:?}"));
    let mut value = bearer();
    value["refresh_token"] = json!(format!("ghr_{SENTINEL}"));
    value.as_object_mut().unwrap().remove("expires_in");
    let error = poll_device_token(&Script::json(&value), "client", &code())
        .await
        .unwrap_err();
    assert_eq!(error, ApiError::Response);
    assert_redacted(&format!("{error:?} {error}"));
    let logs = captured.text();
    assert!(logs.contains("p2b1-capture-control"));
    assert_redacted(&logs);
}

#[tokio::test]
async fn device_errors_accept_only_200_or_400_and_tokens_require_200() {
    for status in [200, 400] {
        for (interval, expected) in [(None, None), (Some(1), Some(1)), (Some(900), Some(900))] {
            let mut body = json!({"error":"slow_down"});
            if let Some(interval) = interval {
                body["interval"] = json!(interval);
            }
            let http = Script::new(Ok(response(
                status,
                serde_json::to_vec(&body).unwrap(),
                ResponseMeta::default(),
            )));
            let TokenReply::Error { code, interval_s } =
                poll_device_token(&http, "client", &code()).await.unwrap()
            else {
                panic!("error reply")
            };
            assert_eq!(code, TokenError::SlowDown);
            assert_eq!(interval_s, expected);
        }
        for invalid in [
            json!(0),
            json!(901),
            json!(-1),
            json!(1.5),
            json!("5"),
            Value::Null,
            json!(65536),
        ] {
            let http = Script::new(Ok(response(
                status,
                serde_json::to_vec(&json!({"error":"slow_down", "interval":invalid})).unwrap(),
                ResponseMeta::default(),
            )));
            assert_eq!(
                poll_device_token(&http, "client", &code())
                    .await
                    .unwrap_err(),
                ApiError::Response
            );
        }
    }
    let body = serde_json::to_vec(&bearer()).unwrap();
    let http = Script::new(Ok(response(400, body, ResponseMeta::default())));
    assert_eq!(
        poll_device_token(&http, "client", &code())
            .await
            .unwrap_err(),
        ApiError::Response
    );
    for status in [201, 401, 403, 429, 500] {
        let http = Script::new(Ok(response(
            status,
            br#"{"error":"authorization_pending"}"#.to_vec(),
            ResponseMeta::default(),
        )));
        assert_eq!(
            poll_device_token(&http, "client", &code())
                .await
                .unwrap_err(),
            ApiError::Refused {
                status,
                meta: ResponseMeta::default()
            }
        );
    }
}

#[tokio::test]
async fn interval_duplicates_and_bad_intervals_on_other_token_shapes_fail_closed() {
    for status in [200, 400] {
        let body = br#"{"error":"slow_down","interval":5,"interval":10}"#.to_vec();
        let http = Script::new(Ok(response(status, body, ResponseMeta::default())));
        assert_eq!(
            poll_device_token(&http, "client", &code())
                .await
                .unwrap_err(),
            ApiError::Response
        );
        let http = Script::new(Ok(response(
            status,
            br#"{"error":"authorization_pending","interval":0}"#.to_vec(),
            ResponseMeta::default(),
        )));
        assert_eq!(
            poll_device_token(&http, "client", &code())
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    let mut body = bearer();
    body["interval"] = json!(901);
    assert_eq!(
        poll_device_token(&Script::json(&body), "client", &code())
            .await
            .unwrap_err(),
        ApiError::Response
    );
    let http = Script::new(Ok(response(
        400,
        br#"{"error":"slow_down","refresh_token":"ghr_fake"}"#.to_vec(),
        ResponseMeta::default(),
    )));
    assert_eq!(
        poll_device_token(&http, "client", &code())
            .await
            .unwrap_err(),
        ApiError::Response
    );
}
