//! Discovery-bound RS256 identity verification, separate from token ownership and persistence.

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::Jwk};
use kumochi_protocol::Secret;
use serde::{Deserialize, de::DeserializeOwned};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::connections::clock::Clock;
use crate::connections::http_client::{
    Endpoint, Http, HttpError, ISSUER, JwksUrl, MAX_BODY_BYTES, Request, Response,
};
use crate::connections::records::{MAX_EMAIL_BYTES, MAX_ID_BYTES, MAX_TOKEN_BYTES};

/// Local parser bounds for opaque nonce values and a discovery key set.
const MAX_NONCE_BYTES: usize = 128;
const MAX_JWKS_KEYS: usize = 64;
/// Local bound on a token's audience list; the normal provider token has one audience.
const MAX_AUDIENCES: usize = 16;
/// Local clock tolerance retained from the owner's spike, checked through the Clock seam.
const EXPIRY_LEEWAY_SECONDS: u64 = 30;

/// Closed verification failures, with no token, identity or provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// A bounded discovery/JWKS transport failed.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// Discovery or JWKS returned a non-success status.
    #[error("identity document unavailable ({status})")]
    Status {
        /// The status only, never the response body.
        status: u16,
    },
    /// Discovery or its same-origin key set was malformed or untrusted.
    #[error("invalid identity discovery")]
    Discovery,
    /// The signature, algorithm, binding or required claims failed validation.
    #[error("invalid identity token")]
    Token,
    /// An identity field or document exceeded a local byte/count bound.
    #[error("identity data exceeds its bounds")]
    Bounds,
    /// A verified returning sign-in belongs to a different subject.
    #[error("sign-in belongs to another account")]
    OtherAccount,
    /// The wall clock could not provide a usable timestamp.
    #[error("account clock unavailable")]
    Clock,
}

/// Identity from a verified signature, not credentials or an automatically committed account.
#[derive(Debug)]
pub struct VerifiedIdentity {
    /// The opaque signed subject, bound to the issued client.
    pub subject: Secret<Zeroizing<String>>,
    /// Optional signed display email, never an account/workspace identifier.
    pub email: Option<Secret<Zeroizing<String>>>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: Zeroizing<String>,
    jwks_uri: Zeroizing<String>,
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(Zeroizing<String>),
    Many(Vec<Zeroizing<String>>),
}

#[derive(Deserialize)]
struct Claims {
    exp: u64,
    iss: Zeroizing<String>,
    sub: Zeroizing<String>,
    aud: Audience,
    nonce: Option<Zeroizing<String>>,
    email: Option<Zeroizing<String>>,
    azp: Option<Zeroizing<String>>,
}

/// Fetch public discovery and its checked same-origin JWKS, then verify this attempt's token.
/// Token ownership stays with the caller even if verification fails.
///
/// # Errors
/// Rejects transport/document failures, non-RS256 or unbound/expired identities and bad clocks.
pub async fn verify(
    http: &impl Http,
    token: &Secret<Zeroizing<String>>,
    client: &str,
    nonce: &str,
    clock: &(impl Clock + ?Sized),
) -> Result<VerifiedIdentity, IdentityError> {
    check_inputs(token.expose(), client, nonce)?;
    let discovery: Discovery = document(
        &http
            .execute(Request {
                endpoint: Endpoint::Discovery,
                fields: Vec::new(),
            })
            .await?,
    )?;
    if discovery.issuer.as_str() != ISSUER {
        return Err(IdentityError::Discovery);
    }
    let url = JwksUrl::parse(&discovery.jwks_uri).map_err(|_| IdentityError::Discovery)?;
    let keys: KeySet = document(
        &http
            .execute(Request {
                endpoint: Endpoint::Jwks(url),
                fields: Vec::new(),
            })
            .await?,
    )?;
    let now = clock.unix_seconds().ok_or(IdentityError::Clock)?;
    verify_signed(token.expose(), &keys, client, nonce, now)
}

fn document<T: DeserializeOwned>(response: &Response) -> Result<T, IdentityError> {
    if response.status != 200 {
        return Err(IdentityError::Status {
            status: response.status,
        });
    }
    if response.body.expose().len() > MAX_BODY_BYTES {
        return Err(IdentityError::Bounds);
    }
    serde_json::from_slice(response.body.expose()).map_err(|_| IdentityError::Discovery)
}

fn check_inputs(token: &str, client: &str, nonce: &str) -> Result<(), IdentityError> {
    if token.len() > MAX_TOKEN_BYTES || client.len() > MAX_ID_BYTES || nonce.len() > MAX_NONCE_BYTES
    {
        return Err(IdentityError::Bounds);
    }
    if token.is_empty() || client.is_empty() || nonce.is_empty() {
        return Err(IdentityError::Token);
    }
    Ok(())
}

fn verify_signed(
    token: &str,
    keys: &KeySet,
    client: &str,
    nonce: &str,
    now: u64,
) -> Result<VerifiedIdentity, IdentityError> {
    check_inputs(token, client, nonce)?;
    let header = decode_header(token).map_err(|_| IdentityError::Token)?;
    if header.alg != Algorithm::RS256 {
        return Err(IdentityError::Token);
    }
    let kid = header
        .kid
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or(IdentityError::Token)?;
    if kid.len() > MAX_ID_BYTES || keys.keys.len() > MAX_JWKS_KEYS {
        return Err(IdentityError::Bounds);
    }
    let mut seen = HashSet::new();
    let mut selected = None;
    for key in &keys.keys {
        let Some(key_id) = key.get("kid").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !seen.insert(key_id) {
            return Err(IdentityError::Discovery);
        }
        if key_id == kid {
            selected = Some(key);
        }
    }
    let key = selected.ok_or(IdentityError::Token)?;
    if key.get("use").is_some_and(|usage| usage != "sig")
        || key.get("alg").is_some_and(|algorithm| algorithm != "RS256")
    {
        return Err(IdentityError::Token);
    }
    let key: Jwk = serde_json::from_value(key.clone()).map_err(|_| IdentityError::Token)?;
    let decoding_key = DecodingKey::from_jwk(&key).map_err(|_| IdentityError::Token)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    // GHSA-h395-gr6q-cpjc: require iss/aud/sub; validate our own Claims.exp: u64.
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_exp = false;
    let claims = decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|_| IdentityError::Token)?
        .claims;
    let audiences = match &claims.aud {
        Audience::One(audience) => std::slice::from_ref(audience),
        Audience::Many(audiences) => audiences.as_slice(),
    };
    if claims.iss.len() > MAX_ID_BYTES
        || claims.sub.len() > MAX_ID_BYTES
        || claims
            .nonce
            .as_ref()
            .is_some_and(|n| n.len() > MAX_NONCE_BYTES)
        || claims
            .email
            .as_ref()
            .is_some_and(|e| e.len() > MAX_EMAIL_BYTES)
        || claims.azp.as_ref().is_some_and(|a| a.len() > MAX_ID_BYTES)
        || audiences.len() > MAX_AUDIENCES
        || audiences.iter().any(|a| a.len() > MAX_ID_BYTES)
    {
        return Err(IdentityError::Bounds);
    }
    if claims.sub.is_empty()
        || claims
            .exp
            .checked_add(EXPIRY_LEEWAY_SECONDS)
            .is_none_or(|expiry| now > expiry)
        || claims.azp.as_ref().is_some_and(|a| a.as_str() != client)
        || (audiences.len() > 1 && claims.azp.as_ref().is_none_or(|a| a.as_str() != client))
        || !bool::from(
            claims
                .nonce
                .as_ref()
                .map_or(&[][..], |n| n.as_bytes())
                .ct_eq(nonce.as_bytes()),
        )
    {
        return Err(IdentityError::Token);
    }
    Ok(VerifiedIdentity {
        subject: Secret::new(claims.sub),
        email: claims.email.map(Secret::new),
    })
}

/// Require the verified subject to preserve a returning registration's identity.
///
/// # Errors
/// Refuses empty/oversized subjects and returns `OtherAccount` for a different saved subject.
pub fn check_subject(saved: Option<&str>, verified: &str) -> Result<(), IdentityError> {
    if verified.is_empty() || verified.len() > MAX_ID_BYTES {
        return Err(IdentityError::Token);
    }
    if saved.is_some_and(|saved| saved != verified) {
        return Err(IdentityError::OtherAccount);
    }
    Ok(())
}

/// Whether the returned grant explicitly permits `ChatGPT` plan usage, independent of identity.
pub fn permits_plan(scope: &str) -> bool {
    scope
        .split_whitespace()
        .any(|s| s == "chatgpt.tokens.use.direct")
}
