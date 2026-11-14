//! A pull's lifetime around the drag owner and the pull's physics: what the presence adapter
//! keys on. Input arrives here in the peek's GPUI coordinates and becomes pull space at the
//! boundary, so growing or moving the overlay never shifts the grip. How it fits:
//! docs/development/kumo.md, "The pull".

use super::drag::{DragOwner, Event, Generation, Outcome};
use super::kumo::{CARRY_WIDTH, Motion, Pull, Rect, SNAP_LEN, Vec2, v2};
use crate::platform::input::Button;

/// Logical pixels from the work area's origin, at the work area and scale fixed at the grab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PullSpace {
    /// The work area, `[x, y, width, height]` in physical screen pixels. All of it is compared:
    /// a taskbar that moves an edge changes the clamp bounds even when the origin stays.
    pub(crate) area: [i32; 4],
    /// Physical pixels per logical pixel.
    pub(crate) scale: f32,
}

impl PullSpace {
    /// `at`, logical pixels in a window whose client area starts at `window` in physical screen
    /// pixels.
    pub(crate) fn point(self, window: [i32; 2], at: [f32; 2]) -> Vec2 {
        let x = f64::from(window[0] - self.area[0]) + f64::from(at[0]) * f64::from(self.scale);
        let y = f64::from(window[1] - self.area[1]) + f64::from(at[1]) * f64::from(self.scale);
        self.physical([x, y])
    }

    /// `p`, in pull space, in physical screen pixels.
    pub(crate) fn screen(self, p: Vec2) -> [f64; 2] {
        let scale = f64::from(self.scale);
        [
            f64::from(self.area[0]) + f64::from(p.x) * scale,
            f64::from(self.area[1]) + f64::from(p.y) * scale,
        ]
    }

    /// A physical screen rectangle in pull space.
    fn rect(self, [x, y, width, height]: [i32; 4]) -> Rect {
        let origin = self.physical([f64::from(x - self.area[0]), f64::from(y - self.area[1])]);
        let size = self.physical([f64::from(width), f64::from(height)]);
        Rect { origin, size }
    }

    /// Physical pixels from the work area's origin, in logical pixels.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "a screen position fits an f32 exactly enough"
    )]
    fn physical(self, [x, y]: [f64; 2]) -> Vec2 {
        let scale = f64::from(self.scale);
        v2((x / scale) as f32, (y / scale) as f32)
    }
}

/// Where a grab happens, in pull space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Grab {
    pub(crate) space: PullSpace,
    /// The line's centre.
    pub(crate) dock: Vec2,
    /// The body's centre in the peek.
    pub(crate) home: Vec2,
    /// Where the body starts: against the line on the peek's side, so the neck starts short.
    pub(crate) start: Vec2,
    /// The overlay the free body stays inside.
    pub(crate) bounds: Rect,
    pub(crate) motion: Motion,
}

impl Grab {
    /// A grab under the line painted at `line`, from the dock `slot`, both physical screen
    /// rectangles, of the body centred at `home` in pull space. The free body stays in the pull
    /// area: the slot grown on every side by `SNAP_LEN` and the carry width, within the work
    /// area.
    pub(crate) fn new(
        space: PullSpace,
        line: [i32; 4],
        slot: [i32; 4],
        home: Vec2,
        motion: Motion,
    ) -> Self {
        let line = space.rect(line);
        let slot = space.rect(slot);
        let reach = SNAP_LEN + CARRY_WIDTH;
        let area = space.rect(space.area);
        let lo = v2(
            (slot.origin.x - reach).max(area.origin.x),
            (slot.origin.y - reach).max(area.origin.y),
        );
        let hi = v2(
            (slot.origin.x + slot.size.x + reach).min(area.origin.x + area.size.x),
            (slot.origin.y + slot.size.y + reach).min(area.origin.y + area.size.y),
        );
        let dock = line.origin + v2(line.size.x * 0.5, line.size.y * 0.5);
        let side = if home.y < dock.y { -1.0 } else { 1.0 };
        Self {
            space,
            dock,
            home,
            start: dock + v2(0.0, side * CARRY_WIDTH * 0.5),
            bounds: Rect {
                origin: lo,
                size: hi - lo,
            },
            motion,
        }
    }
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
        button: Button,
        seq: u64,
        window: [i32; 2],
        at: [f32; 2],
        grab: Grab,
    ) -> Option<Generation> {
        if self.pull.is_some() {
            return None;
        }
        let generation = self.owner.press(button, seq)?;
        let press = grab.space.point(window, at);
        let pull = Pull::grab(
            grab.dock,
            grab.home,
            grab.start,
            press,
            grab.bounds,
            grab.motion,
        );
        self.pull = Some((pull, grab.space));
        Some(generation)
    }

    /// One event for the drag, with its points in logical pixels in the peek whose client area starts at
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

    /// The work area or the scale is now `space`. Any change during the drag, an edge of the
    /// work area included, loses it, since its points and clamp bounds no longer hold.
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

    /// The pull, with the space it was grabbed in, from the press until it has rested.
    pub(crate) fn pull(&self) -> Option<(&Pull, PullSpace)> {
        self.pull.as_ref().map(|(pull, space)| (pull, *space))
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
    use super::{Grab, Phase, PullSpace, Pulling};
    use crate::platform::input::Button;
    use crate::presence::drag::{End, Event, Generation, Outcome};
    use crate::presence::kumo::{Motion, Rect, Vec2, v2};

    const LEFT: Button = Button::Left;
    const FRAME: f32 = 1.0 / 72.0;
    /// A work area left of the primary monitor, at 125%.
    const SPACE: PullSpace = PullSpace {
        area: [-1280, 0, 1280, 1024],
        scale: 1.25,
    };
    /// The peek's client origin, physical screen pixels.
    const PEEK: [i32; 2] = [-1000, 600];

    fn at(x: f32, y: f32) -> [f32; 2] {
        [x, y]
    }

    fn grab(home: Vec2) -> Grab {
        Grab {
            space: SPACE,
            dock: home - v2(0.0, 34.0),
            home,
            start: home,
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
                area: [0, 0, 1280, 1024],
                ..SPACE
            }),
            Outcome::Ended(g, End::Lost)
        );
        assert_eq!(pulling.phase(), Phase::Returning);
        let (mut pulling, g) = pressed(1);
        assert_eq!(
            pulling.rescale(PullSpace {
                area: [-1280, 0, 1280, 984],
                ..SPACE
            }),
            Outcome::Ended(g, End::Lost),
            "a taskbar moved its bottom edge: same origin and scale"
        );
    }

    #[test]
    fn a_grab_takes_the_line_centre_and_the_pull_area_inside_the_work_area() {
        let home = v2(400.0, 60.0);
        let grab = Grab::new(
            SPACE,
            [-760, 18, 80, 4],
            [-800, 0, 160, 40],
            home,
            Motion::Full,
        );
        // The slot is (384, 0) 128 by 32 in pull space, grown by 144 + 84 on every side and
        // cut at the work area's top, 0. Its right edge, 740, stays inside the area's 1024.
        assert_eq!(
            grab,
            Grab {
                space: SPACE,
                dock: v2(448.0, 16.0),
                home,
                start: v2(448.0, 58.0),
                bounds: Rect {
                    origin: v2(156.0, 0.0),
                    size: v2(584.0, 260.0),
                },
                motion: Motion::Full,
            }
        );
    }

    #[test]
    fn a_grab_from_the_default_peek_holds_its_neck_until_the_pointer_moves() {
        use crate::presence::geometry::Position;
        use crate::presence::kumo::SNAP_LEN;
        use crate::presence::kumo_view::SIZE;
        use crate::presence::line;

        // The default dock at 100% on a 2560 by 1400 work area, under it the 380 by 124 peek
        // with Kumo inside its 12 px padding, as Presence lays them out.
        let space = PullSpace {
            area: [0, 0, 2560, 1400],
            scale: 1.0,
        };
        let layout = Position::default().layout(space.area, line::slot(1.0), [380, 124], 0);
        let paint = line::LineFrame::in_slot(layout.dock, 1.0).paint;
        let pad = 12.0 + SIZE * 0.5;
        let home = space.point([layout.panel[0], layout.panel[1]], [pad, pad]);
        let grab = Grab::new(space, paint, layout.dock, home, Motion::Full);
        assert!(
            (home - grab.dock).len() > SNAP_LEN,
            "home alone would start past the snap"
        );
        let mut pulling = Pulling::default();
        pulling
            .press(
                LEFT,
                1,
                [layout.panel[0], layout.panel[1]],
                [pad, pad],
                grab,
            )
            .expect("nothing pulled yet");
        let pull = pulling.step(FRAME).expect("the pull exists");
        assert!(
            !pull.neck().broken(),
            "the neck holds with the pointer still"
        );
        assert!(
            (pull.body() - grab.start).len() < 1e-3,
            "the body waits against the line"
        );
    }
}
