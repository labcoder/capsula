//! Checked GitHub destinations, transient credentials and the sans-I/O device machine.

pub mod api;
pub mod device;
pub mod issues;
pub(crate) mod lane;
pub(crate) mod record;

use std::fmt;
use std::ops::RangeInclusive;
use std::time::Duration;

#[cfg(test)]
pub(crate) mod tests;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{GitHubLogin, RepoName};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;
use zeroize::Zeroizing;

use super::http_client::HttpError;

/// A locally bounded GitHub App client identifier.
#[derive(Clone)]
pub struct ClientId(String);

/// A device credential, kept in [`Secret`] by its owner.
#[derive(Clone)]
pub struct DeviceCodeValue(Zeroizing<String>);

/// A GitHub App user bearer, kept in [`Secret`] by its owner.
#[derive(Clone)]
pub struct GitHubToken(Zeroizing<String>);

impl ClientId {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Check the local 255-byte ASCII, nonempty, whitespace-free bound.
    ///
    /// # Errors
    /// Refuses invalid client identifiers before dispatch.
    pub fn new(value: &str) -> Result<Self, HttpError> {
        bounded_ascii(value, b'!'..=b'~')?;
        Ok(Self(value.to_owned()))
    }
}

impl DeviceCodeValue {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Check the local 255-byte ASCII, nonempty, control-free bound.
    ///
    /// # Errors
    /// Refuses invalid device credentials before dispatch.
    pub fn new(value: Zeroizing<String>) -> Result<Self, HttpError> {
        bounded_ascii(&value, b' '..=b'~')?;
        Ok(Self(value))
    }
}

impl GitHubToken {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Check the user-token prefix and the local printable 255-byte bound.
    ///
    /// # Errors
    /// Refuses other credential kinds, empty suffixes and whitespace.
    pub fn new(value: Zeroizing<String>) -> Result<Self, HttpError> {
        bounded_ascii(&value, b'!'..=b'~')?;
        if !value.starts_with("ghu_") || value.len() == 4 {
            return Err(HttpError::Request);
        }
        Ok(Self(value))
    }
}

fn bounded_ascii(value: &str, allowed: RangeInclusive<u8>) -> Result<(), HttpError> {
    if !(1..=255).contains(&value.len()) || !value.bytes().all(|b| allowed.contains(&b)) {
        return Err(HttpError::Request);
    }
    Ok(())
}

impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientId")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl fmt::Debug for DeviceCodeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCodeValue")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubToken")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// A page inside the local two-page traversal bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page(u8);

impl Page {
    /// Accept page one or two only.
    ///
    /// # Errors
    /// Refuses any other page before dispatch.
    pub fn new(value: u8) -> Result<Self, HttpError> {
        match value {
            1 | 2 => Ok(Self(value)),
            _ => Err(HttpError::Request),
        }
    }
}

/// Method, authentication and body are fixed by the destination, never supplied independently.
pub enum GitHubEndpoint {
    /// Request a device challenge without a bearer.
    DeviceCode {
        /// The registered GitHub App's public identifier.
        client_id: ClientId,
    },
    /// Poll one device challenge without a bearer.
    AccessToken {
        /// The app that requested the challenge.
        client_id: ClientId,
        /// The challenge credential, never a user code.
        code: Secret<DeviceCodeValue>,
    },
    /// Read the authenticated account.
    User {
        /// The configured account's transient bearer.
        token: Secret<GitHubToken>,
    },
    /// Read one page of installations accessible to this user.
    Installations {
        /// The configured account's transient bearer.
        token: Secret<GitHubToken>,
        /// A page within the local traversal bound.
        page: Page,
    },
    /// Read one page of repositories accessible through this installation.
    Repositories {
        /// The configured account's transient bearer.
        token: Secret<GitHubToken>,
        /// A positive installation identifier, checked before dispatch.
        installation: u64,
        /// A page within the local traversal bound.
        page: Page,
    },
    /// Post only a checked draft's serialized title and body.
    CreateIssue {
        /// The configured account's transient bearer.
        token: Secret<GitHubToken>,
        /// The checked owner/repository destination.
        repo: RepoName,
        /// Serialized title/body, wiped when this endpoint drops.
        body: Secret<issues::IssuePayload>,
    },
    /// Read one bounded issue page for exact-match lookup.
    Issues {
        /// The configured account's transient bearer.
        token: Secret<GitHubToken>,
        /// The checked owner/repository destination.
        repo: RepoName,
        /// The exact author filter.
        creator: GitHubLogin,
        /// The lower update-time filter, checked for representability before dispatch.
        since_s: u64,
        /// A page within the local traversal bound.
        page: Page,
    },
}

impl fmt::Debug for GitHubEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DeviceCode { .. } => "GitHubEndpoint::DeviceCode",
            Self::AccessToken { .. } => "GitHubEndpoint::AccessToken",
            Self::User { .. } => "GitHubEndpoint::User",
            Self::Installations { .. } => "GitHubEndpoint::Installations",
            Self::Repositories { .. } => "GitHubEndpoint::Repositories",
            Self::CreateIssue { .. } => "GitHubEndpoint::CreateIssue",
            Self::Issues { .. } => "GitHubEndpoint::Issues",
        })
    }
}

impl GitHubEndpoint {
    pub(super) fn url(&self) -> Result<Url, HttpError> {
        let (host, path) = match self {
            Self::DeviceCode { .. } => ("github.com", "/login/device/code".to_owned()),
            Self::AccessToken { .. } => ("github.com", "/login/oauth/access_token".to_owned()),
            Self::User { .. } => ("api.github.com", "/user".to_owned()),
            Self::Installations { .. } => ("api.github.com", "/user/installations".to_owned()),
            Self::Repositories { installation, .. } => {
                if *installation == 0 {
                    return Err(HttpError::Request);
                }
                (
                    "api.github.com",
                    format!("/user/installations/{installation}/repositories"),
                )
            }
            Self::CreateIssue { repo, .. } | Self::Issues { repo, .. } => {
                ("api.github.com", format!("/repos/{}/issues", repo.as_str()))
            }
        };
        let mut url =
            Url::parse(&format!("https://{host}{path}")).map_err(|_| HttpError::Endpoint)?;
        match self {
            Self::Installations { page, .. }
            | Self::Repositories { page, .. }
            | Self::Issues { page, .. } => {
                url.query_pairs_mut()
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.0.to_string());
            }
            _ => {}
        }
        if let Self::Issues {
            creator, since_s, ..
        } = self
        {
            let since = i64::try_from(*since_s)
                .ok()
                .and_then(|v| OffsetDateTime::from_unix_timestamp(v).ok())
                .and_then(|v| v.format(&Rfc3339).ok())
                .ok_or(HttpError::Request)?;
            url.query_pairs_mut()
                .append_pair("creator", creator.as_str())
                .append_pair("state", "all")
                .append_pair("since", &since)
                .append_pair("sort", "created")
                .append_pair("direction", "desc");
        }
        check_origin(url, host, &path)
    }

    pub(super) fn body_limit(&self) -> usize {
        match self {
            Self::DeviceCode { .. } | Self::AccessToken { .. } => 4096,
            Self::User { .. } => 64 * 1024,
            _ => super::http_client::MAX_BODY_BYTES,
        }
    }

    pub(super) fn deadline(&self) -> Duration {
        Duration::from_secs(match self {
            Self::CreateIssue { .. } | Self::Issues { .. } => 30,
            _ => 15,
        })
    }
}

fn check_origin(url: Url, host: &str, path: &str) -> Result<Url, HttpError> {
    if url.scheme() != "https"
        || url.host_str() != Some(host)
        || url.path() != path
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(HttpError::Endpoint);
    }
    Ok(url)
}
