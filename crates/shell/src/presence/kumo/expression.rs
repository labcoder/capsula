//! The expression layer: a mood picks a target pose, and the animator eases toward it and
//! yields the frame the rasteriser draws. Kumo is still unless something engages it: entering
//! engagement blinks once, and only the voice moods move continuously while they last. With
//! reduced motion nothing moves at all.

use std::f32::consts::TAU;

use super::frame::{Eye, FrameState};
use super::math::{Rgb, Vec2, hash, hex, mix, v2};

/// What Kumo is doing, as far as its face shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mood {
    Idle,
    /// Attentive while the user talks: wide eyes, a head tilt, opening further as their voice
    /// rises.
    Listening,
    /// Pondering while a run works: half-lidded eyes looking up and away.
    Thinking,
    /// Talking: soft eyes that squeeze on each syllable of Kumo's own voice.
    Speaking,
}

impl Mood {
    /// Whether this mood moves continuously, driven by a voice level, while it lasts.
    fn live(self) -> bool {
        matches!(self, Self::Listening | Self::Speaking)
    }

    fn pose(self) -> Pose {
        let idle = Pose::IDLE;
        match self {
            Self::Idle => idle,
            Self::Listening => Pose {
                eye_w: 0.060,
                eye_h: 0.118,
                gaze: v2(0.0, 0.02),
                tilt: 0.024,
                level_open: 0.3,
                glow: hex(0x6F_E0_EE),
                glow_k: 0.55,
                level_glow: 1.1,
                level_squash: 0.012,
                orbit_r: 0.05,
                blink_every: 5.0,
                ..idle
            },
            Self::Thinking => Pose {
                eye_h: 0.09,
                lid: 0.3,
                gaze: v2(0.07, 0.05),
                wander: 0.04,
                glow: hex(0x8C_78_FF),
                glow_k: 1.0,
                orbit_r: 0.30,
                orbit_hz: 0.9,
                breathe_amp: 0.008,
                blink_every: 5.5,
                ..idle
            },
            Self::Speaking => Pose {
                eye_h: 0.104,
                eye_y: -0.07,
                level_open: -0.45,
                glow: hex(0x86_A8_FF),
                glow_k: 0.6,
                level_glow: 0.9,
                level_squash: 0.035,
                level_eyes: 0.018,
                orbit_r: 0.08,
                ..idle
            },
        }
    }
}

/// How much Kumo may move: the operating system's reduced-motion setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Motion {
    Full,
    /// Nothing moves: a mood shows as its static pose, with no blink.
    Reduced,
}

/// Every continuous parameter a mood sets. Moods differ only in these numbers, and a mood
/// change eases each of them, so any mood can follow any other.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pose {
    eye_w: f32,
    eye_h: f32,
    eye_y: f32,
    lid: f32,
    gaze: Vec2,
    /// Eye height offset by side, reading as a head tilt.
    tilt: f32,
    /// Slow side-to-side gaze drift, cloud units.
    wander: f32,
    /// Eye opening per unit of voice level: positive widens, negative squeezes.
    level_open: f32,
    glow: Rgb,
    glow_k: f32,
    orbit_r: f32,
    orbit_hz: f32,
    breathe_amp: f32,
    level_glow: f32,
    level_squash: f32,
    level_eyes: f32,
    /// Mean seconds between blinks while a voice mood moves.
    blink_every: f32,
}

impl Pose {
    const IDLE: Self = Self {
        eye_w: 0.055,
        eye_h: 0.098,
        eye_y: -0.08,
        lid: 0.0,
        gaze: v2(0.0, 0.0),
        tilt: 0.0,
        wander: 0.0,
        level_open: 0.0,
        glow: hex(0xB7_A4_FF),
        glow_k: 0.7,
        orbit_r: 0.12,
        orbit_hz: 0.12,
        breathe_amp: 0.016,
        level_glow: 0.0,
        level_squash: 0.0,
        level_eyes: 0.0,
        blink_every: 3.6,
    };

    fn lerp(&self, o: &Self, t: f32) -> Self {
        let m = |a: f32, b: f32| mix(a, b, t);
        Self {
            eye_w: m(self.eye_w, o.eye_w),
            eye_h: m(self.eye_h, o.eye_h),
            eye_y: m(self.eye_y, o.eye_y),
            lid: m(self.lid, o.lid),
            gaze: v2(m(self.gaze.x, o.gaze.x), m(self.gaze.y, o.gaze.y)),
            tilt: m(self.tilt, o.tilt),
            wander: m(self.wander, o.wander),
            level_open: m(self.level_open, o.level_open),
            glow: self.glow.lerp(o.glow, t),
            glow_k: m(self.glow_k, o.glow_k),
            orbit_r: m(self.orbit_r, o.orbit_r),
            orbit_hz: m(self.orbit_hz, o.orbit_hz),
            breathe_amp: m(self.breathe_amp, o.breathe_amp),
            level_glow: m(self.level_glow, o.level_glow),
            level_squash: m(self.level_squash, o.level_squash),
            level_eyes: m(self.level_eyes, o.level_eyes),
            blink_every: m(self.blink_every, o.blink_every),
        }
    }

    /// The largest difference that shows on screen, for deciding when a mood change is done.
    fn distance(&self, o: &Self) -> f32 {
        [
            self.eye_w - o.eye_w,
            self.eye_h - o.eye_h,
            self.eye_y - o.eye_y,
            self.lid - o.lid,
            self.gaze.x - o.gaze.x,
            self.gaze.y - o.gaze.y,
            self.tilt - o.tilt,
            self.glow_k - o.glow_k,
            self.glow.r - o.glow.r,
            self.glow.g - o.glow.g,
            self.glow.b - o.glow.b,
            self.orbit_r - o.orbit_r,
        ]
        .into_iter()
        .fold(0.0, |a, d| a.max(d.abs()))
    }
}

/// Seconds one blink takes, closing and opening.
const BLINK_SECS: f32 = 0.17;
/// How fast a mood change eases, per second.
const EASE_RATE: f32 = 7.0;
/// A mood change snaps to its target once no parameter is further than this from it.
const SETTLED: f32 = 1e-3;
/// The surface drift's clock wraps after this many seconds of continuous voice.
const DRIFT_WRAP: f64 = 4096.0;
/// Constant parts of every pose.
const EYE_GAP: f32 = 0.175;
const GLOW_R: f32 = 0.52;
const BREATHE_HZ: f32 = 0.28;

/// Drives Kumo's face over time. Deterministic for a given seed and tick sequence. The owner
/// asks for a frame only while [`Self::is_animating`] is true, and that frame is always drawn.
#[derive(Clone, Debug)]
pub(crate) struct Animator {
    mood: Mood,
    pose: Pose,
    reduced: bool,
    /// Seconds of motion, in f64 so frame-sized steps still count after days.
    t: f64,
    seed: u32,
    blinks: u32,
    /// How far into the blink under way, seconds.
    blink: Option<f32>,
    /// Seconds until the next blink while a voice mood moves.
    until_blink: f32,
    level: f32,
    /// A change since the last frame that the next frame shows.
    stale: bool,
}

impl Animator {
    pub(crate) fn new(mood: Mood, seed: u32) -> Self {
        Self {
            mood,
            pose: mood.pose(),
            reduced: false,
            t: 0.0,
            seed,
            blinks: 0,
            blink: None,
            until_blink: f32::INFINITY,
            level: 0.0,
            stale: false,
        }
    }

    /// Eases toward `mood`; a voice mood starts its blink schedule.
    pub(crate) fn set_mood(&mut self, mood: Mood) {
        if mood == self.mood {
            return;
        }
        self.mood = mood;
        self.stale = true;
        self.until_blink = if mood.live() {
            self.blink_gap()
        } else {
            f32::INFINITY
        };
    }

    /// Follows the operating system's reduced-motion setting: when it's reduced, Kumo shows
    /// its mood's pose and nothing moves, and a blink under way stops at once.
    pub(crate) fn set_motion(&mut self, motion: Motion) {
        let reduced = motion == Motion::Reduced;
        self.stale |= reduced != self.reduced;
        self.reduced = reduced;
        if reduced {
            self.blink = None;
        }
    }

    /// Hover, listening or speaking began: one blink, unless motion is reduced.
    pub(crate) fn engage(&mut self) {
        if !self.reduced {
            self.blink = Some(0.0);
        }
    }

    /// Engagement ended: a blink under way is cut, and the next frame opens the eyes.
    pub(crate) fn disengage(&mut self) {
        self.stale |= self.blink.take().is_some();
    }

    /// Whether the owner should draw another frame: true until the frame after the last
    /// change has been drawn, and false only when every later frame would be the same.
    pub(crate) fn is_animating(&self) -> bool {
        if self.stale || self.reduced {
            return self.stale;
        }
        self.blink.is_some() || self.mood.live() || self.pose != self.mood.pose()
    }

    /// Advances `dt` seconds with the current voice level (0..=1) and returns the frame. A
    /// negative or non-finite step counts as no time.
    pub(crate) fn tick(&mut self, dt: f32, level_in: f32) -> FrameState {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.stale = false;
        let target = self.mood.pose();
        if self.reduced {
            self.pose = target;
            return frame(&target, 0.0, 1.0, 0.0, 0.0);
        }
        self.t += f64::from(dt);
        let eased = self.pose.lerp(&target, 1.0 - (-dt * EASE_RATE).exp());
        self.pose = if eased.distance(&target) <= SETTLED {
            target
        } else {
            eased
        };
        let rate = if level_in > self.level { 30.0 } else { 9.0 };
        self.level += (level_in - self.level) * (1.0 - (-dt * rate).exp());
        let live = self.mood.live();
        if let Some(age) = &mut self.blink {
            *age += dt;
        } else if live {
            self.until_blink -= dt;
            if self.until_blink <= 0.0 {
                self.blink = Some(0.0);
            }
        }
        let open = self.blink_open();
        let moving = if live { 1.0 } else { 0.0 };
        frame(&self.pose, self.t, open, self.level * moving, moving)
    }

    /// How open the eyes are during a blink, ending the blink once it's over.
    fn blink_open(&mut self) -> f32 {
        let Some(age) = self.blink else {
            return 1.0;
        };
        let s = age / BLINK_SECS;
        if s >= 1.0 {
            self.blink = None;
            if self.mood.live() {
                self.until_blink = self.blink_gap();
            }
            return 1.0;
        }
        // Close fast, open a little slower.
        if s < 0.35 {
            1.0 - s / 0.35
        } else {
            ((s - 0.35) / 0.65).powf(0.8)
        }
    }

    /// Seconds until the next blink of a voice mood; one in six is a quick double.
    fn blink_gap(&mut self) -> f32 {
        self.blinks = self.blinks.wrapping_add(1);
        let r = hash(self.seed ^ self.blinks.wrapping_mul(0x9e37));
        if r < 0.16 {
            0.32
        } else {
            self.mood.pose().blink_every * (0.6 + 0.9 * r)
        }
    }
}

/// The angle, in radians, of something turning `hz` times a second at time `t`, wrapped to
/// one turn first so a long run keeps its precision.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a fraction of one turn fits f32"
)]
fn turn(t: f64, hz: f32) -> f32 {
    (t * f64::from(hz)).fract() as f32 * TAU
}

/// The frame for pose `p` at time `t`, eyes `open` (0..=1), voice `level` and `moving`
/// (1 while a voice mood moves, else 0, which freezes every time-driven term).
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the wrapped drift clock fits f32"
)]
fn frame(p: &Pose, t: f64, open: f32, level: f32, moving: f32) -> FrameState {
    let breathe = turn(t, BREATHE_HZ).sin() * p.breathe_amp * moving;
    let sy = 1.0 + breathe + level * p.level_squash;
    let sx = 1.0 - (sy - 1.0) * 0.6;
    let orbit = turn(t, p.orbit_hz) * moving;
    let glow_pos = v2(
        -0.05 + orbit.cos() * p.orbit_r,
        0.02 + orbit.sin() * p.orbit_r * 0.6,
    );
    let pulse = 1.0 + 0.08 * turn(t, 0.9).sin() * moving;
    let opening = (1.0 + level * p.level_open).max(0.35);
    let half_h = mix(p.eye_w * 0.28, p.eye_h * opening, open);
    let drift = turn(t, 0.35).sin() * p.wander * moving;
    let eye = |side: f32| Eye {
        center: v2(
            side * EYE_GAP + drift + p.gaze.x,
            p.eye_y + level * p.level_eyes - side * p.tilt + p.gaze.y,
        ),
        half_w: p.eye_w,
        half_h,
        smile: 0.0,
        lid: p.lid,
        droop: 0.0,
        side,
        sparkle: 0.0,
    };
    FrameState {
        body_scale: v2(sx, sy),
        body_lift: 0.0,
        eyes: [eye(-1.0), eye(1.0)],
        blush: 0.0,
        glow: p.glow,
        glow_k: (p.glow_k + level * p.level_glow) * pulse,
        glow_r: GLOW_R * (1.0 + level * 0.25 * p.level_glow),
        glow_pos,
        rim_k: 1.0 + level * p.level_glow * 0.6,
        // shortcut: the surface drift jumps once per wrap of continuous voice, unseen so far;
        // wrap each noise octave on its own if it ever shows.
        time: (t % DRIFT_WRAP) as f32 * moving,
    }
}

#[cfg(test)]
mod tests {
    use super::{Animator, BLINK_SECS, FrameState, Mood, Motion};
    use crate::presence::kumo::math::hex;

    const FRAME: f32 = 1.0 / 72.0;

    fn close(a: &FrameState, b: &FrameState) -> bool {
        let pairs = [
            (a.body_scale.x, b.body_scale.x),
            (a.body_scale.y, b.body_scale.y),
            (a.glow_pos.x, b.glow_pos.x),
            (a.glow_pos.y, b.glow_pos.y),
            (a.glow_k, b.glow_k),
            (a.glow_r, b.glow_r),
            (a.rim_k, b.rim_k),
            (a.time, b.time),
        ];
        let eyes = a.eyes.iter().zip(&b.eyes).flat_map(|(a, b)| {
            [
                (a.center.x, b.center.x),
                (a.center.y, b.center.y),
                (a.half_w, b.half_w),
                (a.half_h, b.half_h),
                (a.lid, b.lid),
                (a.side, b.side),
            ]
        });
        a.glow == b.glow
            && pairs
                .into_iter()
                .chain(eyes)
                .all(|(x, y)| (x - y).abs() < 1e-5)
    }

    /// The owner's protocol: ask for frames only while `is_animating` says so. Returns the last
    /// frame drawn and how many were drawn.
    fn drive(a: &mut Animator, last: FrameState, max: usize) -> (FrameState, usize) {
        let mut last = last;
        let mut frames = 0;
        while a.is_animating() {
            last = a.tick(FRAME, 0.0);
            frames += 1;
            assert!(frames <= max, "still animating after {max} frames");
        }
        (last, frames)
    }

    /// Ticks at 72 fps for `secs` and counts the blinks: runs of frames with the eyes closing.
    fn blinks_over(a: &mut Animator, secs: f32) -> usize {
        let open = FrameState::IDLE.eyes[0].half_h;
        let mut count = 0;
        let mut shut = false;
        let mut elapsed = 0.0;
        while elapsed < secs {
            let closing = a.tick(FRAME, 0.0).eyes[0].half_h < open - 1e-4;
            if closing && !shut {
                count += 1;
            }
            shut = closing;
            elapsed += FRAME;
        }
        count
    }

    /// Seconds of 72 fps frames until the eyes start closing for the next blink.
    fn until_blink(a: &mut Animator) -> f32 {
        let mut open = a.tick(FRAME, 0.0).eyes[0].half_h;
        let mut frames = 1_u16;
        loop {
            frames += 1;
            let h = a.tick(FRAME, 0.0).eyes[0].half_h;
            if h < open * 0.9 {
                return f32::from(frames - 1) * FRAME;
            }
            open = h;
            assert!(frames < 72 * 20, "no blink within 20 s");
        }
    }

    fn eye(f: &FrameState, i: usize) -> [f32; 5] {
        let e = f.eyes[i];
        [e.center.x, e.center.y, e.half_w, e.half_h, e.lid]
    }

    fn near(got: &[f32], want: &[f32]) -> bool {
        got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-5)
    }

    #[test]
    fn each_mood_holds_its_spike_pose_when_still() {
        // (mood, glow, glow strength, glow position, left eye, right eye): x, y, half width,
        // half height and lid, from the approved spike's pose table.
        let table = [
            (
                Mood::Idle,
                0xB7_A4_FF,
                0.7,
                [0.07, 0.02],
                [-0.175, -0.08, 0.055, 0.098, 0.0],
                [0.175, -0.08, 0.055, 0.098, 0.0],
            ),
            (
                Mood::Listening,
                0x6F_E0_EE,
                0.55,
                [0.0, 0.02],
                [-0.175, -0.036, 0.060, 0.118, 0.0],
                [0.175, -0.084, 0.060, 0.118, 0.0],
            ),
            (
                Mood::Thinking,
                0x8C_78_FF,
                1.0,
                [0.25, 0.02],
                [-0.105, -0.03, 0.055, 0.09, 0.3],
                [0.245, -0.03, 0.055, 0.09, 0.3],
            ),
            (
                Mood::Speaking,
                0x86_A8_FF,
                0.6,
                [0.03, 0.02],
                [-0.175, -0.07, 0.055, 0.104, 0.0],
                [0.175, -0.07, 0.055, 0.104, 0.0],
            ),
        ];
        for (mood, glow, glow_k, glow_pos, left, right) in table {
            let mut a = Animator::new(mood, 7);
            a.set_motion(Motion::Reduced);
            let f = a.tick(FRAME, 1.0);
            assert_eq!(f.glow, hex(glow), "{mood:?}");
            assert!(
                (f.glow_k - glow_k).abs() < 1e-5,
                "{mood:?} glow {}",
                f.glow_k
            );
            assert!(
                near(&[f.glow_pos.x, f.glow_pos.y], &glow_pos),
                "{mood:?} {:?}",
                f.glow_pos
            );
            assert!(
                near(&[f.body_scale.x, f.body_scale.y], &[1.0, 1.0]),
                "{mood:?}"
            );
            assert!(near(&eye(&f, 0), &left), "{mood:?} left {:?}", eye(&f, 0));
            assert!(near(&eye(&f, 1), &right), "{mood:?} right {:?}", eye(&f, 1));
            assert!(!a.is_animating(), "{mood:?} is still");
            assert_eq!(a.tick(FRAME, 1.0), f, "{mood:?} stays still, voice or not");
        }
    }

    #[test]
    fn an_idle_kumo_shows_the_still_pose_and_asks_for_nothing() {
        let mut a = Animator::new(Mood::Idle, 7);
        assert!(!a.is_animating(), "nothing to draw before anything happens");
        for _ in 0..60 {
            let f = a.tick(1.0, 0.0);
            assert!(close(&f, &FrameState::IDLE), "a minute in: {f:?}");
            assert!(!a.is_animating(), "no idle blink, breathing or drift");
        }
    }

    #[test]
    fn engaging_blinks_exactly_once_then_stops() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        assert!(a.is_animating());
        let (last, frames) = drive(&mut a, FrameState::IDLE, 72);
        assert_eq!(
            frames, 13,
            "one 0.17 s blink at 72 fps, then the open frame"
        );
        assert!(
            close(&last, &FrameState::IDLE),
            "it ends with the eyes open"
        );
        assert_eq!(blinks_over(&mut a, 30.0), 0, "and it stays still");
    }

    #[test]
    fn disengaging_mid_blink_still_draws_the_open_eyes() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        let mut last = FrameState::IDLE;
        for _ in 0..4 {
            last = a.tick(FRAME, 0.0);
        }
        assert!(last.eyes[0].half_h < 0.098, "mid-blink");
        a.disengage();
        let (last, frames) = drive(&mut a, last, 2);
        assert_eq!(frames, 1, "one more frame, to open the eyes");
        assert!(close(&last, &FrameState::IDLE), "eyes open again");
    }

    #[test]
    fn quick_disengage_and_reengage_blinks_once_from_the_start() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        a.tick(2.0 * FRAME, 0.0);
        a.disengage();
        a.engage();
        let (last, frames) = drive(&mut a, FrameState::IDLE, 72);
        assert_eq!(frames, 13);
        assert!(close(&last, &FrameState::IDLE));
    }

    #[test]
    fn turning_reduced_motion_on_mid_blink_draws_the_still_pose() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        let last = a.tick(4.0 * FRAME, 0.0);
        a.set_motion(Motion::Reduced);
        let (last, frames) = drive(&mut a, last, 2);
        assert_eq!(frames, 1);
        assert!(close(&last, &FrameState::IDLE));
        a.engage();
        assert!(!a.is_animating(), "reduced motion doesn't blink");
        a.set_motion(Motion::Full);
        a.engage();
        assert!(
            a.is_animating(),
            "full motion blinks again on the next engagement"
        );
    }

    #[test]
    fn a_mood_change_under_reduced_motion_draws_the_new_pose_once() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.set_motion(Motion::Reduced);
        a.set_mood(Mood::Thinking);
        let (last, frames) = drive(&mut a, FrameState::IDLE, 2);
        assert_eq!(frames, 1);
        assert!((last.eyes[0].lid - 0.3).abs() < 1e-6, "thinking's lid");
    }

    #[test]
    fn a_voice_mood_moves_its_body_and_light_and_blinks() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.set_mood(Mood::Speaking);
        let first = a.tick(FRAME, 0.5);
        assert!(a.is_animating());
        let later = (0..36).map(|_| a.tick(FRAME, 0.5)).last().expect("frames");
        assert!(
            (first.body_scale.y - later.body_scale.y).abs() > 1e-3,
            "the body squashes"
        );
        assert!(
            (first.glow_pos.x - later.glow_pos.x).abs() > 1e-3,
            "the light orbits"
        );
        assert!(blinks_over(&mut a, 20.0) >= 2, "a long turn still blinks");
    }

    #[test]
    fn switching_voice_moods_late_waits_a_whole_gap() {
        // Without a blink under way: well past every possible gap, so a deadline stored as a
        // bare gap would already have passed.
        let mut a = Animator::new(Mood::Idle, 7);
        a.set_mood(Mood::Speaking);
        a.tick(10.0, 0.0);
        a.tick(BLINK_SECS, 0.0);
        a.tick(BLINK_SECS, 0.0);
        a.set_mood(Mood::Listening);
        let wait = until_blink(&mut a);
        assert!(
            (0.32..=7.5 + FRAME).contains(&wait),
            "next blink {wait} s after the change"
        );
        // With a blink under way: the gap counts from its end.
        a.engage();
        a.tick(4.0 * FRAME, 0.0);
        a.set_mood(Mood::Speaking);
        a.tick(BLINK_SECS, 0.0);
        let wait = until_blink(&mut a);
        assert!(
            (0.32 - FRAME..=5.4 + FRAME).contains(&wait),
            "next blink {wait} s after it"
        );
    }

    #[test]
    fn a_mood_change_eases_then_settles_on_its_pose() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.set_mood(Mood::Thinking);
        let f = a.tick(0.1, 0.0);
        // 1 - e^(-0.7) of the way from lid 0 to 0.3.
        assert!(
            (f.eyes[0].lid - 0.151_02).abs() < 1e-4,
            "lid {}",
            f.eyes[0].lid
        );
        let (last, frames) = drive(&mut a, f, 144);
        assert!(frames > 36, "it eased rather than jumped: {frames} frames");
        let mut still = Animator::new(Mood::Thinking, 7);
        still.set_motion(Motion::Reduced);
        assert!(
            close(&last, &still.tick(FRAME, 0.0)),
            "it lands exactly on thinking"
        );
        assert_eq!(a.tick(FRAME, 0.0), last, "and the next frame is the same");
        a.engage();
        let (after, _) = drive(&mut a, last, 72);
        assert!(close(&after, &last), "a later blink resumes nothing");
    }

    #[test]
    fn a_mood_change_mid_blink_finishes_the_blink() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        let f = a.tick(4.0 * FRAME, 0.0);
        a.set_mood(Mood::Thinking);
        let (last, _) = drive(&mut a, f, 144);
        assert!(
            (last.eyes[0].half_h - 0.09).abs() < 1e-5,
            "eyes open, at thinking's height"
        );
    }

    #[test]
    fn odd_time_steps_never_leave_it_stuck() {
        let mut a = Animator::new(Mood::Idle, 7);
        let f = a.tick(524_288.0, 0.0);
        assert!(close(&f, &FrameState::IDLE));
        a.engage();
        let (last, frames) = drive(&mut a, f, 72);
        assert_eq!(frames, 13, "a blink after a huge step still ends");
        assert!(close(&last, &FrameState::IDLE));
        a.engage();
        let before = a.tick(FRAME, 0.0);
        assert_eq!(a.tick(0.0, 0.0), before, "a zero step changes nothing");
        assert_eq!(a.tick(f32::NAN, 0.0), before, "nor does a broken one");
        assert_eq!(a.tick(-1.0, 0.0), before, "nor a backwards one");
        let (_, frames) = drive(&mut a, before, 72);
        assert_eq!(frames, 12, "the blink goes on where it was");
    }
}
