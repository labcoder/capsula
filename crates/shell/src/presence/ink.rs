//! A screen hold's pointer line: where the pointer goes while the keys are down, read on every
//! frame and stroked in the drawing overlay as the picture will show it. The brain paints the
//! same path on the hold's image (`crates/brain/src/screen/ink.rs`) as one capsule per segment,
//! pink inside a white edge. The colours and widths here are that painter's, the widths mapped
//! back from image pixels to the display, and a test on each side pins the same numbers.
// shortcut: the line shows on the primary display only, so a hold released on another display sends that display with no line, and a line started while no overlay window is open is sized with the dock's display scale; upgrade with the overlay's per-display windows
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{Path, Pixels, Rgba, rgb};
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

/// The running screen hold's sampler, shared with the overlay's root, which paints its line.
pub(super) type Held = Rc<RefCell<Option<Sampler>>>;

/// Whether a hold's line has anything to paint.
pub(super) fn drawn(held: &Held) -> bool {
    held.borrow()
        .as_ref()
        .is_some_and(|sampler| !sampler.line.is_empty())
}

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

/// One screen hold's sampling: its recorder and the line as the overlay strokes it. The hold's
/// frames feed it (`Presence::line_frame`). It has no `Debug`: where the user pointed is
/// theirs.
pub(super) struct Sampler {
    gesture: GestureId,
    recorder: PathRecorder,
    /// The recorder's points and then the pointer's own place, as [`strokes`] takes them,
    /// copied only when either changes.
    line: Vec<Vec2>,
}

impl Sampler {
    pub(super) fn new(gesture: GestureId, recorder: PathRecorder) -> Self {
        Self {
            gesture,
            recorder,
            line: Vec::with_capacity(MAX_PATH_POINTS),
        }
    }

    pub(super) fn gesture(&self) -> GestureId {
        self.gesture
    }

    /// Records where the pointer is at `now`. True when the line changed and the overlay must
    /// repaint: a point was kept, or the pointer the line ends at moved.
    pub(super) fn sample(&mut self, point: PhysicalPoint, now: Instant) -> bool {
        let end = self.recorder.end();
        let changed = self.recorder.sample(point, now) || self.recorder.end() != end;
        if changed {
            let points = self.recorder.points();
            // The kept points trail the pointer by the easing; the line still reaches it.
            let end = self.recorder.end().filter(|end| points.last() != Some(end));
            self.line.clear();
            self.line
                .extend(points.iter().copied().chain(end).map(centre));
        }
        changed
    }

    /// The line so far, in physical pixels from the primary display's origin, which is the
    /// desktop's.
    pub(super) fn line(&self) -> &[Vec2] {
        &self.line
    }

    /// The physical size of the display the line is drawn on.
    pub(super) const fn display(&self) -> [u32; 2] {
        self.recorder.display().size
    }

    /// Stops sampling and gives the path a release carries, which ends at `pointer`: where
    /// the pointer was when the keys came up.
    pub(super) fn finish(self, pointer: PhysicalPoint) -> PointerPath {
        self.recorder.finish(pointer)
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
