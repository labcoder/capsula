use std::future::{pending, poll_fn, ready};
use std::pin::pin;
use std::task::Poll;

use serde_json::json;
use zeroize::Zeroizing;

use super::*;
use crate::connections::anthropic::tls::{Script, Scripted};
use crate::connections::model::conn::Connect;
use crate::connections::model::sse::SseEvent;

const KEY: &str = "sk-ant-api-synthetic-transport";
const END: &str = concat!(
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

fn key() -> Secret<AnthropicKey> {
    AnthropicKey::parse(Secret::new(Zeroizing::new(KEY.as_bytes().to_vec()))).expect("synthetic")
}

fn response(media: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn body() -> MessagesBody {
    MessagesBody::new(&json!({
        "model":"claude-sonnet-5-5",
        "max_tokens":2048,
        "stream":true,
        "messages":[{"role":"user","content":"synthetic"}],
        "system":[{"type":"text","text":"synthetic","cache_control":{"type":"ephemeral"}}]
    }))
    .expect("valid body")
}

#[test]
fn mint_failure_cannot_produce_a_verified_authority() {
    for invalid in [None, Some(0)] {
        assert!(Verified::from_entropy(7, key(), invalid).is_none());
    }
    let tag = Verified::from_entropy(7, key(), Some(801)).expect("nonzero");
    assert_eq!(tag.tag, ConnectionTag(801));
    assert_eq!(tag.revision, 7);
}

#[cfg(windows)]
#[test]
fn native_route_is_served_with_os_roots() {
    let native =
        Transport::native(ModelTuning::default()).expect("production serves the key route");
    assert!(matches!(native.connector, RouteConnector::Native(Some(_))));
}

#[tokio::test]
async fn unavailable_tls_has_no_fallback() {
    assert_eq!(
        RouteConnector::Native(None)
            .connect()
            .await
            .err()
            .expect("no transport")
            .kind(),
        std::io::ErrorKind::NotConnected
    );
}

#[cfg(windows)]
#[tokio::test]
async fn the_native_connector_is_never_allowed_to_dial_in_tests() {
    let tls = AnthropicTls::build().expect("the OS has trust roots");
    assert_eq!(
        RouteConnector::Native(Some(Arc::new(tls)))
            .connect()
            .await
            .err()
            .expect("tests never dial")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[tokio::test]
async fn a_failed_start_is_not_refunded() {
    let peer = Scripted::default();
    let transport = Transport::offline(peer.clone(), 0, 1);
    let authority = Verified::from_entropy(1, key(), Some(801)).expect("nonzero");
    let fence = Fence::default();
    let body = body();
    let budget = Budget::limited(Some(2));
    let offer = || Offer {
        tag: authority.tag,
        body: &body,
        fence: &fence,
        budget: &budget,
    };
    assert!(matches!(
        transport
            .messages(
                &authority,
                offer(),
                ready(Some(())),
                Capture::Nothing,
                |_: &SseEvent| {}
            )
            .await,
        Err(Failure::Transport)
    ));
    assert!(matches!(
        transport
            .messages(
                &authority,
                offer(),
                ready(Some(())),
                Capture::Nothing,
                |_: &SseEvent| {}
            )
            .await,
        Err(Failure::Spent)
    ));
    assert_eq!(peer.0.lock().expect("peer").starts, 1);
}

#[tokio::test]
async fn catalog_and_messages_spend_only_their_own_budget_before_connect() {
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        responses: [
            response("application/json", r#"{"data":[],"has_more":false}"#),
            response("text/event-stream", END),
        ]
        .into(),
        ..Script::default()
    })));
    let route = Transport::offline(peer.clone(), 1, 1);
    let authority = Verified::from_entropy(1, key(), Some(801)).expect("nonzero");
    let fence = Fence::default();
    let body = body();
    let budget = Budget::limited(Some(3));
    let send = |tag| {
        route.messages(
            &authority,
            Offer {
                tag,
                body: &body,
                fence: &fence,
                budget: &budget,
            },
            ready(Some(())),
            Capture::Nothing,
            |_: &SseEvent| {},
        )
    };

    assert_eq!(route.catalog(&fence, &key()).await, Ok(Vec::new()));
    assert_eq!(
        route.catalog(&fence, &key()).await,
        Err(CheckError::Unconfirmed)
    );
    assert!(matches!(
        send(ConnectionTag(802)).await,
        Err(Failure::Credential)
    ));
    assert_eq!(
        send(authority.tag).await.expect("one POST"),
        StopReason::EndTurn
    );
    assert!(matches!(send(authority.tag).await, Err(Failure::Spent)));
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, 2);
    assert_eq!(script.requests.len(), 2);
    assert_eq!(
        script.requests[0],
        format!(
            "GET /v1/models?limit=65 HTTP/1.1\r\nHost: api.anthropic.com\r\n\
             Authorization: Bearer {KEY}\r\nanthropic-version: 2023-06-01\r\n\
             Accept: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    );
    assert!(
        script.requests[1]
            .starts_with(b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\n")
    );
}

#[tokio::test]
async fn a_zero_slice_refuses_both_endpoints_before_any_connect() {
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script::default())));
    let route = Transport::offline(peer.clone(), 0, 0);
    let authority = Verified::from_entropy(1, key(), Some(803)).expect("nonzero");
    let fence = Fence::default();
    let body = body();
    let budget = Budget::limited(Some(17));
    assert_eq!(
        route.catalog(&fence, &key()).await,
        Err(CheckError::Unconfirmed)
    );
    let sent = route
        .messages(
            &authority,
            Offer {
                tag: authority.tag,
                body: &body,
                fence: &fence,
                budget: &budget,
            },
            ready(Some(())),
            Capture::Nothing,
            |_: &SseEvent| {},
        )
        .await;
    assert!(matches!(sent, Err(Failure::Spent)));
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, 0);
    assert!(script.requests.is_empty());
}

#[tokio::test]
async fn an_aliased_run_and_credential_fence_is_refused_before_admission() {
    let peer = Scripted::default();
    let route = Transport::offline(peer.clone(), 0, 1);
    let authority = Verified::from_entropy(1, key(), Some(801)).expect("nonzero");
    let aliased = authority.fence.clone();
    let body = body();
    let budget = Budget::limited(Some(1));
    let mut attempt = pin!(route.messages(
        &authority,
        Offer {
            tag: authority.tag,
            body: &body,
            fence: &aliased,
            budget: &budget,
        },
        pending::<Option<()>>(),
        Capture::Nothing,
        |_: &SseEvent| {},
    ));
    assert!(matches!(
        poll_fn(|cx| Poll::Ready(attempt.as_mut().poll(cx))).await,
        Poll::Ready(Err(Failure::Credential))
    ));
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, 0);
    assert_eq!(script.requests, Vec::<Vec<u8>>::new());
}

#[tokio::test]
async fn queued_catalog_and_unused_messages_grants_cannot_start_after_revocation() {
    let peer = Scripted::default();
    let route = Transport::offline(peer.clone(), 1, 1);
    let authority = Verified::from_entropy(1, key(), Some(801)).expect("nonzero");
    let catalog_fence = Fence::default();
    let queued_key = key();
    let queued_catalog = route.catalog(&catalog_fence, &queued_key);
    let fence = Fence::default();
    let body = body();
    let budget = Budget::limited(Some(3));
    let offer = || Offer {
        tag: authority.tag,
        body: &body,
        fence: &fence,
        budget: &budget,
    };
    let mut waiting = pin!(route.messages(
        &authority,
        offer(),
        pending::<Option<()>>(),
        Capture::Nothing,
        |_: &SseEvent| {},
    ));
    assert!(
        poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    authority.fence.revoke();
    catalog_fence.revoke();
    assert!(matches!(waiting.await, Err(Failure::Transport)));
    assert!(matches!(
        route
            .messages(
                &authority,
                offer(),
                ready(Some(())),
                Capture::Nothing,
                |_: &SseEvent| {}
            )
            .await,
        Err(Failure::Transport)
    ));
    assert_eq!(queued_catalog.await, Err(CheckError::Unconfirmed));
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, 0);
    assert_eq!(script.requests, Vec::<Vec<u8>>::new());
}
