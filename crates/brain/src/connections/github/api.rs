//! Bounded, transient GitHub calls; no account state, retry loop or background request lives here.

use std::{fmt, num::NonZeroU64};

use kumochi_protocol::{
    Secret,
    message::{FailReason, GitHubLogin, RepoName, UserCode},
};
use serde::{Deserialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use super::{ClientId, DeviceCodeValue, GitHubEndpoint, GitHubToken, Page};
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response, ResponseMeta};

#[cfg(test)]
mod tests;

/// The only page where the user enters a device challenge.
pub const VERIFICATION_URI: &str = "https://github.com/login/device";

/// Local ceiling for retry advice, not an automatic retry schedule.
pub const MAX_RETRY_AFTER_S: u64 = 3600;

/// Closed failures, without provider diagnostics or payloads.
#[derive(Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The bounded request did not complete.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// A status other than the expected success status.
    #[error("github response refused ({status})")]
    Refused {
        /// The numeric HTTP status.
        status: u16,
        /// Parsed retry and clock metadata.
        meta: ResponseMeta,
    },
    /// An invalid, oversized or incomplete success payload.
    #[error("invalid github response")]
    Response,
    /// Refresh or expiring-token support has not been enabled.
    #[error("github refresh unsupported")]
    RefreshUnsupported,
}

impl ApiError {
    /// The definite not-created reason, absent whenever sending cannot be ruled out.
    pub fn definite_reason(self) -> Option<FailReason> {
        match self {
            Self::Http(
                HttpError::Connect
                | HttpError::TrustStoreUnavailable
                | HttpError::Configuration
                | HttpError::Endpoint
                | HttpError::Request,
            ) => Some(FailReason::Network),
            Self::Refused { status, meta } => match status {
                401 => Some(FailReason::NotConnected),
                403 if meta.retry_after_present || meta.rate_remaining == Some(0) => {
                    Some(FailReason::RateLimited)
                }
                403 => Some(FailReason::PermissionDenied),
                404 | 410 => Some(FailReason::NotFound),
                422 => Some(FailReason::InvalidIssue),
                429 => Some(FailReason::RateLimited),
                400..=499 => Some(FailReason::Rejected),
                _ => None,
            },
            _ => None,
        }
    }
}

impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(kind) => f.debug_tuple("Http").field(kind).finish(),
            Self::Refused { status, meta } => f
                .debug_struct("Refused")
                .field("status", status)
                .field("meta", meta)
                .finish(),
            Self::Response => f.write_str("Response"),
            Self::RefreshUnsupported => f.write_str("RefreshUnsupported"),
        }
    }
}

/// A challenge credential and the bounded values displayed to the user.
pub struct DeviceCode {
    /// Used only for polling this attempt.
    pub code: Secret<DeviceCodeValue>,
    /// Entered by the user on the fixed verification page.
    pub user_code: UserCode,
    /// Positive lifetime, at most 900 seconds.
    pub expires_in_s: u16,
    /// Positive minimum polling interval, at most 900 seconds.
    pub interval_s: u16,
}

/// Documented device-flow refusals; unknown codes retain no provider text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum TokenError {
    /// The user has not authorized the app yet.
    AuthorizationPending,
    /// Increase the polling interval.
    SlowDown,
    /// The attempt has expired.
    ExpiredToken,
    /// The user refused authorization.
    AccessDenied,
    /// Device flow is disabled for the app.
    DeviceFlowDisabled,
    /// The challenge is invalid.
    IncorrectDeviceCode,
    /// The grant is unsupported.
    UnsupportedGrantType,
    /// The app identifier is invalid.
    IncorrectClientCredentials,
    /// An unknown provider code.
    Other,
}

impl From<String> for TokenError {
    fn from(value: String) -> Self {
        match value.as_str() {
            "authorization_pending" => Self::AuthorizationPending,
            "slow_down" => Self::SlowDown,
            "expired_token" => Self::ExpiredToken,
            "access_denied" => Self::AccessDenied,
            "device_flow_disabled" => Self::DeviceFlowDisabled,
            "incorrect_device_code" => Self::IncorrectDeviceCode,
            "unsupported_grant_type" => Self::UnsupportedGrantType,
            "incorrect_client_credentials" => Self::IncorrectClientCredentials,
            _ => Self::Other,
        }
    }
}

/// One polling result; callers own scheduling and attempt generations.
pub enum TokenReply {
    /// A non-expiring, checked GitHub App user bearer.
    Token(Secret<GitHubToken>),
    /// A closed polling refusal.
    Error(TokenError),
}

/// The authenticated account, without provider profile text.
pub struct GitHubUser {
    /// A positive account identifier.
    pub id: u64,
    /// A checked login.
    pub login: GitHubLogin,
}

/// One bounded installation page; the account layer owns total traversal limits.
pub struct InstallationPage {
    /// Positive installation identifiers, at most 100.
    pub installations: Vec<u64>,
    /// The provider's advertised total, not permission to traverse without a bound.
    pub total_count: u64,
}

/// One bounded repository page; the account layer owns aggregation.
pub struct RepositoryPage {
    /// Checked names, at most 100.
    pub repositories: Vec<RepoName>,
    /// The provider's advertised total.
    pub total_count: u64,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceCode")
    }
}
impl fmt::Debug for TokenReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token(_) => f.write_str("TokenReply::Token"),
            Self::Error(kind) => f.debug_tuple("TokenReply::Error").field(kind).finish(),
        }
    }
}
impl fmt::Debug for GitHubUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubUser")
    }
}
impl fmt::Debug for InstallationPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallationPage")
            .field("count", &self.installations.len())
            .field("total_count", &self.total_count)
            .finish()
    }
}
impl fmt::Debug for RepositoryPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepositoryPage")
            .field("count", &self.repositories.len())
            .field("total_count", &self.total_count)
            .finish()
    }
}
#[derive(Deserialize)]
#[serde(try_from = "Zeroizing<String>")]
struct WireSecret(Zeroizing<String>);
impl TryFrom<Zeroizing<String>> for WireSecret {
    type Error = ApiError;
    fn try_from(value: Zeroizing<String>) -> Result<Self, Self::Error> {
        if value.len() > 255 {
            return Err(ApiError::Response);
        }
        Ok(Self(value))
    }
}

#[derive(Deserialize)]
struct DeviceWire {
    device_code: WireSecret,
    user_code: UserCode,
    verification_uri: String,
    expires_in: u16,
    #[serde(default = "default_interval")]
    interval: u16,
}
fn default_interval() -> u16 {
    5
}

#[derive(Deserialize)]
struct TokenWire {
    #[serde(default, deserialize_with = "present")]
    access_token: Option<WireSecret>,
    #[serde(default, deserialize_with = "present")]
    refresh_token: Option<WireSecret>,
    #[serde(default, deserialize_with = "present")]
    expires_in: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    refresh_token_expires_in: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    token_type: Option<String>,
    #[serde(default, deserialize_with = "present")]
    error: Option<TokenError>,
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct UserWire {
    id: NonZeroU64,
    login: GitHubLogin,
}
#[derive(Deserialize)]
struct Installation {
    id: NonZeroU64,
}
#[derive(Deserialize)]
struct InstallationsWire {
    total_count: u64,
    installations: Vec<Installation>,
}
#[derive(Deserialize)]
struct Repository {
    full_name: RepoName,
}
#[derive(Deserialize)]
struct RepositoriesWire {
    total_count: u64,
    repositories: Vec<Repository>,
}
pub(super) async fn execute(
    http: &impl Http,
    endpoint: GitHubEndpoint,
) -> Result<Response, HttpError> {
    endpoint.url()?;
    tokio::time::timeout(
        endpoint.deadline(),
        http.execute(Request {
            endpoint: Endpoint::GitHub(endpoint),
            fields: Vec::new(),
        }),
    )
    .await
    .map_err(|_| HttpError::Timeout)?
}
pub(super) fn parse<T: DeserializeOwned>(
    response: &Response,
    status: u16,
    cap: usize,
) -> Result<T, ApiError> {
    if response.status != status {
        let mut meta = response.meta;
        meta.retry_after_s = meta
            .retry_after_s
            .map(|seconds| seconds.min(MAX_RETRY_AFTER_S));
        return Err(ApiError::Refused {
            status: response.status,
            meta,
        });
    }
    if response.body.expose().len() > cap {
        return Err(ApiError::Response);
    }
    serde_json::from_slice(response.body.expose()).map_err(|_| ApiError::Response)
}

/// One call, parsed under the same byte cap and deadline the transport uses for it.
pub(super) async fn fetch<T: DeserializeOwned>(
    http: &impl Http,
    endpoint: GitHubEndpoint,
    status: u16,
) -> Result<T, ApiError> {
    let cap = endpoint.body_limit();
    parse(&execute(http, endpoint).await?, status, cap)
}

/// Request a challenge without looking up or persisting a credential.
///
/// # Errors
/// Refuses invalid input, transport failures and malformed bounded replies.
pub async fn request_device_code(
    http: &impl Http,
    client_id: &str,
) -> Result<DeviceCode, ApiError> {
    let raw: DeviceWire = fetch(
        http,
        GitHubEndpoint::DeviceCode {
            client_id: ClientId::new(client_id)?,
        },
        200,
    )
    .await?;
    if raw.verification_uri != VERIFICATION_URI
        || !(1..=900).contains(&raw.expires_in)
        || !(1..=900).contains(&raw.interval)
    {
        return Err(ApiError::Response);
    }
    Ok(DeviceCode {
        code: Secret::new(DeviceCodeValue::new(raw.device_code.0).map_err(|_| ApiError::Response)?),
        user_code: raw.user_code,
        expires_in_s: raw.expires_in,
        interval_s: raw.interval,
    })
}

/// Poll once; callers own the documented interval and attempt lifecycle.
///
/// # Errors
/// Refuses refresh/expiry shapes, malformed replies and request failures.
pub async fn poll_device_token(
    http: &impl Http,
    client_id: &str,
    code: &Secret<DeviceCodeValue>,
) -> Result<TokenReply, ApiError> {
    let raw: TokenWire = fetch(
        http,
        GitHubEndpoint::AccessToken {
            client_id: ClientId::new(client_id)?,
            code: code.clone(),
        },
        200,
    )
    .await?;
    if raw.refresh_token.is_some()
        || raw.expires_in.is_some()
        || raw.refresh_token_expires_in.is_some()
    {
        return Err(ApiError::RefreshUnsupported);
    }
    match (raw.access_token, raw.error) {
        (Some(token), None) if raw.token_type.as_deref() == Some("bearer") => {
            Ok(TokenReply::Token(Secret::new(
                GitHubToken::new(token.0).map_err(|_| ApiError::Response)?,
            )))
        }
        (None, Some(error)) => Ok(TokenReply::Error(error)),
        _ => Err(ApiError::Response),
    }
}

/// Read the authenticated account without retaining profile text.
///
/// # Errors
/// Refuses non-200, malformed, oversized or failed replies.
pub async fn user(http: &impl Http, token: &Secret<GitHubToken>) -> Result<GitHubUser, ApiError> {
    let raw: UserWire = fetch(
        http,
        GitHubEndpoint::User {
            token: token.clone(),
        },
        200,
    )
    .await?;
    Ok(GitHubUser {
        id: raw.id.get(),
        login: raw.login,
    })
}

/// Read one installation page, never following provider pagination links.
///
/// # Errors
/// Refuses malformed identifiers, more than 100 rows or request failures.
pub async fn installations(
    http: &impl Http,
    token: &Secret<GitHubToken>,
    page: Page,
) -> Result<InstallationPage, ApiError> {
    let raw: InstallationsWire = fetch(
        http,
        GitHubEndpoint::Installations {
            token: token.clone(),
            page,
        },
        200,
    )
    .await?;
    if raw.installations.len() > 100 {
        return Err(ApiError::Response);
    }
    Ok(InstallationPage {
        installations: raw.installations.into_iter().map(|v| v.id.get()).collect(),
        total_count: raw.total_count,
    })
}

/// Read one repository page; aggregation belongs to the account owner.
///
/// # Errors
/// Refuses invalid input, malformed identifiers, excess rows or request failures.
pub async fn repositories(
    http: &impl Http,
    token: &Secret<GitHubToken>,
    installation: u64,
    page: Page,
) -> Result<RepositoryPage, ApiError> {
    let raw: RepositoriesWire = fetch(
        http,
        GitHubEndpoint::Repositories {
            token: token.clone(),
            installation,
            page,
        },
        200,
    )
    .await?;
    if raw.repositories.len() > 100 {
        return Err(ApiError::Response);
    }
    Ok(RepositoryPage {
        repositories: raw.repositories.into_iter().map(|v| v.full_name).collect(),
        total_count: raw.total_count,
    })
}
