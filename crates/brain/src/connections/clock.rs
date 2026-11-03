//! Wall time for token expiry, separate from Tokio's monotonic operation deadlines.

use std::time::{SystemTime, UNIX_EPOCH};

/// A controllable source of Unix seconds; absence means the clock is before the epoch.
pub trait Clock: Send + Sync {
    /// Current Unix seconds, or no usable timestamp.
    fn unix_seconds(&self) -> Option<u64>;
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

#[cfg(test)]
pub(crate) struct FixedClock(pub(crate) u64);

#[cfg(test)]
impl Clock for FixedClock {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.0)
    }
}
