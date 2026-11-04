//! What a failed Responses request means, from the published plan-usage error ledger (the
//! "Errors and recovery" page). Documented codes decide first, then the status. Provider
//! prose, such as `message` or `detail`, is never kept, and nothing here erases a token set.

#[cfg(test)]
mod tests;

use kumochi_protocol::message::{Action, ErrorKey, UserError};
use serde_json::Value;

/// A documented error code this build acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// `subscription_sharing_usage_limit_exceeded`: pause and open usage settings.
    UsageLimit,
    /// `subscription_sharing_usage_unavailable` or `subscription_sharing_user_unavailable`.
    Unavailable,
    /// `subscription_sharing_user_not_eligible`.
    NotEligible,
    /// `subscription_sharing_unsupported_capability`, naming the input in `param`.
    Unsupported,
    /// `subscription_sharing_route_not_supported`.
    RouteRefused,
    /// `subscription_sharing_invalid_user`.
    InvalidUser,
    /// `chatpass_v2_scope_not_authorized` or `chatpass_v2_invalid_authorization_context`.
    NotAuthorized,
    /// Any other code; its text isn't kept.
    Other,
}

impl Code {
    fn parse(code: &str) -> Self {
        match code {
            "subscription_sharing_usage_limit_exceeded" => Self::UsageLimit,
            "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
                Self::Unavailable
            }
            "subscription_sharing_user_not_eligible" => Self::NotEligible,
            "subscription_sharing_unsupported_capability" => Self::Unsupported,
            "subscription_sharing_route_not_supported" => Self::RouteRefused,
            "subscription_sharing_invalid_user" => Self::InvalidUser,
            "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
                Self::NotAuthorized
            }
            _ => Self::Other,
        }
    }
}

/// The request part an error's `param` names, by its first path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    /// `model`.
    Model,
    /// `tools`, or one of them.
    Tools,
    /// `input`, or part of it.
    Input,
    /// `reasoning`.
    Reasoning,
    /// `service_tier`.
    ServiceTier,
    /// Anything else; its text isn't kept.
    Other,
}

impl Param {
    fn parse(param: &str) -> Self {
        let segment = param.split(['.', '[']).next().unwrap_or_default();
        match segment {
            "model" => Self::Model,
            "tools" => Self::Tools,
            "input" => Self::Input,
            "reasoning" => Self::Reasoning,
            "service_tier" => Self::ServiceTier,
            _ => Self::Other,
        }
    }
}

/// A provider's refusal: its status, and its code and parameter when this build knows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderError {
    /// The HTTP status; 200 when the refusal arrived inside the event stream.
    pub status: u16,
    /// `error.code`, when there is one.
    pub code: Option<Code>,
    /// `error.param`, when there is one.
    pub param: Option<Param>,
}

impl ProviderError {
    /// An error response: its status, and a body the caller has already bounded. A body that
    /// isn't a Responses error object, such as direct admission's `{"detail": …}`, has no code.
    #[must_use]
    pub fn answered(status: u16, body: &[u8]) -> Self {
        let body = serde_json::from_slice::<Value>(body).ok();
        let error = body.as_ref().map_or(&Value::Null, |body| &body["error"]);
        Self::from_object(status, error)
    }

    /// A refusal inside the event stream: an `error` event, or a failed response's `error`.
    pub(super) fn streamed(event: &Value) -> Self {
        let error = &event["error"];
        Self::from_object(200, if error.is_object() { error } else { event })
    }

    fn from_object(status: u16, error: &Value) -> Self {
        Self {
            status,
            code: error["code"].as_str().map(Code::parse),
            param: error["param"].as_str().map(Param::parse),
        }
    }

    /// What the user is told, and the one thing they can do, if any.
    fn recovery(self) -> (ErrorKey, Option<Action>) {
        match (self.code, self.status) {
            (Some(Code::UsageLimit), _) => (ErrorKey::UsageLimit, Some(Action::ManageUsage)),
            (Some(Code::Unavailable), _) => (ErrorKey::AccountOffline, Some(Action::Retry)),
            (Some(Code::NotEligible), _) => (ErrorKey::PlanUnavailable, None),
            (Some(Code::Unsupported | Code::RouteRefused), _) => (ErrorKey::RequestRefused, None),
            (Some(Code::NotAuthorized), _) => (ErrorKey::AccountMisconfigured, None),
            // Not a confirmed revocation, so the token set stays; a new sign-in replaces it.
            (Some(Code::InvalidUser), _) | (_, 401) => {
                (ErrorKey::AccountRefused, Some(Action::SignIn))
            }
            (_, 403) => (ErrorKey::PlanUnavailable, None),
            (_, 200 | 429 | 500..) => (ErrorKey::AccountOffline, Some(Action::Retry)),
            (_, 400..=499) => (ErrorKey::RequestRefused, None),
            _ => (ErrorKey::ResponseUnreadable, Some(Action::Retry)),
        }
    }
}

/// Why a Responses request didn't complete. It holds no provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// The provider refused the request, before the stream or inside it.
    #[error("the provider refused the request ({0:?})")]
    Provider(ProviderError),
    /// `response.incomplete`.
    #[error("the response ended incomplete")]
    Incomplete,
    /// The stream ended before its terminal event.
    #[error("the response stream ended early")]
    Truncated,
    /// Bytes that aren't a Responses stream, or a cap passed.
    #[error("the response couldn't be read")]
    Protocol,
}

impl Failure {
    /// What the user is told.
    ///
    /// # Panics
    /// Never: it offers at most one action.
    #[must_use]
    pub fn user_error(self) -> UserError {
        let (key, action) = match self {
            Self::Provider(error) => error.recovery(),
            Self::Incomplete | Self::Truncated => (ErrorKey::AccountOffline, Some(Action::Retry)),
            Self::Protocol => (ErrorKey::ResponseUnreadable, Some(Action::Retry)),
        };
        UserError::new(key, Vec::new(), action.into_iter().collect())
            .expect("one action is within the limit")
    }
}
