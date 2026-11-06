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

    /// The same transform applied to points first offset by `origin`: `self(origin + p)`.
    #[must_use]
    pub fn shifted(mut self, origin: [f64; 3]) -> Self {
        for row in &mut self.0 {
            row[3] += row[0] * origin[0] + row[1] * origin[1] + row[2] * origin[2];
        }
        self
    }

    /// Transforms a point in f64 without narrowing, as the frame stage does.
    pub(crate) fn apply(&self, point: [f64; 3]) -> [f64; 3] {
        self.0
            .map(|row| row[0] * point[0] + row[1] * point[1] + row[2] * point[2] + row[3])
    }

    /// Transforms a point and checks that narrowing to world-space f32 remains finite.
    /// # Errors
    /// Returns `Degenerate("pos")` if the result cannot be represented as finite f32.
    pub fn point(&self, point: Vec3) -> Result<Vec3, size::MeshError> {
        self.point64([f64::from(point.x), f64::from(point.y), f64::from(point.z)])
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "world coordinates deliberately narrow to f32 and are checked immediately"
    )]
    fn point64(&self, point: [f64; 3]) -> Result<Vec3, size::MeshError> {
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
