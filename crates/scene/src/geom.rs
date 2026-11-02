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
    /// Point where a ray from the center toward `toward` leaves the rectangle.
    pub fn edge_toward(self, toward: Vec2) -> Vec2 {
        let center = self.center();
        let delta = toward - center;
        let half = self.size * 0.5;
        if delta.x.abs() < f32::EPSILON && delta.y.abs() < f32::EPSILON {
            return center;
        }
        let x = if delta.x.abs() > f32::EPSILON {
            half.x / delta.x.abs()
        } else {
            f32::INFINITY
        };
        let y = if delta.y.abs() > f32::EPSILON {
            half.y / delta.y.abs()
        } else {
            f32::INFINITY
        };
        center + delta * x.min(y)
    }
    /// Checks dimensions and both corners against the logical coordinate cap.
    pub fn within_canvas(self) -> bool {
        self.size.x >= 0.0
            && self.size.y >= 0.0
            && self.origin.within_canvas()
            && self.max().within_canvas()
    }
}

/// Affine coefficients `[a, b, c, d, e, f]` for `[a c e; b d f]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine(pub [f32; 6]);
impl Default for Affine {
    fn default() -> Self {
        Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    }
}
impl Affine {
    /// Scale and rotate about the scene origin, then translate.
    pub fn pose(scale: f32, radians: f32, offset: Vec2) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self([
            scale * cos,
            scale * sin,
            -scale * sin,
            scale * cos,
            offset.x,
            offset.y,
        ])
    }
    /// Applies this transform after `local`.
    #[must_use]
    pub fn compose(self, local: Self) -> Self {
        let outer = self.0;
        let inner = local.0;
        Self([
            outer[0] * inner[0] + outer[2] * inner[1],
            outer[1] * inner[0] + outer[3] * inner[1],
            outer[0] * inner[2] + outer[2] * inner[3],
            outer[1] * inner[2] + outer[3] * inner[3],
            outer[0] * inner[4] + outer[2] * inner[5] + outer[4],
            outer[1] * inner[4] + outer[3] * inner[5] + outer[5],
        ])
    }
    /// Inverts a finite nonsingular transform, rejecting an unrepresentable inverse.
    pub fn inverse(self) -> Option<Self> {
        let matrix = self.0;
        let norm = matrix[..4]
            .iter()
            .map(|value| value.abs())
            .fold(0.0, f32::max);
        if !norm.is_finite() || norm <= 0.0 {
            return None;
        }
        let normalized = [
            matrix[0] / norm,
            matrix[1] / norm,
            matrix[2] / norm,
            matrix[3] / norm,
        ];
        let determinant = normalized[0] * normalized[3] - normalized[1] * normalized[2];
        if !determinant.is_finite() || determinant.abs() <= f32::EPSILON {
            return None;
        }
        let reciprocal = 1.0 / determinant / norm;
        let mut inverse = [
            normalized[3] * reciprocal,
            -normalized[1] * reciprocal,
            -normalized[2] * reciprocal,
            normalized[0] * reciprocal,
            0.0,
            0.0,
        ];
        inverse[4] = -(inverse[0] * matrix[4] + inverse[2] * matrix[5]);
        inverse[5] = -(inverse[1] * matrix[4] + inverse[3] * matrix[5]);
        inverse
            .iter()
            .all(|value| value.is_finite())
            .then_some(Self(inverse))
    }
    /// Transforms a point.
    pub fn point(self, point: Vec2) -> Vec2 {
        let matrix = self.0;
        Vec2::new(
            matrix[0] * point.x + matrix[2] * point.y + matrix[4],
            matrix[1] * point.x + matrix[3] * point.y + matrix[5],
        )
    }
    /// Axis-aligned bounds of all four transformed corners.
    pub fn bounds(self, rect: Rect) -> Rect {
        let max = rect.max();
        let points = [
            rect.origin,
            Vec2::new(max.x, rect.origin.y),
            max,
            Vec2::new(rect.origin.x, max.y),
        ];
        let transformed = points.map(|point| self.point(point));
        let min = transformed
            .into_iter()
            .fold(Vec2::new(f32::INFINITY, f32::INFINITY), |a, b| {
                Vec2::new(a.x.min(b.x), a.y.min(b.y))
            });
        let max = transformed
            .into_iter()
            .fold(Vec2::new(f32::NEG_INFINITY, f32::NEG_INFINITY), |a, b| {
                Vec2::new(a.x.max(b.x), a.y.max(b.y))
            });
        Rect {
            origin: min,
            size: max - min,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_composition_keeps_detached_geometry_in_its_parent_basis() {
        let parent = Affine::pose(2.0, 0.0, Vec2::new(10.0, 20.0));
        let local = Affine::pose(1.0, 0.0, Vec2::new(3.0, 4.0));
        assert_eq!(
            parent.compose(local).point(Vec2::new(1.0, 2.0)),
            Vec2::new(18.0, 32.0)
        );
        let bounds = parent.bounds(Rect {
            origin: Vec2::new(1.0, 2.0),
            size: Vec2::new(3.0, 4.0),
        });
        assert_eq!(
            bounds,
            Rect {
                origin: Vec2::new(12.0, 24.0),
                size: Vec2::new(6.0, 8.0)
            }
        );
    }

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
