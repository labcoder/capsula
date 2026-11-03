//! One stateless authorization-code exchange. The caller retains registration and token ownership.

#[cfg(test)]
mod tests;

use kumochi_protocol::Secret;
use serde::Deserialize;
use url::Url;
use zeroize::Zeroizing;

use super::{Authorization, DYNAMIC_CLIENT, RESOURCE};
use crate::connections::clock::Clock;
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
    /// Another typed OAuth error; never log its raw text.
    Other,
}

/// A bounded exchange failure. Only a typed OAuth 4xx conclusively reports no tokens issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExchangeError {
    /// Local inputs were invalid; no request was dispatched.
    #[error("invalid code-exchange request")]
    Request,
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
    /// A response field or the whole record exceeded its byte limit.
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
    /// The signed identity token, still untrusted until verification succeeds.
    pub id_token: Secret<Zeroizing<String>>,
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
struct TokenResponse {
    access_token: Zeroizing<String>,
    refresh_token: Option<Zeroizing<String>>,
    id_token: Option<Zeroizing<String>>,
    scope: Option<Zeroizing<String>>,
    token_type: Zeroizing<String>,
    expires_in: u64,
    #[serde(default, rename = "earliest_refresh_at")]
    _earliest_refresh_at: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorValue {
    Code(Zeroizing<String>),
    Object { code: Zeroizing<String> },
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorValue,
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
    let response = http
        .execute(Request {
            endpoint: Endpoint::Token,
            fields,
        })
        .await?;
    parse_response(&response)
}

fn parse_response(response: &Response) -> Result<TokenSet, ExchangeError> {
    let bytes = response.body.expose();
    if bytes.len() > MAX_BODY_BYTES {
        return Err(ExchangeError::Http(HttpError::BodyTooLarge));
    }
    let invalid = ExchangeError::Response {
        status: response.status,
    };
    if response.status != 200 {
        if (400..500).contains(&response.status)
            && let Ok(body) = serde_json::from_slice::<ErrorBody>(bytes)
        {
            let (ErrorValue::Code(code) | ErrorValue::Object { code }) = body.error;
            if code.len() > MAX_ID_BYTES {
                return Err(ExchangeError::Bounds);
            }
            if !code.is_empty() {
                return Err(ExchangeError::OAuth {
                    status: response.status,
                    code: match code.as_str() {
                        "invalid_grant" => OAuthCode::InvalidGrant,
                        "invalid_client" => OAuthCode::InvalidClient,
                        _ => OAuthCode::Other,
                    },
                });
            }
        }
        return Err(invalid);
    }
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ExchangeError::Bounds);
    }
    let data: TokenResponse = serde_json::from_slice(bytes).map_err(|_| invalid)?;
    let refresh_token = data.refresh_token;
    let id_token = data.id_token.ok_or(invalid)?;
    let scope = data.scope.ok_or(invalid)?;
    for (value, limit) in [
        (&data.access_token, MAX_TOKEN_BYTES),
        (&id_token, MAX_TOKEN_BYTES),
        (&scope, MAX_SCOPE_BYTES),
        (&data.token_type, MAX_ID_BYTES),
    ]
    .into_iter()
    .chain(
        refresh_token
            .as_ref()
            .map(|value| (value, MAX_REFRESH_BYTES)),
    ) {
        if value.len() > limit {
            return Err(ExchangeError::Bounds);
        }
        if value.is_empty() {
            return Err(invalid);
        }
    }
    if !data.token_type.eq_ignore_ascii_case("bearer") {
        return Err(invalid);
    }
    Ok(TokenSet {
        access_token: Secret::new(data.access_token),
        refresh_token: refresh_token.map(Secret::new),
        id_token: Secret::new(id_token),
        scope: Secret::new(scope),
        expires_in: data.expires_in.min(MAX_EXPIRES_IN),
    })
}
