//! Revoking listed refresh tokens at the discovered revocation endpoint (design §5), inside one
//! network budget per run. Revocation issues nothing, so a request may be dropped at the deadline.

#[cfg(test)]
mod tests;

use std::time::Duration;

use kumochi_protocol::Secret;
use serde::Deserialize;
use tokio::time::{Instant, sleep_until, timeout_at};
use zeroize::Zeroizing;

use crate::connections::http_client::{
    Endpoint, Http, HttpError, ISSUER, IssuerUrl, MAX_BODY_BYTES, Request,
};
use crate::connections::records::{Origin, Revocable};

/// Local policy: one run's network budget, covering discovery, every request and the backoff.
pub const BUDGET: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Discovery {
    issuer: Zeroizing<String>,
    revocation_endpoint: Zeroizing<String>,
}

/// Revokes each entry, the account's first, until `deadline`; returns the ones confirmed by an
/// empty 200. Anything else leaves an entry listed for a later run.
pub async fn revoke(
    http: &impl Http,
    entries: Vec<Secret<Revocable>>,
    deadline: Instant,
    backoff: [Duration; 2],
) -> Vec<Secret<Revocable>> {
    let Some(url) = endpoint(http, deadline).await else {
        return Vec::new();
    };
    let (mut ordered, others): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.expose().origin == Origin::Account);
    ordered.extend(others);
    let mut confirmed = Vec::new();
    for entry in ordered {
        if revoke_one(http, &url, entry.expose(), deadline, backoff).await {
            confirmed.push(entry);
        }
    }
    confirmed
}

async fn endpoint(http: &impl Http, deadline: Instant) -> Option<IssuerUrl> {
    if Instant::now() >= deadline {
        return None;
    }
    let request = Request {
        endpoint: Endpoint::Discovery,
        fields: Vec::new(),
    };
    let response = timeout_at(deadline, http.execute(request))
        .await
        .ok()?
        .ok()?;
    let body = response.body.expose();
    if response.status != 200 || body.len() > MAX_BODY_BYTES {
        return None;
    }
    let discovery: Discovery = serde_json::from_slice(body).ok()?;
    if discovery.issuer.as_str() != ISSUER {
        return None;
    }
    IssuerUrl::parse(&discovery.revocation_endpoint).ok()
}

/// One entry, tried again only after a transport failure or a 5xx.
async fn revoke_one(
    http: &impl Http,
    url: &IssuerUrl,
    entry: &Revocable,
    deadline: Instant,
    backoff: [Duration; 2],
) -> bool {
    for wait in std::iter::once(None).chain(backoff.map(Some)) {
        if Instant::now() >= deadline {
            return false;
        }
        if let Some(wait) = wait {
            let wake = Instant::now() + wait;
            if wake >= deadline {
                return false;
            }
            sleep_until(wake).await;
            if Instant::now() >= deadline {
                return false;
            }
        }
        let fields = [
            ("token", entry.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", entry.client_id.as_str()),
        ]
        .into_iter()
        .map(|(key, value)| (key, Secret::new(Zeroizing::new(value.to_owned()))))
        .collect();
        let request = Request {
            endpoint: Endpoint::Revocation(url.clone()),
            fields,
        };
        match timeout_at(deadline, http.execute(request)).await {
            Ok(Ok(response)) if response.status >= 500 => {}
            Ok(Ok(response)) => return response.status == 200 && response.body.expose().is_empty(),
            Ok(Err(HttpError::Timeout | HttpError::Transport | HttpError::Connect)) => {}
            Ok(Err(_)) | Err(_) => return false,
        }
    }
    false
}
