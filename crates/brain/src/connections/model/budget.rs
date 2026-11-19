//! A cap on model requests for live checks. Only debug builds read it; release builds have none.

use std::sync::atomic::{AtomicU32, Ordering};

use kumochi_protocol::constants::LIVE_BUDGET_ENV;

/// The model requests this process may still send.
#[derive(Debug, Default)]
pub struct Budget {
    limit: Option<u32>,
    used: AtomicU32,
}

impl Budget {
    /// The cap the live-budget variable sets in a debug build; none in a release build.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_var(LIVE_BUDGET_ENV)
    }

    /// The cap `var` sets in a debug build; none in a release build.
    #[must_use]
    pub fn from_var(var: &str) -> Self {
        if !cfg!(debug_assertions) {
            return Self::default();
        }
        Self::limited(limit(std::env::var(var).ok().as_deref()))
    }

    /// At most `limit` requests, or any number.
    #[must_use]
    pub fn limited(limit: Option<u32>) -> Self {
        Self {
            limit,
            used: AtomicU32::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn spent(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    /// Takes one request from the budget; `false` once it's spent.
    pub fn reserve(&self) -> bool {
        let Some(limit) = self.limit else {
            return true;
        };
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < limit).then_some(used + 1)
            })
            .is_ok()
    }
}

/// The variable's cap; a value that isn't a count allows nothing.
fn limit(value: Option<&str>) -> Option<u32> {
    value.map(|value| value.trim().parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{Budget, limit};

    #[test]
    fn the_variable_sets_the_cap_and_garbage_allows_nothing() {
        assert_eq!(limit(None), None);
        assert_eq!(limit(Some("2")), Some(2));
        assert_eq!(limit(Some(" 3 ")), Some(3));
        assert_eq!(limit(Some("two")), Some(0));
        assert_eq!(limit(Some("-1")), Some(0));
    }

    #[test]
    fn a_budget_of_two_refuses_the_third() {
        let budget = Budget::limited(Some(2));
        assert_eq!(
            [budget.reserve(), budget.reserve(), budget.reserve()],
            [true, true, false]
        );
        assert!(Budget::limited(None).reserve());
    }
}
