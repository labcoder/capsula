use kumochi_protocol::message::{Action, ErrorKey, UserError};

use super::{Code, Failure, Param, ProviderError};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../../../fixtures/responses/", $name, ".json")).as_slice()
    };
}

/// A body, its status, and what it must mean.
type Case = (
    &'static [u8],
    u16,
    Code,
    Option<Param>,
    ErrorKey,
    &'static [Action],
);
/// A body, its status, its code if any, and what it must mean.
type Fallback = (
    &'static [u8],
    u16,
    Option<Code>,
    ErrorKey,
    &'static [Action],
);

fn user_error(key: ErrorKey, actions: &[Action]) -> UserError {
    UserError::new(key, Vec::new(), actions.to_vec()).expect("valid")
}

#[test]
fn each_ledger_code_maps_to_its_recovery_before_its_status() {
    use Action::{ManageUsage, Retry, SignIn};
    let cases: [Case; 9] = [
        (
            fixture!("subscription_sharing_usage_limit_exceeded"),
            429,
            Code::UsageLimit,
            None,
            ErrorKey::UsageLimit,
            &[ManageUsage],
        ),
        (
            fixture!("subscription_sharing_usage_unavailable"),
            503,
            Code::Unavailable,
            None,
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (
            fixture!("subscription_sharing_user_unavailable"),
            503,
            Code::Unavailable,
            None,
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (
            fixture!("subscription_sharing_user_not_eligible"),
            403,
            Code::NotEligible,
            None,
            ErrorKey::PlanUnavailable,
            &[],
        ),
        (
            fixture!("subscription_sharing_unsupported_capability"),
            400,
            Code::Unsupported,
            Some(Param::Tools),
            ErrorKey::RequestRefused,
            &[],
        ),
        (
            fixture!("subscription_sharing_route_not_supported"),
            403,
            Code::RouteRefused,
            None,
            ErrorKey::RequestRefused,
            &[],
        ),
        (
            fixture!("subscription_sharing_invalid_user"),
            401,
            Code::InvalidUser,
            None,
            ErrorKey::AccountRefused,
            &[SignIn],
        ),
        (
            fixture!("chatpass_v2_scope_not_authorized"),
            403,
            Code::NotAuthorized,
            None,
            ErrorKey::AccountMisconfigured,
            &[],
        ),
        (
            fixture!("chatpass_v2_invalid_authorization_context"),
            403,
            Code::NotAuthorized,
            None,
            ErrorKey::AccountMisconfigured,
            &[],
        ),
    ];
    for (body, status, code, param, key, actions) in cases {
        let error = ProviderError::answered(status, body);
        assert_eq!(
            error,
            ProviderError {
                status,
                code: Some(code),
                param
            }
        );
        // The same code inside the stream (200), or under the status with its own fallback
        // (401), means the same: the code decides first.
        for status in [status, 200, 401] {
            assert_eq!(
                Failure::Provider(ProviderError { status, ..error }).user_error(),
                user_error(key, actions),
                "{code:?} at {status}"
            );
        }
    }
}

#[test]
fn without_a_known_code_the_status_decides_and_never_ends_the_session() {
    use Action::{Retry, SignIn};
    let detail = fixture!("detail");
    let unlisted = fixture!("an_unlisted_code");
    let cases: [Fallback; 13] = [
        (detail, 401, None, ErrorKey::AccountRefused, &[SignIn]),
        (detail, 403, None, ErrorKey::PlanUnavailable, &[]),
        (detail, 503, None, ErrorKey::AccountOffline, &[Retry]),
        (
            unlisted,
            401,
            Some(Code::Other),
            ErrorKey::AccountRefused,
            &[SignIn],
        ),
        (
            unlisted,
            403,
            Some(Code::Other),
            ErrorKey::PlanUnavailable,
            &[],
        ),
        (
            unlisted,
            400,
            Some(Code::Other),
            ErrorKey::RequestRefused,
            &[],
        ),
        (
            unlisted,
            404,
            Some(Code::Other),
            ErrorKey::RequestRefused,
            &[],
        ),
        (
            unlisted,
            413,
            Some(Code::Other),
            ErrorKey::RequestRefused,
            &[],
        ),
        (
            unlisted,
            429,
            Some(Code::Other),
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (
            unlisted,
            500,
            Some(Code::Other),
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (
            unlisted,
            200,
            Some(Code::Other),
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (
            b"<html>bad gateway</html>",
            502,
            None,
            ErrorKey::AccountOffline,
            &[Retry],
        ),
        (b"", 302, None, ErrorKey::ResponseUnreadable, &[Retry]),
    ];
    for (body, status, code, key, actions) in cases {
        let error = ProviderError::answered(status, body);
        assert_eq!(
            error,
            ProviderError {
                status,
                code,
                param: None
            },
            "{status}"
        );
        assert_eq!(
            Failure::Provider(error).user_error(),
            user_error(key, actions),
            "{status} {code:?}"
        );
    }
}

#[test]
fn a_stream_that_ends_badly_offers_retry() {
    for (failure, key) in [
        (Failure::Incomplete, ErrorKey::AccountOffline),
        (Failure::Truncated, ErrorKey::AccountOffline),
        (Failure::Protocol, ErrorKey::ResponseUnreadable),
    ] {
        assert_eq!(failure.user_error(), user_error(key, &[Action::Retry]));
    }
}

#[test]
fn a_param_is_kept_only_as_its_first_segment() {
    for (param, expected) in [
        ("model", Param::Model),
        ("tools[2].type", Param::Tools),
        ("input[0].content[1]", Param::Input),
        ("reasoning.effort", Param::Reasoning),
        ("service_tier", Param::ServiceTier),
        ("PLANTED-PARAM", Param::Other),
        ("", Param::Other),
    ] {
        assert_eq!(Param::parse(param), expected, "{param}");
    }
}

#[test]
fn no_provider_prose_reaches_debug_or_display() {
    for body in [
        fixture!("subscription_sharing_usage_limit_exceeded"),
        fixture!("subscription_sharing_unsupported_capability"),
        fixture!("an_unlisted_code"),
        fixture!("detail"),
    ] {
        assert!(
            String::from_utf8_lossy(body).contains("PLANTED-PROSE"),
            "the fixture plants its marker"
        );
        let failure = Failure::Provider(ProviderError::answered(400, body));
        for shown in [format!("{failure:?}"), failure.to_string()] {
            assert!(!shown.contains("PLANTED"), "{shown}");
        }
    }
}
