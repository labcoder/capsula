//! Maps the displays' Quartz arrangement onto physical pixels with one integer scale, and
//! refuses any arrangement that doesn't map exactly.

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

use crate::screen::Refusal;

/// The DPI the screen code reads as one physical pixel per logical point.
const POINT_DPI: u32 = 96;

/// One active display as Quartz reports it: bounds in points and its mode's size in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Screen {
    pub(super) id: u32,
    pub(super) origin: [f64; 2],
    pub(super) points: [f64; 2],
    pub(super) pixels: [usize; 2],
    pub(super) mirrored: bool,
}

/// A display on physical pixels: physical = points × scale, with the same scale everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Display {
    pub(super) id: u32,
    pub(super) origin: [i32; 2],
    pub(super) points: [u32; 2],
    pub(super) bounds: PhysicalRect,
    pub(super) dpi: u32,
}

/// The display under `point`.
///
/// # Errors
/// `Layout` when the arrangement doesn't map exactly or no display holds the point.
pub(super) fn display_at(screens: &[Screen], point: PhysicalPoint) -> Result<Display, Refusal> {
    map(screens)?
        .into_iter()
        .find(|display| contains(display.bounds, point))
        .ok_or(Refusal::Layout)
}

/// The bounds and DPI of display `id` when it still has exactly `bounds`, for the seal's check.
pub(super) fn covering(
    screens: &[Screen],
    id: u32,
    bounds: PhysicalRect,
) -> Option<(PhysicalRect, u32)> {
    map(screens)
        .ok()?
        .into_iter()
        .find(|display| display.id == id && display.bounds == bounds)
        .map(|display| (display.bounds, display.dpi))
}

/// Every display mapped with one scale.
///
/// # Errors
/// `Layout` for no display, mirroring, a fractional or mixed scale, a fractional or overflowing
/// rectangle, or overlapping displays.
fn map(screens: &[Screen]) -> Result<Vec<Display>, Refusal> {
    let mut displays: Vec<Display> = Vec::with_capacity(screens.len());
    for screen in screens {
        let display = mapped(screen).ok_or(Refusal::Layout)?;
        let mixed = displays
            .first()
            .is_some_and(|first| first.dpi != display.dpi);
        if mixed
            || displays
                .iter()
                .any(|other| overlap(other.bounds, display.bounds))
        {
            return Err(Refusal::Layout);
        }
        displays.push(display);
    }
    if displays.is_empty() {
        return Err(Refusal::Layout);
    }
    Ok(displays)
}

fn mapped(screen: &Screen) -> Option<Display> {
    if screen.mirrored {
        return None;
    }
    let [width, height] = screen.points.map(|side| u32::try_from(whole(side)?).ok());
    let [width, height] = [width?, height?];
    let [wide, high] = screen.pixels.map(|side| u32::try_from(side).ok());
    let size = [wide?, high?];
    if width == 0 || size[0] % width != 0 {
        return None;
    }
    let scale = size[0] / width;
    if scale == 0 || height == 0 || size[1] != height.checked_mul(scale)? {
        return None;
    }
    let factor = i32::try_from(scale).ok()?;
    let [x, y] = screen.origin.map(|side| i32::try_from(whole(side)?).ok());
    let origin = [x?, y?];
    let [left, top] = origin.map(|side| side.checked_mul(factor));
    Some(Display {
        id: screen.id,
        origin,
        points: [width, height],
        bounds: PhysicalRect {
            origin: [left?, top?],
            size,
        },
        dpi: POINT_DPI.checked_mul(scale)?,
    })
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "only a whole value inside i32's range is converted, so nothing is lost"
)]
pub(super) fn whole(value: f64) -> Option<i64> {
    (value.trunc() == value && value.abs() <= f64::from(i32::MAX)).then_some(value as i64)
}

fn span(rect: PhysicalRect, axis: usize) -> (i64, i64) {
    let start = i64::from(rect.origin[axis]);
    (start, start + i64::from(rect.size[axis]))
}

fn overlap(a: PhysicalRect, b: PhysicalRect) -> bool {
    (0..2).all(|axis| {
        let ((a0, a1), (b0, b1)) = (span(a, axis), span(b, axis));
        a0 < b1 && b0 < a1
    })
}

fn contains(rect: PhysicalRect, point: PhysicalPoint) -> bool {
    (0..2).all(|axis| {
        let (start, end) = span(rect, axis);
        (start..end).contains(&i64::from(point.0[axis]))
    })
}

#[cfg(test)]
mod tests;
