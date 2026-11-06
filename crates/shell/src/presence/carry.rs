//! The carry after the snap: the app under the pointer, the watch on it, the drop label under
//! the body, and the drop decision on `Released`. How it fits: docs/development/kumo.md,
//! "The carry".

use gpui_kit::{Context, SharedString};

use super::drag::{End, Generation};
use super::perch::{Check, Drop, Extents, Placement, Targeting, WindowTarget, target_of};
use super::{Presence, diagnostics};
use crate::platform::host::WatchGen;
use crate::platform::{self, HostWatch, PresenceEvent};
use crate::strings::Strings;

/// The perched body's side and the task card, in logical pixels.
// shortcut: the card is the peek's size; C2b-3 sizes it from the text panel it opens.
const EXTENTS: Extents = Extents {
    body: 56,
    card: [380, 124],
};

/// What Presence holds of a carry. It has no `Debug`, since the label is screen text.
#[derive(Default)]
pub(super) struct Carry {
    targeting: Targeting,
    /// The armed generation's watch, and its drop label.
    watch: Option<(WatchGen, HostWatch, SharedString)>,
    /// Where the pointer was when the candidate was last read.
    read_at: Option<[i32; 2]>,
}

impl Presence {
    /// One pull frame: once the neck has snapped, the window under the pointer is the
    /// candidate, read again only when the pointer has moved.
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
        if !self.carry.targeting.carries(drag) {
            self.carry.targeting.begin(drag);
            self.carry.read_at = None;
        }
        if self.carry.read_at.replace(point) == Some(point) {
            return;
        }
        let target = query(point);
        if let Some((id, w)) = self.carry.targeting.candidate(drag, target.as_ref())
            && let Some(target) = target
        {
            self.carry.watch = None;
            match platform::watch_host(id, w, self.events.clone()) {
                Ok(watch) => {
                    let title = String::from_utf16_lossy(target.label.units());
                    let label = cx.global::<Strings>().perch_on(&title).into();
                    self.carry.watch = Some((w, watch, label));
                }
                Err(error) => {
                    tracing::debug!(%error, "can't watch the window under the carry");
                    self.carry.targeting.watch_failed(w);
                }
            }
        }
        self.sync_carry(cx);
    }

    /// A watch's `HostChanged`, or an Esc or Quit that ends a drop under way.
    pub(super) fn carry_event(&mut self, event: &PresenceEvent, cx: &mut Context<'_, Self>) {
        deliver(&mut self.carry.targeting, event);
        self.sync_carry(cx);
    }

    /// The drag `drag` ended with `end`. A release after the snap runs the drop decision.
    pub(super) fn carry_ended(&mut self, drag: Generation, end: End, cx: &mut Context<'_, Self>) {
        let at = match end {
            End::Released(at) if self.carry.targeting.carries(drag) => {
                self.kumo.read(cx).screen_point(at)
            }
            _ => None,
        };
        if let Some(at) = at {
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
                |this| &mut this.carry.targeting,
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
        self.carry.targeting.end();
        self.carry.read_at = None;
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
        let watching = self.carry.targeting.watching();
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

/// The perch target under `point`, if there's one.
fn query(point: [i32; 2]) -> Option<WindowTarget> {
    platform::host_at(point).as_ref().and_then(target_of)
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
    use super::{EXTENTS, decide, deliver};
    use crate::platform::PresenceEvent;
    use crate::platform::host::{HostChange, PhysicalRect, WatchGen};
    use crate::presence::perch::target_fixtures::{HOST, drag, host};
    use crate::presence::perch::{Placement, Targeting};

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
}
