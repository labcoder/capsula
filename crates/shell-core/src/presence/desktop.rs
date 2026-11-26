//! The Mac desktop as the presence places onto it: Quartz displays mapped onto physical pixels
//! with one integer scale, refusing any arrangement that doesn't map exactly. Plain data, no
//! operating-system calls; a Mac presence adapter reads the displays and the visible frame and
//! hands them here.
//!
//! The rules match the screen worker's (`brain/src/platform/macos/capture/layout.rs`), so the
//! shell never places onto a desktop whose picture the worker would refuse.
// shortcut: two copies of the mapping rules; move the worker onto this one when its owner opens
// that file.

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

/// The DPI the screen worker reads as one physical pixel per point; its scale must fit with it.
const POINT_DPI: u32 = 96;

/// One active display as Quartz reports it: top-left global bounds in points, and its mode's
/// size in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub id: u32,
    pub origin: [f64; 2],
    pub points: [f64; 2],
    pub pixels: [usize; 2],
    pub mirrored: bool,
}

/// A display on physical pixels: physical = points × scale, with one scale everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub id: u32,
    pub origin: [i32; 2],
    pub points: [u32; 2],
    pub bounds: PhysicalRect,
    pub scale: u32,
}

/// The arrangement can't be placed onto exactly: no display, no primary at the origin,
/// mirroring, a fractional or mixed scale, a fractional or overflowing rectangle, or overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the display arrangement doesn't map onto physical pixels exactly")]
pub struct UnsupportedLayout;

/// Every display mapped with one scale, and the primary (the display at the Quartz origin).
#[derive(Debug, PartialEq, Eq)]
pub struct Desktop {
    displays: Vec<Display>,
    primary: usize,
}

impl Desktop {
    /// # Errors
    /// [`UnsupportedLayout`] for any arrangement the screen worker would refuse, or one with no
    /// display at the Quartz origin.
    pub fn map(screens: &[Screen]) -> Result<Self, UnsupportedLayout> {
        let mut displays: Vec<Display> = Vec::with_capacity(screens.len());
        for screen in screens {
            let display = mapped(screen).ok_or(UnsupportedLayout)?;
            let mixed = displays
                .first()
                .is_some_and(|first| first.scale != display.scale);
            if mixed
                || displays
                    .iter()
                    .any(|other| overlap(other.bounds, display.bounds))
            {
                return Err(UnsupportedLayout);
            }
            displays.push(display);
        }
        let primary = displays
            .iter()
            .position(|display| display.origin == [0, 0])
            .ok_or(UnsupportedLayout)?;
        Ok(Self { displays, primary })
    }

    pub fn primary(&self) -> &Display {
        &self.displays[self.primary]
    }

    /// The display under a Quartz point, chosen in points before any scaling, and the point on
    /// physical pixels. A point on no display, or not a finite number, has neither.
    pub fn physical(&self, point: [f64; 2]) -> Option<(&Display, PhysicalPoint)> {
        let display = self.displays.iter().find(|display| {
            (0..2).all(|axis| {
                let start = f64::from(display.origin[axis]);
                (start..start + f64::from(display.points[axis])).contains(&point[axis])
            })
        })?;
        let scale = f64::from(display.scale);
        let [x, y] = point.map(|side| pixel((side * scale).floor()));
        Some((display, PhysicalPoint([x?, y?])))
    }

    /// The primary display's work area on physical pixels, from `AppKit`'s `visibleFrame`
    /// (`[x, y, width, height]` in bottom-left points). A fractional edge rounds inwards to a
    /// whole pixel; a frame that is empty or leaves the primary display has no work area.
    pub fn work_area(&self, visible: [f64; 4]) -> Option<PhysicalRect> {
        let display = self.primary();
        let [x, bottom, width, height] = visible;
        let top = f64::from(display.points[1]) - (bottom + height);
        let [right, low] = [x + width, top + height];
        let [wide, high] = display.points.map(f64::from);
        if !(x >= 0.0 && top >= 0.0 && right <= wide && low <= high) {
            return None;
        }
        let scale = f64::from(display.scale);
        let [left, top] = [x, top].map(|side| pixel((side * scale).ceil()));
        let [right, low] = [right, low].map(|side| pixel((side * scale).floor()));
        let [left, top, right, low] = [left?, top?, right?, low?];
        let size =
            [right - left, low - top].map(|side| u32::try_from(side).ok().filter(|&side| side > 0));
        Some(PhysicalRect {
            origin: [left, top],
            size: [size[0]?, size[1]?],
        })
    }
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
    if scale == 0
        || height == 0
        || size[1] != height.checked_mul(scale)?
        || POINT_DPI.checked_mul(scale).is_none()
    {
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
        scale,
    })
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "only a whole value inside i32's range is converted, so nothing is lost"
)]
fn whole(value: f64) -> Option<i64> {
    (value.trunc() == value && value.abs() <= f64::from(i32::MAX)).then_some(value as i64)
}

/// A whole pixel coordinate, the full signed range; not a number has none.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "only a whole value inside i32's range is converted, so nothing is lost"
)]
fn pixel(value: f64) -> Option<i32> {
    (value.trunc() == value && value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX))
        .then_some(value as i32)
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

#[cfg(test)]
mod tests;
