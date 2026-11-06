//! Which app a carried Kumo would perch on, and whether a drop still may. Presence asks after
//! each pointer sample which window to watch, routes the watch's `HostChanged` here, and on
//! `Released` runs the first check, a drain, the second check, a second drain and the publish,
//! each a plain call. Every ending of a watch generation is final for it: nothing revives a
//! window that went away, a watch that failed or a drop that was refused. How it fits:
//! handoffs/CAPS-94/c2b-design.md, "M1".

use super::caption::WindowTarget;
use super::place::{Extents, Placement, place};
use crate::platform::host::{HostChange, HostId, WatchGen};
use crate::presence::drag::Generation;

/// What the first check says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Proceed,
    Return,
}

/// What the second check says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drop {
    /// Perch at this placement, if `publish` agrees after the last drain.
    Ready(Placement, WatchGen),
    /// Back to the line; nothing is created.
    Return,
}

/// One armed watch's state since it was armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Live,
    /// It moved or resized since the last check.
    Moved,
    /// Gone, failed, refused, consumed or superseded: final for this generation.
    Ended,
}

#[derive(Clone, Copy, Debug)]
struct Armed {
    id: HostId,
    w: WatchGen,
    life: Life,
    /// The one allowed move between the checks has been used.
    retried: bool,
    /// The second check's placement, which only `publish` hands out.
    ready: Option<Placement>,
}

/// The drop target of the carry under way.
#[derive(Debug, Default)]
pub(crate) struct Targeting {
    drag: Option<Generation>,
    armed: Option<Armed>,
    /// The last generation minted. Never reset, so generations stay unique in the process.
    next: u64,
}

impl Targeting {
    /// The neck snapped in `drag`: candidates count from here.
    pub(crate) fn begin(&mut self, drag: Generation) {
        self.drag = Some(drag);
        self.armed = None;
    }

    /// The window under the pointer in `drag` is `target`. Returns the window and generation
    /// to watch when it's a new candidate; the old watch, if any, is dropped either way.
    pub(crate) fn candidate(
        &mut self,
        drag: Generation,
        target: Option<&WindowTarget>,
    ) -> Option<(HostId, WatchGen)> {
        if self.drag != Some(drag) {
            return None;
        }
        let Some(target) = target else {
            self.armed = None;
            return None;
        };
        if self
            .armed
            .is_some_and(|armed| armed.id == target.id && armed.life != Life::Ended)
        {
            return None;
        }
        self.next += 1;
        let w = WatchGen(self.next);
        self.armed = Some(Armed {
            id: target.id,
            w,
            life: Life::Live,
            retried: false,
            ready: None,
        });
        Some((target.id, w))
    }

    /// The watch for `w` couldn't be installed.
    pub(crate) fn watch_failed(&mut self, w: WatchGen) {
        if let Some(armed) = self.armed_mut(w) {
            armed.life = Life::Ended;
        }
    }

    /// The host watched under `w` changed. A stale `w` is ignored.
    pub(crate) fn host_changed(&mut self, w: WatchGen, change: HostChange) {
        if let Some(armed) = self.armed_mut(w) {
            armed.life = match (armed.life, change) {
                (Life::Ended, _) | (_, HostChange::Gone) => Life::Ended,
                _ => Life::Moved,
            };
        }
    }

    /// On `Released` in `drag`: `again` is a fresh query under the release point. It proceeds
    /// only for the armed window, still live; a move before now is absorbed by this fresh query.
    pub(crate) fn first_check(&mut self, drag: Generation, again: Option<&WindowTarget>) -> Check {
        let Some(armed) = self.armed.as_mut().filter(|_| self.drag == Some(drag)) else {
            return Check::Return;
        };
        if armed.life == Life::Ended || again.is_none_or(|target| target.id != armed.id) {
            armed.life = Life::Ended;
            return Check::Return;
        }
        armed.life = Life::Live;
        Check::Proceed
    }

    /// After the first drain: `again` is queried once more and placed from `from`, the body's
    /// top-left at the drop. One move since the first check is allowed, and this placement is
    /// then the only one that can publish; a second, or anything else, returns.
    pub(crate) fn second_check(
        &mut self,
        again: Option<&WindowTarget>,
        from: [i32; 2],
        extents: Extents,
    ) -> Drop {
        let Some(armed) = self.armed.as_mut() else {
            return Drop::Return;
        };
        let retry = armed.life == Life::Moved && !armed.retried;
        let placed = again
            .filter(|target| target.id == armed.id)
            .filter(|_| armed.life == Life::Live || retry)
            .and_then(|target| place(target, from, extents));
        let Some(placement) = placed else {
            armed.life = Life::Ended;
            return Drop::Return;
        };
        armed.retried |= retry;
        armed.life = Life::Live;
        armed.ready = Some(placement);
        Drop::Ready(placement, armed.w)
    }

    /// After the second drain: the placement the second check made, only while `drag` is still
    /// the carry, `w` still armed and nothing changed since. Consumes `w` either way.
    pub(crate) fn publish(&mut self, drag: Generation, w: WatchGen) -> Option<Placement> {
        let current = self.drag == Some(drag);
        let armed = self.armed.take_if(|armed| armed.w == w)?;
        (current && armed.life == Life::Live).then_some(armed.ready)?
    }

    /// The carry is over: nothing counts until the next `begin`.
    pub(crate) fn end(&mut self) {
        self.drag = None;
        self.armed = None;
    }

    /// The generation Presence should keep a watch for.
    pub(crate) fn watching(&self) -> Option<WatchGen> {
        self.armed
            .filter(|armed| armed.life != Life::Ended)
            .map(|armed| armed.w)
    }

    fn armed_mut(&mut self, w: WatchGen) -> Option<&mut Armed> {
        self.armed.as_mut().filter(|armed| armed.w == w)
    }
}

#[cfg(test)]
mod tests {
    use super::{Check, Drop, Targeting};
    use crate::platform::host::{HostChange, HostId, Label, PhysicalRect, WatchGen};
    use crate::presence::drag::Generation;
    use crate::presence::perch::caption::{Caption, Exclusions, Source, WindowTarget};
    use crate::presence::perch::place::{Extents, Placement};

    const KUMO: Extents = Extents {
        body: 56,
        card: [200, 120],
    };
    const HOST: HostId = HostId {
        window: 1,
        process: 2,
        thread: 3,
    };
    const OTHER: HostId = HostId {
        window: 9,
        process: 2,
        thread: 3,
    };

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PhysicalRect {
        PhysicalRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// An 800 × 600 host at 100 %, `dx` right of x = 100, on a 1920 × 1040 work area.
    fn host(id: HostId, dx: i32) -> WindowTarget {
        let mut exclusions = Exclusions::default();
        exclusions.rects[0] = Some((rect(792 + dx, 200, 900 + dx, 231), Source::Inferred));
        WindowTarget {
            id,
            frame: rect(100 + dx, 200, 900 + dx, 800),
            work_area: rect(0, 0, 1920, 1040),
            monitor_dpi: 96,
            caption: Caption {
                rect: rect(100 + dx, 200, 792 + dx, 231),
                source: Source::Inferred,
            },
            exclusions,
            label: Label::new(&[]),
        }
    }

    /// The perch on `host(_, 0)`: centred above the caption, the card to its right.
    const PERCH: Placement = Placement {
        body: PhysicalRect {
            left: 418,
            top: 144,
            right: 474,
            bottom: 200,
        },
        card: PhysicalRect {
            left: 478,
            top: 144,
            right: 678,
            bottom: 264,
        },
        above: true,
    };
    /// The perch on `host(_, 50)`.
    const MOVED: Placement = Placement {
        body: PhysicalRect {
            left: 468,
            top: 144,
            right: 524,
            bottom: 200,
        },
        card: PhysicalRect {
            left: 528,
            top: 144,
            right: 728,
            bottom: 264,
        },
        above: true,
    };

    fn drag(n: u64) -> Generation {
        Generation::new(n)
    }

    /// A carry in drag 1 armed on `HOST`.
    fn armed() -> (Targeting, WatchGen) {
        let mut targeting = Targeting::default();
        targeting.begin(drag(1));
        let (id, w) = targeting
            .candidate(drag(1), Some(&host(HOST, 0)))
            .expect("a new candidate");
        assert_eq!(id, HOST);
        (targeting, w)
    }

    /// Release, the first check, the second check and publish, with `between` run after each check.
    fn drop(
        targeting: &mut Targeting,
        again: [&WindowTarget; 2],
        between: [&dyn Fn(&mut Targeting); 2],
    ) -> Option<Placement> {
        if targeting.first_check(drag(1), Some(again[0])) == Check::Return {
            return None;
        }
        between[0](targeting);
        let Drop::Ready(_, w) = targeting.second_check(Some(again[1]), [418, 144], KUMO) else {
            return None;
        };
        between[1](targeting);
        targeting.publish(drag(1), w)
    }

    fn nothing(_: &mut Targeting) {}

    #[test]
    fn an_unchanged_host_perches_once() {
        let (mut targeting, w) = armed();
        assert_eq!(targeting.watching(), Some(w));
        assert_eq!(
            drop(&mut targeting, [&host(HOST, 0); 2], [&nothing, &nothing]),
            Some(PERCH)
        );
        assert_eq!(targeting.publish(drag(1), w), None, "w was consumed");
        assert_eq!(targeting.watching(), None);
    }

    #[test]
    fn the_same_window_keeps_its_watch_and_another_mints_the_next() {
        let (mut targeting, w) = armed();
        assert_eq!(targeting.candidate(drag(1), Some(&host(HOST, 0))), None);
        assert_eq!(
            targeting.candidate(drag(1), Some(&host(OTHER, 0))),
            Some((OTHER, WatchGen(w.0 + 1)))
        );
        assert_eq!(targeting.candidate(drag(1), None), None);
        assert_eq!(
            targeting.watching(),
            None,
            "off every app, nothing is watched"
        );
        assert_eq!(
            targeting.candidate(drag(1), Some(&host(HOST, 0))),
            Some((HOST, WatchGen(w.0 + 2))),
            "coming back is a new generation"
        );
    }

    #[test]
    fn a_window_that_went_away_never_comes_back_under_its_generation() {
        // Destroyed and recreated on the same handle and thread, identical geometry.
        let (mut targeting, w) = armed();
        targeting.host_changed(w, HostChange::Gone);
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Return
        );
        assert_eq!(
            targeting.candidate(drag(1), Some(&host(HOST, 0))),
            Some((HOST, WatchGen(w.0 + 1))),
            "the recreated window is watched afresh"
        );
        // At the first drain instead.
        let (mut targeting, w) = armed();
        let gone = move |t: &mut Targeting| t.host_changed(w, HostChange::Gone);
        assert_eq!(
            drop(&mut targeting, [&host(HOST, 0); 2], [&gone, &nothing]),
            None
        );
        assert_eq!(
            targeting.second_check(Some(&host(HOST, 0)), [418, 144], KUMO),
            Drop::Return,
            "a later check can't revive it"
        );
    }

    #[test]
    fn a_move_before_the_release_is_read_fresh() {
        let (mut targeting, w) = armed();
        targeting.host_changed(w, HostChange::Moved);
        assert_eq!(
            drop(&mut targeting, [&host(HOST, 50); 2], [&nothing, &nothing]),
            Some(MOVED)
        );
    }

    #[test]
    fn one_move_between_the_checks_is_placed_again_and_a_second_returns() {
        let (mut targeting, w) = armed();
        let moved = move |t: &mut Targeting| t.host_changed(w, HostChange::Moved);
        assert_eq!(
            drop(
                &mut targeting,
                [&host(HOST, 0), &host(HOST, 50)],
                [&moved, &nothing]
            ),
            Some(MOVED),
            "only the new placement publishes"
        );
        let (mut targeting, w) = armed();
        let moved = move |t: &mut Targeting| t.host_changed(w, HostChange::Moved);
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Proceed
        );
        moved(&mut targeting);
        assert!(matches!(
            targeting.second_check(Some(&host(HOST, 50)), [418, 144], KUMO),
            Drop::Ready(..)
        ));
        moved(&mut targeting);
        assert_eq!(
            targeting.second_check(Some(&host(HOST, 100)), [418, 144], KUMO),
            Drop::Return,
            "the second move isn't placed again"
        );
        assert_eq!(
            targeting.publish(drag(1), w),
            None,
            "the second move fails closed"
        );
    }

    #[test]
    fn a_change_in_the_final_drain_returns_with_the_same_window_and_geometry() {
        for change in [HostChange::Gone, HostChange::Moved] {
            let (mut targeting, w) = armed();
            let late = move |t: &mut Targeting| t.host_changed(w, change);
            assert_eq!(
                drop(&mut targeting, [&host(HOST, 0); 2], [&nothing, &late]),
                None,
                "{change:?}"
            );
        }
    }

    #[test]
    fn a_different_window_under_the_release_returns() {
        let (mut targeting, _) = armed();
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(OTHER, 0))),
            Check::Return
        );
        let (mut targeting, _) = armed();
        assert_eq!(targeting.first_check(drag(1), None), Check::Return);
        let (mut targeting, _) = armed();
        assert_eq!(
            drop(
                &mut targeting,
                [&host(HOST, 0), &host(OTHER, 0)],
                [&nothing, &nothing]
            ),
            None
        );
    }

    #[test]
    fn a_failed_watch_returns_and_stays_failed() {
        let (mut targeting, w) = armed();
        targeting.watch_failed(w);
        targeting.host_changed(w, HostChange::Moved);
        assert_eq!(targeting.watching(), None);
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Return
        );
    }

    #[test]
    fn a_stale_generation_changes_nothing() {
        let (mut targeting, old) = armed();
        let (_, new) = targeting
            .candidate(drag(1), Some(&host(OTHER, 0)))
            .expect("a new candidate");
        targeting.host_changed(old, HostChange::Gone);
        targeting.watch_failed(old);
        assert_eq!(targeting.watching(), Some(new));
        assert_eq!(targeting.publish(drag(1), old), None);
        assert_eq!(
            targeting.watching(),
            Some(new),
            "a stale publish consumes nothing"
        );
    }

    #[test]
    fn publish_needs_the_carry_it_was_checked_in() {
        let (mut targeting, _) = armed();
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Proceed
        );
        let Drop::Ready(placement, w) =
            targeting.second_check(Some(&host(HOST, 0)), [418, 144], KUMO)
        else {
            panic!("ready");
        };
        assert_eq!(placement, PERCH);
        assert_eq!(
            targeting.publish(drag(2), w),
            None,
            "another carry's publish"
        );
        assert_eq!(targeting.publish(drag(1), w), None, "and it used w up");
        let (mut targeting, _) = armed();
        targeting.first_check(drag(1), Some(&host(HOST, 0)));
        let Drop::Ready(_, w) = targeting.second_check(Some(&host(HOST, 0)), [418, 144], KUMO)
        else {
            panic!("ready");
        };
        targeting.end();
        targeting.begin(drag(2));
        assert_eq!(targeting.publish(drag(1), w), None);
        assert_eq!(targeting.publish(drag(2), w), None);
    }

    #[test]
    fn after_a_return_or_the_end_nothing_revives_the_drop() {
        let (mut targeting, w) = armed();
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(OTHER, 0))),
            Check::Return
        );
        targeting.host_changed(w, HostChange::Moved);
        assert_eq!(
            targeting.second_check(Some(&host(HOST, 0)), [418, 144], KUMO),
            Drop::Return
        );
        assert_eq!(targeting.publish(drag(1), w), None);
        targeting.end();
        assert_eq!(targeting.candidate(drag(1), Some(&host(HOST, 0))), None);
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Return
        );
    }

    #[test]
    fn generations_stay_unique_across_carries() {
        let (mut targeting, first) = armed();
        targeting.end();
        targeting.begin(drag(2));
        assert_eq!(
            targeting.candidate(drag(2), Some(&host(HOST, 0))),
            Some((HOST, WatchGen(first.0 + 1)))
        );
        assert_eq!(
            targeting.candidate(drag(1), Some(&host(OTHER, 0))),
            None,
            "an old drag"
        );
    }

    #[test]
    fn no_room_returns() {
        let (mut targeting, _) = armed();
        assert_eq!(
            targeting.first_check(drag(1), Some(&host(HOST, 0))),
            Check::Proceed
        );
        let mut cramped = host(HOST, 0);
        cramped.work_area = rect(100, 200, 900, 800);
        cramped.frame = rect(100, 200, 900, 240);
        assert_eq!(
            targeting.second_check(Some(&cramped), [418, 144], KUMO),
            Drop::Return
        );
    }
}
