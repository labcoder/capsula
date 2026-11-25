//! Startup values shared by the brain and the shell.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The built-in microphone start budget, in milliseconds.
pub const HOLD_START_TIMEOUT_MS: u64 = 10_000;
/// The built-in final-audio drain budget, in milliseconds; never a stop receipt.
pub const HOLD_DRAIN_TIMEOUT_MS: u64 = 2_000;

/// One connection's immutable tuning snapshot. Receivers check `valid` after decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellTuning {
    start_ms: u64,
    drain_ms: u64,
}

impl Default for ShellTuning {
    fn default() -> Self {
        Self {
            start_ms: HOLD_START_TIMEOUT_MS,
            drain_ms: HOLD_DRAIN_TIMEOUT_MS,
        }
    }
}

impl ShellTuning {
    /// The shortest microphone start budget, in milliseconds.
    pub const MIN_START_MS: u64 = 2_000;
    /// The shortest final-audio drain budget, in milliseconds.
    pub const MIN_DRAIN_MS: u64 = 500;

    /// Builds a snapshot only when both values fit their shared limits.
    pub fn from_ms(start_ms: u64, drain_ms: u64) -> Option<Self> {
        let tuning = Self { start_ms, drain_ms };
        tuning.valid().then_some(tuning)
    }

    /// Rejects a wire snapshot with either value outside its supported range.
    pub fn valid(self) -> bool {
        (Self::MIN_START_MS..=HOLD_START_TIMEOUT_MS).contains(&self.start_ms)
            && (Self::MIN_DRAIN_MS..=HOLD_DRAIN_TIMEOUT_MS).contains(&self.drain_ms)
    }

    /// Time allowed for microphone startup.
    pub const fn start(self) -> Duration {
        Duration::from_millis(self.start_ms)
    }

    /// Time allowed for final audio to drain.
    pub const fn drain(self) -> Duration {
        Duration::from_millis(self.drain_ms)
    }
}
