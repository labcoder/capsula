use std::{fmt, num::NonZeroU64};

use kumochi_protocol::{
    Secret,
    message::{FailReason, RepoName},
};
use kumochi_scene::text::Bounded;

use super::transport::{BoundedHttp, Control};
use crate::connections::github::{
    GitHubToken,
    api::{self, ApiError, GitHubUser},
    device::Repositories,
    issues::{IssueContent, MatchQuery, list_matches},
};
use crate::issues::{Action, Lookup, Mark, Stage};
use crate::runtime::account::github::repositories;

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;

pub(super) struct Prepared {
    pub http: BoundedHttp,
    pub user: GitHubUser,
    pub repos: Repositories,
    pub mark: Mark,
}

impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuePreflight")
            .field("repositories", &self.repos.names.len())
            .field("presend", &self.mark.presend.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub reason: FailReason,
    pub unauthorized: bool,
}

const INCONCLUSIVE: Failure = Failure {
    reason: FailReason::PreflightFailed,
    unauthorized: false,
};
const NO_ACCOUNT: Failure = Failure {
    reason: FailReason::NotConnected,
    unauthorized: false,
};
const NO_REPO: Failure = Failure {
    reason: FailReason::NotFound,
    unauthorized: false,
};

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self {
            reason: error
                .definite_reason()
                .unwrap_or(FailReason::PreflightFailed),
            unauthorized: matches!(error, ApiError::Refused { status: 401, .. }),
        }
    }
}

async fn authority<'a>(
    http: &BoundedHttp,
    token: &Secret<GitHubToken>,
    action: &'a Action,
) -> Result<(&'a RepoName, GitHubUser, Repositories), Failure> {
    let bound = action.account.as_ref().ok_or(NO_ACCOUNT)?;
    let repo = action.repo.as_ref().ok_or(NO_REPO)?;
    let user = api::user(http, token).await?;
    if user.id != bound.id {
        return Err(NO_ACCOUNT);
    }
    let repos = repositories::fetch(http, token, http.tuning()).await?;
    if !repos.names.contains(repo) {
        return Err(NO_REPO);
    }
    // Only the id bound at review names the repository: none bound, or another listed, refuses.
    if action.repo_id.is_none() || repos.id(repo) != action.repo_id {
        return Err(NO_REPO);
    }
    Ok((repo, user, repos))
}

pub(super) async fn preflight(
    http: BoundedHttp,
    token: Secret<GitHubToken>,
    action: &Action,
    control: &Control,
) -> Result<Prepared, Failure> {
    let (repo, user, repos) = authority(&http, &token, action).await?;
    let anchor = control.server_date_s().ok_or(INCONCLUSIVE)?;
    let query = MatchQuery {
        repo,
        creator: &user.login,
        since_s: anchor.saturating_sub(1),
        content: IssueContent {
            title: &action.title,
            body: &action.body,
        },
    };
    let found = list_matches(&http, &token, &query).await?;
    let at_s = found
        .meta
        .date_s
        .filter(|date| *date > 0 && *date >= anchor)
        .and_then(NonZeroU64::new)
        .ok_or(INCONCLUSIVE)?;
    if !found.complete {
        return Err(INCONCLUSIVE);
    }
    let numbers = found
        .issues
        .into_iter()
        .map(|issue| NonZeroU64::new(issue.number).ok_or(INCONCLUSIVE))
        .collect::<Result<Vec<_>, _>>()?;
    let presend = Bounded::new(numbers).map_err(|_| INCONCLUSIVE)?;
    Ok(Prepared {
        http,
        user,
        repos,
        mark: Mark { at_s, presend },
    })
}

pub(super) async fn check(
    http: &BoundedHttp,
    token: &Secret<GitHubToken>,
    action: &Action,
) -> Result<Lookup, Failure> {
    let Stage::Unknown { mark, .. } = &action.stage else {
        return Err(INCONCLUSIVE);
    };
    let (repo, user, _) = authority(http, token, action).await?;
    let query = MatchQuery {
        repo,
        creator: &user.login,
        since_s: mark.at_s.get().saturating_sub(1),
        content: IssueContent {
            title: &action.title,
            body: &action.body,
        },
    };
    let found = list_matches(http, token, &query).await?;
    let Some(date) = found
        .meta
        .date_s
        .filter(|date| *date > 0 && *date >= mark.at_s.get())
    else {
        return Ok(Lookup::Failed);
    };
    if !found.complete {
        return Ok(Lookup::Failed);
    }
    let mut result = Lookup::None;
    for issue in found.issues {
        if issue.created_s > date {
            return Ok(Lookup::Failed);
        }
        if mark
            .presend
            .iter()
            .any(|number| number.get() == issue.number)
        {
            continue;
        }
        if issue.created_s == mark.at_s.get() {
            return Ok(Lookup::Failed);
        }
        if issue.created_s > mark.at_s.get() {
            let number = NonZeroU64::new(issue.number).ok_or(INCONCLUSIVE)?;
            result = match result {
                Lookup::None => Lookup::Unplaced(number),
                _ => Lookup::Several,
            };
        }
    }
    Ok(result)
}
