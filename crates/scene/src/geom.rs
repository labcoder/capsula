//! Renderer-independent geometry in logical pixels.
use std::ops::{Add, Mul, Sub};

/// A point or displacement in the scene's logical coordinate system.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Vec2 {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}
impl Vec2 {
    /// Constructs a point without applying layout or transforms.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    /// Whether both coordinates are finite and inside the scene coordinate cap.
    pub fn within_canvas(self) -> bool {
        [self.x, self.y]
            .into_iter()
            .all(|v| v.is_finite() && v.abs() <= crate::limits::MAX_COORD)
    }
}
impl Add for Vec2 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}
impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}
impl Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

/// An axis-aligned rectangle with a top-left origin and nonnegative size.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Top-left point.
    pub origin: Vec2,
    /// Width and height.
    pub size: Vec2,
}
impl Rect {
    /// Places a rectangle around its center.
    pub fn centered(center: Vec2, size: Vec2) -> Self {
        Self {
            origin: center - size * 0.5,
            size,
        }
    }
    /// Bottom-right point.
    pub fn max(self) -> Vec2 {
        self.origin + self.size
    }
    /// Center point.
    pub fn center(self) -> Vec2 {
        self.origin + self.size * 0.5
    }
    /// Smallest rectangle enclosing both inputs.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let origin = Vec2::new(
            self.origin.x.min(other.origin.x),
            self.origin.y.min(other.origin.y),
        );
        let a = self.max();
        let b = other.max();
        Self {
            origin,
            size: Vec2::new(a.x.max(b.x), a.y.max(b.y)) - origin,
        }
    }
    /// Whether the interiors overlap; touching edges do not intersect.
    pub fn intersects(self, other: Self) -> bool {
        let (a, b) = (self.max(), other.max());
        self.origin.x < b.x && other.origin.x < a.x && self.origin.y < b.y && other.origin.y < a.y
    }
    /// Checks dimensions and both corners against the logical coordinate cap.
    pub fn within_canvas(self) -> bool {
        self.size.x >= 0.0
            && self.size.y >= 0.0
            && self.origin.within_canvas()
            && self.max().within_canvas()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_include_sizes_and_touching_edges_do_not_overlap() {
        let a = Rect::centered(Vec2::new(5.0, 5.0), Vec2::new(10.0, 10.0));
        let b = Rect::centered(Vec2::new(15.0, 5.0), Vec2::new(10.0, 10.0));
        assert!(!a.intersects(b), "touching is not overlap");
        assert_eq!(a.union(b).size, Vec2::new(20.0, 10.0));
        assert!(a.within_canvas(), "ordinary dimensions fit");
        assert!(
            !Rect {
                origin: Vec2::new(20_000.0, 0.0),
                size: Vec2::new(1.0, 1.0)
            }
            .within_canvas(),
            "far corner exceeds the cap"
        );
        assert!(
            !Vec2::new(f32::NAN, 0.0).within_canvas(),
            "NaN is not a coordinate"
        );
        assert!(
            !Rect {
                origin: Vec2::default(),
                size: Vec2::new(-1.0, 1.0)
            }
            .within_canvas(),
            "negative dimensions fail"
        );
    }
}
