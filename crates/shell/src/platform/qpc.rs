//! Admission to the native counter used by the Windows diagnostic records.

use std::io;

/// Reads the same clock as the native proxy and the Windows gate harness.
#[derive(Clone, Copy, Debug)]
pub struct QpcClock {
    counter: fn() -> i64,
    frequency: fn() -> i64,
}

impl QpcClock {
    /// Binds the native Windows counter and its frequency.
    #[cfg(windows)]
    #[must_use]
    pub const fn windows(counter: fn() -> i64, frequency: fn() -> i64) -> Self {
        Self { counter, frequency }
    }

    /// Require a backend for a requested diagnostic run before opening an output.
    ///
    /// # Errors
    /// Returns `Unsupported` when there is no native QPC backend.
    pub fn require(backend: Option<Self>) -> io::Result<Self> {
        backend.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "native performance-counter diagnostics are unavailable",
            )
        })
    }

    /// Reads the native counter without using a substitute clock.
    pub fn stamp(self) -> i64 {
        (self.counter)()
    }

    /// Reads the native clock's counts per second.
    pub fn frequency(self) -> i64 {
        (self.frequency)()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_without_a_native_clock_are_refused() {
        let error = QpcClock::require(None).expect_err("missing clock must refuse diagnostics");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}
