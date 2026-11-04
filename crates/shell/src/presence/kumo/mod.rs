//! Kumo's body, shaded live on the CPU into straight-alpha images that GPUI draws. Plain Rust:
//! no GPUI, no platform code. How it works: docs/development/kumo.md.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the peek draws the body; the pull and the goo arrive with OSC-179's C2"
    )
)]

mod coverage;
mod expression;
mod frame;
mod look;
mod math;
mod neck;
mod pull;
mod raster;
mod shape;

pub(crate) use expression::{Animator, Mood, Motion};
pub(crate) use frame::FrameState;
pub(crate) use look::Look;
pub(crate) use math::{Vec2, v2};
pub(crate) use neck::{Rect, SNAP_LEN};
pub(crate) use pull::{CARRY_WIDTH, Pull};
pub(crate) use raster::Renderer;
pub(crate) use shape::Silhouette;
