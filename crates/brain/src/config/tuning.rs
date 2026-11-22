//! `[tuning]` in `config.toml`: optional overrides of built-in limits, read once at start.
//!
//! Each key has a default, the built-in value, and a range. A value of the wrong type uses its
//! default, and one outside its range is clamped to the nearer edge. A group whose values don't
//! fit together uses all its defaults. Logs name only keys this build owns: an unknown key may
//! hold anything, so it's counted.

use std::time::Duration;

use toml::{Table, Value};

const TUNING: &str = "tuning";
const GITHUB_GROUP: &str = "github";

/// One tunable value: its key in its group, its built-in default and its allowed range.
pub(crate) struct Limit {
    pub(crate) key: &'static str,
    pub(crate) default: u64,
    pub(crate) min: u64,
    pub(crate) max: u64,
}

/// `[tuning.github]`. These bound requests to the user's account, so every build keeps them in
/// range and they only tighten.
pub(crate) const GITHUB: [Limit; 4] = [
    Limit {
        key: "filing_s",
        default: 60,
        min: 20,
        max: 60,
    },
    Limit {
        key: "filing_requests_count",
        default: 16,
        min: 12,
        max: 16,
    },
    Limit {
        key: "listing_s",
        default: 60,
        min: 20,
        max: 60,
    },
    Limit {
        key: "listing_requests_count",
        default: 12,
        min: 4,
        max: 12,
    },
];

/// Requests a filing makes besides its listing: the user, two match pages and the create.
const FILING_EXTRA_REQUESTS: u64 = 4;

/// GitHub's filing and listing budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GitHubTuning {
    /// One whole filing or review rearm, retries included; local settlement may finish after it.
    pub(crate) filing_deadline: Duration,
    /// Counted requests in one filing, its listing included.
    pub(crate) filing_requests: usize,
    /// One repository listing.
    pub(crate) listing_budget: Duration,
    /// Page requests in one repository listing.
    pub(crate) listing_requests: usize,
}

impl Default for GitHubTuning {
    fn default() -> Self {
        Self::from_values(GITHUB.map(|limit| limit.default))
    }
}

impl GitHubTuning {
    /// `[tuning.github]` from the settings table, or the defaults.
    pub(crate) fn from_table(table: &Table) -> Self {
        let Some(group) = group(table, GITHUB_GROUP) else {
            return Self::default();
        };
        let unknown = group
            .keys()
            .filter(|key| GITHUB.iter().all(|limit| limit.key != key.as_str()))
            .count();
        if unknown > 0 {
            tracing::warn!(
                group = GITHUB_GROUP,
                unknown,
                "unknown tuning keys; ignoring them"
            );
        }
        let values = GITHUB.each_ref().map(|limit| value(group, limit));
        let [filing_s, filing_requests, listing_s, listing_requests] = values;
        if filing_requests < listing_requests + FILING_EXTRA_REQUESTS || listing_s > filing_s {
            tracing::warn!(
                group = GITHUB_GROUP,
                "tuning values don't fit together; using the group's defaults"
            );
            return Self::default();
        }
        Self::from_values(values)
    }

    fn from_values([filing_s, filing_requests, listing_s, listing_requests]: [u64; 4]) -> Self {
        let count = |value: u64| usize::try_from(value).expect("a tuning count is at most 16");
        Self {
            filing_deadline: Duration::from_secs(filing_s),
            filing_requests: count(filing_requests),
            listing_budget: Duration::from_secs(listing_s),
            listing_requests: count(listing_requests),
        }
    }
}

/// `[tuning.<name>]` when it's a table.
fn group<'a>(table: &'a Table, name: &'static str) -> Option<&'a Table> {
    let Value::Table(tuning) = table.get(TUNING)? else {
        tracing::warn!(key = TUNING, "tuning isn't a table; using the defaults");
        return None;
    };
    let Value::Table(group) = tuning.get(name)? else {
        tracing::warn!(
            group = name,
            "tuning group isn't a table; using its defaults"
        );
        return None;
    };
    Some(group)
}

fn value(group: &Table, limit: &Limit) -> u64 {
    let Some(value) = group.get(limit.key) else {
        return limit.default;
    };
    let Some(value) = value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
    else {
        tracing::warn!(
            key = limit.key,
            "tuning value isn't a whole number; using its default"
        );
        return limit.default;
    };
    if !(limit.min..=limit.max).contains(&value) {
        tracing::warn!(
            key = limit.key,
            "tuning value is out of range; using the nearer edge"
        );
    }
    value.clamp(limit.min, limit.max)
}

#[cfg(test)]
#[path = "tuning_tests.rs"]
mod tests;
