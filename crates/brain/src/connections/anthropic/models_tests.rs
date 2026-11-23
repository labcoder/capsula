//! Synthetic responses built from the public Models API reference, not live recordings.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::KeyRejection;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use zeroize::Zeroizing;

use super::{Capabilities, CatalogModel, CheckError};
use crate::connections::anthropic::fence::Fence;
use crate::connections::anthropic::key::AnthropicKey;
use crate::connections::anthropic::route::Transport;
use crate::connections::anthropic::tls::{Script, Scripted};
use crate::connections::http_client::MAX_BODY_BYTES;
use crate::test_support::Captured;

const CATALOG_FIXTURE_KEY: &str = "sk-ant-api-fake-only";

fn key() -> Secret<AnthropicKey> {
    AnthropicKey::parse(Secret::new(Zeroizing::new(
        CATALOG_FIXTURE_KEY.as_bytes().to_vec(),
    )))
    .expect("synthetic API key")
}

fn wire(status: u16, bytes: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    response.extend_from_slice(bytes);
    response
}

async fn check(status: u16, body: &str) -> Result<Vec<CatalogModel>, CheckError> {
    raw(wire(status, body.as_bytes())).await
}

async fn raw(response: Vec<u8>) -> Result<Vec<CatalogModel>, CheckError> {
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: [response].into(),
        ..Script::default()
    })));
    Transport::offline(peer, 1, 0)
        .catalog(&Fence::default(), &key())
        .await
}

fn model(id: &str, name: &str) -> Value {
    json!({ "type": "model", "id": id, "display_name": name, "created_at": "2026-01-01T00:00:00Z" })
}

fn body(data: Vec<Value>) -> String {
    json!({ "data": Value::Array(data), "has_more": false, "first_id": null, "last_id": null })
        .to_string()
}

#[tokio::test]
async fn complete_catalog_preserves_names_and_order_of_known_models() {
    let catalog = check(
        200,
        &body(vec![
            model("claude-sonnet-5-5", "First \"model\""),
            model("claude-unknown-1", "Not offered"),
            model("claude-haiku-5-5", "Second model"),
            model("claude-fable-5-1", "Third model"),
            model("claude-opus-5-5-future", "Not offered"),
            model("claude-opus-5-5", "Fourth model"),
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
    let supported = Capabilities {
        images: false,
        pdfs: true,
        tool_argument_streaming: true,
        effort: true,
    };
    assert_eq!(
        kept,
        [
            ("claude-sonnet-5-5", "First \"model\"", supported),
            ("claude-haiku-5-5", "Second model", supported),
            ("claude-fable-5-1", "Third model", supported),
            ("claude-opus-5-5", "Fourth model", supported),
        ]
    );
    for models in [Vec::new(), vec![model("claude-unknown-9", "Model")]] {
        assert_eq!(check(200, &body(models)).await, Ok(Vec::new()));
    }
}

#[tokio::test]
async fn only_an_entrys_literal_true_image_input_says_it_takes_pictures() {
    let cases = [
        (json!({ "image_input": { "supported": true } }), true),
        (
            json!({ "image_input": { "supported": true, "future": 1 }, "unknown": {} }),
            true,
        ),
        (json!({ "image_input": { "supported": false } }), false),
        (json!({ "image_input": {} }), false),
        (json!({ "image_input": null }), false),
        (json!({ "image_input": { "supported": "true" } }), false),
        (json!({ "image_input": { "supported": [] } }), false),
        (json!({ "image_input": { "supported": 1 } }), false),
        (json!({ "image_input": [] }), false),
        (json!({ "unknown": { "supported": true } }), false),
        (json!({}), false),
        (json!(null), false),
        (json!("image_input"), false),
    ];
    for (index, (capabilities, images)) in cases.into_iter().enumerate() {
        let mut known = model("claude-opus-5-5", "Known");
        known["capabilities"] = capabilities.clone();
        let mut other = model("claude-unknown-9", "Other");
        other["capabilities"] = capabilities;
        let catalog = check(200, &body(vec![known, other]))
            .await
            .unwrap_or_else(|error| panic!("case {index}: {error:?}"));
        let flags: Vec<_> = catalog.iter().map(|model| model.capabilities).collect();
        assert_eq!(
            flags,
            [Capabilities {
                images,
                pdfs: true,
                tool_argument_streaming: true,
                effort: true,
            }],
            "case {index}",
        );
    }
}

#[tokio::test]
async fn count_id_and_name_limits_accept_exact_and_refuse_one_over() {
    let entries = |count| {
        std::iter::once(model("claude-haiku-5-5", "Model"))
            .chain((1..count).map(|n| model(&format!("m-{n}"), "Model")))
            .collect()
    };
    let catalog = check(200, &body(entries(64)))
        .await
        .expect("exact model cap");
    let ids: Vec<_> = catalog
        .iter()
        .map(|model| model.info.slug.as_str())
        .collect();
    assert_eq!(ids, ["claude-haiku-5-5"]);
    assert_eq!(
        check(200, &body(entries(65))).await,
        Err(CheckError::Unconfirmed)
    );
    let exact_id = "a".repeat(64);
    assert_eq!(
        check(200, &body(vec![model(&exact_id, "Model")])).await,
        Ok(Vec::new())
    );
    let exact_name = "界".repeat(80);
    let catalog = check(200, &body(vec![model("claude-haiku-5-5", &exact_name)]))
        .await
        .expect("exact text caps");
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
    assert_eq!(raw(wire(200, &[0xff])).await, Err(CheckError::Unconfirmed));
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

#[tokio::test(start_paused = true)]
async fn oversized_catalog_drops_the_transport_while_the_body_is_still_open() {
    let (reader, mut writer) = tokio::io::duplex(16 * 1024);
    let peer = Scripted(Arc::new(Mutex::new(Script {
        reader: Some(Box::new(reader)),
        ..Script::default()
    })));
    let transport = Transport::offline(peer.clone(), 1, 0);
    let fence = Fence::default();
    let key = key();
    let feed = async {
        let mut bytes = body(Vec::new()).into_bytes();
        bytes.resize(MAX_BODY_BYTES + 1, b' ');
        writer
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .await
            .expect("close-delimited headers");
        writer
            .write_all(&bytes)
            .await
            .expect("one byte over the cap");
    };
    let read = transport.catalog(&fence, &key);
    let ((), result) =
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(feed, read) })
            .await
            .expect("oversized body drops before EOF or the catalog deadline");
    assert_eq!(result, Err(CheckError::Unconfirmed));
    assert_eq!(peer.0.lock().expect("peer").drops, 1);
    drop(writer);
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
        let printed = format!("{error:?} {error}");
        assert!(!printed.contains(marker), "error text is redacted");
        let bytes = format!("{:?}", marker.as_bytes());
        assert!(!printed.contains(bytes.trim_matches(['[', ']'])));
    }
    for bytes in [
        b"not HTTP".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx".to_vec(),
    ] {
        assert_eq!(raw(bytes).await, Err(CheckError::Unconfirmed));
    }
    let unavailable = Transport::offline(Scripted::default(), 1, 0);
    assert_eq!(
        unavailable.catalog(&Fence::default(), &key()).await,
        Err(CheckError::Unconfirmed)
    );
    let catalog = check(200, &body(vec![model("claude-sonnet-5-5", marker)]))
        .await
        .expect("valid sentinel catalog");
    let [row] = catalog.as_slice() else {
        panic!("one known model");
    };
    assert_eq!(row.info.display_name.as_str(), marker);
    let printed = format!("{:?} {:?}", Some(Ok::<_, CheckError>(catalog)), key());
    let logs = captured.text();
    assert!(logs.contains("capture-control"), "capture active");
    for output in [printed, logs] {
        for secret in [marker, CATALOG_FIXTURE_KEY] {
            assert!(!output.contains(secret), "content text is redacted");
            assert!(
                !output.contains(format!("{:?}", secret.as_bytes()).trim_matches(['[', ']'])),
                "content bytes are redacted"
            );
        }
    }
}

async fn delayed(headers: u64, body: u64) -> Result<Vec<CatalogModel>, CheckError> {
    let (reader, mut writer) = tokio::io::duplex(256);
    let task = tokio::spawn(async move {
        let body_text = br#"{"data":[],"has_more":false}"#;
        let headers_text = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            body_text.len()
        );
        tokio::time::sleep(Duration::from_secs(headers)).await;
        if writer.write_all(headers_text.as_bytes()).await.is_ok() {
            tokio::time::sleep(Duration::from_secs(body)).await;
            writer.write_all(body_text).await.ok();
        }
    });
    let peer = Scripted(Arc::new(Mutex::new(Script {
        reader: Some(Box::new(reader)),
        ..Script::default()
    })));
    let result = Transport::offline(peer, 1, 0)
        .catalog(&Fence::default(), &key())
        .await;
    task.abort();
    if let Err(error) = task.await {
        assert!(error.is_cancelled(), "synthetic peer must not panic");
    }
    result
}

#[tokio::test(start_paused = true)]
async fn whole_deadline_includes_headers_and_body_for_every_transport() {
    for (headers, body) in [(16, 0), (0, 16), (10, 10)] {
        let started = tokio::time::Instant::now();
        assert_eq!(delayed(headers, body).await, Err(CheckError::Unconfirmed));
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }
    assert_eq!(delayed(7, 7).await, Ok(Vec::new()));
}

#[tokio::test]
async fn dropping_a_catalog_check_drops_its_held_transport() {
    let (reader, _writer) = tokio::io::duplex(256);
    let peer = Scripted(Arc::new(Mutex::new(Script {
        reader: Some(Box::new(reader)),
        ..Script::default()
    })));
    let transport = Transport::offline(peer.clone(), 1, 0);
    let fence = Fence::default();
    let key = key();
    let mut check = Box::pin(transport.catalog(&fence, &key));
    tokio::select! {
        biased;
        _ = &mut check => panic!("held transport cannot complete"),
        () = tokio::task::yield_now() => {}
    }
    assert_eq!(peer.0.lock().expect("peer").drops, 0);
    drop(check);
    assert_eq!(
        peer.0.lock().expect("peer").drops,
        1,
        "cancel drops transport immediately"
    );
}
