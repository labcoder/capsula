//! Complete, bounded repository authority from the user's installed GitHub App intersections.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kumochi_protocol::{
    Secret,
    message::{MAX_GITHUB_REPOS, RepoName},
};
use kumochi_scene::text::Bounded;
use tokio::time::Instant;

use crate::connections::{
    github::{
        GitHubToken, Page,
        api::{self, ApiError},
        device,
    },
    http_client::{Http, HttpError},
};

#[cfg(test)]
mod tests;

// shortcut: refresh stops at 12 requests, 60 seconds and 200 repositories; extend only with bounded traversal support.
const MAX_PAGE_REQUESTS: usize = 12;
const REFRESH_BUDGET: Duration = Duration::from_secs(60);

/// Provisional whole-operation filing deadline, awaiting the owner's review.
pub(in crate::runtime) const FILING_DEADLINE: Duration = Duration::from_secs(60);
/// Provisional counted filing request cap, awaiting the owner's review.
pub(in crate::runtime) const MAX_FILING_REQUESTS: usize = 16;

pub(in crate::runtime) async fn fetch(
    http: &impl Http,
    token: &Secret<GitHubToken>,
) -> Result<device::Repositories, ApiError> {
    let deadline = Instant::now() + REFRESH_BUDGET;
    tokio::time::timeout_at(deadline, async {
        let mut requests = 0;
        let mut installations = BTreeSet::new();
        let mut installation_total = None;
        for page in 1..=2 {
            admit(&mut requests, deadline)?;
            let reply = api::installations(http, token, Page::new(page)?).await?;
            if installation_total.is_some_and(|total| total != reply.total_count) {
                return Err(ApiError::Response);
            }
            check_page(reply.total_count, page, reply.installations.len())?;
            installation_total = Some(reply.total_count);
            for installation in reply.installations {
                if !installations.insert(installation) {
                    return Err(ApiError::Response);
                }
            }
            if reply.total_count <= u64::from(page) * 100 {
                break;
            }
        }

        let mut names = BTreeMap::new();
        for installation in installations {
            let mut seen = BTreeSet::new();
            let mut repository_total = None;
            for page in 1..=2 {
                admit(&mut requests, deadline)?;
                let reply = api::repositories(http, token, installation, Page::new(page)?).await?;
                if repository_total.is_some_and(|total| total != reply.total_count) {
                    return Err(ApiError::Response);
                }
                check_page(reply.total_count, page, reply.repositories.len())?;
                repository_total = Some(reply.total_count);
                for (name, id) in reply.repositories {
                    let canonical = name.as_str().to_ascii_lowercase();
                    if !seen.insert(canonical.clone()) {
                        return Err(ApiError::Response);
                    }
                    if !names.contains_key(&canonical) && names.len() == MAX_GITHUB_REPOS {
                        return Err(ApiError::Response);
                    }
                    names.entry(canonical).or_insert((name, id));
                }
                if reply.total_count <= u64::from(page) * 100 {
                    break;
                }
            }
        }
        let (names, ids): (Vec<RepoName>, Vec<_>) = names.into_values().unzip();
        let names =
            Bounded::<RepoName, MAX_GITHUB_REPOS>::new(names).map_err(|_| ApiError::Response)?;
        if Instant::now() >= deadline {
            return Err(ApiError::Http(HttpError::Timeout));
        }
        Ok(device::Repositories { names, ids })
    })
    .await
    .map_err(|_| ApiError::Http(HttpError::Timeout))?
}

fn admit(requests: &mut usize, deadline: Instant) -> Result<(), ApiError> {
    if Instant::now() >= deadline {
        return Err(ApiError::Http(HttpError::Timeout));
    }
    if *requests >= MAX_PAGE_REQUESTS {
        return Err(ApiError::Response);
    }
    *requests += 1;
    Ok(())
}

fn check_page(total: u64, page: u8, count: usize) -> Result<(), ApiError> {
    let total = usize::try_from(total).map_err(|_| ApiError::Response)?;
    if total > 2 * 100 || count != total.saturating_sub(usize::from(page - 1) * 100).min(100) {
        return Err(ApiError::Response);
    }
    Ok(())
}
