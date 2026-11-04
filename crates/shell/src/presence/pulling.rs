//! A pull's lifetime around the drag owner and the pull's physics: what the presence adapter
//! keys on. Input arrives here in the peek's GPUI coordinates and becomes pull space at the
//! boundary, so growing or moving the overlay never shifts the grip. How it fits:
//! docs/development/kumo.md, "The pull".
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "C2a wires the pull to the peek's Kumo")
)]

use gpui_kit::{MouseButton, Pixels, Point};

use super::drag::{DragOwner, Event, Generation, Outcome};
use super::kumo::{Motion, Pull, Rect, Vec2, v2};

/// Logical pixels from the work area's origin, at the work area and scale fixed at the grab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PullSpace {
    /// The work area's origin, physical screen pixels.
    pub(crate) origin: [i32; 2],
    /// Physical pixels per logical pixel.
    pub(crate) scale: f32,
}

impl PullSpace {
    /// `at`, a GPUI point in a window whose client area starts at `window` in physical screen
    /// pixels.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "a screen position fits an f32 exactly enough"
    )]
    fn point(self, window: [i32; 2], at: Point<Pixels>) -> Vec2 {
        let scale = f64::from(self.scale);
        let axis = |window: i32, origin: i32, at: Pixels| {
            ((f64::from(window - origin) + f64::from(f32::from(at)) * scale) / scale) as f32
        };
        v2(
            axis(window[0], self.origin[0], at.x),
            axis(window[1], self.origin[1], at.y),
        )
    }
}

/// Where a grab happens, in pull space.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Grab {
    pub(crate) space: PullSpace,
    /// The line's centre.
    pub(crate) dock: Vec2,
    /// The body's centre in the peek.
    pub(crate) home: Vec2,
    /// The overlay the free body stays inside.
    pub(crate) bounds: Rect,
    pub(crate) motion: Motion,
}

/// Which lifetime is current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No pull: the body is in the peek.
    Idle,
    /// The drag: Esc cancels it and the peek stays open.
    Live(Generation),
    /// The drag has ended and the body is going home. The peek gets its body back, and the
    /// overlay shrinks, only once the resting picture is accepted.
    Returning,
}

#[derive(Default)]
pub(crate) struct Pulling {
    owner: DragOwner,
    /// With the space it was grabbed in, from the press until the resting picture is accepted.
    pull: Option<(Pull, PullSpace)>,
}

impl Pulling {
    pub(crate) fn phase(&self) -> Phase {
        match (self.owner.live(), &self.pull) {
            (Some(generation), _) => Phase::Live(generation),
            (None, Some(_)) => Phase::Returning,
            (None, None) => Phase::Idle,
        }
    }

    /// Grabs the body on `button`'s press, stamped `seq`, at `at` in the peek whose client
    /// area starts at `window`. A press while a pull exists starts nothing.
    pub(crate) fn press(
        &mut self,
        button: MouseButton,
        seq: u64,
        window: [i32; 2],
        at: Point<Pixels>,
        grab: Grab,
    ) -> Option<Generation> {
        if self.pull.is_some() {
            return None;
        }
        let generation = self.owner.press(button, seq)?;
        let press = grab.space.point(window, at);
        let pull = Pull::grab(grab.dock, grab.home, press, grab.bounds, grab.motion);
        self.pull = Some((pull, grab.space));
        Some(generation)
    }

    /// One event for the drag, with GPUI's points in the peek whose client area starts at
    /// `window`.
    pub(crate) fn handle(&mut self, event: Event, window: [i32; 2]) -> Outcome {
        let outcome = self.owner.handle(event);
        self.apply(outcome, window);
        outcome
    }

    /// See [`DragOwner::reconcile`]: only once the forwarding of the Up that took capture has
    /// returned.
    pub(crate) fn reconcile(&mut self) -> Outcome {
        let outcome = self.owner.reconcile();
        self.apply(outcome, [0, 0]);
        outcome
    }

    /// The work area or the scale is now `space`. A change during the drag loses it, since its
    /// points no longer mean what they did.
    pub(crate) fn rescale(&mut self, space: PullSpace) -> Outcome {
        match &self.pull {
            Some((_, grabbed)) if *grabbed != space => self.handle(Event::SourceClosed, [0, 0]),
            _ => Outcome::Ignored,
        }
    }

    /// Steps the pull `dt` seconds, if there is one.
    pub(crate) fn step(&mut self, dt: f32) -> Option<&Pull> {
        let (pull, _) = self.pull.as_mut()?;
        pull.update(dt);
        Some(pull)
    }

    /// The picture just accepted shows the body at rest at home: the pull is over. Physics
    /// alone never ends it. Returns whether it did.
    pub(crate) fn rested(&mut self) -> bool {
        let done = self.phase() == Phase::Returning
            && self.pull.as_ref().is_some_and(|(pull, _)| pull.settled());
        if done {
            self.pull = None;
        }
        done
    }

    fn apply(&mut self, outcome: Outcome, window: [i32; 2]) {
        let Some((pull, space)) = self.pull.as_mut() else {
            return;
        };
        match outcome {
            Outcome::Moved(_, at) => pull.aim(space.point(window, at)),
            Outcome::Ended(..) => pull.release(),
            Outcome::Ignored => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{MouseButton, Pixels, Point, point, px};

    use super::{Grab, Phase, PullSpace, Pulling};
    use crate::presence::drag::{End, Event, Generation, Outcome};
    use crate::presence::kumo::{Motion, Rect, Vec2, v2};

    const LEFT: MouseButton = MouseButton::Left;
    const FRAME: f32 = 1.0 / 72.0;
    /// A work area left of the primary monitor, at 125%.
    const SPACE: PullSpace = PullSpace {
        origin: [-1280, 0],
        scale: 1.25,
    };
    /// The peek's client origin, physical screen pixels.
    const PEEK: [i32; 2] = [-1000, 600];

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    fn grab(home: Vec2) -> Grab {
        Grab {
            space: SPACE,
            dock: home - v2(0.0, 34.0),
            home,
            bounds: Rect {
                origin: v2(0.0, 0.0),
                size: v2(1024.0, 1152.0),
            },
            motion: Motion::Full,
        }
    }

    fn pressed(seq: u64) -> (Pulling, Generation) {
        let mut pulling = Pulling::default();
        let generation = pulling
            .press(LEFT, seq, PEEK, at(10.0, 10.0), grab(v2(232.0, 488.0)))
            .expect("nothing pulled yet");
        (pulling, generation)
    }

    fn up() -> Event {
        Event::Up {
            button: LEFT,
            at: at(10.0, 10.0),
        }
    }

    fn lost(seq: u64) -> Event {
        Event::CaptureLost {
            seq,
            released: Some(LEFT),
        }
    }

    fn settle(pulling: &mut Pulling) {
        for _ in 0..144 {
            pulling.step(FRAME);
        }
    }

    #[test]
    fn a_peek_point_maps_to_the_work_area_at_its_scale() {
        // (-1000 + 10.5 * 1.25 + 1280) / 1.25 and (600 + 4.25 * 1.25) / 1.25, both exact.
        assert_eq!(SPACE.point(PEEK, at(10.5, 4.25)), v2(234.5, 484.25));
    }

    #[test]
    fn holding_still_where_it_was_pressed_keeps_the_body_home() {
        let home = v2(240.0, 500.0);
        let mut pulling = Pulling::default();
        pulling
            .press(LEFT, 1, PEEK, at(10.5, 4.25), grab(home))
            .expect("nothing pulled yet");
        let moved = pulling.handle(
            Event::Move {
                at: at(10.5, 4.25),
                pressed: Some(LEFT),
            },
            PEEK,
        );
        assert!(matches!(moved, Outcome::Moved(..)), "{moved:?}");
        let body = pulling.step(FRAME).expect("pulled").body();
        assert_eq!(body, home, "the grip is kept, so nothing jumps");
    }

    #[test]
    fn up_then_its_loss_then_reconcile_ends_once() {
        let (mut pulling, g) = pressed(1);
        assert_eq!(
            pulling.handle(up(), PEEK),
            Outcome::Ended(g, End::Released(at(10.0, 10.0)))
        );
        assert_eq!(pulling.handle(lost(2), PEEK), Outcome::Ignored);
        assert_eq!(pulling.reconcile(), Outcome::Ignored);
        assert_eq!(pulling.phase(), Phase::Returning);
    }

    #[test]
    fn a_loss_then_its_up_then_reconcile_ends_once_as_released() {
        let (mut pulling, g) = pressed(1);
        assert_eq!(pulling.handle(lost(2), PEEK), Outcome::Ignored);
        assert_eq!(pulling.phase(), Phase::Live(g), "the loss is held");
        assert_eq!(
            pulling.handle(up(), PEEK),
            Outcome::Ended(g, End::Released(at(10.0, 10.0)))
        );
        assert_eq!(pulling.reconcile(), Outcome::Ignored);
    }

    #[test]
    fn a_loss_with_no_up_is_lost_at_reconcile_and_the_next_press_is_a_new_drag() {
        let (mut pulling, g) = pressed(1);
        assert_eq!(pulling.handle(lost(2), PEEK), Outcome::Ignored);
        assert_eq!(pulling.reconcile(), Outcome::Ended(g, End::Lost));
        assert_eq!(pulling.phase(), Phase::Returning);
        settle(&mut pulling);
        assert!(pulling.rested());
        let next = pulling.press(LEFT, 3, PEEK, at(10.0, 10.0), grab(v2(232.0, 488.0)));
        assert!(next.is_some_and(|next| next != g), "{next:?}");
    }

    #[test]
    fn a_duplicate_loss_ends_the_drag_once() {
        let (mut pulling, g) = pressed(1);
        let outside = Event::CaptureLost {
            seq: 2,
            released: None,
        };
        assert_eq!(pulling.handle(outside, PEEK), Outcome::Ended(g, End::Lost));
        assert_eq!(pulling.handle(outside, PEEK), Outcome::Ignored);
    }

    #[test]
    fn only_an_accepted_resting_picture_ends_the_return() {
        let (mut pulling, g) = pressed(1);
        assert!(!pulling.rested(), "the drag is live");
        assert_eq!(pulling.phase(), Phase::Live(g));
        let away = Event::Move {
            at: at(10.0, 90.0),
            pressed: Some(LEFT),
        };
        pulling.handle(away, PEEK);
        for _ in 0..36 {
            pulling.step(FRAME);
        }
        pulling.handle(Event::Escape, PEEK);
        assert!(!pulling.rested(), "not home yet");
        settle(&mut pulling);
        assert!(
            pulling.step(FRAME).is_some_and(super::Pull::settled),
            "physics settled"
        );
        assert_eq!(
            pulling.phase(),
            Phase::Returning,
            "a failed upload keeps the return going"
        );
        assert!(pulling.rested());
        assert_eq!(pulling.phase(), Phase::Idle);
        assert!(pulling.step(FRAME).is_none());
    }

    #[test]
    fn a_press_while_the_body_returns_starts_nothing() {
        let (mut pulling, _) = pressed(1);
        pulling.handle(up(), PEEK);
        assert_eq!(
            pulling.press(LEFT, 2, PEEK, at(10.0, 10.0), grab(v2(232.0, 488.0))),
            None
        );
        assert_eq!(pulling.phase(), Phase::Returning);
    }

    #[test]
    fn a_new_scale_or_work_area_mid_drag_loses_it() {
        let (mut pulling, g) = pressed(1);
        assert_eq!(pulling.rescale(SPACE), Outcome::Ignored);
        assert_eq!(
            pulling.rescale(PullSpace {
                scale: 1.5,
                ..SPACE
            }),
            Outcome::Ended(g, End::Lost)
        );
        let (mut pulling, g) = pressed(1);
        assert_eq!(
            pulling.rescale(PullSpace {
                origin: [0, 0],
                ..SPACE
            }),
            Outcome::Ended(g, End::Lost)
        );
        assert_eq!(pulling.phase(), Phase::Returning);
    }
}
