//! Single-attempt issue creation and bounded exact matching, without persistent action state.

use std::{collections::BTreeSet, fmt, num::NonZeroU64};

use kumochi_protocol::{
    Secret,
    message::{FailReason, GitHubLogin, IssueBody, IssueTitle, IssueUrl, RepoName},
};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroizing;

use super::api::{ApiError, MAX_RETRY_AFTER_S, execute, parse};
use super::{GitHubEndpoint, GitHubToken, Page};
use crate::connections::http_client::{Http, HttpError, ResponseMeta};
use crate::platform::connect_timeout_at;

#[cfg(test)]
mod tests;

/// Borrowed checked text, not a second stored draft.
#[derive(Clone, Copy)]
pub struct IssueContent<'a> {
    /// The exact title to send or compare.
    pub title: &'a IssueTitle,
    /// The exact normalized local body.
    pub body: &'a IssueBody,
}

/// Serialized checked issue text; only [`IssueContent`] can construct this payload.
///
/// Raw bytes cannot be passed through the public endpoint API:
///
/// ```compile_fail
/// use kumochi_brain::connections::github::{GitHubEndpoint, GitHubToken};
/// use kumochi_protocol::{Secret, message::RepoName};
/// use zeroize::Zeroizing;
/// fn bypass(token: Secret<GitHubToken>, repo: RepoName) -> GitHubEndpoint {
///     GitHubEndpoint::CreateIssue {
///         token, repo, body: Secret::new(Zeroizing::new(b"{}".to_vec())),
///     }
/// }
/// ```
///
/// The same paths build the endpoint from a checked payload, so a renamed path fails here too:
///
/// ```
/// use kumochi_brain::connections::github::issues::IssueContent;
/// use kumochi_brain::connections::github::{GitHubEndpoint, GitHubToken};
/// use kumochi_protocol::{Secret, message::{IssueBody, IssueTitle, RepoName}};
/// use zeroize::Zeroizing;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let token = Secret::new(GitHubToken::new(Zeroizing::new("ghu_doc".to_owned()))?);
/// let repo = RepoName::new("octo/repo")?;
/// let (title, body) = (IssueTitle::new("A title")?, IssueBody::new("A body")?);
/// let body = IssueContent { title: &title, body: &body }.into_payload()?;
/// let _endpoint = GitHubEndpoint::CreateIssue { token, repo, body };
/// # Ok(())
/// # }
/// ```
///
/// The payload's tuple constructor is private too:
///
/// ```compile_fail
/// use kumochi_brain::connections::github::issues::IssuePayload;
/// use zeroize::Zeroizing;
/// let payload = IssuePayload(Zeroizing::new(b"{}".to_vec()));
/// ```
#[derive(Clone)]
pub struct IssuePayload(Zeroizing<Vec<u8>>);

impl IssueContent<'_> {
    /// Serialize only the checked title and body, with no additional issue fields.
    ///
    /// # Examples
    /// ```
    /// use kumochi_brain::connections::github::issues::{IssueContent, IssuePayload};
    /// use kumochi_protocol::{Secret, message::{IssueTitle, IssueBody}};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let title = IssueTitle::new("A checked title")?;
    /// let body = IssueBody::new("An exact body")?;
    /// let payload: Secret<IssuePayload> = IssueContent { title: &title, body: &body }
    ///     .into_payload()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Refuses a serialization failure before dispatch.
    pub fn into_payload(self) -> Result<Secret<IssuePayload>, HttpError> {
        #[derive(Serialize)]
        struct Json<'a> {
            title: &'a str,
            body: &'a str,
        }
        let mut bytes = Zeroizing::new(Vec::new());
        serde_json::to_writer(
            &mut *bytes,
            &Json {
                title: self.title.as_str(),
                body: self.body.as_str(),
            },
        )
        .map_err(|_| HttpError::Request)?;
        Ok(Secret::new(IssuePayload(bytes)))
    }
}

impl IssuePayload {
    pub(in crate::connections) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn bytes_for_test(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for IssuePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuePayload")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// A bounded lookup scoped to one repository and creator.
pub struct MatchQuery<'a> {
    /// The requested repository.
    pub repo: &'a RepoName,
    /// The connected account's login.
    pub creator: &'a GitHubLogin,
    /// The lower update-time filter as Unix seconds.
    pub since_s: u64,
    /// Borrowed exact text to compare.
    pub content: IssueContent<'a>,
}

/// A checked candidate, without remote title or body text.
#[derive(Clone, PartialEq, Eq)]
pub struct Match {
    /// A positive issue number.
    pub number: u64,
    /// Built locally from the requested repository and number.
    pub url: IssueUrl,
    /// The checked nonnegative creation timestamp.
    pub created_s: u64,
}

/// Exact matches and the evidence needed to decide whether lookup was complete.
pub struct Matches {
    /// Checked issue candidates only, not pull requests.
    pub issues: Vec<Match>,
    /// Page one's metadata, unchanged even when page two differs.
    pub meta: ResponseMeta,
    /// False whenever page two is full, including when no row matched.
    pub complete: bool,
}

/// A single attempt's result; no variant authorizes an automatic retry.
#[derive(Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    /// A 201 whose number and canonical URL agree.
    Created {
        /// A positive issue number.
        number: u64,
        /// The locally built canonical URL.
        url: IssueUrl,
    },
    /// A refusal known not to have created an issue.
    Definite {
        /// A closed reason for the action machine.
        reason: FailReason,
        /// A known response status, absent for a pre-dispatch failure.
        status: Option<u16>,
        /// Parsed retry and clock metadata.
        meta: ResponseMeta,
    },
    /// The request might have created an issue; never resend automatically.
    Uncertain {
        /// A known response status, when headers were available.
        status: Option<u16>,
        /// Parsed retry and clock metadata.
        meta: ResponseMeta,
    },
}

impl fmt::Debug for IssueContent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueContent")
            .field("title_bytes", &self.title.as_str().len())
            .field("body_bytes", &self.body.as_str().len())
            .finish()
    }
}
impl fmt::Debug for MatchQuery<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MatchQuery")
    }
}
impl fmt::Debug for Match {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Match")
    }
}
impl fmt::Debug for Matches {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Matches")
            .field("count", &self.issues.len())
            .field("complete", &self.complete)
            .field("meta", &self.meta)
            .finish()
    }
}
impl fmt::Debug for CreateOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created { .. } => f.write_str("CreateOutcome::Created"),
            Self::Definite {
                reason,
                status,
                meta,
            } => f
                .debug_struct("CreateOutcome::Definite")
                .field("reason", reason)
                .field("status", status)
                .field("meta", meta)
                .finish(),
            Self::Uncertain { status, meta } => f
                .debug_struct("CreateOutcome::Uncertain")
                .field("status", status)
                .field("meta", meta)
                .finish(),
        }
    }
}

#[derive(Deserialize)]
struct CreatedWire {
    number: NonZeroU64,
    html_url: String,
}
#[derive(Deserialize)]
struct IssueWire {
    number: NonZeroU64,
    title: Zeroizing<String>,
    #[serde(deserialize_with = "nullable_body")]
    body: Option<Zeroizing<String>>,
    created_at: String,
    #[serde(default, deserialize_with = "pull_request")]
    pull_request: bool,
}
fn nullable_body<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Zeroizing<String>>, D::Error> {
    Option::deserialize(d)
}
fn pull_request<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(d).map(|_| true)
}

/// Make exactly one create attempt, preserving the difference between refusal and uncertainty.
pub async fn create_issue(
    http: &impl Http,
    token: &Secret<GitHubToken>,
    repo: &RepoName,
    content: IssueContent<'_>,
) -> CreateOutcome {
    let response = match content.into_payload() {
        Ok(body) => {
            let endpoint = GitHubEndpoint::CreateIssue {
                token: token.clone(),
                repo: repo.clone(),
                body,
            };
            let cap = endpoint.body_limit();
            execute(http, endpoint)
                .await
                .map(|response| (response, cap))
        }
        Err(error) => Err(error),
    };
    let (response, cap) = match response {
        Ok(response) => response,
        Err(error) => {
            let meta = ResponseMeta::default();
            return match ApiError::Http(error).definite_reason() {
                Some(reason) => CreateOutcome::Definite {
                    reason,
                    status: None,
                    meta,
                },
                None => CreateOutcome::Uncertain { status: None, meta },
            };
        }
    };
    let status = Some(response.status);
    let mut meta = response.meta;
    meta.retry_after_s = meta
        .retry_after_s
        .map(|seconds| seconds.min(MAX_RETRY_AFTER_S));
    match parse::<CreatedWire>(&response, 201, cap) {
        Ok(raw) => {
            if let Ok(url) = IssueUrl::for_issue(repo, raw.number.get())
                && raw.html_url == url.as_str()
            {
                return CreateOutcome::Created {
                    number: raw.number.get(),
                    url,
                };
            }
        }
        Err(error) => {
            if let Some(reason) = error.definite_reason() {
                return CreateOutcome::Definite {
                    reason,
                    status,
                    meta,
                };
            }
        }
    }
    CreateOutcome::Uncertain { status, meta }
}

/// Find exact matches with at most two pages sharing one deadline.
///
/// # Errors
/// Refuses invalid queries, malformed/oversized pages, duplicate numbers and request failures.
pub async fn list_matches(
    http: &impl Http,
    token: &Secret<GitHubToken>,
    query: &MatchQuery<'_>,
) -> Result<Matches, ApiError> {
    let endpoint = |page| GitHubEndpoint::Issues {
        token: token.clone(),
        repo: query.repo.clone(),
        creator: query.creator.clone(),
        since_s: query.since_s,
        page,
    };
    let deadline = tokio::time::Instant::now() + endpoint(Page::new(1)?).deadline();
    connect_timeout_at(deadline, async {
        let mut found = Matches {
            issues: Vec::new(),
            meta: ResponseMeta::default(),
            complete: false,
        };
        let mut seen = BTreeSet::new();
        for page in 1..=2 {
            let endpoint = endpoint(Page::new(page)?);
            let cap = endpoint.body_limit();
            let response = execute(http, endpoint).await?;
            if page == 1 {
                found.meta = response.meta;
            }
            let rows: Vec<IssueWire> = parse(&response, 200, cap)?;
            if rows.len() > 100 {
                return Err(ApiError::Response);
            }
            let count = rows.len();
            for row in rows {
                let number = row.number.get();
                let created_s = OffsetDateTime::parse(&row.created_at, &Rfc3339)
                    .ok()
                    .and_then(|v| u64::try_from(v.unix_timestamp()).ok())
                    .ok_or(ApiError::Response)?;
                if !seen.insert(number) {
                    return Err(ApiError::Response);
                }
                if !row.pull_request
                    && row.title.as_str() == query.content.title.as_str()
                    && row.body.as_ref().map_or("", |body| body.as_str())
                        == query.content.body.as_str()
                {
                    found.issues.push(Match {
                        number,
                        url: IssueUrl::for_issue(query.repo, number)
                            .map_err(|_| ApiError::Response)?,
                        created_s,
                    });
                }
            }
            if count < 100 {
                found.complete = true;
                break;
            }
        }
        Ok(found)
    })
    .await
    .map_err(|_| ApiError::Http(HttpError::Timeout))?
}
