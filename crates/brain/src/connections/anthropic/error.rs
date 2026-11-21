//! Closed recovery from the public errors reference, read 2026-11-05:
//! <https://platform.claude.com/docs/en/api/errors> (including 402 `billing_error`).
//! Legacy credit text: <https://platform.claude.com/docs/en/about-claude/api-credits-for-subscribers>.

use std::fmt;

use kumochi_protocol::{
    Secret,
    message::{Action, ErrorKey, UserError},
};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::connections::{error::Recovery, http_client::MAX_BODY_BYTES, model};

const CREDIT_MESSAGE: &str = "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.";

/// Provider categories whose text never reaches diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The key was refused.
    Authentication,
    /// The key lacks permission.
    Permission,
    /// A rate limit was reached.
    RateLimited,
    /// The provider is overloaded.
    Overloaded,
    /// Billing needs attention.
    Credit,
    /// A transient provider failure.
    Api,
    /// The request cannot be used as sent.
    InvalidRequest,
    /// No recognized category.
    Other,
}

/// One bounded provider refusal. Its body remains private and wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    status: u16,
    kind: Kind,
    body: Secret<Zeroizing<Vec<u8>>>,
}

impl ProviderFailure {
    /// Classifies HTTP and SSE refusals without retaining parse diagnostics.
    ///
    /// # Errors
    /// Protocol for an oversized body or invalid status.
    pub fn answered(status: u16, body: Secret<Zeroizing<Vec<u8>>>) -> Result<Self, Failure> {
        if body.expose().len() > MAX_BODY_BYTES || !(status == 200 || (400..600).contains(&status))
        {
            return Err(Failure::Protocol);
        }
        let value = serde_json::from_slice::<Value>(body.expose()).ok();
        Ok(Self {
            status,
            kind: classify(status, value.as_ref()),
            body,
        })
    }

    /// The HTTP status, or 200 for an SSE refusal.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The closed provider category.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Advice only: never retries or mutates credentials.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        match self.kind {
            Kind::RateLimited | Kind::Overloaded | Kind::Api => Recovery::RetryLater,
            _ => Recovery::FixRequest,
        }
    }

    /// A content-free catalog key and at most one action.
    #[must_use]
    pub fn user_error(&self) -> UserError {
        let (key, action) = match self.kind {
            Kind::Authentication | Kind::Permission => {
                (ErrorKey::AnthropicKeyRejected, Some(Action::OpenSettings))
            }
            Kind::Credit => (ErrorKey::AnthropicCredit, Some(Action::OpenSettings)),
            Kind::RateLimited => (ErrorKey::AnthropicRateLimited, Some(Action::Retry)),
            Kind::Overloaded | Kind::Api => (ErrorKey::AnthropicUnreachable, Some(Action::Retry)),
            Kind::InvalidRequest | Kind::Other => (ErrorKey::RequestRefused, None),
        };
        user_error(key, action)
    }
}

/// The closed category for a refusal's status and, when it parsed, its JSON.
fn classify(status: u16, value: Option<&Value>) -> Kind {
    let kind = value
        .filter(|value| value["type"] == "error" && value["error"]["message"].is_string())
        .and_then(|value| value["error"]["type"].as_str());
    match (status, kind) {
        (401, _) | (_, Some("authentication_error")) => Kind::Authentication,
        (403, _) | (_, Some("permission_error")) => Kind::Permission,
        (402, _) | (_, Some("billing_error")) => Kind::Credit,
        (400, Some("invalid_request_error"))
            if value.and_then(|value| value["error"]["message"].as_str())
                == Some(CREDIT_MESSAGE) =>
        {
            Kind::Credit
        }
        (429, _) | (_, Some("rate_limit_error")) => Kind::RateLimited,
        (529, _) | (_, Some("overloaded_error")) => Kind::Overloaded,
        (500..=599, _) | (_, Some("api_error" | "timeout_error")) => Kind::Api,
        (400, _) | (_, Some("invalid_request_error")) => Kind::InvalidRequest,
        _ => Kind::Other,
    }
}

impl fmt::Debug for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderFailure")
            .field("status", &self.status)
            .field("kind", &self.kind)
            .field("bytes", &self.body.expose().len())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the provider refused the request")
    }
}

impl std::error::Error for ProviderFailure {}

/// One failed attempt, without foreign diagnostics or automatic recovery actions.
#[derive(Clone, PartialEq, Eq)]
pub enum Failure {
    /// Invalid envelope or request bound exceeded; nothing was sent.
    Request,
    /// The credential cannot be sent.
    Credential,
    /// Admission requires rebuilding the request.
    Rebuild,
    /// A request budget has been spent.
    Spent,
    /// I/O failed or stalled.
    Transport,
    /// The send's own lease stopped admitting writes after one may have written. Only its
    /// picture's grant refuses, so the coordinator names why; it's never sent again on its own.
    Withheld,
    /// Invalid framing, event shape or byte bound.
    Protocol,
    /// The transport ended before the answer completed.
    Truncated,
    /// The answer reached the request's token or context limit.
    Exhausted,
    /// The model explicitly refused.
    Refused,
    /// The consumer did not accept events before its deadline.
    BackpressureTimeout,
    /// An HTTP or streamed provider error.
    Provider(ProviderFailure),
}

impl Failure {
    /// Classifies a bounded SSE error event using the HTTP error mapping.
    #[must_use]
    pub fn streamed(event: &Value) -> Self {
        if event["type"] != "error"
            || !event["error"]["type"].is_string()
            || !event["error"]["message"].is_string()
        {
            return Self::Protocol;
        }
        let Ok(body) = super::request::encode(event, MAX_BODY_BYTES) else {
            return Self::Protocol;
        };
        // The event as already read decides the category; the body only travels, bounded.
        Self::Provider(ProviderFailure {
            status: 200,
            kind: classify(200, Some(event)),
            body: Secret::new(body),
        })
    }

    /// Advice only: never retries or mutates credentials.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        match self {
            Self::Provider(error) => error.recovery(),
            Self::Transport
            | Self::Truncated
            | Self::Rebuild
            | Self::Protocol
            | Self::BackpressureTimeout => Recovery::RetryLater,
            _ => Recovery::FixRequest,
        }
    }

    /// A content-free catalog key and at most one action.
    #[must_use]
    pub fn user_error(&self) -> UserError {
        match self {
            Self::Provider(error) => error.user_error(),
            Self::Credential => {
                user_error(ErrorKey::AnthropicKeyRejected, Some(Action::OpenSettings))
            }
            Self::Transport | Self::Truncated | Self::Rebuild | Self::BackpressureTimeout => {
                user_error(ErrorKey::AnthropicUnreachable, Some(Action::Retry))
            }
            Self::Protocol => user_error(ErrorKey::ResponseUnreadable, Some(Action::Retry)),
            Self::Withheld => user_error(ErrorKey::ScreenPictureStopped, None),
            Self::Exhausted => user_error(ErrorKey::AnthropicAnswerLimit, None),
            Self::Request | Self::Spent | Self::Refused => {
                user_error(ErrorKey::RequestRefused, None)
            }
        }
    }
}

fn user_error(key: ErrorKey, action: Option<Action>) -> UserError {
    UserError::new(key, Vec::new(), action.into_iter().collect()).expect("at most one action")
}

impl From<model::error::Failure> for Failure {
    fn from(error: model::error::Failure) -> Self {
        match error {
            model::error::Failure::Request => Self::Request,
            model::error::Failure::Credential => Self::Credential,
            model::error::Failure::Rebuild => Self::Rebuild,
            model::error::Failure::Spent => Self::Spent,
            model::error::Failure::Transport => Self::Transport,
            model::error::Failure::Withheld => Self::Withheld,
            model::error::Failure::Incomplete | model::error::Failure::Truncated => Self::Truncated,
            model::error::Failure::Protocol | model::error::Failure::Provider(_) => Self::Protocol,
        }
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => f.debug_tuple("Provider").field(error).finish(),
            Self::Request => f.write_str("Request"),
            Self::Credential => f.write_str("Credential"),
            Self::Rebuild => f.write_str("Rebuild"),
            Self::Spent => f.write_str("Spent"),
            Self::Transport => f.write_str("Transport"),
            Self::Withheld => f.write_str("Withheld"),
            Self::Protocol => f.write_str("Protocol"),
            Self::Truncated => f.write_str("Truncated"),
            Self::Exhausted => f.write_str("Exhausted"),
            Self::Refused => f.write_str("Refused"),
            Self::BackpressureTimeout => f.write_str("BackpressureTimeout"),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
