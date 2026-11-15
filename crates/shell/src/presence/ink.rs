//! A screen hold's pointer line: where the pointer goes while the keys are down, read every
//! 16 ms and stroked in the drawing overlay as the picture will show it. The brain paints the
//! same path on the hold's image (`crates/brain/src/screen/ink.rs`) as one capsule per segment,
//! pink inside a white edge. The colours and widths here are that painter's, the widths mapped
//! back from image pixels to the display, and a test on each side pins the same numbers.
// shortcut: the line shows on the primary display only, so a hold released on another display sends that display with no line; upgrade with the overlay's per-display windows
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the screen hold's activation is the first caller")
)]

use std::time::Duration;

use gpui_kit::{Context, Path, Pixels, Rgba, Task, rgb};
use kumochi_protocol::geometry::{MAX_PATH_POINTS, PhysicalPoint, PointerPath};
use kumochi_protocol::message::GestureId;
use kumochi_scene::geom::{Affine, Vec2};

use super::paint_paths::{BuildError, narrow};
use super::paint_stroke::Scratch;
use crate::input::path::PathRecorder;

/// The painter's colours.
const PINK: [u8; 3] = [255, 20, 147];
const WHITE: [u8; 3] = [255, 255, 255];
/// The painter's half-widths in image pixels: 3 pixels of pink inside a 1-pixel white edge.
const CORE: f32 = 1.5;
const EDGE: f32 = 2.5;
/// What the brain shrinks a display's picture to fit; it never enlarges one.
const IMAGE_LIMIT: [u32; 2] = [1280, 720];
/// How often a screen hold reads the pointer.
const PERIOD: Duration = Duration::from_millis(16);

/// One of the line's two paths and the colour it is painted in.
type Stroke = (Path<Pixels>, Rgba);

/// How many of a display's physical pixels one pixel of its picture covers.
fn shrink(display: [u32; 2]) -> f32 {
    let [across, down] = [0, 1].map(|axis| f64::from(display[axis]) / f64::from(IMAGE_LIMIT[axis]));
    narrow(across.max(down).max(1.0))
}

/// The line over `points` as two paths in the overlay's logical pixels, the white edge first
/// and the pink core to paint over it; none while there is no segment to draw. `points` are
/// physical pixels from the display's origin, `display` is its physical size and `scale` its
/// physical pixels per logical pixel. Each path is one allocation, handed to GPUI when painted.
///
/// # Errors
/// [`BuildError::Stroke`] when the line needs more vertices than the stroke scratch holds.
pub(super) fn strokes(
    scratch: &mut Scratch,
    points: &[Vec2],
    display: [u32; 2],
    scale: f32,
) -> Result<Option<[Stroke; 2]>, BuildError> {
    if scale.is_nan() || scale <= 0.0 {
        return Ok(None);
    }
    let shrink = shrink(display);
    let transform = Affine::pose(scale.recip(), 0.0, Vec2::default());
    let mut stroke = |half: f32, [red, green, blue]: [u8; 3]| {
        let path = scratch.capsules(points, 2.0 * half * shrink, transform)?;
        Ok(path.map(|path| (path, rgb(u32::from_be_bytes([0, red, green, blue])))))
    };
    let (Some(edge), Some(core)) = (stroke(EDGE, WHITE)?, stroke(CORE, PINK)?) else {
        return Ok(None);
    };
    Ok(Some([edge, core]))
}

/// One screen hold's sampling: its recorder, the line as the overlay strokes it, and the timer
/// that asks for the pointer. Dropping it stops the timer. It has no `Debug`: where the user
/// pointed is theirs.
pub(super) struct Sampler {
    gesture: GestureId,
    recorder: PathRecorder,
    /// The recorder's points as [`strokes`] takes them, copied only when the path changes.
    line: Vec<Vec2>,
    _timer: Task<()>,
}

impl Sampler {
    /// Starts sampling for `gesture`. Every 16 ms `tick` runs on the UI thread, in an update of
    /// the entity that starts this, until it answers false, that entity is gone or the sampler
    /// is dropped.
    pub(super) fn start<T: 'static>(
        gesture: GestureId,
        recorder: PathRecorder,
        cx: &mut Context<'_, T>,
        mut tick: impl FnMut(&mut T, &mut Context<'_, T>) -> bool + 'static,
    ) -> Self {
        let timer = cx.spawn(async move |owner, cx| {
            loop {
                cx.background_executor().timer(PERIOD).await;
                if !owner
                    .update(cx, |owner, cx| tick(owner, cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        });
        Self {
            gesture,
            recorder,
            line: Vec::with_capacity(MAX_PATH_POINTS + 1),
            _timer: timer,
        }
    }

    pub(super) fn gesture(&self) -> GestureId {
        self.gesture
    }

    /// Records where the pointer is. True when the line changed and the overlay must repaint.
    pub(super) fn sample(&mut self, point: PhysicalPoint) -> bool {
        let changed = self.recorder.sample(point);
        if changed {
            self.line.clear();
            self.line
                .extend(self.recorder.points().iter().copied().map(centre));
        }
        changed
    }

    /// The line so far, in physical pixels from the primary display's origin, which is the
    /// desktop's.
    pub(super) fn line(&self) -> &[Vec2] {
        &self.line
    }

    /// Stops sampling and gives the path a release carries.
    pub(super) fn finish(self) -> PointerPath {
        self.recorder.finish()
    }
}

/// The middle of a physical pixel, which is where the painter puts a point of the path.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "desktop coordinates are far inside the integers f32 holds exactly"
)]
fn centre(point: PhysicalPoint) -> Vec2 {
    let [x, y] = point.0;
    Vec2::new(x as f32 + 0.5, y as f32 + 0.5)
}

#[cfg(test)]
#[path = "ink_tests.rs"]
mod tests;
