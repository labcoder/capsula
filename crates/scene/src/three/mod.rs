//! Bounded solid sizing, transforms and shared three-dimensional numeric policy.

pub mod camera;
pub mod frame;
pub mod mesh;
pub mod pick;
pub mod raster;
pub mod size;
pub mod view;

use crate::geom::Vec3;

/// An outward-wound world-space triangle with its source part identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    /// Counter-clockwise vertices seen from outside the solid.
    pub v: [Vec3; 3],
    /// Caller-owned part identity.
    pub part: u16,
}

/// Relative geometric tolerance, shared by tessellation, ordering and clipping.
pub const REL: f64 = 1e-6;

/// Distance tolerance for a bounding radius, with a minimum radius of 0.001.
pub fn dist_eps(radius: f64) -> f64 {
    REL * radius.max(1e-3)
}

/// A column-vector affine transform, calculated and composed in f64.
#[derive(Clone, Copy, PartialEq)]
pub struct Transform([[f64; 4]; 3]);

impl std::fmt::Debug for Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transform(..)")
    }
}

impl Transform {
    /// The identity: no rotation, unit scale, no translation.
    pub const IDENTITY: Self = Self([
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ]);

    /// Rotates about x, then y, then z in degrees, scales uniformly, then translates.
    pub fn local(pos: [f32; 3], rot_degrees: [f32; 3], scale: f32) -> Self {
        let [(sx, cx), (sy, cy), (sz, cz)] =
            rot_degrees.map(|angle| f64::from(angle).to_radians().sin_cos());
        let scale = f64::from(scale);
        let mut matrix = [
            [cy * cz, sx * sy * cz - cx * sz, cx * sy * cz + sx * sz, 0.0],
            [cy * sz, sx * sy * sz + cx * cz, cx * sy * sz - sx * cz, 0.0],
            [-sy, sx * cy, cx * cy, 0.0],
        ]
        .map(|mut row| {
            for value in &mut row[..3] {
                *value *= scale;
            }
            row
        });
        for (row, offset) in matrix.iter_mut().zip(pos) {
            row[3] = f64::from(offset);
        }
        Self(matrix)
    }

    /// Applies this child transform first, followed by its parent.
    #[must_use]
    pub fn then(self, parent: &Self) -> Self {
        let mut result = [[0.0; 4]; 3];
        for (row, outer) in result.iter_mut().zip(parent.0) {
            for (column, value) in row.iter_mut().enumerate() {
                *value = (0..3).map(|i| outer[i] * self.0[i][column]).sum();
            }
            row[3] += outer[3];
        }
        Self(result)
    }

    /// Transforms a point in f64 without narrowing.
    #[must_use]
    pub fn apply(&self, point: [f64; 3]) -> [f64; 3] {
        self.0
            .map(|row| row[0] * point[0] + row[1] * point[1] + row[2] * point[2] + row[3])
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "world coordinates deliberately narrow to f32 and are checked immediately"
    )]
    pub(crate) fn point64(&self, point: [f64; 3]) -> Result<Vec3, size::MeshError> {
        let [x, y, z] = self.0.map(|row| {
            let value = (row[0] * point[0] + row[1] * point[1] + row[2] * point[2] + row[3]) as f32;
            if value == 0.0 { 0.0 } else { value }
        });
        if ![x, y, z].into_iter().all(f32::is_finite) {
            return Err(size::MeshError::Degenerate("pos"));
        }
        Ok(Vec3::new(x, y, z))
    }
}

#[cfg(test)]
mod tests {
    use super::{Transform, Vec3, size::MeshError};

    #[test]
    fn transform_rotation_order_and_child_then_parent_are_analytic() {
        let quarter = Transform::local([0.0; 3], [90.0, 0.0, 0.0], 1.0);
        let p = quarter.point64([0.0, 1.0, 0.0]).expect("finite rotation");
        assert_eq!(p.x.to_bits(), 0.0_f32.to_bits());
        assert!(
            p.y.abs() < 1e-15,
            "cosine of ninety degrees rounds near zero"
        );
        assert_eq!(p.z.to_bits(), 1.0_f32.to_bits());
        let child = Transform::local([10.0, 20.0, 30.0], [90.0, 90.0, 90.0], 2.0);
        assert_eq!(
            child.point64([1.0, 2.0, 3.0]),
            Ok(Vec3::new(16.0, 24.0, 28.0))
        );
        let parent = Transform::local([1.0, 2.0, 3.0], [0.0, 0.0, 90.0], 3.0);
        assert_eq!(
            child.then(&parent).point64([1.0, 2.0, 3.0]),
            Ok(Vec3::new(-71.0, 50.0, 87.0))
        );
        assert_eq!(
            Transform::local([0.0; 3], [0.0; 3], 1.0)
                .then(&child)
                .point64([1.0, 2.0, 3.0]),
            Ok(Vec3::new(16.0, 24.0, 28.0))
        );
        let asymmetric = Transform::local([5.0, -7.0, 11.0], [30.0, 60.0, 45.0], 2.0);
        let p = asymmetric
            .point64([1.0, 2.0, 3.0])
            .expect("finite asymmetric rotation");
        let x = 5.0 + 2.0_f64.sqrt() * (17.0 / 4.0 - 3.0_f64.sqrt() / 2.0);
        let y = -7.0 + 2.0_f64.sqrt() * (5.0 / 4.0 + 3.0 * 3.0_f64.sqrt() / 2.0);
        let z = 12.0 + 3.0_f64.sqrt() / 2.0;
        assert!((f64::from(p.x) - x).abs() < 1e-6, "analytic Rz Ry Rx x");
        assert!((f64::from(p.y) - y).abs() < 1e-6, "analytic Rz Ry Rx y");
        assert!((f64::from(p.z) - z).abs() < 1e-6, "analytic Rz Ry Rx z");
    }

    #[test]
    fn transformed_point_finiteness_is_checked_after_narrowing() {
        let identity = Transform::local([0.0; 3], [0.0; 3], 1.0);
        let max = [f64::from(f32::MAX), 0.0, 0.0];
        assert_eq!(identity.point64(max), Ok(Vec3::new(f32::MAX, 0.0, 0.0)));
        let double = Transform::local([0.0; 3], [0.0; 3], 2.0);
        assert_eq!(double.point64(max), Err(MeshError::Degenerate("pos")));
        assert_eq!(
            identity.point64([f64::NAN, 0.0, 0.0]),
            Err(MeshError::Degenerate("pos"))
        );
        assert_eq!(
            Transform::local([f32::INFINITY, 0.0, 0.0], [0.0; 3], 1.0).point64([0.0; 3]),
            Err(MeshError::Degenerate("pos"))
        );
    }
}
