//! One stateless authorization-code exchange. The caller retains registration and token ownership.

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{Scripted, authorization, tokens};

use kumochi_protocol::Secret;
use serde::Deserialize;
use url::Url;
use zeroize::Zeroizing;

use super::{Authorization, DYNAMIC_CLIENT, RESOURCE};
use crate::connections::clock::Clock;
use crate::connections::error::Recovery;
use crate::connections::http_client::{
    Endpoint, Http, HttpError, MAX_BODY_BYTES, Request, Response,
};
use crate::connections::records::{
    MAX_ID_BYTES, MAX_RECORD_BYTES, MAX_REFRESH_BYTES, MAX_SCOPE_BYTES, MAX_TOKEN_BYTES,
};

/// Local policy from the spike: never retain an advertised access lifetime over one day.
const MAX_EXPIRES_IN: u64 = 86400;

/// Documented token errors, without retaining arbitrary provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthCode {
    /// This one-use code cannot be exchanged; start a fresh attempt.
    InvalidGrant,
    /// The issued client registration wasn't accepted.
    InvalidClient,
    /// A refresh token that's expired, invalidated or reused.
    SessionEnded,
    /// `subscription_sharing_user_not_eligible`.
    NotEligible,
    /// `subscription_sharing_usage_unavailable` or `subscription_sharing_user_unavailable`.
    PlanBusy,
    /// `chatpass_v2_scope_not_authorized` or `chatpass_v2_invalid_authorization_context`.
    GrantRefused,
    /// Another typed OAuth error; never log its raw text.
    Other,
}

impl OAuthCode {
    pub(crate) fn parse(code: &str) -> Self {
        match code {
            "invalid_grant" => Self::InvalidGrant,
            "invalid_client" => Self::InvalidClient,
            "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused" => Self::SessionEnded,
            "subscription_sharing_user_not_eligible" => Self::NotEligible,
            "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
                Self::PlanBusy
            }
            "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
                Self::GrantRefused
            }
            _ => Self::Other,
        }
    }

    /// What this code asks for on a refresh or a plan request. An unlisted code goes by status:
    /// a busy or failing server asks for a retry, and any other refusal for a new sign-in, so the
    /// user always has a way out.
    #[must_use]
    pub fn recovery(self, status: u16) -> Recovery {
        match self {
            Self::Other if status == 429 || status >= 500 => Recovery::RetryLater,
            Self::InvalidGrant | Self::SessionEnded | Self::Other => Recovery::SignInAgain,
            Self::NotEligible => Recovery::Unavailable,
            Self::PlanBusy => Recovery::RetryLater,
            Self::InvalidClient | Self::GrantRefused => Recovery::FixRequest,
        }
    }
}

/// A bounded exchange failure. Only a typed OAuth 4xx conclusively reports no tokens issued.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExchangeError {
    /// Local inputs were invalid; no request was dispatched.
    #[error("invalid code-exchange request")]
    Request,
    /// The deadline elapsed before any request could be dispatched.
    #[error("account request expired before dispatch")]
    PreDispatchTimeout,
    /// The transport failed; the caller must account for an unknown issuance outcome.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// A typed OAuth error on a 4xx response.
    #[error("code exchange refused ({status}, {code:?})")]
    OAuth {
        /// The provider's status.
        status: u16,
        /// A closed, content-free code.
        code: OAuthCode,
    },
    /// The response status or shape was unusable; issuance is not ruled out.
    #[error("invalid code-exchange response ({status})")]
    Response {
        /// The provider's status, never its body.
        status: u16,
    },
    /// Parsed HTTP 200 credentials failed validation; the caller owns any received refresh token.
    #[error("unusable code-exchange credentials")]
    Unusable {
        /// Retained for in-memory revocation, including empty or oversized received values.
        refresh_token: Option<Secret<Zeroizing<String>>>,
    },
    /// An unparsed response record or OAuth error code exceeded its byte limit.
    #[error("code-exchange response exceeds its bounds")]
    Bounds,
}

/// Issued, bounded credentials, not yet an authenticated account or a durable record.
/// Keep this value alive through verification, save retries or later revocation ownership.
#[derive(Debug)]
pub struct TokenSet {
    /// The opaque bearer, never interpreted as identity.
    pub access_token: Secret<Zeroizing<String>>,
    /// The rotating credential, including when later identity verification fails.
    pub refresh_token: Option<Secret<Zeroizing<String>>>,
    /// The signed identity token, still untrusted until verification succeeds. A refresh may
    /// leave it out; an exchange never does.
    pub id_token: Option<Secret<Zeroizing<String>>>,
    /// The actual grant, not the originally requested scopes.
    pub scope: Secret<Zeroizing<String>>,
    /// Provider lifetime clipped to the local one-day cap.
    pub expires_in: u64,
}

impl TokenSet {
    /// Compute expiry without consuming credentials if wall time is unavailable or overflows.
    pub fn expires_at(&self, clock: &impl Clock) -> Option<u64> {
        clock.unix_seconds()?.checked_add(self.expires_in)
    }
}

#[derive(Deserialize)]
struct RefreshOnly {
    refresh_token: Option<Zeroizing<String>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<Zeroizing<String>>,
    id_token: Option<Zeroizing<String>>,
    scope: Option<Zeroizing<String>>,
    token_type: Option<Zeroizing<String>>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Zeroizing<String>,
}

/// Exchange exactly this callback's code, verifier, client id and redirect URI.
/// Registration persistence and account replacement belong to the caller, not this function.
///
/// # Errors
/// Rejects invalid local inputs, bounded transport failures, OAuth refusals or unusable tokens.
///
/// # Cancellation safety
/// After dispatch, dropping this future doesn't prove no credentials were issued. The caller
/// must retain the token-lane request owner and account for an unknown outcome.
pub async fn exchange(
    http: &impl Http,
    authorization: &Authorization,
) -> Result<TokenSet, ExchangeError> {
    prepare_exchange(authorization)?.send(http).await
}

#[derive(Debug)]
#[must_use]
pub(in crate::connections) struct ExchangeRequest(Request);

pub(in crate::connections) fn prepare_exchange(
    authorization: &Authorization,
) -> Result<ExchangeRequest, ExchangeError> {
    let client = authorization.client_id.expose();
    let code = authorization.code.expose();
    let verifier = authorization.verifier.expose();
    if client.is_empty()
        || client.len() > MAX_ID_BYTES
        || client.as_str() == DYNAMIC_CLIENT
        || code.is_empty()
        || code.len() > MAX_TOKEN_BYTES
        || !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        || authorization.redirect.len() > MAX_ID_BYTES
    {
        return Err(ExchangeError::Request);
    }
    let redirect = Url::parse(&authorization.redirect).map_err(|_| ExchangeError::Request)?;
    if redirect.scheme() != "http"
        || redirect.host_str() != Some("127.0.0.1")
        || redirect.port().is_none_or(|p| p == 0)
        || redirect.path() != "/auth/callback"
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.query().is_some()
        || redirect.fragment().is_some()
    {
        return Err(ExchangeError::Request);
    }
    let fields = [
        ("grant_type", "authorization_code"),
        ("client_id", client.as_str()),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", authorization.redirect.as_str()),
        ("resource", RESOURCE),
    ]
    .into_iter()
    .map(|(key, value)| (key, Secret::new(Zeroizing::new(value.to_owned()))))
    .collect();
    Ok(ExchangeRequest(Request {
        endpoint: Endpoint::Token,
        fields,
    }))
}

impl ExchangeRequest {
    pub(in crate::connections) async fn send(
        self,
        http: &impl Http,
    ) -> Result<TokenSet, ExchangeError> {
        let response = http.execute(self.0).await?;
        let tokens = parse_response(&response)?;
        if tokens.id_token.is_none() {
            return Err(ExchangeError::Unusable {
                refresh_token: tokens.refresh_token,
            });
        }
        Ok(tokens)
    }
}

pub(crate) fn parse_response(response: &Response) -> Result<TokenSet, ExchangeError> {
    let bytes = response.body.expose();
    if bytes.len() > MAX_BODY_BYTES {
        return Err(ExchangeError::Http(HttpError::BodyTooLarge));
    }
    let invalid = ExchangeError::Response {
        status: response.status,
    };
    let refresh_token = if (200..300).contains(&response.status) {
        match serde_json::from_slice::<RefreshOnly>(bytes) {
            Ok(data) => data.refresh_token,
            Err(_) if bytes.len() > MAX_RECORD_BYTES => return Err(ExchangeError::Bounds),
            Err(_) => return Err(invalid),
        }
    } else {
        None
    };
    if response.status != 200 {
        if (200..300).contains(&response.status) {
            return Err(ExchangeError::Unusable {
                refresh_token: refresh_token.map(Secret::new),
            });
        }
        if (400..500).contains(&response.status)
            && let Ok(body) = serde_json::from_slice::<ErrorBody>(bytes)
        {
            let code = body.error;
            if code.len() > MAX_ID_BYTES {
                return Err(ExchangeError::Bounds);
            }
            if !code.is_empty() {
                return Err(ExchangeError::OAuth {
                    status: response.status,
                    code: OAuthCode::parse(&code),
                });
            }
        }
        return Err(invalid);
    }
    let data: TokenResponse = match serde_json::from_slice(bytes) {
        Ok(data) => data,
        Err(_) => {
            return Err(ExchangeError::Unusable {
                refresh_token: refresh_token.map(Secret::new),
            });
        }
    };
    let id_token = data.id_token;
    let (Some(access_token), Some(scope), Some(token_type), Some(expires_in)) = (
        data.access_token,
        data.scope,
        data.token_type,
        data.expires_in,
    ) else {
        return Err(ExchangeError::Unusable {
            refresh_token: refresh_token.map(Secret::new),
        });
    };
    let invalid_fields = [
        (&access_token, MAX_TOKEN_BYTES),
        (&scope, MAX_SCOPE_BYTES),
        (&token_type, MAX_ID_BYTES),
    ]
    .into_iter()
    .chain(id_token.as_ref().map(|value| (value, MAX_TOKEN_BYTES)))
    .chain(
        refresh_token
            .as_ref()
            .map(|value| (value, MAX_REFRESH_BYTES)),
    )
    .any(|(value, limit)| value.is_empty() || value.len() > limit);
    if bytes.len() > MAX_RECORD_BYTES
        || invalid_fields
        || !token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(ExchangeError::Unusable {
            refresh_token: refresh_token.map(Secret::new),
        });
    }
    Ok(TokenSet {
        access_token: Secret::new(access_token),
        refresh_token: refresh_token.map(Secret::new),
        id_token: id_token.map(Secret::new),
        scope: Secret::new(scope),
        expires_in: expires_in.min(MAX_EXPIRES_IN),
    })
}
