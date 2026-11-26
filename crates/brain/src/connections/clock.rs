//! Wall time for token expiry, separate from Tokio's monotonic operation deadlines.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A controllable source of Unix seconds; absence means the clock is before the epoch.
pub trait Clock: Send + Sync {
    /// Current Unix seconds, or no usable timestamp.
    fn unix_seconds(&self) -> Option<u64>;

    /// Monotonic time, so a running wall clock can be told from a jump.
    fn monotonic(&self) -> Instant {
        Instant::now()
    }
}

/// The operating system's wall clock.
#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> Option<u64> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }
}

#[cfg(any(test, all(debug_assertions, feature = "test-support")))]
pub(crate) struct FixedClock(pub(crate) u64);

#[cfg(any(test, all(debug_assertions, feature = "test-support")))]
impl Clock for FixedClock {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.0)
    }
}
