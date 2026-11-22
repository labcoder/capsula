//! One refresh-token grant (design §3 rule 3). The caller owns its result, and the old token,
//! from the moment the request is sent.

#[cfg(test)]
mod tests;

use std::time::Duration;

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::RESOURCE;
use super::exchange::{ExchangeError, TokenSet, parse_response};
use crate::connections::http_client::{Endpoint, Http, HttpError, Request};
use crate::connections::records::{MAX_ID_BYTES, MAX_REFRESH_BYTES};

/// Local policy: the refresh's token request gives up after this long (design §6).
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Sends `refresh_token` for `client` and parses what comes back; a refresh may leave out the
/// identity token or the refresh token.
///
/// # Errors
/// Refuses empty or oversized inputs before sending; then as an exchange, with the deadline
/// firing as a timeout.
///
/// # Cancellation safety
/// Once sent, dropping this future doesn't prove the token didn't rotate; the caller keeps the
/// lane and counts an unknown outcome.
pub async fn refresh(
    http: &impl Http,
    client: &str,
    refresh_token: &str,
    limit: Duration,
) -> Result<TokenSet, ExchangeError> {
    if client.is_empty()
        || client.len() > MAX_ID_BYTES
        || refresh_token.is_empty()
        || refresh_token.len() > MAX_REFRESH_BYTES
    {
        return Err(ExchangeError::Request);
    }
    let fields = [
        ("grant_type", "refresh_token"),
        ("client_id", client),
        ("refresh_token", refresh_token),
        ("resource", RESOURCE),
    ]
    .into_iter()
    .map(|(key, value)| (key, Secret::new(Zeroizing::new(value.to_owned()))))
    .collect();
    let request = http.execute(Request {
        endpoint: Endpoint::Token,
        fields,
    });
    let response = tokio::time::timeout(limit, request)
        .await
        .map_err(|_| HttpError::Timeout)??;
    parse_response(&response)
}
