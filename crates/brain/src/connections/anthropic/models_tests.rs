//! Synthetic responses built from the public Models API reference, not live recordings.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::KeyRejection;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{Capabilities, CatalogModel, CheckError, read};
use crate::connections::anthropic::key::AnthropicKey;
use crate::connections::http_client::{Http, HttpError, MAX_BODY_BYTES, Request, Response};
use crate::test_support::Captured;

struct Once(Mutex<Option<Result<Response, HttpError>>>);

impl Http for Once {
    fn execute(
        &self,
        _: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        std::future::ready(self.0.lock().expect("response").take().expect("one check"))
    }
}

fn key() -> Secret<AnthropicKey> {
    AnthropicKey::parse(Secret::new(Zeroizing::new(
        b"sk-ant-api-fake-only".to_vec(),
    )))
    .expect("synthetic API key")
}

fn response(status: u16, bytes: Vec<u8>) -> Response {
    Response {
        status,
        request_id: None,
        meta: crate::connections::http_client::ResponseMeta::default(),
        body: Secret::new(Zeroizing::new(bytes)),
    }
}

async fn check(status: u16, body: &str) -> Result<Vec<CatalogModel>, CheckError> {
    let http = Once(Mutex::new(Some(Ok(response(
        status,
        body.as_bytes().to_vec(),
    )))));
    read(&http, key()).await
}

fn model(id: &str, name: &str) -> Value {
    json!({ "type": "model", "id": id, "display_name": name, "created_at": "2026-01-01T00:00:00Z" })
}

fn body(data: Vec<Value>) -> String {
    json!({ "data": Value::Array(data), "has_more": false, "first_id": null, "last_id": null })
        .to_string()
}

#[tokio::test]
async fn complete_catalog_preserves_names_order_and_conservative_capabilities() {
    let catalog = check(
        200,
        &body(vec![
            model("claude-unknown-1", "First \"model\""),
            model("claude-unknown-2", "Second model"),
        ]),
    )
    .await
    .expect("complete catalog");
    let kept: Vec<_> = catalog
        .iter()
        .map(|model| {
            (
                model.info.slug.as_str(),
                model.info.display_name.as_str(),
                model.capabilities,
            )
        })
        .collect();
    let disabled = Capabilities {
        images: false,
        pdfs: false,
        tool_argument_streaming: false,
        effort: false,
    };
    assert_eq!(
        kept,
        [
            ("claude-unknown-1", "First \"model\"", disabled),
            ("claude-unknown-2", "Second model", disabled),
        ]
    );
    assert_eq!(check(200, &body(Vec::new())).await, Ok(Vec::new()));
}

#[tokio::test]
async fn only_documented_exact_models_receive_capability_facts() {
    let catalog = check(
        200,
        &body(
            [
                "claude-fable-5-1",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-haiku-5-5",
                "claude-opus-5-5-future",
                "claude-unknown-9",
            ]
            .into_iter()
            .map(|id| model(id, "Model"))
            .collect(),
        ),
    )
    .await
    .expect("valid models");
    let flags: Vec<_> = catalog
        .into_iter()
        .map(|model| model.capabilities)
        .collect();
    let supported = Capabilities {
        images: true,
        pdfs: true,
        tool_argument_streaming: true,
        effort: true,
    };
    let unknown = Capabilities {
        images: false,
        pdfs: false,
        tool_argument_streaming: false,
        effort: false,
    };
    assert_eq!(
        flags,
        [supported, supported, supported, supported, unknown, unknown]
    );
}

#[tokio::test]
async fn count_id_and_name_limits_accept_exact_and_refuse_one_over() {
    let entries = |count| {
        (0..count)
            .map(|n| model(&format!("m-{n}"), "Model"))
            .collect()
    };
    let catalog = check(200, &body(entries(64)))
        .await
        .expect("exact model cap");
    let ids: Vec<_> = catalog
        .iter()
        .map(|model| model.info.slug.as_str())
        .collect();
    let expected: Vec<_> = (0..64).map(|n| format!("m-{n}")).collect();
    assert_eq!(ids, expected);
    assert_eq!(
        check(200, &body(entries(65))).await,
        Err(CheckError::Unconfirmed)
    );
    let exact_id = "a".repeat(64);
    let exact_name = "界".repeat(80);
    let catalog = check(200, &body(vec![model(&exact_id, &exact_name)]))
        .await
        .expect("exact text caps");
    assert_eq!(catalog[0].info.slug.as_str(), exact_id);
    assert_eq!(catalog[0].info.display_name.as_str(), exact_name);
    for entry in [
        model(&"a".repeat(65), "Model"),
        model("valid", &"界".repeat(81)),
        model("", "Model"),
        model("UPPER", "Model"),
        model("valid", ""),
        model("valid", "Two\nlines"),
    ] {
        assert_eq!(
            check(200, &body(vec![entry])).await,
            Err(CheckError::Unconfirmed)
        );
    }
}

#[tokio::test]
async fn invalid_or_incomplete_lists_never_publish_partial_models() {
    for data in [
        vec![model("duplicate", "First"), model("duplicate", "Second")],
        vec![
            model("valid", "First"),
            json!({ "type": "model", "id": 7, "display_name": "Bad" }),
        ],
        vec![json!({ "type": "model", "id": "missing-name" })],
        vec![json!({ "type": "other", "id": "valid", "display_name": "Bad" })],
        vec![json!({ "id": "valid", "display_name": "Missing type" })],
        vec![Value::Null],
    ] {
        assert_eq!(check(200, &body(data)).await, Err(CheckError::Unconfirmed));
    }
    for text in [
        "",
        "{}",
        "{\"data\":[",
        "{\"data\":null,\"has_more\":false}",
        "{\"data\":[],\"has_more\":\"false\"}",
        "{\"data\":[]}",
    ] {
        assert_eq!(check(200, text).await, Err(CheckError::Unconfirmed));
    }
    let mut page: Value = serde_json::from_str(&body(
        (0..20).map(|n| model(&format!("m-{n}"), "Model")).collect(),
    ))
    .expect("fixture");
    page["has_more"] = json!(true);
    assert_eq!(
        check(200, &page.to_string()).await,
        Err(CheckError::Unconfirmed)
    );
    let invalid_utf8 = Once(Mutex::new(Some(Ok(response(200, vec![0xff])))));
    assert_eq!(
        read(&invalid_utf8, key()).await,
        Err(CheckError::Unconfirmed)
    );
}

#[tokio::test]
async fn workspace_required_is_narrow_and_retains_no_provider_message() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("capture-control");
    for (status, kind, message, expected) in [
        (
            400,
            "invalid_request_error",
            "Set the `anthropic-workspace-id` header: private-sentinel",
            CheckError::Rejected(KeyRejection::WorkspaceRequired),
        ),
        (
            400,
            "invalid_request_error",
            "Set anthropic-workspace-ids",
            CheckError::Unconfirmed,
        ),
        (
            400,
            "invalid_request_error",
            "Set other-anthropic-workspace-id",
            CheckError::Unconfirmed,
        ),
        (
            400,
            "authentication_error",
            "Set anthropic-workspace-id",
            CheckError::Unconfirmed,
        ),
        (
            429,
            "invalid_request_error",
            "Set anthropic-workspace-id",
            CheckError::Unconfirmed,
        ),
        (
            401,
            "invalid_request_error",
            "Set anthropic-workspace-id",
            CheckError::Rejected(KeyRejection::Refused),
        ),
    ] {
        let error = check(
            status,
            &json!({"error":{"type":kind,"message":message}}).to_string(),
        )
        .await
        .expect_err("closed failure");
        assert_eq!(error, expected);
        assert!(
            !format!("{error:?} {error}").contains("private-sentinel"),
            "provider message is redacted"
        );
    }
    let logs = captured.text();
    assert!(logs.contains("capture-control"), "capture active");
    assert!(
        !logs.contains("private-sentinel"),
        "provider message never enters logs"
    );
    assert!(
        !logs.contains(&format!("{:?}", b"private-sentinel")),
        "provider message bytes never enter logs"
    );
}

#[tokio::test]
async fn body_limit_is_enforced_even_for_injected_transports() {
    let mut text = body(Vec::new());
    text.extend(std::iter::repeat_n(' ', MAX_BODY_BYTES - text.len()));
    assert_eq!(text.len(), 1_048_576);
    assert_eq!(check(200, &text).await, Ok(Vec::new()));
    text.push(' ');
    assert_eq!(check(200, &text).await, Err(CheckError::Unconfirmed));
}

#[tokio::test]
async fn only_auth_refusals_reject_and_neither_provider_errors_nor_content_are_logged() {
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("capture-control");
    let marker = "private-provider-sentinel";
    for status in [201, 204, 301, 400, 401, 403, 404, 408, 429, 500, 503, 529] {
        let expected = match status {
            401 | 403 => CheckError::Rejected(KeyRejection::Refused),
            _ => CheckError::Unconfirmed,
        };
        let error = check(status, &json!({"error":{"message":marker}}).to_string())
            .await
            .expect_err("closed failure");
        assert_eq!(error, expected);
        assert!(
            !format!("{error:?} {error}").contains(marker),
            "error text is redacted"
        );
    }
    for error in [
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
            read(&Once(Mutex::new(Some(Err(error)))), key()).await,
            Err(CheckError::Unconfirmed)
        );
    }
    let catalog = check(200, &body(vec![model(marker, marker)]))
        .await
        .expect("valid sentinel catalog");
    let printed = format!("{:?}", Some(Ok::<_, CheckError>(catalog)));
    let logs = captured.text();
    assert!(logs.contains("capture-control"), "capture active");
    for output in [printed, logs] {
        assert!(!output.contains(marker), "provider text is redacted");
        assert!(
            !output.contains(&format!("{:?}", marker.as_bytes())),
            "provider bytes are redacted"
        );
    }
}

struct Delayed {
    headers: Duration,
    body: Duration,
}

impl Http for Delayed {
    async fn execute(&self, _: Request) -> Result<Response, HttpError> {
        tokio::time::sleep(self.headers).await;
        tokio::time::sleep(self.body).await;
        Ok(response(200, b"{\"data\":[],\"has_more\":false}".to_vec()))
    }
}

#[tokio::test(start_paused = true)]
async fn whole_deadline_includes_headers_and_body_for_every_transport() {
    for (headers, body) in [(16, 0), (0, 16), (10, 10)] {
        let started = tokio::time::Instant::now();
        let result = read(
            &Delayed {
                headers: Duration::from_secs(headers),
                body: Duration::from_secs(body),
            },
            key(),
        )
        .await;
        assert_eq!(result, Err(CheckError::Unconfirmed));
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }
    assert_eq!(
        read(
            &Delayed {
                headers: Duration::from_secs(7),
                body: Duration::from_secs(7)
            },
            key()
        )
        .await,
        Ok(Vec::new())
    );
}

struct Held(AtomicBool);

struct Dropped<'a>(&'a AtomicBool);

impl Drop for Dropped<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Http for Held {
    async fn execute(&self, _: Request) -> Result<Response, HttpError> {
        let _in_flight = Dropped(&self.0);
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropping_a_catalog_check_drops_its_held_transport() {
    let http = Held(AtomicBool::new(false));
    let mut check = Box::pin(read(&http, key()));
    tokio::select! {
        biased;
        _ = &mut check => panic!("held transport cannot complete"),
        () = tokio::task::yield_now() => {}
    }
    assert!(!http.0.load(Ordering::SeqCst), "transport is held");
    drop(check);
    assert!(
        http.0.load(Ordering::SeqCst),
        "cancel drops transport immediately"
    );
}
