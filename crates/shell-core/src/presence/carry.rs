//! Candidate sampling and the decision to publish a drop. The shell supplies the native
//! reads, watch, timer, label and queue drains.

use std::fmt;
use std::time::{Duration, Instant};

use super::drag::Generation;
use super::perch::{Check, Drop, Extents, Placement, Targeting, WindowTarget};
use crate::platform::host::{HostChange, HostId, Label, WatchGen};

/// The perched body's side and the task card, in logical pixels.
// shortcut: the card is the peek's size; C2b-3 sizes it from the text panel it opens.
const EXTENTS: Extents = Extents {
    body: 56,
    card: [380, 124],
};

/// The least time between two candidate reads under a moving pointer. A pointer that comes
/// to rest inside it gets one trailing read at its end.
const READ_INTERVAL: Duration = Duration::from_millis(50);

/// The carry's candidate reads, apart from the watch they ask for.
#[derive(Default)]
pub struct Candidates {
    targeting: Targeting,
    /// Where the pointer was at the last read, and when; a host event for the watched
    /// generation clears it, so the next frame reads again under a still pointer.
    read_at: Option<([i32; 2], Instant)>,
    /// When a move that the interval held back is read.
    due: Option<Instant>,
    /// The watched generation's title as last read.
    title: Option<(WatchGen, Label)>,
}

/// What a frame's read asks Presence to do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// Nothing to change.
    Same,
    /// A new candidate: watch it under `w` and label it.
    Arm(HostId, WatchGen, Label),
    /// The watched candidate's title changed: relabel it, keeping the watch.
    Retitle(Label),
}

impl fmt::Debug for Sample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Same => f.write_str("Same"),
            Self::Arm(id, watch, title) => f
                .debug_tuple("Arm")
                .field(id)
                .field(watch)
                .field(title)
                .finish(),
            Self::Retitle(title) => f.debug_tuple("Retitle").field(title).finish(),
        }
    }
}

/// A queued event that can affect a carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Interrupts the drop so it cannot publish.
    Cancel,
    /// A watched host changed.
    HostChanged {
        /// The generation that reported the change.
        watch: WatchGen,
        /// What happened to the host.
        change: HostChange,
    },
}

impl Candidates {
    /// The read for one frame of `drag` with the pointer at `point` at `now`. `query` runs
    /// only when a host event made the last read stale, or the pointer moved and
    /// `READ_INTERVAL` has passed since the last read; a move held back sets `due`.
    pub fn sample(
        &mut self,
        drag: Generation,
        point: [i32; 2],
        now: Instant,
        query: impl FnOnce([i32; 2]) -> Option<WindowTarget>,
    ) -> Sample {
        if !self.targeting.carries(drag) {
            self.targeting.begin(drag);
            self.read_at = None;
            self.title = None;
        }
        if let Some((at, when)) = self.read_at {
            if at == point {
                self.due = None;
                return Sample::Same;
            }
            if now < when + READ_INTERVAL {
                self.due = Some(when + READ_INTERVAL);
                return Sample::Same;
            }
        }
        self.read_at = Some((point, now));
        self.due = None;
        let target = query(point);
        if let Some((id, w)) = self.targeting.candidate(drag, target.as_ref())
            && let Some(target) = target
        {
            self.title = Some((w, target.label));
            return Sample::Arm(id, w, target.label);
        }
        let watching = self.targeting.watching();
        match (target, &mut self.title) {
            (Some(target), Some((w, title))) if Some(*w) == watching && *title != target.label => {
                *title = target.label;
                Sample::Retitle(target.label)
            }
            _ => Sample::Same,
        }
    }

    /// Applies a queued event. A change to the watched host makes the last read stale.
    pub fn event(&mut self, event: Event) {
        match event {
            Event::Cancel => self.targeting.end(),
            Event::HostChanged { watch, change } => {
                if self.watching() == Some(watch) {
                    self.stale();
                }
                self.targeting.host_changed(watch, change);
            }
        }
    }

    /// Whether this drag is the carry being sampled.
    pub fn carries(&self, drag: Generation) -> bool {
        self.targeting.carries(drag)
    }

    /// The generation that should keep its watch and label.
    pub fn watching(&self) -> Option<WatchGen> {
        self.targeting.watching()
    }

    /// The watch could not be installed; that generation cannot publish a drop.
    pub fn watch_failed(&mut self, watch: WatchGen) {
        self.targeting.watch_failed(watch);
    }

    /// The deadline of a read held back by the interval.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The next sample reads, whatever the pointer and the interval say.
    pub fn stale(&mut self) {
        self.read_at = None;
    }

    /// Ends the carry and clears its reads.
    pub fn end(&mut self) {
        self.targeting.end();
        self.read_at = None;
        self.due = None;
        self.title = None;
    }
}

/// §10.4's drop on `Released` in `drag`: the first check, a drain, the second check, a
/// second drain and the publish, each once. `query` reads the window under the release point
/// now, `from` is the body's top-left, and `drain` handles every queued event.
pub fn decide<S>(
    owner: &mut S,
    drag: Generation,
    candidates: fn(&mut S) -> &mut Candidates,
    query: impl Fn() -> Option<WindowTarget>,
    from: [i32; 2],
    mut drain: impl FnMut(&mut S),
) -> Option<Placement> {
    if candidates(owner)
        .targeting
        .first_check(drag, query().as_ref())
        == Check::Return
    {
        return None;
    }
    drain(owner);
    let Drop::Ready(_, w) =
        candidates(owner)
            .targeting
            .second_check(query().as_ref(), from, EXTENTS)
    else {
        return None;
    };
    drain(owner);
    candidates(owner).targeting.publish(drag, w)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::{Candidates, Event, READ_INTERVAL, Sample, decide};
    use crate::platform::host::{HostChange, HostId, Label, PhysicalRect, WatchGen};
    use crate::presence::perch::target_fixtures::{
        HOST, OTHER, drag, host, placement_above, watch,
    };
    use crate::presence::perch::{Placement, WindowTarget};

    /// The perch on `host(HOST, 0)` at `EXTENTS`: centred above the caption, with the card on
    /// the left, since on the right it would cover the caption buttons.
    const PERCH: Placement = placement_above(
        PhysicalRect {
            left: 418,
            top: 144,
            right: 474,
            bottom: 200,
        },
        PhysicalRect {
            left: 34,
            top: 144,
            right: 414,
            bottom: 268,
        },
    );

    struct Owner {
        candidates: Candidates,
        /// What each drain finds queued, in order.
        queued: [Vec<Event>; 2],
        drains: usize,
    }

    /// A drop in drag 1 over `HOST`, armed as `w`, with `queued` waiting at each drain.
    fn drop(queued: impl FnOnce(WatchGen) -> [Vec<Event>; 2]) -> (Option<Placement>, usize) {
        let (candidates, w, _) = watching();
        let mut owner = Owner {
            candidates,
            queued: queued(w),
            drains: 0,
        };
        let placed = decide(
            &mut owner,
            drag(1),
            |owner| &mut owner.candidates,
            || Some(host(HOST, 0)),
            [418, 144],
            |owner| {
                for event in std::mem::take(&mut owner.queued[owner.drains]) {
                    owner.candidates.event(event);
                }
                owner.drains += 1;
            },
        );
        (placed, owner.drains)
    }

    fn gone(w: WatchGen) -> Event {
        Event::HostChanged {
            watch: w,
            change: HostChange::Gone,
        }
    }

    #[test]
    fn an_unchanged_host_publishes_its_placement_after_both_drains() {
        assert_eq!(drop(|_| [vec![], vec![]]).0, Some(PERCH));
    }

    #[test]
    fn a_host_gone_in_the_final_drain_publishes_nothing() {
        assert_eq!(drop(|w| [vec![], vec![gone(w)]]), (None, 2));
    }

    #[test]
    fn a_host_gone_in_the_first_drain_returns_before_the_second() {
        assert_eq!(drop(|w| [vec![gone(w)], vec![]]), (None, 1));
    }

    #[test]
    fn cancellation_in_either_drain_ends_the_drop() {
        assert_eq!(drop(|_| [vec![Event::Cancel], vec![]]), (None, 1));
        assert_eq!(drop(|_| [vec![], vec![Event::Cancel]]), (None, 2));
    }

    #[test]
    fn a_stale_generation_in_the_final_drain_changes_nothing() {
        assert_eq!(drop(|_| [vec![], vec![gone(watch(2))]]), (Some(PERCH), 2));
    }

    /// The pointer, still, over `HOST`'s caption.
    const STILL: [i32; 2] = [400, 210];

    fn titled(id: HostId, title: &str) -> WindowTarget {
        let mut target = host(id, 0);
        target.label = Label::new(&title.encode_utf16().collect::<Vec<_>>());
        target
    }

    fn moved(w: WatchGen) -> Event {
        Event::HostChanged {
            watch: w,
            change: HostChange::Moved,
        }
    }

    /// A carry in drag 1 watching `HOST` titled Alpha under `STILL`, read at the instant
    /// it returns.
    fn watching() -> (Candidates, WatchGen, Instant) {
        let mut candidates = Candidates::default();
        let alpha = titled(HOST, "Alpha");
        let t0 = Instant::now();
        let Sample::Arm(id, w, title) = candidates.sample(drag(1), STILL, t0, |_| Some(alpha))
        else {
            panic!("the first read arms");
        };
        assert_eq!((id, title), (HOST, alpha.label));
        (candidates, w, t0)
    }

    #[test]
    fn a_new_or_empty_title_relabels_the_same_watch() {
        let (mut candidates, w, mut now) = watching();
        let [alpha, beta, empty] = ["Alpha", "Beta", ""].map(|title| titled(HOST, title));
        let mut at = STILL;
        let mut read = |candidates: &mut Candidates, target: WindowTarget| {
            at[0] += 1;
            now += READ_INTERVAL;
            candidates.sample(drag(1), at, now, |_| Some(target))
        };
        assert_eq!(
            read(&mut candidates, alpha),
            Sample::Same,
            "an unchanged title"
        );
        assert_eq!(read(&mut candidates, beta), Sample::Retitle(beta.label));
        assert_eq!(read(&mut candidates, beta), Sample::Same, "unchanged again");
        assert_eq!(read(&mut candidates, empty), Sample::Retitle(empty.label));
        assert_eq!(
            candidates.watching(),
            Some(w),
            "the watch and generation stay"
        );
    }

    #[test]
    fn a_still_pointer_reads_again_only_after_its_own_host_changes() {
        let (mut candidates, w, t0) = watching();
        let unread =
            |_: [i32; 2]| -> Option<WindowTarget> { panic!("no read under a still pointer") };
        assert_eq!(candidates.sample(drag(1), STILL, t0, unread), Sample::Same);
        candidates.event(moved(watch(2)));
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, unread),
            Sample::Same,
            "another generation's event"
        );
        candidates.event(moved(w));
        assert_eq!(candidates.watching(), Some(w), "a move alone keeps it");
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, |_| None),
            Sample::Same
        );
        assert_eq!(candidates.watching(), None, "moved away from the pointer");
        candidates.event(moved(w));
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, unread),
            Sample::Same,
            "a late event for the candidate it left"
        );
    }

    #[test]
    fn a_host_gone_under_a_still_pointer_arms_the_window_now_under_it() {
        let (mut candidates, w, t0) = watching();
        candidates.event(gone(w));
        assert_eq!(candidates.watching(), None);
        let beneath = titled(OTHER, "Beneath");
        let sample = candidates.sample(drag(1), STILL, t0, |_| Some(beneath));
        assert_eq!(sample, Sample::Arm(OTHER, watch(2), beneath.label));
    }

    #[test]
    fn a_move_inside_the_interval_then_rest_reads_once_at_the_deadline() {
        let (mut candidates, _, t0) = watching();
        let reads = Cell::new(0);
        let beta = titled(HOST, "Beta");
        let read = |_: [i32; 2]| {
            reads.set(reads.get() + 1);
            Some(beta)
        };
        let rest = [STILL[0] + 5, STILL[1]];
        let ms = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(candidates.sample(drag(1), rest, ms(10), read), Sample::Same);
        assert_eq!(
            (reads.get(), candidates.due()),
            (0, Some(t0 + READ_INTERVAL)),
            "held back to the deadline"
        );
        assert_eq!(
            candidates.sample(drag(1), rest, t0 + READ_INTERVAL, read),
            Sample::Retitle(beta.label),
            "the label settles under the resting pointer"
        );
        assert_eq!((reads.get(), candidates.due()), (1, None));
        assert_eq!(
            candidates.sample(drag(1), rest, ms(200), read),
            Sample::Same
        );
        assert_eq!(reads.get(), 1, "exactly one read");
    }

    #[test]
    fn a_stale_read_runs_at_once_inside_the_interval() {
        let (mut candidates, _, t0) = watching();
        let reads = Cell::new(0);
        let read = |_: [i32; 2]| {
            reads.set(reads.get() + 1);
            Some(titled(OTHER, "Beneath"))
        };
        let released = [STILL[0] + 9, STILL[1]];
        let soon = t0 + Duration::from_millis(5);
        assert_eq!(
            candidates.sample(drag(1), released, soon, read),
            Sample::Same
        );
        assert_eq!(reads.get(), 0, "held back");
        candidates.stale();
        assert!(matches!(
            candidates.sample(drag(1), released, soon, read),
            Sample::Arm(OTHER, ..)
        ));
        assert_eq!(
            (reads.get(), candidates.due()),
            (1, None),
            "the release reads now"
        );
    }

    #[test]
    fn sample_debug_does_not_expose_window_titles() {
        let sentinel = "window title sentinel";
        let target = titled(HOST, sentinel);
        let bytes = format!("{:?}", sentinel.as_bytes());
        let units = format!("{:?}", target.label.units());
        for sample in [
            Sample::Arm(HOST, watch(1), target.label),
            Sample::Retitle(target.label),
        ] {
            let debug = format!("{sample:?}");
            assert!(!debug.contains(sentinel));
            assert!(!debug.contains(&bytes[1..bytes.len() - 1]));
            assert!(!debug.contains(&units[1..units.len() - 1]));
        }
    }
}
