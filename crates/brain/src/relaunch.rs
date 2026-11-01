//! When to relaunch a shell that died without saying `Quitting`.

use std::time::{Duration, Instant};

/// The delay before each relaunch within one window [guess, v1 §2.3].
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_secs(1),
    Duration::from_secs(5),
];
/// Crashes older than this are forgotten.
const WINDOW: Duration = Duration::from_secs(60);

/// Recent shell crashes.
#[derive(Debug, Default)]
pub(crate) struct Crashes(Vec<Instant>);

impl Crashes {
    /// Records a crash at `now`. Returns the delay before relaunching, or `None` once the shell
    /// has used its three relaunches in the last minute: the fourth crash stops.
    pub(crate) fn record(&mut self, now: Instant) -> Option<Duration> {
        self.0.retain(|&at| now.duration_since(at) < WINDOW);
        self.0.push(now);
        BACKOFF.get(self.0.len() - 1).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::Crashes;

    #[test]
    fn backs_off_then_gives_up_within_a_minute() {
        let start = Instant::now();
        let mut crashes = Crashes::default();
        let at = |s: u64| start + Duration::from_secs(s);
        assert_eq!(crashes.record(at(0)), Some(Duration::from_millis(200)));
        assert_eq!(crashes.record(at(10)), Some(Duration::from_secs(1)));
        assert_eq!(crashes.record(at(20)), Some(Duration::from_secs(5)));
        assert_eq!(
            crashes.record(at(30)),
            None,
            "a fourth crash within a minute"
        );
    }

    #[test]
    fn crashes_older_than_a_minute_are_forgotten() {
        let start = Instant::now();
        let mut crashes = Crashes::default();
        for s in [0, 1, 2] {
            crashes.record(start + Duration::from_secs(s));
        }
        assert_eq!(
            crashes.record(start + Duration::from_secs(62)),
            Some(Duration::from_millis(200)),
            "the three earlier crashes left the window",
        );
    }
}
