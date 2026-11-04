//! The pull: the body follows the pointer on a critically damped spring, grows from the peek's
//! size to the carry width as it leaves home, and drags the neck, which breaks past
//! [`SNAP_LEN`]. Once broken, the free body stays inside the dock overlay. Ending the drag
//! springs the body back home, which before C2b is also the invalid drop after the snap.
//! Positions are logical pixels of the pull surface, y down, and `body` is the body's centre.

use super::expression::Motion;
use super::math::{Vec2, mix, smoothstep, v2};
use super::neck::{Neck, Rect, SNAP_LEN};

/// The body's width in the peek, logical pixels.
pub(crate) const PEEK_WIDTH: f32 = 64.0;
/// The body's width once it's pulled out, logical pixels.
pub(crate) const CARRY_WIDTH: f32 = 84.0;
/// The spring's natural frequency, per second: the body catches up with the pointer, or gets
/// home, in about a fifth of a second, with no overshoot.
const SPRING: f32 = 28.0;
/// The largest step the spring takes at once, as the neck's, so a stall can't fling the body.
const MAX_STEP: f32 = 0.1;
/// Substeps per update, so the spring is stable at any frame rate.
const SUBSTEPS: u8 = 24;
/// How close to home, in logical pixels, and how slow, in logical pixels a second, a released
/// body must be to land exactly home.
const SETTLED: f32 = 0.05;

pub(crate) struct Pull {
    neck: Neck,
    home: Vec2,
    /// The pointer's offset from the body's centre at the press, kept so the body doesn't jump.
    grip: Vec2,
    /// The body's centre less `home`. Kept relative so the spring's small steps near home
    /// aren't rounded away against large screen coordinates.
    offset: Vec2,
    velocity: Vec2,
    target: Vec2,
    /// The dock overlay, which the free body stays inside.
    bounds: Rect,
    motion: Motion,
    released: bool,
}

impl Pull {
    /// A pull grabbed at `press` on the body at `home`, under the line's centre `dock`.
    pub(crate) fn grab(dock: Vec2, home: Vec2, press: Vec2, bounds: Rect, motion: Motion) -> Self {
        let mut neck = Neck::new(dock, CARRY_WIDTH);
        neck.set_motion(motion);
        Self {
            neck,
            home,
            grip: press - home,
            offset: v2(0.0, 0.0),
            velocity: v2(0.0, 0.0),
            target: home,
            bounds,
            motion,
            released: false,
        }
    }

    /// Follows the pointer at `pointer`. After the release the body is going home, so this
    /// does nothing.
    pub(crate) fn aim(&mut self, pointer: Vec2) {
        if !self.released {
            self.target = pointer - self.grip;
        }
    }

    /// Ends the drag, whatever ended it: the body goes home.
    pub(crate) fn release(&mut self) {
        self.released = true;
        self.target = self.home;
    }

    /// Steps the body `dt` seconds toward its target, then the neck. A negative or
    /// non-finite step counts as none, and a long one as [`MAX_STEP`].
    pub(crate) fn update(&mut self, dt: f32) {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, MAX_STEP)
        } else {
            0.0
        };
        let half = CARRY_WIDTH * 0.5;
        let (lo, hi) = (self.bounds.origin, self.bounds.origin + self.bounds.size);
        let inside = |p: Vec2| {
            v2(
                p.x.clamp(lo.x + half, (hi.x - half).max(lo.x + half)),
                p.y.clamp(lo.y + half, (hi.y - half).max(lo.y + half)),
            )
        };
        let free = |neck: &Neck, released: bool| neck.broken() && !released;
        let target = if free(&self.neck, self.released) {
            inside(self.target)
        } else {
            self.target
        } - self.home;
        if self.motion == Motion::Reduced {
            self.offset = target;
            self.velocity = v2(0.0, 0.0);
        } else if dt > 0.0 {
            let h = dt / f32::from(SUBSTEPS);
            for _ in 0..SUBSTEPS {
                let pull = target - self.offset;
                let acc = v2(
                    SPRING * SPRING * pull.x - 2.0 * SPRING * self.velocity.x,
                    SPRING * SPRING * pull.y - 2.0 * SPRING * self.velocity.y,
                );
                self.velocity = self.velocity + v2(acc.x * h, acc.y * h);
                self.offset = self.offset + v2(self.velocity.x * h, self.velocity.y * h);
            }
        }
        // Slow and close is rest, not a fast pass through home, so the pull can go idle.
        if self.released && self.offset.len() <= SETTLED && self.velocity.len() <= SETTLED {
            self.offset = v2(0.0, 0.0);
            self.velocity = v2(0.0, 0.0);
        }
        // The snap is decided on the body's own path.
        self.neck.update(dt, self.body());
        // The step that breaks the neck may have taken the body past the overlay's edge. The
        // neck then takes the clamped body without another step, so the stub stays on it.
        if free(&self.neck, self.released) {
            self.offset = inside(self.body()) - self.home;
            self.neck.update(0.0, self.body());
        }
    }

    pub(crate) fn body(&self) -> Vec2 {
        self.home + self.offset
    }

    /// The body's width: the peek's at home, the carry width from [`SNAP_LEN`] away.
    pub(crate) fn width(&self) -> f32 {
        let out = smoothstep(0.0, SNAP_LEN, self.offset.len());
        mix(PEEK_WIDTH, CARRY_WIDTH, out)
    }

    pub(crate) fn neck(&self) -> &Neck {
        &self.neck
    }

    /// Whether the released body is exactly home and still, so the pull can end.
    pub(crate) fn settled(&self) -> bool {
        self.released && self.offset == v2(0.0, 0.0) && self.velocity == v2(0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{CARRY_WIDTH, MAX_STEP, PEEK_WIDTH, Pull};
    use crate::presence::kumo::expression::Motion;
    use crate::presence::kumo::math::{Vec2, v2};
    use crate::presence::kumo::neck::{Piece, Rect, SNAP_LEN};

    const FRAME: f32 = 1.0 / 72.0;
    const DOCK: Vec2 = v2(400.0, 10.0);
    const HOME: Vec2 = v2(400.0, 44.0);
    const BOUNDS: Rect = Rect {
        origin: v2(0.0, 0.0),
        size: v2(800.0, 300.0),
    };

    fn grab(motion: Motion) -> Pull {
        Pull::grab(DOCK, HOME, HOME, BOUNDS, motion)
    }

    fn run(pull: &mut Pull, frames: usize) {
        for _ in 0..frames {
            pull.update(FRAME);
        }
    }

    fn near(a: Vec2, b: Vec2, by: f32) -> bool {
        (a - b).len() <= by
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the width's ends are exact")]
    fn a_grab_starts_at_home_at_the_peek_width() {
        let pull = grab(Motion::Full);
        assert_eq!(pull.body(), HOME);
        assert_eq!(pull.width(), PEEK_WIDTH);
        assert!(!pull.settled(), "a held pull isn't settled");
        assert!(!pull.neck().broken());
    }

    #[test]
    fn the_body_follows_the_pointer_from_where_it_was_pressed() {
        let mut pull = Pull::grab(DOCK, HOME, HOME + v2(10.0, 4.0), BOUNDS, Motion::Full);
        pull.aim(v2(450.0, 104.0));
        run(&mut pull, 72);
        assert!(
            near(pull.body(), v2(440.0, 100.0), 0.01),
            "{:?}",
            pull.body()
        );
        assert!(!pull.neck().broken(), "about 76 px out, under the snap");
    }

    #[test]
    fn a_release_before_the_snap_springs_home_and_shrinks_back() {
        let mut pull = grab(Motion::Full);
        pull.aim(HOME + v2(0.0, 100.0));
        run(&mut pull, 36);
        assert!(pull.width() > PEEK_WIDTH + 5.0, "{}", pull.width());
        pull.release();
        pull.aim(v2(0.0, 0.0));
        run(&mut pull, 72);
        assert!(pull.settled(), "{:?}", pull.body());
        assert!(!pull.neck().broken());
        assert!((pull.width() - PEEK_WIDTH).abs() < 1e-3, "{}", pull.width());
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the width's ends are exact")]
    fn past_the_snap_the_neck_breaks_and_the_free_body_stays_in_the_overlay() {
        let mut pull = grab(Motion::Full);
        pull.aim(v2(400.0, 900.0));
        run(&mut pull, 72);
        assert!(pull.neck().broken());
        let bottom = BOUNDS.origin.y + BOUNDS.size.y - CARRY_WIDTH * 0.5;
        assert!(
            near(pull.body(), v2(400.0, bottom), 0.01),
            "{:?}",
            pull.body()
        );
        assert_eq!(pull.width(), CARRY_WIDTH, "past SNAP_LEN from home");
        assert!(bottom - HOME.y > SNAP_LEN);
        pull.release();
        run(&mut pull, 72);
        assert!(
            pull.settled(),
            "the invalid drop goes home: {:?}",
            pull.body()
        );
    }

    #[test]
    fn the_stub_stays_on_the_clamped_body_from_the_breaking_step() {
        let mut pull = grab(Motion::Full);
        pull.aim(v2(400.0, 900.0));
        for dt in [0.1, FRAME, FRAME] {
            pull.update(dt);
            assert!(pull.neck().broken());
            assert_eq!(pull.body(), v2(400.0, 300.0 - CARRY_WIDTH * 0.5));
            // The stub's last capsule ends where the neck met the body's top.
            let top = pull.body() - v2(0.0, CARRY_WIDTH * 0.12);
            let r = pull
                .neck()
                .bounds(Piece::Stub)
                .expect("a stub while recoiling");
            let (lo, hi) = (r.origin, r.origin + r.size);
            assert!(
                (lo.x..=hi.x).contains(&top.x) && (lo.y..=hi.y).contains(&top.y),
                "{r:?} {top:?}"
            );
        }
    }

    #[test]
    fn a_release_lands_exactly_home_at_screen_coordinates() {
        let screen = Rect {
            origin: v2(0.0, 0.0),
            size: v2(2560.0, 1440.0),
        };
        // Down and aside, sideways, and far enough to break.
        let pulls = [
            (v2(60.0, 90.0), false),
            (v2(60.0, 0.0), false),
            (v2(-100.0, 200.0), true),
        ];
        for home in [v2(1280.0, 1200.0), v2(2400.0, 1200.0)] {
            for rate in [72_u16, 144] {
                for (reach, breaks) in pulls {
                    let dt = 1.0 / f32::from(rate);
                    let second = usize::from(rate);
                    let mut pull =
                        Pull::grab(home - v2(0.0, 34.0), home, home, screen, Motion::Full);
                    pull.aim(home + reach);
                    for _ in 0..second {
                        pull.update(dt);
                    }
                    assert_eq!(pull.neck().broken(), breaks, "{home:?} {rate} {reach:?}");
                    pull.release();
                    let home_after = (1..=second).find(|_| {
                        pull.update(dt);
                        pull.settled()
                    });
                    assert!(home_after.is_some(), "{home:?} {rate} {reach:?}");
                    assert_eq!(pull.body(), home);
                }
            }
        }
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "halfway to SNAP_LEN is exactly halfway")]
    fn halfway_to_the_snap_the_body_is_halfway_to_the_carry_width() {
        let mut pull = grab(Motion::Reduced);
        pull.aim(HOME + v2(0.0, 72.0));
        pull.update(FRAME);
        assert_eq!(pull.width(), 74.0);
    }

    #[test]
    fn an_overlay_narrower_than_the_body_holds_it_at_its_corner() {
        let tight = Rect {
            origin: v2(380.0, 0.0),
            size: v2(40.0, 30.0),
        };
        let mut pull = Pull::grab(DOCK, HOME, HOME, tight, Motion::Reduced);
        pull.aim(v2(400.0, 900.0));
        pull.update(FRAME);
        assert!(pull.neck().broken());
        let half = CARRY_WIDTH * 0.5;
        assert_eq!(pull.body(), v2(380.0 + half, half));
    }

    #[test]
    fn reduced_motion_jumps_but_still_snaps_and_goes_home() {
        let mut pull = grab(Motion::Reduced);
        pull.aim(HOME + v2(30.0, 60.0));
        pull.update(FRAME);
        assert_eq!(pull.body(), HOME + v2(30.0, 60.0));
        pull.aim(v2(400.0, 900.0));
        pull.update(FRAME);
        assert!(pull.neck().broken(), "the snap is still decided");
        assert_eq!(pull.body(), v2(400.0, 300.0 - CARRY_WIDTH * 0.5));
        pull.release();
        pull.update(FRAME);
        assert_eq!(pull.body(), HOME);
        assert!(pull.settled());
    }

    #[test]
    fn a_bad_or_long_step_is_none_or_the_largest_step() {
        let stepped = |dt: f32| {
            let mut pull = grab(Motion::Full);
            pull.aim(HOME + v2(0.0, 80.0));
            pull.update(dt);
            pull.body()
        };
        for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0] {
            assert_eq!(stepped(dt), HOME, "{dt}");
        }
        assert_eq!(stepped(1e9), stepped(MAX_STEP));
    }

    #[test]
    fn the_spring_moves_alike_at_72_and_144_frames_a_second() {
        let at = |rate: u16, frames: usize| {
            let mut pull = grab(Motion::Full);
            pull.aim(HOME + v2(60.0, 90.0));
            for _ in 0..frames {
                pull.update(1.0 / f32::from(rate));
            }
            pull.body()
        };
        let (slow, fast) = (at(72, 9), at(144, 18));
        assert!(near(slow, fast, 0.5), "{slow:?} {fast:?}");
    }
}
