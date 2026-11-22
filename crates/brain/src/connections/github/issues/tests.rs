//! Offline issue replies and exact single-attempt dispatch evidence.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

use serde_json::{Value, json};

use super::*;
use crate::connections::http_client::{Endpoint, Request, Response};
use crate::test_support::{Captured, SENTINEL};

struct Step {
    delay: u64,
    reply: Result<Response, HttpError>,
}
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Request>>,
    active: Arc<AtomicUsize>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Script {
    fn new(replies: Vec<Result<Response, HttpError>>) -> Self {
        Self {
            steps: Mutex::new(
                replies
                    .into_iter()
                    .map(|reply| Step { delay: 0, reply })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn bytes(status: u16, bytes: Vec<u8>) -> Self {
        Self::new(vec![Ok(response(status, bytes, ResponseMeta::default()))])
    }
    fn json(status: u16, value: &Value) -> Self {
        Self::bytes(status, serde_json::to_vec(value).unwrap())
    }
    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn delay(&self, index: usize, seconds: u64) {
        self.steps.lock().unwrap()[index].delay = seconds;
    }
}
impl Http for Script {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        self.requests.lock().unwrap().push(request);
        let step = self.steps.lock().unwrap().pop_front().unwrap_or(Step {
            delay: 0,
            reply: Err(HttpError::Transport),
        });
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(Arc::clone(&self.active));
        if step.delay > 0 {
            tokio::time::sleep(Duration::from_secs(step.delay)).await;
        }
        step.reply
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
    Secret::new(GitHubToken::new(Zeroizing::new(format!("ghu_{SENTINEL}"))).unwrap())
}
fn repo() -> RepoName {
    RepoName::new("owner/repo").unwrap()
}
fn created(number: u64) -> Value {
    json!({"number":number,"html_url":format!("https://github.com/owner/repo/issues/{number}")})
}
fn row(number: u64) -> Value {
    json!({"number":number,"title":"title","body":"body","created_at":"2024-02-29T23:59:59Z"})
}
fn page(first: u64, count: u64) -> Value {
    Value::Array((first..first + count).map(row).collect())
}
async fn create(http: &impl Http) -> CreateOutcome {
    create_issue(
        http,
        &token(),
        &repo(),
        IssueContent {
            title: &IssueTitle::new("title").unwrap(),
            body: &IssueBody::new("body").unwrap(),
        },
    )
    .await
}
async fn lookup(http: &impl Http) -> Result<Matches, ApiError> {
    lookup_text(http, "title", "body", 0).await
}
async fn lookup_text(
    http: &impl Http,
    title: &str,
    body: &str,
    since_s: u64,
) -> Result<Matches, ApiError> {
    list_matches(
        http,
        &token(),
        &MatchQuery {
            repo: &repo(),
            creator: &GitHubLogin::new("owner").unwrap(),
            since_s,
            content: IssueContent {
                title: &IssueTitle::new(title).unwrap(),
                body: &IssueBody::new(body).unwrap(),
            },
        },
    )
    .await
}
fn candidate(number: u64, created_s: u64) -> Match {
    Match {
        number,
        url: IssueUrl::for_issue(&repo(), number).unwrap(),
        created_s,
    }
}
fn uncertain(status: Option<u16>, meta: ResponseMeta) -> CreateOutcome {
    CreateOutcome::Uncertain { status, meta }
}

#[tokio::test]
async fn create_serializes_only_checked_title_and_body_and_validates_canonical_success() {
    for number in [1, u64::MAX] {
        let http = Script::json(201, &created(number));
        assert_eq!(
            create(&http).await,
            CreateOutcome::Created {
                number,
                url: IssueUrl::for_issue(&repo(), number).unwrap(),
            }
        );
        assert_eq!(http.calls(), 1);
        let requests = http.requests.lock().unwrap();
        let Endpoint::GitHub(GitHubEndpoint::CreateIssue {
            token,
            repo: destination,
            body,
        }) = &requests[0].endpoint
        else {
            panic!("typed create endpoint")
        };
        assert_eq!(token.expose().as_str(), format!("ghu_{SENTINEL}"));
        assert_eq!(destination, &repo());
        assert_eq!(
            body.expose().as_bytes(),
            br#"{"title":"title","body":"body"}"#
        );
        assert_eq!(requests[0].fields.len(), 0);
    }
    let title = IssueTitle::new(&"界".repeat(256)).unwrap();
    let body = IssueBody::new(&"x".repeat(32_768)).unwrap();
    let payload = IssueContent {
        title: &title,
        body: &body,
    }
    .into_payload()
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(payload.expose().as_bytes()).unwrap(),
        json!({"title":"界".repeat(256),"body":"x".repeat(32_768)})
    );
    let title = IssueTitle::new(" \"title\\ ").unwrap();
    let body = IssueBody::new("first\r\n\t\"second\\").unwrap();
    let payload = IssueContent {
        title: &title,
        body: &body,
    }
    .into_payload()
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(payload.expose().as_bytes()).unwrap(),
        json!({"title":" \"title\\ ","body":"first\n\t\"second\\"})
    );
}

#[tokio::test]
async fn every_status_is_classified_without_retrying_or_reading_diagnostics() {
    let meta = ResponseMeta {
        date_s: Some(123),
        retry_after_s: Some(7200),
        retry_after_present: true,
        rate_remaining: Some(4),
        rate_reset_s: Some(456),
    };
    let bounded = ResponseMeta {
        retry_after_s: Some(3600),
        ..meta
    };
    for status in 100..=599 {
        let http = Script::new(vec![Ok(response(
            status,
            SENTINEL.as_bytes().to_vec(),
            meta,
        ))]);
        let reason = match status {
            401 => Some(FailReason::NotConnected),
            403 | 429 => Some(FailReason::RateLimited),
            404 | 410 => Some(FailReason::NotFound),
            422 => Some(FailReason::InvalidIssue),
            400..=499 => Some(FailReason::Rejected),
            _ => None,
        };
        let expected = match reason {
            Some(reason) => CreateOutcome::Definite {
                reason,
                status: Some(status),
                meta: bounded,
            },
            None => uncertain(Some(status), bounded),
        };
        assert_eq!(create(&http).await, expected, "status {status}");
        assert_eq!(http.calls(), 1, "status {status}");
    }
    for (meta, reason) in [
        (ResponseMeta::default(), FailReason::PermissionDenied),
        (
            ResponseMeta {
                retry_after_present: true,
                ..ResponseMeta::default()
            },
            FailReason::RateLimited,
        ),
        (
            ResponseMeta {
                retry_after_s: Some(0),
                ..ResponseMeta::default()
            },
            FailReason::PermissionDenied,
        ),
        (
            ResponseMeta {
                rate_remaining: Some(0),
                ..ResponseMeta::default()
            },
            FailReason::RateLimited,
        ),
        (
            ResponseMeta {
                rate_remaining: Some(1),
                rate_reset_s: Some(999),
                ..ResponseMeta::default()
            },
            FailReason::PermissionDenied,
        ),
    ] {
        let http = Script::new(vec![Ok(response(403, vec![b'x'; 1_048_577], meta))]);
        assert_eq!(
            create(&http).await,
            CreateOutcome::Definite {
                reason,
                status: Some(403),
                meta
            }
        );
        assert_eq!(http.calls(), 1);
    }
}

#[tokio::test]
async fn create_429_caps_retry_advice_at_one_hour_without_capping_absolute_reset() {
    for advice in [3600, 3601, u64::MAX] {
        let meta = ResponseMeta {
            retry_after_s: Some(advice),
            retry_after_present: true,
            rate_reset_s: Some(u64::MAX),
            ..ResponseMeta::default()
        };
        let http = Script::new(vec![Ok(response(429, SENTINEL.as_bytes().to_vec(), meta))]);
        assert_eq!(
            create(&http).await,
            CreateOutcome::Definite {
                reason: FailReason::RateLimited,
                status: Some(429),
                meta: ResponseMeta {
                    retry_after_s: Some(3600),
                    ..meta
                },
            }
        );
        assert_eq!(http.calls(), 1);
    }
}

#[tokio::test]
async fn each_transport_failure_makes_exactly_one_attempt() {
    for (error, definite) in [
        (HttpError::TrustStoreUnavailable, true),
        (HttpError::Configuration, true),
        (HttpError::Endpoint, true),
        (HttpError::Request, true),
        (HttpError::Connect, true),
        (HttpError::Redirect, false),
        (HttpError::Timeout, false),
        (HttpError::Transport, false),
        (HttpError::BodyTooLarge, false),
    ] {
        let http = Script::new(vec![Err(error)]);
        let expected = if definite {
            CreateOutcome::Definite {
                reason: FailReason::Network,
                status: None,
                meta: ResponseMeta::default(),
            }
        } else {
            uncertain(None, ResponseMeta::default())
        };
        assert_eq!(create(&http).await, expected, "{error:?}");
        assert_eq!(http.calls(), 1, "{error:?}");
    }
}

#[tokio::test]
async fn malformed_201_and_foreign_urls_are_uncertain_with_exactly_one_attempt() {
    let mut bodies = vec![
        vec![],
        b"{".to_vec(),
        b"null".to_vec(),
        b"[]".to_vec(),
        b"{}".to_vec(),
        vec![0xff],
    ];
    bodies.push(
        br#"{"number":18446744073709551616,"html_url":"https://github.com/owner/repo/issues/1"}"#
            .to_vec(),
    );
    for number in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
        json!(true),
    ] {
        bodies.push(
            serde_json::to_vec(
                &json!({"number":number,"html_url":"https://github.com/owner/repo/issues/1"}),
            )
            .unwrap(),
        );
    }
    for url in [
        json!(null),
        json!(1),
        json!(true),
        json!(""),
        json!("https://evil.invalid/owner/repo/issues/1"),
        json!("https://github.com/other/repo/issues/1"),
        json!("https://github.com/owner/repo/issues/2"),
        json!("https://github.com/owner/repo/issues/01"),
        json!("https://github.com/owner/repo/issues/1?x=y"),
        json!("https://github.com/owner/repo/issues/1#fragment"),
        json!("https://github.com:443/owner/repo/issues/1"),
        json!("http://github.com/owner/repo/issues/1"),
    ] {
        bodies.push(serde_json::to_vec(&json!({"number":1,"html_url":url})).unwrap());
    }
    bodies.extend([
        br#"{"number":1,"number":1,"html_url":"https://github.com/owner/repo/issues/1"}"#.to_vec(),
        br#"{"number":1,"html_url":"https://github.com/owner/repo/issues/1","html_url":"https://github.com/owner/repo/issues/1"}"#.to_vec(),
        br#"{"number":1}"#.to_vec(),
        br#"{"html_url":"https://github.com/owner/repo/issues/1"}"#.to_vec(),
    ]);
    for body in bodies {
        let http = Script::bytes(201, body);
        assert_eq!(
            create(&http).await,
            uncertain(Some(201), ResponseMeta::default())
        );
        assert_eq!(http.calls(), 1);
    }
    let mut value = created(1);
    value["unknown"] = json!(SENTINEL);
    let http = Script::json(201, &value);
    assert_eq!(
        create(&http).await,
        CreateOutcome::Created {
            number: 1,
            url: IssueUrl::for_issue(&repo(), 1).unwrap()
        }
    );
    assert_eq!(http.calls(), 1);
}

#[tokio::test]
async fn scripted_parser_entry_caps_accept_exactly_one_mib_and_refuse_one_more() {
    for create_call in [true, false] {
        let value = if create_call {
            created(1)
        } else {
            json!([row(1)])
        };
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.resize(1_048_576, b' ');
        let http = Script::bytes(if create_call { 201 } else { 200 }, bytes.clone());
        if create_call {
            assert_eq!(
                create(&http).await,
                CreateOutcome::Created {
                    number: 1,
                    url: IssueUrl::for_issue(&repo(), 1).unwrap()
                }
            );
        } else {
            let matches = lookup(&http).await.unwrap();
            assert_eq!(matches.issues, vec![candidate(1, 1_709_251_199)]);
            assert!(matches.complete);
        }
        assert_eq!(http.calls(), 1);
        bytes.push(b' ');
        let http = Script::bytes(if create_call { 201 } else { 200 }, bytes);
        if create_call {
            assert_eq!(
                create(&http).await,
                uncertain(Some(201), ResponseMeta::default())
            );
        } else {
            assert_eq!(lookup(&http).await.unwrap_err(), ApiError::Response);
        }
        assert_eq!(http.calls(), 1);
    }
}

#[tokio::test]
async fn match_comparison_is_exact_and_pr_presence_including_null_excludes_rows() {
    let mut rows = vec![row(1)];
    for (field, text) in [
        ("title", " title"),
        ("title", "Title"),
        ("body", "body\r\n"),
        ("body", "body "),
        ("body", "BODY"),
        ("title", "界"),
        ("title", "x"),
        ("body", "x"),
    ] {
        let mut value = row(u64::try_from(rows.len() + 1).unwrap());
        value[field] = json!(text);
        rows.push(value);
    }
    for pr in [
        Value::Null,
        json!({}),
        json!({"url":SENTINEL}),
        json!(false),
    ] {
        let mut value = row(u64::try_from(rows.len() + 1).unwrap());
        value["pull_request"] = pr;
        rows.push(value);
    }
    let mut large = row(50);
    large["title"] = json!("x".repeat(257));
    large["body"] = json!("x".repeat(32_769));
    rows.push(large);
    let http = Script::json(200, &Value::Array(rows));
    let found = lookup(&http).await.unwrap();
    assert_eq!(found.issues, vec![candidate(1, 1_709_251_199)]);
    assert!(found.complete);
    assert_eq!(http.calls(), 1);
    let mut value = row(u64::MAX);
    value["body"] = Value::Null;
    value["unknown"] = json!(SENTINEL);
    let http = Script::json(200, &json!([value]));
    assert_eq!(
        lookup_text(&http, "title", "", 0).await.unwrap().issues,
        vec![candidate(u64::MAX, 1_709_251_199)]
    );
    let mut remote_crlf = row(1);
    remote_crlf["body"] = json!("first\r\nsecond");
    let mut exact_lf = row(2);
    exact_lf["body"] = json!("first\nsecond");
    let http = Script::json(200, &json!([remote_crlf, exact_lf]));
    assert_eq!(
        lookup_text(&http, "title", "first\nsecond", 0)
            .await
            .unwrap()
            .issues,
        vec![candidate(2, 1_709_251_199)]
    );
}

#[tokio::test]
async fn pagination_is_bounded_and_full_second_page_is_incomplete_even_if_filtered() {
    let first_meta = ResponseMeta {
        date_s: Some(123),
        retry_after_present: true,
        retry_after_s: Some(7200),
        rate_remaining: Some(4),
        rate_reset_s: Some(456),
    };
    for second_count in [0, 1, 99, 100] {
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&page(1, 100)).unwrap(),
                first_meta,
            )),
            Ok(response(
                200,
                serde_json::to_vec(&page(101, second_count)).unwrap(),
                ResponseMeta {
                    date_s: Some(999),
                    ..ResponseMeta::default()
                },
            )),
        ]);
        let found = lookup_text(&http, "no match", "", 1_709_251_199)
            .await
            .unwrap();
        assert_eq!(found.issues, vec![]);
        assert_eq!(found.complete, second_count < 100);
        assert_eq!(found.meta, first_meta);
        assert_eq!(http.calls(), 2);
        for (index, request) in http.requests.lock().unwrap().iter().enumerate() {
            assert_eq!(request.fields.len(), 0);
            let Endpoint::GitHub(endpoint @ GitHubEndpoint::Issues { .. }) = &request.endpoint
            else {
                panic!("typed issue endpoint")
            };
            assert_eq!(
                endpoint.url().unwrap().as_str(),
                format!(
                    "https://api.github.com/repos/owner/repo/issues?per_page=100&page={}&creator=owner&state=all&since=2024-02-29T23%3A59%3A59Z&sort=created&direction=desc",
                    index + 1
                )
            );
        }
    }
    let http = Script::new(vec![
        Ok(response(
            200,
            serde_json::to_vec(&page(1, 100)).unwrap(),
            first_meta,
        )),
        Ok(response(
            200,
            serde_json::to_vec(&page(101, 1)).unwrap(),
            ResponseMeta::default(),
        )),
    ]);
    let found = lookup(&http).await.unwrap();
    assert_eq!(
        found.issues,
        (1..=101)
            .map(|n| candidate(n, 1_709_251_199))
            .collect::<Vec<_>>()
    );
    assert!(found.complete);
    let http = Script::json(200, &json!([]));
    let found = lookup(&http).await.unwrap();
    assert_eq!(found.issues, vec![]);
    assert!(found.complete);
    assert_eq!(http.calls(), 1);
}

#[tokio::test]
async fn malformed_rows_duplicates_and_excess_rows_make_the_whole_lookup_unusable() {
    let mut bodies = vec![
        vec![],
        b"[".to_vec(),
        b"null".to_vec(),
        b"{}".to_vec(),
        b"[{}]".to_vec(),
        vec![0xff],
        b"true".to_vec(),
        b"1".to_vec(),
        br#"[{"number":18446744073709551616,"title":"title","body":"body","created_at":"1970-01-01T00:00:00Z"}]"#.to_vec(),
    ];
    for field in ["number", "title", "body", "created_at"] {
        let mut missing = row(1);
        missing.as_object_mut().unwrap().remove(field);
        bodies.push(serde_json::to_vec(&json!([missing])).unwrap());
        for wrong in [Value::Null, json!(true), json!(5), json!([]), json!({})] {
            if field == "body" && wrong.is_null() || field == "number" && wrong == json!(5) {
                continue;
            }
            let mut value = row(1);
            value[field] = wrong;
            bodies.push(serde_json::to_vec(&json!([value])).unwrap());
        }
        let mut text = serde_json::to_string(&row(1)).unwrap();
        text.insert_str(1, &format!("{field:?}:{},", row(1)[field]));
        bodies.push(format!("[{text}]").into_bytes());
    }
    for number in [json!(0), json!(-1), json!(1.5), json!("1")] {
        let mut value = row(1);
        value["number"] = number;
        bodies.push(serde_json::to_vec(&json!([value])).unwrap());
    }
    bodies.push(br#"[{"number":1,"title":"title","body":null,"body":null,"created_at":"1970-01-01T00:00:00Z"}]"#.to_vec());
    bodies.push(br#"[{"number":1,"title":"title","body":"body","created_at":"1970-01-01T00:00:00Z","pull_request":null,"pull_request":null}]"#.to_vec());
    bodies.push(serde_json::to_vec(&page(1, 101)).unwrap());
    for body in bodies {
        let http = Script::bytes(200, body);
        assert_eq!(lookup(&http).await.unwrap_err(), ApiError::Response);
        assert_eq!(http.calls(), 1);
    }
    for across_pages in [false, true] {
        let first = if across_pages {
            page(1, 100)
        } else {
            json!([row(1), row(1)])
        };
        let mut duplicate_pr = row(1);
        duplicate_pr["pull_request"] = Value::Null;
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&first).unwrap(),
                ResponseMeta::default(),
            )),
            Ok(response(
                200,
                serde_json::to_vec(&json!([duplicate_pr])).unwrap(),
                ResponseMeta::default(),
            )),
        ]);
        assert_eq!(lookup(&http).await.unwrap_err(), ApiError::Response);
        assert_eq!(http.calls(), if across_pages { 2 } else { 1 });
    }
}

#[tokio::test]
async fn timestamps_are_checked_and_unrepresentable_since_never_dispatches() {
    for (stamp, seconds) in [
        ("1970-01-01T00:00:00Z", 0),
        ("2024-02-29T23:59:59Z", 1_709_251_199),
        ("2024-03-01T00:59:59+01:00", 1_709_251_199),
        ("9999-12-31T23:59:59Z", 253_402_300_799),
    ] {
        let mut value = row(1);
        value["created_at"] = json!(stamp);
        assert_eq!(
            lookup(&Script::json(200, &json!([value])))
                .await
                .unwrap()
                .issues,
            vec![candidate(1, seconds)]
        );
    }
    for stamp in [
        "",
        "2023-02-29T00:00:00Z",
        "2024-13-01T00:00:00Z",
        "2024-02-29",
        "1969-12-31T23:59:59Z",
        "2024-02-29T25:00:00Z",
        "not a date",
    ] {
        let mut value = row(1);
        value["created_at"] = json!(stamp);
        assert_eq!(
            lookup(&Script::json(200, &json!([value])))
                .await
                .unwrap_err(),
            ApiError::Response
        );
    }
    let mut malformed_pr = row(1);
    malformed_pr["pull_request"] = Value::Null;
    malformed_pr["created_at"] = json!("not a date");
    assert_eq!(
        lookup(&Script::json(200, &json!([malformed_pr])))
            .await
            .unwrap_err(),
        ApiError::Response
    );
    let http = Script::json(200, &json!([]));
    assert_eq!(
        lookup_text(&http, "title", "body", u64::MAX)
            .await
            .unwrap_err(),
        ApiError::Http(HttpError::Request)
    );
    assert_eq!(http.calls(), 0);
}

#[tokio::test]
async fn failed_second_page_cannot_return_partial_matches() {
    for error in [
        HttpError::Connect,
        HttpError::Transport,
        HttpError::Timeout,
        HttpError::BodyTooLarge,
        HttpError::Redirect,
    ] {
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&page(1, 100)).unwrap(),
                ResponseMeta::default(),
            )),
            Err(error),
        ]);
        assert_eq!(lookup(&http).await.unwrap_err(), ApiError::Http(error));
        assert_eq!(http.calls(), 2);
    }
    for (status, bytes, expected) in [
        (200, b"[".to_vec(), ApiError::Response),
        (
            200,
            serde_json::to_vec(&page(101, 101)).unwrap(),
            ApiError::Response,
        ),
        (
            503,
            SENTINEL.as_bytes().to_vec(),
            ApiError::Refused {
                status: 503,
                meta: ResponseMeta::default(),
            },
        ),
    ] {
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&page(1, 100)).unwrap(),
                ResponseMeta::default(),
            )),
            Ok(response(status, bytes, ResponseMeta::default())),
        ]);
        assert_eq!(lookup(&http).await.unwrap_err(), expected);
        assert_eq!(http.calls(), 2);
    }
}

async fn pending_once<T>(mut future: Pin<&mut impl Future<Output = T>>) {
    assert!(matches!(
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await,
        Poll::Pending
    ));
}

#[tokio::test(start_paused = true)]
async fn create_deadline_is_thirty_seconds_and_drops_the_only_request() {
    let http = Script::json(201, &created(1));
    http.delay(0, 100);
    let future = create(&http);
    tokio::pin!(future);
    pending_once(future.as_mut()).await;
    assert_eq!(http.active.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(29)).await;
    pending_once(future.as_mut()).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await,
        Poll::Ready(uncertain(None, ResponseMeta::default()))
    );
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
    assert_eq!(http.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn both_match_pages_share_one_thirty_second_deadline() {
    for page_two in [false, true] {
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&page(1, 100)).unwrap(),
                ResponseMeta::default(),
            )),
            Ok(response(200, b"[]".to_vec(), ResponseMeta::default())),
        ]);
        http.delay(0, if page_two { 20 } else { 100 });
        http.delay(1, 100);
        let future = lookup(&http);
        tokio::pin!(future);
        pending_once(future.as_mut()).await;
        tokio::time::advance(Duration::from_secs(20)).await;
        pending_once(future.as_mut()).await;
        assert_eq!(http.calls(), if page_two { 2 } else { 1 });
        tokio::time::advance(Duration::from_secs(9)).await;
        pending_once(future.as_mut()).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let state = std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
        assert!(matches!(
            state,
            Poll::Ready(Err(ApiError::Http(HttpError::Timeout)))
        ));
        assert_eq!(http.active.load(Ordering::SeqCst), 0);
        assert_eq!(http.calls(), if page_two { 2 } else { 1 });
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_create_or_either_match_page_drops_its_request_future() {
    let http = Script::json(201, &created(1));
    http.delay(0, 100);
    {
        let future = create(&http);
        tokio::pin!(future);
        pending_once(future.as_mut()).await;
    }
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
    assert_eq!(http.calls(), 1);
    for page_two in [false, true] {
        let http = Script::new(vec![
            Ok(response(
                200,
                serde_json::to_vec(&page(1, 100)).unwrap(),
                ResponseMeta::default(),
            )),
            Ok(response(200, b"[]".to_vec(), ResponseMeta::default())),
        ]);
        http.delay(0, if page_two { 20 } else { 100 });
        http.delay(1, 100);
        {
            let future = lookup(&http);
            tokio::pin!(future);
            pending_once(future.as_mut()).await;
            tokio::time::advance(Duration::from_secs(20)).await;
            pending_once(future.as_mut()).await;
            assert_eq!(http.active.load(Ordering::SeqCst), 1);
        }
        assert_eq!(http.active.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(http.calls(), if page_two { 2 } else { 1 });
    }
}

fn assert_redacted(text: &str) {
    assert!(!text.contains(SENTINEL));
    let bytes = format!("{:?}", SENTINEL.as_bytes());
    assert!(!text.contains(&bytes));
    assert!(!text.contains(bytes.trim_start_matches('[').trim_end_matches(']')));
}

#[tokio::test]
async fn outermost_carriers_errors_and_logs_redact_text_and_embedded_byte_lists() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("p2b2-capture-control");
    let title = IssueTitle::new(SENTINEL).unwrap();
    let body = IssueBody::new(SENTINEL).unwrap();
    let content = IssueContent {
        title: &title,
        body: &body,
    };
    let query = MatchQuery {
        repo: &RepoName::new(&format!("{SENTINEL}/repo")).unwrap(),
        creator: &GitHubLogin::new(SENTINEL).unwrap(),
        since_s: 0,
        content,
    };
    assert_redacted(&format!("{content:?} {query:?}"));
    let payload = content.into_payload().unwrap();
    assert_redacted(&format!("{payload:?} {:?}", payload.expose()));
    let http = Script::json(
        201,
        &json!({"number":1,"html_url":format!("https://github.com/{SENTINEL}/repo/issues/1")}),
    );
    let outcome = create_issue(&http, &token(), query.repo, content).await;
    let url = IssueUrl::for_issue(query.repo, 1).unwrap();
    assert_eq!(
        url.as_str(),
        format!("https://github.com/{SENTINEL}/repo/issues/1")
    );
    assert_eq!(
        outcome,
        CreateOutcome::Created {
            number: 1,
            url: url.clone()
        }
    );
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(outcome)));
    let mut value = row(1);
    value["title"] = json!(SENTINEL);
    value["body"] = json!(SENTINEL);
    let found = list_matches(&Script::json(200, &json!([value])), &token(), &query)
        .await
        .unwrap();
    assert_eq!(
        found.issues.as_slice(),
        [Match {
            number: 1,
            url,
            created_s: 1_709_251_199
        }]
    );
    assert!(found.complete);
    assert_redacted(&format!("{:?}", Ok::<_, ApiError>(found)));
    for bytes in [
        SENTINEL.as_bytes().to_vec(),
        format!(
            "[0, {}, 255]",
            SENTINEL
                .as_bytes()
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into_bytes(),
    ] {
        for status in [201, 403, 503] {
            let http = Script::bytes(status, bytes.clone());
            assert_redacted(&format!("{:?}", create(&http).await));
        }
        let error = lookup(&Script::bytes(200, bytes)).await.unwrap_err();
        assert_eq!(error, ApiError::Response);
        assert_redacted(&format!("{error:?} {error}"));
    }
    let logs = captured.text();
    assert!(logs.contains("p2b2-capture-control"));
    assert_redacted(&logs);
}
