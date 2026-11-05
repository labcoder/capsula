//! Synthetic errors shaped from the public reference; not scrubbed provider recordings.

use super::{Failure, Kind, ProviderFailure};
use crate::connections::{error::Recovery, model};
use kumochi_protocol::{
    Secret,
    message::{Action, ErrorKey},
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

fn answered(status: u16, value: &Value) -> ProviderFailure {
    ProviderFailure::answered(
        status,
        Secret::new(Zeroizing::new(serde_json::to_vec(value).unwrap())),
    )
    .expect("bounded error")
}

fn check(error: &Failure, key: ErrorKey, actions: &[Action], recovery: Recovery) {
    let user = error.user_error();
    assert_eq!(user.key(), key);
    assert_eq!(user.actions(), actions);
    assert_eq!(user.args(), &[]);
    assert_eq!(error.recovery(), recovery);
}

#[test]
fn authentication_billing_and_invalid_request_recovery() {
    check_cases(&[
        (
            401,
            "authentication_error",
            Kind::Authentication,
            ErrorKey::AnthropicKeyRejected,
            Some(Action::OpenSettings),
            Recovery::FixRequest,
        ),
        (
            403,
            "permission_error",
            Kind::Permission,
            ErrorKey::AnthropicKeyRejected,
            Some(Action::OpenSettings),
            Recovery::FixRequest,
        ),
        (
            402,
            "billing_error",
            Kind::Credit,
            ErrorKey::AnthropicCredit,
            Some(Action::OpenSettings),
            Recovery::FixRequest,
        ),
        (
            400,
            "invalid_request_error",
            Kind::InvalidRequest,
            ErrorKey::RequestRefused,
            None,
            Recovery::FixRequest,
        ),
        (
            404,
            "new_error_type",
            Kind::Other,
            ErrorKey::RequestRefused,
            None,
            Recovery::FixRequest,
        ),
    ]);
}

#[test]
fn transient_http_and_stream_recovery() {
    check_cases(&[
        (
            429,
            "rate_limit_error",
            Kind::RateLimited,
            ErrorKey::AnthropicRateLimited,
            Some(Action::Retry),
            Recovery::RetryLater,
        ),
        (
            529,
            "overloaded_error",
            Kind::Overloaded,
            ErrorKey::AnthropicUnreachable,
            Some(Action::Retry),
            Recovery::RetryLater,
        ),
        (
            500,
            "api_error",
            Kind::Api,
            ErrorKey::AnthropicUnreachable,
            Some(Action::Retry),
            Recovery::RetryLater,
        ),
        (
            504,
            "timeout_error",
            Kind::Api,
            ErrorKey::AnthropicUnreachable,
            Some(Action::Retry),
            Recovery::RetryLater,
        ),
    ]);
}

type Case<'a> = (u16, &'a str, Kind, ErrorKey, Option<Action>, Recovery);

fn check_cases(cases: &[Case<'_>]) {
    for &(status, name, kind, key, action, recovery) in cases {
        let event = json!({"type":"error","error":{"type":name,"message":"synthetic"}});
        let error = answered(status, &event);
        assert_eq!(error.status(), status);
        assert_eq!(error.kind(), kind);
        let actions: Vec<_> = action.into_iter().collect();
        check(&Failure::Provider(error), key, &actions, recovery);
        check(&Failure::streamed(&event), key, &actions, recovery);
        let fallback = answered(status, &json!({}));
        assert_eq!(fallback.kind(), kind);
        check(&Failure::Provider(fallback), key, &actions, recovery);
    }
}

#[test]
fn malformed_and_unknown_errors_do_not_invent_diagnoses() {
    for event in [
        json!(null),
        json!({"type":"error"}),
        json!({"type":"error","error":{"type":1,"message":"synthetic"}}),
        json!({"type":"error","error":{"type":"api_error","message":1}}),
        json!({"type":"other","error":{"type":"api_error","message":"synthetic"}}),
    ] {
        assert!(
            matches!(Failure::streamed(&event), Failure::Protocol),
            "malformed error event is a protocol failure"
        );
    }
    let malformed =
        ProviderFailure::answered(400, Secret::new(Zeroizing::new(b"{".to_vec()))).unwrap();
    assert_eq!(malformed.kind(), Kind::InvalidRequest);
    for message in [
        "credit",
        "invalid credit field",
        "Your credit balance is sufficient",
        "billing_error",
    ] {
        let error = answered(
            400,
            &json!({"type":"error","error":{"type":"invalid_request_error","message":message}}),
        );
        check(
            &Failure::Provider(error),
            ErrorKey::RequestRefused,
            &[],
            Recovery::FixRequest,
        );
    }
    assert!(
        matches!(
            Failure::streamed(
                &json!({"type":"error","error":{"type":"model_context_window_exceeded","message":"synthetic"}})
            ),
            Failure::Truncated
        ),
        "streamed context exhaustion truncates"
    );
    let body = Secret::new(Zeroizing::new(serde_json::to_vec(&json!({"type":"error","error":{"type":"model_context_window_exceeded","message":"synthetic"}})).unwrap()));
    assert!(
        matches!(
            ProviderFailure::answered(400, body),
            Err(Failure::Truncated)
        ),
        "HTTP context exhaustion truncates"
    );
}

#[test]
fn legacy_credit_requires_the_exact_documented_http_error() {
    let message = "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.";
    let event = json!({"type":"error","error":{"type":"invalid_request_error","message":message}});
    check(
        &Failure::Provider(answered(400, &event)),
        ErrorKey::AnthropicCredit,
        &[Action::OpenSettings],
        Recovery::FixRequest,
    );
    for (status, kind, text) in [
        (400, "invalid_request_error", format!("{message} ")),
        (400, "invalid_request_error", message.to_lowercase()),
        (
            400,
            "invalid_request_error",
            "Your credit balance is too low".to_owned(),
        ),
        (400, "new_error", message.to_owned()),
        (404, "invalid_request_error", message.to_owned()),
    ] {
        let near = json!({"type":"error","error":{"type":kind,"message":text}});
        check(
            &Failure::Provider(answered(status, &near)),
            ErrorKey::RequestRefused,
            &[],
            Recovery::FixRequest,
        );
    }
    check(
        &Failure::streamed(&event),
        ErrorKey::RequestRefused,
        &[],
        Recovery::FixRequest,
    );
}

#[test]
fn provider_body_bound_and_outermost_formatters_are_content_free() {
    let exact = Secret::new(Zeroizing::new(vec![b'x'; 1024 * 1024]));
    assert_eq!(
        ProviderFailure::answered(500, exact).unwrap().kind(),
        Kind::Api
    );
    let over = Secret::new(Zeroizing::new(vec![b'x'; 1024 * 1024 + 1]));
    assert!(
        matches!(ProviderFailure::answered(500, over), Err(Failure::Protocol)),
        "oversized body is refused"
    );
    for status in [0, 199, 201, 302, 399, 600] {
        assert!(
            matches!(
                ProviderFailure::answered(status, Secret::new(Zeroizing::new(Vec::new()))),
                Err(Failure::Protocol)
            ),
            "non-error status is refused"
        );
    }
    let marker = "PRIVATE_PROVIDER_MARKER";
    let value =
        json!({"type":"error","error":{"type":marker,"message":marker},"request_id":marker});
    let error = Failure::Provider(answered(400, &value));
    let byte_marker = format!("{:?}", marker.as_bytes());
    for formatted in [format!("{error:?}"), format!("{error}")] {
        assert!(!formatted.contains(marker), "provider text is redacted");
        assert!(
            !formatted.contains(byte_marker.trim_matches(['[', ']'])),
            "provider bytes are redacted"
        );
    }
}

#[test]
fn local_and_reused_transport_failures_remain_closed() {
    for error in [Failure::Request, Failure::Spent, Failure::Refused] {
        check(&error, ErrorKey::RequestRefused, &[], Recovery::FixRequest);
    }
    check(
        &Failure::Credential,
        ErrorKey::AnthropicKeyRejected,
        &[Action::OpenSettings],
        Recovery::FixRequest,
    );
    for error in [
        Failure::Transport,
        Failure::Truncated,
        Failure::Rebuild,
        Failure::BackpressureTimeout,
    ] {
        check(
            &error,
            ErrorKey::AnthropicUnreachable,
            &[Action::Retry],
            Recovery::RetryLater,
        );
    }
    check(
        &Failure::Protocol,
        ErrorKey::ResponseUnreadable,
        &[Action::Retry],
        Recovery::RetryLater,
    );
    for (source, expected) in [
        (model::error::Failure::Request, "Request"),
        (model::error::Failure::Credential, "Credential"),
        (model::error::Failure::Spent, "Spent"),
        (model::error::Failure::Rebuild, "Rebuild"),
        (model::error::Failure::Transport, "Transport"),
        (model::error::Failure::Incomplete, "Truncated"),
        (model::error::Failure::Truncated, "Truncated"),
        (model::error::Failure::Protocol, "Protocol"),
        (
            model::error::Failure::Provider(model::error::ProviderError {
                status: 401,
                code: None,
                param: None,
            }),
            "Protocol",
        ),
    ] {
        assert_eq!(format!("{:?}", Failure::from(source)), expected);
    }
}
