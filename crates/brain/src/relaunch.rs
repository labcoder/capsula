//! When to relaunch a shell that died without saying `Quitting`.

use std::time::{Duration, Instant};

use kumochi_protocol::message::SceneId;

/// The delay before each relaunch within one window [guess, v1 §2.3].
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_secs(1),
    Duration::from_secs(5),
];
/// Crashes older than this are forgotten.
const WINDOW: Duration = Duration::from_secs(60);
const SOON: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) delay: Option<Duration>,
    pub(crate) clear: Vec<SceneId>,
}

#[derive(Debug)]
struct Membership {
    ids: Vec<SceneId>,
    queued: Option<Instant>,
}

/// Recent shell crashes.
#[derive(Debug, Default)]
pub(crate) struct Crashes {
    deaths: Vec<Instant>,
    history: Vec<(Instant, Vec<SceneId>)>,
    current: Option<Membership>,
}

impl Crashes {
    pub(crate) fn begin_session(&mut self) {
        self.current = None;
    }

    pub(crate) fn described(&mut self, ids: Vec<SceneId>, now: Instant) {
        self.history
            .retain(|(at, _)| now.duration_since(*at) < WINDOW);
        if ids.is_empty() {
            return;
        }
        self.history.push((now, ids.clone()));
        self.current = Some(Membership { ids, queued: None });
    }

    pub(crate) fn queued(&mut self, now: Instant) {
        if let Some(current) = &mut self.current {
            current.queued = Some(now);
        }
    }

    pub(crate) fn cleared(&mut self, ids: &[SceneId]) {
        for (_, scenes) in &mut self.history {
            scenes.retain(|scene| !ids.contains(scene));
        }
        self.history.retain(|(_, scenes)| !scenes.is_empty());
        if let Some(current) = &mut self.current {
            current.ids.retain(|scene| !ids.contains(scene));
        }
    }

    pub(crate) fn plan(&mut self, now: Instant) -> Plan {
        let delay = self.record(now);
        self.history
            .retain(|(at, _)| now.duration_since(*at) < WINDOW);
        let current = self.current.take();
        let mut clear = if delay.is_none() {
            self.history
                .iter()
                .flat_map(|(_, ids)| ids)
                .copied()
                .collect()
        } else if self.deaths.len() > 1 {
            current
                .filter(|membership| {
                    membership
                        .queued
                        .is_none_or(|at| now.duration_since(at) <= SOON)
                })
                .map_or_else(Vec::new, |membership| membership.ids)
        } else {
            Vec::new()
        };
        clear.sort_unstable();
        clear.dedup();
        Plan { delay, clear }
    }

    /// Records a crash at `now`. Returns the delay before relaunching, or `None` once the shell
    /// has used its three relaunches in the last minute: the fourth crash stops.
    fn record(&mut self, now: Instant) -> Option<Duration> {
        self.deaths.retain(|&at| now.duration_since(at) < WINDOW);
        self.deaths.push(now);
        BACKOFF.get(self.deaths.len() - 1).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kumochi_protocol::message::SceneId;

    use super::Crashes;

    #[test]
    fn first_repeat_late_and_preconnect_deaths_use_only_the_current_membership() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut crashes = Crashes::default();
        crashes.described(vec![SceneId(1)], at(0));
        assert_eq!(crashes.plan(at(1)).clear, []);
        crashes.described(vec![SceneId(2)], at(2));
        assert_eq!(crashes.plan(at(3)).clear, [SceneId(2)]);
        crashes.described(vec![SceneId(99)], at(3));
        crashes.begin_session();
        assert_eq!(crashes.plan(at(4)).clear, []);

        let mut crashes = Crashes::default();
        crashes.plan(at(0));
        crashes.described(vec![SceneId(3)], at(1));
        crashes.queued(at(2));
        assert_eq!(crashes.plan(at(12)).clear, [SceneId(3)]);
        crashes.described(vec![SceneId(4)], at(13));
        crashes.queued(at(14));
        assert_eq!(crashes.plan(at(25)).clear, []);
    }

    #[test]
    fn deaths_without_a_typed_membership_never_request_a_clear() {
        let now = Instant::now();
        let mut crashes = Crashes::default();
        for delay in [
            Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_secs(5),
        ] {
            let plan = crashes.plan(now);
            assert_eq!(plan.clear, []);
            assert_eq!(plan.delay, Some(delay));
        }
        let plan = crashes.plan(now);
        assert_eq!(plan.clear, []);
        assert_eq!(plan.delay, None);
    }

    #[test]
    fn give_up_clears_old_memberships_but_not_expired_or_confirmed_ids() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut crashes = Crashes::default();
        crashes.described(vec![SceneId(9)], at(0));
        crashes.plan(at(0));
        crashes.described(vec![SceneId(1), SceneId(2)], at(61));
        crashes.queued(at(61));
        crashes.plan(at(72));
        crashes.begin_session();
        crashes.plan(at(73));
        crashes.described(vec![SceneId(2), SceneId(3)], at(74));
        crashes.queued(at(74));
        crashes.plan(at(85));
        crashes.cleared(&[SceneId(2)]);
        let plan = crashes.plan(at(86));
        assert_eq!(plan.delay, None);
        assert_eq!(plan.clear, [SceneId(1), SceneId(3)]);
    }

    #[test]
    fn give_up_excludes_memberships_exactly_sixty_seconds_old() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut crashes = Crashes::default();
        crashes.described(vec![SceneId(9)], at(0));
        crashes.described(vec![SceneId(1)], at(1));
        for second in [1, 20, 40] {
            crashes.plan(at(second));
        }
        let plan = crashes.plan(at(60));
        assert_eq!(plan.delay, None);
        assert_eq!(plan.clear, [SceneId(1)]);
    }

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
