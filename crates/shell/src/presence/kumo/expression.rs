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
        ]
        .into_iter()
        .fold(0.0, |a, d| a.max(d.abs()))
    }
}

/// Seconds one blink takes, closing and opening.
const BLINK_SECS: f32 = 0.17;
/// How fast a mood change eases, per second.
const EASE_RATE: f32 = 7.0;
/// A mood change is done once no parameter is further than this from its target.
const SETTLED: f32 = 1e-3;
/// Constant parts of every pose.
const EYE_GAP: f32 = 0.175;
const GLOW_R: f32 = 0.52;
const BREATHE_HZ: f32 = 0.28;

/// Drives Kumo's face over time. Deterministic for a given seed and tick sequence; the owner
/// asks for a frame only while [`Self::is_animating`] says the next one differs.
#[derive(Clone, Debug)]
pub(crate) struct Animator {
    mood: Mood,
    pose: Pose,
    reduced: bool,
    t: f32,
    seed: u32,
    blinks: u32,
    blink_at: Option<f32>,
    /// When the next blink falls due while a voice mood moves.
    next_blink: f32,
    level: f32,
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
            blink_at: None,
            next_blink: f32::INFINITY,
            level: 0.0,
        }
    }

    pub(crate) fn mood(&self) -> Mood {
        self.mood
    }

    /// Eases toward `mood`; a voice mood starts its blink schedule.
    pub(crate) fn set_mood(&mut self, mood: Mood) {
        if mood == self.mood {
            return;
        }
        self.mood = mood;
        self.next_blink = if mood.live() {
            self.blink_gap()
        } else {
            f32::INFINITY
        };
    }

    /// Follows the operating system's reduced-motion setting: when it's reduced, Kumo shows
    /// its mood's pose and nothing moves, and a blink under way stops at once.
    pub(crate) fn set_motion(&mut self, motion: Motion) {
        self.reduced = motion == Motion::Reduced;
        if self.reduced {
            self.blink_at = None;
        }
    }

    /// Hover, listening or speaking began: one blink, unless motion is reduced.
    pub(crate) fn engage(&mut self) {
        if !self.reduced {
            self.blink_at = Some(self.t);
        }
    }

    /// Engagement ended: a blink under way is cut, so nothing is left animating.
    pub(crate) fn disengage(&mut self) {
        self.blink_at = None;
    }

    /// Whether the next frame differs from this one.
    pub(crate) fn is_animating(&self) -> bool {
        if self.reduced {
            return false;
        }
        self.blink_at.is_some()
            || self.mood.live()
            || self.pose.distance(&self.mood.pose()) > SETTLED
    }

    /// Advances `dt` seconds with the current voice level (0..=1) and returns the frame.
    pub(crate) fn tick(&mut self, dt: f32, level_in: f32) -> FrameState {
        let target = self.mood.pose();
        if self.reduced {
            self.pose = target;
            return frame(&target, 0.0, 1.0, 0.0, 0.0);
        }
        self.t += dt;
        self.pose = self.pose.lerp(&target, 1.0 - (-dt * EASE_RATE).exp());
        let rate = if level_in > self.level { 30.0 } else { 9.0 };
        self.level += (level_in - self.level) * (1.0 - (-dt * rate).exp());
        let live = self.mood.live();
        if live && self.blink_at.is_none() && self.t >= self.next_blink {
            self.blink_at = Some(self.t);
        }
        let open = self.blink_open();
        let moving = if live { 1.0 } else { 0.0 };
        frame(&self.pose, self.t, open, self.level * moving, moving)
    }

    /// How open the eyes are during a blink, ending the blink once it's over.
    fn blink_open(&mut self) -> f32 {
        let Some(start) = self.blink_at else {
            return 1.0;
        };
        let s = (self.t - start) / BLINK_SECS;
        if s >= 1.0 {
            self.blink_at = None;
            if self.mood.live() {
                self.next_blink = self.t + self.blink_gap();
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

/// The frame for pose `p` at time `t`, eyes `open` (0..=1), voice `level` and `moving`
/// (1 while a voice mood moves, else 0, which freezes every time-driven term).
fn frame(p: &Pose, t: f32, open: f32, level: f32, moving: f32) -> FrameState {
    let breathe = (t * BREATHE_HZ * TAU).sin() * p.breathe_amp * moving;
    let sy = 1.0 + breathe + level * p.level_squash;
    let sx = 1.0 - (sy - 1.0) * 0.6;
    let orbit = t * p.orbit_hz * TAU * moving;
    let glow_pos = v2(
        -0.05 + orbit.cos() * p.orbit_r,
        0.02 + orbit.sin() * p.orbit_r * 0.6,
    );
    let pulse = 1.0 + 0.08 * (t * 0.9 * TAU).sin() * moving;
    let opening = (1.0 + level * p.level_open).max(0.35);
    let half_h = mix(p.eye_w * 0.28, p.eye_h * opening, open);
    let drift = (t * 0.35 * TAU).sin() * p.wander * moving;
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
        time: t * moving,
    }
}

#[cfg(test)]
mod tests {
    use super::{Animator, BLINK_SECS, FrameState, Mood, Motion};

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
                .all(|(x, y)| (x - y).abs() < 1e-6)
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
        assert_eq!(blinks_over(&mut a, BLINK_SECS + 2.0 * FRAME), 1);
        assert!(!a.is_animating(), "settled once the blink is over");
        assert_eq!(blinks_over(&mut a, 30.0), 0, "and it stays still");
    }

    #[test]
    fn disengaging_mid_blink_cuts_it() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        for _ in 0..4 {
            a.tick(FRAME, 0.0);
        }
        a.disengage();
        assert!(!a.is_animating());
        assert!(
            close(&a.tick(FRAME, 0.0), &FrameState::IDLE),
            "eyes open again"
        );
    }

    #[test]
    fn reduced_motion_is_a_static_pose_with_no_blink() {
        let mut a = Animator::new(Mood::Listening, 7);
        a.set_motion(Motion::Reduced);
        a.engage();
        assert!(!a.is_animating());
        let first = a.tick(FRAME, 1.0);
        for _ in 0..100 {
            assert_eq!(
                a.tick(FRAME, 1.0),
                first,
                "nothing moves, not even with a voice"
            );
        }
    }

    #[test]
    fn turning_reduced_motion_on_mid_blink_stops_it_at_once() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.engage();
        a.tick(4.0 * FRAME, 0.0);
        a.set_motion(Motion::Reduced);
        assert!(!a.is_animating());
        assert!(close(&a.tick(FRAME, 0.0), &FrameState::IDLE));
        a.set_motion(Motion::Full);
        a.engage();
        assert!(
            a.is_animating(),
            "full motion blinks again on the next engagement"
        );
    }

    #[test]
    fn a_voice_mood_moves_while_it_lasts_and_blinks_on_its_schedule() {
        let mut a = Animator::new(Mood::Idle, 7);
        a.set_mood(Mood::Speaking);
        let first = a.tick(FRAME, 0.5);
        assert!(a.is_animating());
        let later = (0..36).map(|_| a.tick(FRAME, 0.5)).last().expect("frames");
        assert_ne!(first, later, "the body breathes and the light drifts");
        assert!(blinks_over(&mut a, 20.0) >= 2, "a long turn still blinks");
    }

    #[test]
    fn a_mood_change_eases_then_settles() {
        let mut a = Animator::new(Mood::Speaking, 7);
        a.set_mood(Mood::Thinking);
        assert!(a.is_animating(), "easing toward thinking");
        let mut frames = 0;
        while a.is_animating() {
            a.tick(FRAME, 0.0);
            frames += 1;
            assert!(frames < 144, "settles within two seconds");
        }
        assert_eq!(a.mood(), Mood::Thinking);
        assert!(frames > 36, "it eased rather than jumped: {frames} frames");
    }
}
