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

/// The painted pixels' bounding rectangle in image pixels: left and top included, right and
/// bottom not. Where the user pointed is theirs, so `Debug` prints none of it.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InkBounds {
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) right: u32,
    pub(crate) bottom: u32,
}

impl fmt::Debug for InkBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InkBounds(***)")
    }
}

/// Paints `path`, in physical desktop pixels, on `rgb`, an image of `size` showing `region`, and
/// returns what it painted. A segment that leaves the image is painted up to its edge; a path of
/// fewer than two points paints nothing.
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
    let mut bounds: Option<InkBounds> = None;
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
                    if distance_squared([f64::from(x) + 0.5, row], a, b) > radius * radius {
                        continue;
                    }
                    let at =
                        usize::try_from((u64::from(y) * u64::from(size[0]) + u64::from(x)) * 3)
                            .expect("a pixel inside the image");
                    rgb[at..at + 3].copy_from_slice(&colour);
                    let grown = bounds.map_or(
                        InkBounds {
                            left: x,
                            top: y,
                            right: x + 1,
                            bottom: y + 1,
                        },
                        |seen| InkBounds {
                            left: seen.left.min(x),
                            top: seen.top.min(y),
                            right: seen.right.max(x + 1),
                            bottom: seen.bottom.max(y + 1),
                        },
                    );
                    bounds = Some(grown);
                }
            }
        }
    }
    bounds
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
