//! The carry after the snap: the app under the pointer, the watch on it, the drop label under
//! the body, and the drop decision on `Released`. How it fits: docs/development/kumo.md,
//! "The carry".

use std::time::{Duration, Instant};

use gpui_kit::{Context, SharedString, Task};

use super::drag::{End, Generation};
use super::perch::{Check, Drop, Extents, Placement, Targeting, WindowTarget, drop_target};
use super::{Presence, diagnostics};
use crate::platform::host::{HostId, Label, WatchGen};
use crate::platform::{self, HostWatch, PresenceEvent};
use crate::strings::{Strings, Text};

/// The perched body's side and the task card, in logical pixels.
// shortcut: the card is the peek's size; C2b-3 sizes it from the text panel it opens.
const EXTENTS: Extents = Extents {
    body: 56,
    card: [380, 124],
};

/// The least time between two candidate reads under a moving pointer. A pointer that comes
/// to rest inside it gets one trailing read at its end.
const READ_INTERVAL: Duration = Duration::from_millis(50);

/// What Presence holds of a carry. It has no `Debug`, since the label is screen text.
#[derive(Default)]
pub(super) struct Carry {
    candidates: Candidates,
    /// The armed generation's watch, and its drop label.
    watch: Option<(WatchGen, HostWatch, SharedString)>,
    /// The trailing read, waiting for `Candidates::due`.
    trailing: Option<Task<()>>,
}

/// The carry's candidate reads, apart from the watch they ask for.
#[derive(Default)]
struct Candidates {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    /// Nothing to change.
    Same,
    /// A new candidate: watch it under `w` and label it.
    Arm(HostId, WatchGen, Label),
    /// The watched candidate's title changed: relabel it, keeping the watch.
    Retitle(Label),
}

impl Candidates {
    /// The read for one frame of `drag` with the pointer at `point` at `now`. `query` runs
    /// only when a host event made the last read stale, or the pointer moved and
    /// `READ_INTERVAL` has passed since the last read; a move held back sets `due`.
    fn sample(
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

    /// A queued event: a host event for the watched generation makes the last read stale.
    fn event(&mut self, event: &PresenceEvent) {
        if let PresenceEvent::HostChanged { w, .. } = *event
            && self.targeting.watching() == Some(w)
        {
            self.stale();
        }
        deliver(&mut self.targeting, event);
    }

    /// The next sample reads, whatever the pointer and the interval say.
    fn stale(&mut self) {
        self.read_at = None;
    }

    fn end(&mut self) {
        self.targeting.end();
        self.read_at = None;
        self.due = None;
        self.title = None;
    }
}

impl Presence {
    /// One pull frame: once the neck has snapped, the window under the pointer is the
    /// candidate. A move the interval held back gets a trailing read at its deadline, so the
    /// label settles under a resting pointer even when no frame follows.
    pub(super) fn follow_carry(&mut self, cx: &mut Context<'_, Self>) {
        let Some(drag) = self.kumo.read(cx).carrying() else {
            return;
        };
        let point = match platform::cursor_position() {
            Ok(point) => point,
            Err(error) => {
                tracing::debug!(%error, "can't read the pointer mid-carry");
                return;
            }
        };
        self.read_carry(drag, point, cx);
        if let Some(due) = self.carry.candidates.due
            && self.carry.trailing.is_none()
        {
            self.carry.trailing = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(due.saturating_duration_since(Instant::now()))
                    .await;
                let _gone = this.update(cx, |this, cx| {
                    this.carry.trailing = None;
                    this.follow_carry(cx);
                });
            }));
        }
    }

    /// The candidate read for `drag` with the pointer at `point`, and what it asks for.
    fn read_carry(&mut self, drag: Generation, point: [i32; 2], cx: &mut Context<'_, Self>) {
        match self
            .carry
            .candidates
            .sample(drag, point, Instant::now(), query)
        {
            Sample::Same => {}
            Sample::Arm(id, w, title) => {
                self.carry.watch = None;
                match platform::watch_host(id, w, self.events.clone()) {
                    Ok(watch) => {
                        let label = label(cx.global::<Strings>(), &title);
                        self.carry.watch = Some((w, watch, label));
                    }
                    Err(error) => {
                        tracing::debug!(%error, "can't watch the window under the carry");
                        self.carry.candidates.targeting.watch_failed(w);
                    }
                }
            }
            Sample::Retitle(title) => {
                let text = label(cx.global::<Strings>(), &title);
                if let Some((.., shown)) = &mut self.carry.watch {
                    *shown = text;
                }
            }
        }
        self.sync_carry(cx);
    }

    /// A watch's `HostChanged`, or an Esc or Quit that ends a drop under way.
    pub(super) fn carry_event(&mut self, event: &PresenceEvent, cx: &mut Context<'_, Self>) {
        self.carry.candidates.event(event);
        self.sync_carry(cx);
    }

    /// The drag `drag` ended with `end`. A release after the snap runs the drop decision.
    pub(super) fn carry_ended(&mut self, drag: Generation, end: End, cx: &mut Context<'_, Self>) {
        let at = match end {
            End::Released(at) if self.carry.candidates.targeting.carries(drag) => {
                self.kumo.read(cx).screen_point(at)
            }
            _ => None,
        };
        if let Some(at) = at {
            self.carry.candidates.stale();
            self.read_carry(drag, at, cx);
            let from = self
                .dock
                .read(cx)
                .pull
                .as_ref()
                .and_then(|picture| picture.images[2].as_ref())
                .map_or(at, |(quad, _)| [quad.x, quad.y]);
            let placed = decide(
                self,
                drag,
                |this| &mut this.carry.candidates.targeting,
                || query(at),
                from,
                |this| this.drain(cx),
            );
            // shortcut: C2b-3's perch takes a published placement; until then every drop
            // returns to the line, so every ending creates nothing.
            diagnostics::record(cx, format_args!("carry drop perch={}", placed.is_some()));
        }
        self.end_carry(cx);
    }

    /// No carry: nothing is watched and no label shows.
    pub(super) fn end_carry(&mut self, cx: &mut Context<'_, Self>) {
        self.carry.trailing = None;
        self.carry.candidates.end();
        self.sync_carry(cx);
    }

    /// Handles every presence event already queued, through the usual handlers.
    fn drain(&mut self, cx: &mut Context<'_, Self>) {
        while let Ok(event) = self.events_in.try_recv() {
            self.carry_event(&event, cx);
            if !matches!(event, PresenceEvent::HostChanged { .. })
                && let Err(error) = self.on_event(event, cx)
            {
                tracing::error!(%error, "presence update failed");
            }
        }
    }

    /// Keeps the watch only for the generation Targeting still watches, and shows its label.
    fn sync_carry(&mut self, cx: &mut Context<'_, Self>) {
        let watching = self.carry.candidates.targeting.watching();
        if self
            .carry
            .watch
            .as_ref()
            .is_some_and(|(w, ..)| Some(*w) != watching)
        {
            self.carry.watch = None;
        }
        let label = self.carry.watch.as_ref().map(|(.., label)| label.clone());
        self.dock.update(cx, |dock, cx| {
            if dock.label != label {
                dock.label = label;
                cx.notify();
            }
        });
    }
}

/// The drop target under `point`: its caption, outside the caption buttons.
fn query(point: [i32; 2]) -> Option<WindowTarget> {
    platform::host_at(point)
        .as_ref()
        .and_then(|facts| drop_target(facts, point))
}

/// The drop label for a window titled `title`, or the fixed copy when it has none.
fn label(strings: &Strings, title: &Label) -> SharedString {
    if title.units().is_empty() {
        return strings.get(Text::PerchUntitled).into();
    }
    strings
        .perch_on(&String::from_utf16_lossy(title.units()))
        .into()
}

/// What a queued event does to the drop under way: Esc and Quit end it, and a host change
/// counts against its generation.
fn deliver(targeting: &mut Targeting, event: &PresenceEvent) {
    match *event {
        PresenceEvent::Escape | PresenceEvent::Quit => targeting.end(),
        PresenceEvent::HostChanged { w, change } => targeting.host_changed(w, change),
        _ => {}
    }
}

/// §10.4's drop on `Released` in `drag`: the first check, a drain, the second check, a
/// second drain and the publish, each once. `query` reads the window under the release point
/// now, `from` is the body's top-left, and `drain` handles every queued event.
fn decide<S>(
    owner: &mut S,
    drag: Generation,
    targeting: fn(&mut S) -> &mut Targeting,
    query: impl Fn() -> Option<WindowTarget>,
    from: [i32; 2],
    mut drain: impl FnMut(&mut S),
) -> Option<Placement> {
    if targeting(owner).first_check(drag, query().as_ref()) == Check::Return {
        return None;
    }
    drain(owner);
    let Drop::Ready(_, w) = targeting(owner).second_check(query().as_ref(), from, EXTENTS) else {
        return None;
    };
    drain(owner);
    targeting(owner).publish(drag, w)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::{Candidates, EXTENTS, READ_INTERVAL, Sample, decide, deliver, label};
    use crate::platform::PresenceEvent;
    use crate::platform::host::{HostChange, Label, PhysicalRect, WatchGen};
    use crate::presence::perch::target_fixtures::{HOST, OTHER, drag, host};
    use crate::presence::perch::{Placement, Targeting, WindowTarget};
    use crate::strings::Strings;

    /// The perch on `host(HOST, 0)` at `EXTENTS`: centred above the caption, with the card on
    /// the left, since on the right it would cover the caption buttons.
    const PERCH: Placement = Placement {
        body: PhysicalRect {
            left: 418,
            top: 144,
            right: 474,
            bottom: 200,
        },
        card: PhysicalRect {
            left: 34,
            top: 144,
            right: 414,
            bottom: 268,
        },
        above: true,
    };

    struct Owner {
        targeting: Targeting,
        /// What each drain finds queued, in order.
        queued: [Vec<PresenceEvent>; 2],
        drains: usize,
    }

    /// A drop in drag 1 over `HOST`, armed as `w`, with `queued` waiting at each drain.
    fn drop(
        queued: impl FnOnce(WatchGen) -> [Vec<PresenceEvent>; 2],
    ) -> (Option<Placement>, usize) {
        let mut targeting = Targeting::default();
        targeting.begin(drag(1));
        let (_, w) = targeting
            .candidate(drag(1), Some(&host(HOST, 0)))
            .expect("a new candidate");
        let mut owner = Owner {
            targeting,
            queued: queued(w),
            drains: 0,
        };
        let placed = decide(
            &mut owner,
            drag(1),
            |owner| &mut owner.targeting,
            || Some(host(HOST, 0)),
            [418, 144],
            |owner| {
                for event in std::mem::take(&mut owner.queued[owner.drains]) {
                    deliver(&mut owner.targeting, &event);
                }
                owner.drains += 1;
            },
        );
        (placed, owner.drains)
    }

    fn gone(w: WatchGen) -> PresenceEvent {
        PresenceEvent::HostChanged {
            w,
            change: HostChange::Gone,
        }
    }

    #[test]
    fn an_unchanged_host_publishes_its_placement_after_both_drains() {
        assert_eq!(EXTENTS.body, 56);
        assert_eq!(drop(|_| [vec![], vec![]]), (Some(PERCH), 2));
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
    fn esc_or_quit_in_either_drain_ends_the_drop() {
        for event in [PresenceEvent::Escape, PresenceEvent::Quit] {
            assert_eq!(
                drop(|_| [vec![event], vec![]]),
                (None, 1),
                "{event:?} first"
            );
            assert_eq!(drop(|_| [vec![], vec![event]]), (None, 2), "{event:?} last");
        }
    }

    #[test]
    fn a_stale_generation_in_the_final_drain_changes_nothing() {
        assert_eq!(
            drop(|w| [vec![], vec![gone(WatchGen(w.0 + 1))]]),
            (Some(PERCH), 2)
        );
    }

    /// The pointer, still, over `HOST`'s caption.
    const STILL: [i32; 2] = [400, 210];

    fn titled(id: crate::platform::host::HostId, title: &str) -> WindowTarget {
        WindowTarget {
            label: Label::new(&title.encode_utf16().collect::<Vec<_>>()),
            ..host(id, 0)
        }
    }

    fn moved(w: WatchGen) -> PresenceEvent {
        PresenceEvent::HostChanged {
            w,
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
    fn a_new_title_relabels_the_same_watch_and_an_empty_one_gets_the_fixed_copy() {
        let strings = Strings::english();
        let (mut candidates, w, mut now) = watching();
        let [alpha, beta, empty] = ["Alpha", "Beta", ""].map(|title| titled(HOST, title));
        assert_eq!(label(&strings, &alpha.label).as_ref(), "Drop on Alpha");
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
        assert_eq!(label(&strings, &beta.label).as_ref(), "Drop on Beta");
        assert_eq!(read(&mut candidates, beta), Sample::Same, "unchanged again");
        assert_eq!(read(&mut candidates, empty), Sample::Retitle(empty.label));
        assert_eq!(label(&strings, &empty.label).as_ref(), "Drop on this app");
        assert_eq!(
            candidates.targeting.watching(),
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
        candidates.event(&moved(WatchGen(w.0 + 1)));
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, unread),
            Sample::Same,
            "another generation's event"
        );
        candidates.event(&moved(w));
        assert_eq!(
            candidates.targeting.watching(),
            Some(w),
            "a move alone keeps it"
        );
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, |_| None),
            Sample::Same
        );
        assert_eq!(
            candidates.targeting.watching(),
            None,
            "moved away from the pointer"
        );
        candidates.event(&moved(w));
        assert_eq!(
            candidates.sample(drag(1), STILL, t0, unread),
            Sample::Same,
            "a late event for the candidate it left"
        );
    }

    #[test]
    fn a_host_gone_under_a_still_pointer_arms_the_window_now_under_it() {
        let (mut candidates, w, t0) = watching();
        candidates.event(&gone(w));
        assert_eq!(candidates.targeting.watching(), None);
        let beneath = titled(OTHER, "Beneath");
        let sample = candidates.sample(drag(1), STILL, t0, |_| Some(beneath));
        assert_eq!(sample, Sample::Arm(OTHER, WatchGen(w.0 + 1), beneath.label));
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
            (reads.get(), candidates.due),
            (0, Some(t0 + READ_INTERVAL)),
            "held back to the deadline"
        );
        assert_eq!(
            candidates.sample(drag(1), rest, t0 + READ_INTERVAL, read),
            Sample::Retitle(beta.label),
            "the label settles under the resting pointer"
        );
        assert_eq!((reads.get(), candidates.due), (1, None));
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
            (reads.get(), candidates.due),
            (1, None),
            "the release reads now"
        );
    }
}
