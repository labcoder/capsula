//! The pointer's path, painted on a hold's resampled RGB image before it's encoded.

use std::fmt;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};
use serde::{Deserialize, Serialize};

const PINK: [u8; 3] = [255, 20, 147];
const WHITE: [u8; 3] = [255, 255, 255];
/// Half the pink core's width: the line is 3 pixels of pink.
const CORE: f64 = 1.5;
/// Half the whole line's width: a 1-pixel white edge on each side of the core.
const EDGE: f64 = 2.5;
/// The side of the cells the path's points are counted in, in image pixels. Four of them make the
/// square a circle drawn around a word in a 1280-pixel picture fits in [guess].
const GATHER: u32 = 48;

/// The painted pixels' bounding rectangle in image pixels: left and top included, right and
/// bottom not, and the pixel where the path gathers most. Where the user pointed is theirs, so
/// `Debug` prints none of it.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InkBounds {
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) right: u32,
    pub(crate) bottom: u32,
    /// Where the user circled, went back and forth or slowed down: kept points come at most
    /// every 16 ms while the pointer moves, so they crowd where it spent its time.
    pub(crate) focus: [u32; 2],
}

impl fmt::Debug for InkBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InkBounds(***)")
    }
}

/// Paints `path`, in physical desktop pixels, on `rgb`, an image of `size` showing `region`, and
/// returns what it painted and where the path gathers. A segment that leaves the image is painted
/// up to its edge; a path of fewer than two points paints nothing, and one with no point on the
/// image returns none, though it may paint.
pub(crate) fn paint(
    rgb: &mut [u8],
    size: [u32; 2],
    region: PhysicalRect,
    path: &[PhysicalPoint],
) -> Option<InkBounds> {
    // A physical pixel's centre maps to where its area lands in the image, with no refusal off it.
    let place = |point: PhysicalPoint| -> [f64; 2] {
        std::array::from_fn(|axis| {
            (f64::from(point.0[axis]) - f64::from(region.origin[axis]) + 0.5)
                * f64::from(size[axis])
                / f64::from(region.size[axis])
        })
    };
    let mut bounds: Option<[u32; 4]> = None;
    for (radius, colour) in [(EDGE, WHITE), (CORE, PINK)] {
        for pair in path.windows(2) {
            let (a, b) = (place(pair[0]), place(pair[1]));
            let Some((top, bottom)) =
                span(a[1].min(b[1]) - radius, a[1].max(b[1]) + radius, size[1])
            else {
                continue;
            };
            let [dx, dy] = [b[0] - a[0], b[1] - a[1]];
            let length = dx.hypot(dy);
            for y in top..=bottom {
                let row = f64::from(y) + 0.5;
                // Where the segment crosses this row, and how far either side its band can reach.
                let (centre, half) = if dy.abs() < 1.0 {
                    (f64::midpoint(a[0], b[0]), dx.abs() / 2.0 + radius + 1.0)
                } else {
                    let t = ((row - a[1]) / dy).clamp(0.0, 1.0);
                    (a[0] + t * dx, radius * length / dy.abs() + radius + 1.0)
                };
                let Some((left, right)) = span(centre - half, centre + half, size[0]) else {
                    continue;
                };
                for x in left..=right {
                    let distance = distance_squared([f64::from(x) + 0.5, row], a, b);
                    if distance.is_nan() || distance > radius * radius {
                        continue;
                    }
                    let at =
                        usize::try_from((u64::from(y) * u64::from(size[0]) + u64::from(x)) * 3)
                            .expect("a pixel inside the image");
                    rgb[at..at + 3].copy_from_slice(&colour);
                    bounds = Some(bounds.map_or(
                        [x, y, x + 1, y + 1],
                        |[left, top, right, bottom]| {
                            [left.min(x), top.min(y), right.max(x + 1), bottom.max(y + 1)]
                        },
                    ));
                }
            }
        }
    }
    let [left, top, right, bottom] = bounds?;
    let seen = |at: &[f64; 2]| (0..2).all(|axis| (0.0..f64::from(size[axis])).contains(&at[axis]));
    let places: Vec<[f64; 2]> = path
        .iter()
        .map(|&point| place(point))
        .filter(seen)
        .collect();
    // A line with no kept point on the image has no place to name; the shell keeps only points
    // on the display the picture shows, so it never sends one.
    let mean = gathered(&places, size)?;
    let focus = [inside(mean[0], left, right), inside(mean[1], top, bottom)];
    Some(InkBounds {
        left,
        top,
        right,
        bottom,
        focus,
    })
}

/// Where `places`, all inside an image of `size`, are densest: the mean of the places in the
/// square of two by two [`GATHER`] cells holding the most, the one holding the latest place on a
/// tie; none for no places. One pass over the places and one over the cells.
fn gathered(places: &[[f64; 2]], size: [u32; 2]) -> Option<[f64; 2]> {
    let [columns, rows] =
        size.map(|side| usize::try_from(side.div_ceil(GATHER)).expect("an image side fits usize"));
    // Each cell's count, the sum of its places and its latest place's index.
    let mut cells = vec![(0_u32, [0.0, 0.0], 0_usize); columns * rows];
    for (index, at) in places.iter().enumerate() {
        let cell = &mut cells[cell(at[1]) * columns + cell(at[0])];
        *cell = (cell.0 + 1, [cell.1[0] + at[0], cell.1[1] + at[1]], index);
    }
    let mut best: Option<(u32, usize, [f64; 2])> = None;
    for top in 0..rows {
        for left in 0..columns {
            let mut square = (0, 0, [0.0, 0.0]);
            for (x, y) in [
                (left, top),
                (left + 1, top),
                (left, top + 1),
                (left + 1, top + 1),
            ] {
                if x < columns && y < rows {
                    let (count, sum, latest) = cells[y * columns + x];
                    if count > 0 {
                        square = (
                            square.0 + count,
                            square.1.max(latest),
                            [square.2[0] + sum[0], square.2[1] + sum[1]],
                        );
                    }
                }
            }
            if square.0 > 0
                && best.is_none_or(|(count, latest, _)| (square.0, square.1) > (count, latest))
            {
                best = Some(square);
            }
        }
    }
    best.map(|(count, _, sum)| [sum[0] / f64::from(count), sum[1] / f64::from(count)])
}

/// The [`GATHER`] cell a place inside the image falls in, along one axis.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a place inside the image is non-negative and under its side"
)]
fn cell(at: f64) -> usize {
    (at / f64::from(GATHER)).floor() as usize
}

/// The pixel `at` falls in, moved into `low..high`, which isn't empty.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the place is clamped to the painted pixels first"
)]
fn inside(at: f64, low: u32, high: u32) -> u32 {
    at.floor().clamp(f64::from(low), f64::from(high - 1)) as u32
}

/// The pixel indices from `low` to `high` that lie in `0..limit`, or none.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "both ends are clamped to the image's pixels first"
)]
fn span(low: f64, high: f64, limit: u32) -> Option<(u32, u32)> {
    let low = low.floor().max(0.0);
    let high = high.floor().min(f64::from(limit) - 1.0);
    (low <= high).then_some((low as u32, high as u32))
}

fn distance_squared(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let [dx, dy] = [b[0] - a[0], b[1] - a[1]];
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let [qx, qy] = [a[0] + t * dx, a[1] + t * dy];
    (p[0] - qx).powi(2) + (p[1] - qy).powi(2)
}

#[cfg(test)]
mod tests;
