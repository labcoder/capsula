//! Physical desktop coordinates and opaque native window references shared by capture messages.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};

/// A point in the encoded overview, relative to its top-left corner.
/// The first parser of external coordinates must validate that both values are finite.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverviewPoint(pub [f32; 2]);

/// A point in physical desktop pixels, including the display origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPoint(pub [i32; 2]);

/// A point in logical units (96 per inch), relative to the captured display origin.
/// The first parser of external coordinates must validate that both values are finite.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogicalPoint(pub [f32; 2]);

/// A rectangle in physical desktop pixels; its origin may be on a negative-coordinate display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalRect {
    /// Top-left desktop coordinate, in physical pixels.
    pub origin: [i32; 2],
    /// Width and height, in physical pixels.
    pub size: [u32; 2],
}

/// The physical bounds of the display containing the pointer when a hold was recognized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRef(pub PhysicalRect);

/// The exact native display instance selected for a typed screenshot.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedDisplay {
    /// A native instance token, never a position, index or nearest-display fallback.
    pub id: NonZeroU64,
    /// The display's physical bounds when selected.
    pub bounds: PhysicalRect,
    /// The display's effective DPI when selected.
    pub dpi: u32,
}

impl TypedDisplay {
    /// Whether the point lies strictly within this display's half-open pixel bounds.
    #[must_use]
    pub fn contains(self, point: PhysicalPoint) -> bool {
        (0..2).all(|axis| {
            let offset = i64::from(point.0[axis]) - i64::from(self.bounds.origin[axis]);
            (0..i64::from(self.bounds.size[axis])).contains(&offset)
        })
    }
}

impl fmt::Debug for TypedDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypedDisplay").finish_non_exhaustive()
    }
}

/// An opaque native window reference: an HWND on Windows or a `CGWindowID` on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRef(pub u64);

/// The most points a [`PointerPath`] carries.
pub const MAX_PATH_POINTS: usize = 2_048;

/// The pointer's path over a hold, oldest point first, in physical desktop pixels. Where the
/// user pointed is theirs, so `Debug` prints only how many points there are.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerPath(pub Vec<PhysicalPoint>);

/// A [`PointerPath`] with more than [`MAX_PATH_POINTS`] points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a pointer path has too many points")]
pub struct PathTooLong;

impl PointerPath {
    /// Checks the path's length. An empty path is a hold whose pointer never moved.
    ///
    /// # Errors
    /// [`PathTooLong`] past [`MAX_PATH_POINTS`].
    pub fn check(&self) -> Result<(), PathTooLong> {
        if self.0.len() > MAX_PATH_POINTS {
            return Err(PathTooLong);
        }
        Ok(())
    }
}

impl fmt::Debug for PointerPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PointerPath").field(&self.0.len()).finish()
    }
}

/// Where the pointer was when a hold's keys were released, in physical desktop pixels. `Debug`
/// prints no numbers.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasePointer(pub PhysicalPoint);

impl fmt::Debug for ReleasePointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReleasePointer").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_display_geometry_is_half_open_and_debug_hides_its_record() {
        let display = TypedDisplay {
            id: NonZeroU64::new(81_827).unwrap(),
            bounds: PhysicalRect {
                origin: [i32::MIN, -42],
                size: [u32::MAX, 2],
            },
            dpi: 321,
        };
        assert!(display.contains(PhysicalPoint([i32::MIN, -42])));
        assert!(display.contains(PhysicalPoint([i32::MAX - 1, -41])));
        assert!(!display.contains(PhysicalPoint([i32::MAX, -41])));
        assert!(!display.contains(PhysicalPoint([i32::MIN, -40])));
        let debug = format!("{display:?}");
        for private in ["81827", "-2147483648", "4294967295", "321"] {
            assert!(!debug.contains(private));
        }
    }
}
