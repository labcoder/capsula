//! What the rasteriser draws in one frame: the body's squash and lift, the two eyes and the
//! inner light. The animator produces these; `FrameState::IDLE` is the still pose at rest.

use super::math::{Rgb, Vec2, hex, v2};

/// One eye, in cloud units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Eye {
    pub(crate) center: Vec2,
    pub(crate) half_w: f32,
    pub(crate) half_h: f32,
    /// 0 is a pill, 1 an upturned arc (a smiling closed eye).
    pub(crate) smile: f32,
    /// How far the top lid comes down, 0..=1.
    pub(crate) lid: f32,
    /// Lid slope; positive lowers the outer corner (sad). Mirrored per eye.
    pub(crate) droop: f32,
    /// Which side is outward: -1 for the left eye, 1 for the right.
    pub(crate) side: f32,
    pub(crate) sparkle: f32,
}

/// Everything about one frame that isn't baked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameState {
    /// Body scale about the bottom centre (squash and stretch), and lift off the floor.
    pub(crate) body_scale: Vec2,
    pub(crate) body_lift: f32,
    pub(crate) eyes: [Eye; 2],
    pub(crate) blush: f32,
    /// The mood's inner light: its colour, strength, radius and where it sits.
    pub(crate) glow: Rgb,
    pub(crate) glow_k: f32,
    pub(crate) glow_r: f32,
    pub(crate) glow_pos: Vec2,
    pub(crate) rim_k: f32,
    /// Seconds of slow surface drift; 0 when nothing moves.
    pub(crate) time: f32,
}

impl FrameState {
    /// The idle mood, eyes open and nothing moving: what a resting Kumo shows.
    pub const IDLE: Self = Self {
        body_scale: v2(1.0, 1.0),
        body_lift: 0.0,
        eyes: [Self::idle_eye(-1.0), Self::idle_eye(1.0)],
        blush: 0.0,
        glow: hex(0xB7_A4_FF),
        glow_k: 0.7,
        glow_r: 0.52,
        glow_pos: v2(0.07, 0.02),
        rim_k: 1.0,
        time: 0.0,
    };

    const fn idle_eye(side: f32) -> Eye {
        Eye {
            center: v2(side * 0.175, -0.08),
            half_w: 0.055,
            half_h: 0.098,
            smile: 0.0,
            lid: 0.0,
            droop: 0.0,
            side,
            sparkle: 0.0,
        }
    }
}
