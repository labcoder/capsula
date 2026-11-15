//! Kumo's body, shaded live on the CPU into straight-alpha images that GPUI draws. Plain Rust:
//! no GPUI, no platform code. How it works: docs/development/kumo.md.

mod coverage;
mod expression;
mod frame;
mod look;
mod math;
mod neck;
mod pull;
mod raster;
mod shape;

pub use coverage::{Placement as Quad, Span, device_spans, row_hulls};
pub use expression::{Animator, Mood, Motion};
pub use frame::FrameState;
pub use look::Look;
pub use math::{Vec2, v2};
pub use neck::{GooMismatch, GooShader, MAX_SCALE, Piece, Rect, SNAP_LEN};
pub use pull::{CARRY_WIDTH, Pull};
pub use raster::{Renderer, SizeMismatch};
pub use shape::Silhouette;
