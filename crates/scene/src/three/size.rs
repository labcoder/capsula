//! Allocation-free solid capacity counts and local geometry validation.

use super::{REL, dist_eps};
use crate::{
    Finite,
    record::{Geometry, Segments, ShapeError, Solid},
};

/// A content-free failure while sizing or tessellating one solid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeshError {
    /// Required fields, bounds or shape-dependent relations are invalid.
    #[error(transparent)]
    Shape(#[from] ShapeError),
    /// The named schema field cannot form a representable surface.
    #[error("{0}: scene.Degenerate")]
    Degenerate(&'static str),
    /// The caller requested cancellation; appended triangles may remain.
    #[error("scene.Cancelled")]
    Cancelled,
    /// Spare output capacity cannot hold the candidate count; nothing was appended.
    #[error("scene.Full")]
    Full,
}

/// Returns the exact candidate capacity count, independent of world transforms, without allocating.
///
/// Consecutive points closer than the distance tolerance merge against the last retained point.
/// Lathe candidates touching the axis include triangles that generation will drop.
/// # Errors
/// Rejects invalid shape fields, collapsed or non-simple outlines, collapsed profiles or paths,
/// axis-only lathes and antiparallel tube segments.
pub fn triangles(solid: &Solid) -> Result<u32, MeshError> {
    let geometry = solid.geometry()?;
    let n = u32::from(solid.segments.map_or(24, Segments::get));
    let eps = dist_eps(radius(geometry));
    Ok(match geometry {
        Geometry::Box { .. } => 12,
        Geometry::Sphere { .. } => match n {
            3..=5 => 20,
            6..=11 => 80,
            12..=23 => 320,
            _ => 1280,
        },
        Geometry::Cylinder { .. } => n.saturating_mul(4) - 4,
        Geometry::Cone { top, .. } => {
            if top == 0.0 {
                n.saturating_mul(2) - 2
            } else {
                n.saturating_mul(4) - 4
            }
        }
        Geometry::Torus { .. } => n.saturating_mul(n).saturating_mul(2),
        Geometry::Extrude { outline, .. } => {
            let mut points = Retained::new(outline, eps);
            points.close(eps);
            points.outline(eps)?;
            points.count().saturating_mul(4) - 4
        }
        Geometry::Lathe { profile } => {
            let points = Retained::new(profile, eps);
            if points.len < 2 || (0..points.len).all(|i| points.point(i)[0] == 0.0) {
                return Err(MeshError::Degenerate("points"));
            }
            n.saturating_mul(2).saturating_mul(points.count() - 1)
        }
        Geometry::Tube { path, .. } => {
            let points = Retained::new(path, eps);
            points.tube()?;
            n.saturating_mul(2)
                .saturating_mul(points.count() - 1)
                .saturating_add(2 * (n - 2))
        }
    })
}

pub(super) fn radius(geometry: Geometry<'_>) -> f64 {
    let dimensions = match geometry {
        Geometry::Box { size } => size.map(f64::from),
        Geometry::Sphere { r } => [2.0 * f64::from(r); 3],
        Geometry::Cylinder { r, h } => [2.0 * f64::from(r), f64::from(h), 2.0 * f64::from(r)],
        Geometry::Cone { r, top, h } => {
            let diameter = 2.0 * f64::from(r.max(top));
            [diameter, f64::from(h), diameter]
        }
        Geometry::Torus { ring, tube } => {
            let diameter = 2.0 * (f64::from(ring) + f64::from(tube));
            [diameter, 2.0 * f64::from(tube), diameter]
        }
        Geometry::Extrude { outline, depth } => {
            let [x, y] = spans(outline);
            [x, y, f64::from(depth)]
        }
        Geometry::Lathe { profile } => {
            let diameter = 2.0
                * profile
                    .iter()
                    .map(|p| f64::from(p[0].get()))
                    .fold(0.0, f64::max);
            [diameter, spans(profile)[1], diameter]
        }
        Geometry::Tube { path, thickness } => spans(path).map(|d| d + 2.0 * f64::from(thickness)),
    };
    0.5 * dimensions.into_iter().map(|d| d * d).sum::<f64>().sqrt()
}

fn spans<const N: usize>(points: &[[Finite; N]]) -> [f64; N] {
    let mut low = [f64::INFINITY; N];
    let mut high = [f64::NEG_INFINITY; N];
    for point in points {
        for i in 0..N {
            let value = f64::from(point[i].get());
            low[i] = low[i].min(value);
            high[i] = high[i].max(value);
        }
    }
    std::array::from_fn(|i| high[i] - low[i])
}

pub(super) struct Retained<'a, const N: usize> {
    source: &'a [[Finite; N]],
    indices: [u16; 512],
    pub(super) len: usize,
}

impl<'a, const N: usize> Retained<'a, N> {
    pub(super) fn new(source: &'a [[Finite; N]], eps: f64) -> Self {
        let mut result = Self {
            source,
            indices: [0; 512],
            len: 0,
        };
        for (index, point) in source.iter().enumerate() {
            let point = point.map(|v| f64::from(v.get()));
            if result.len == 0 || distance_squared(result.point(result.len - 1), point) >= eps * eps
            {
                result.indices[result.len] =
                    u16::try_from(index).expect("decoded points are capped at 512");
                result.len += 1;
            }
        }
        result
    }

    pub(super) fn point(&self, index: usize) -> [f64; N] {
        self.source[usize::from(self.indices[index])].map(|v| f64::from(v.get()))
    }

    fn count(&self) -> u32 {
        u32::try_from(self.len).expect("retained points are capped at 512")
    }
}

fn distance_squared<const N: usize>(a: [f64; N], b: [f64; N]) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| (a - b) * (a - b)).sum()
}

impl Retained<'_, 2> {
    pub(super) fn close(&mut self, eps: f64) {
        while self.len > 1 && distance_squared(self.point(0), self.point(self.len - 1)) < eps * eps
        {
            self.len -= 1;
        }
    }

    fn outline(&self, eps: f64) -> Result<(), MeshError> {
        let fail = MeshError::Degenerate("points");
        if self.len < 3 {
            return Err(fail);
        }
        let origin = self.point(0);
        let area: f64 = (1..self.len - 1)
            .map(|i| orient(origin, self.point(i), self.point(i + 1)))
            .sum();
        if area.abs() < area_cutoff(eps) {
            return Err(fail);
        }
        for i in 0..self.len {
            let a = self.point(i);
            let b = self.point((i + 1) % self.len);
            let c = self.point((i + 2) % self.len);
            if orient(a, b, c) == 0.0
                && (0..2).map(|d| (b[d] - a[d]) * (c[d] - b[d])).sum::<f64>() < 0.0
            {
                return Err(fail);
            }
            for j in i + 2..self.len {
                if i == 0 && j == self.len - 1 {
                    continue;
                }
                if intersects(a, b, self.point(j), self.point((j + 1) % self.len)) {
                    return Err(fail);
                }
            }
        }
        Ok(())
    }
}

pub(super) fn orient(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn intersects(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let orientations = [
        orient(a, b, c),
        orient(a, b, d),
        orient(c, d, a),
        orient(c, d, b),
    ];
    if orientations[0] * orientations[1] < 0.0 && orientations[2] * orientations[3] < 0.0 {
        return true;
    }
    [(a, b, c), (a, b, d), (c, d, a), (c, d, b)]
        .into_iter()
        .zip(orientations)
        .any(|((a, b, p), cross)| {
            cross == 0.0 && (0..2).all(|i| p[i] >= a[i].min(b[i]) && p[i] <= a[i].max(b[i]))
        })
}

impl Retained<'_, 3> {
    fn tube(&self) -> Result<(), MeshError> {
        let fail = MeshError::Degenerate("path");
        if self.len < 2 {
            return Err(fail);
        }
        for i in 1..self.len - 1 {
            let a = self.point(i - 1);
            let b = self.point(i);
            let c = self.point(i + 1);
            let dot: f64 = (0..3).map(|d| (b[d] - a[d]) * (c[d] - b[d])).sum();
            let length = (distance_squared(a, b) * distance_squared(b, c)).sqrt();
            if dot / length < -1.0 + REL {
                return Err(fail);
            }
        }
        Ok(())
    }
}

pub(super) fn area_cutoff(eps: f64) -> f64 {
    eps * eps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_distance_keeps_the_exact_limit_and_merges_one_float_below() {
        for (x, expected) in [(1.0_f32.next_down(), 1), (1.0, 2), (1.0_f32.next_up(), 2)] {
            let source = [[0.0, 0.0], [x, 0.0]]
                .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
            assert_eq!(Retained::new(&source, 1.0).len, expected);
        }
        for (x, expected) in [(1.0_f32.next_down(), 2), (1.0, 3), (1.0_f32.next_up(), 3)] {
            let source = [[0.0, 0.0], [0.0, 2.0], [x, 0.0]]
                .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
            let mut retained = Retained::new(&source, 1.0);
            retained.close(1.0);
            assert_eq!(retained.len, expected);
        }
        let source = [
            [0.0, 0.0],
            [2.0, 0.0],
            [0.0, 2.0],
            [0.75, 0.0],
            [-0.75, 0.0],
        ]
        .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
        let mut retained = Retained::new(&source, 1.0);
        retained.close(1.0);
        assert_eq!(retained.len, 3);
    }

    #[test]
    fn outline_twice_area_keeps_the_exact_cutoff_and_refuses_one_float_below() {
        for (height, expected) in [
            (0.25_f32.next_down(), Err(MeshError::Degenerate("points"))),
            (0.25, Ok(())),
            (0.25_f32.next_up(), Ok(())),
        ] {
            let source = [[0.0, 0.0], [1.0, 0.0], [0.5, height]]
                .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
            let retained = Retained::new(&source, 0.5);
            assert_eq!(retained.outline(0.5), expected);
        }
    }

    #[test]
    fn aabb_radius_is_half_the_diagonal_not_distance_from_the_origin() {
        assert_eq!(
            radius(Geometry::Box {
                size: [2.0, 4.0, 4.0]
            })
            .to_bits(),
            3.0_f64.to_bits()
        );
        assert_eq!(
            radius(Geometry::Sphere { r: 2.0 }).to_bits(),
            12.0_f64.sqrt().to_bits()
        );
        assert_eq!(
            radius(Geometry::Cylinder { r: 2.0, h: 2.0 }).to_bits(),
            3.0_f64.to_bits()
        );
        assert_eq!(
            radius(Geometry::Cone {
                r: 1.0,
                top: 2.0,
                h: 2.0
            })
            .to_bits(),
            3.0_f64.to_bits()
        );
        assert_eq!(
            radius(Geometry::Torus {
                ring: 1.5,
                tube: 0.5
            })
            .to_bits(),
            (33.0_f64.sqrt() / 2.0).to_bits()
        );
        let outline = [[1000.0, 2000.0], [1002.0, 2000.0], [1000.0, 2004.0]]
            .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
        assert_eq!(
            radius(Geometry::Extrude {
                outline: &outline,
                depth: 4.0
            })
            .to_bits(),
            3.0_f64.to_bits()
        );
        let profile = [[2.0, 1000.0], [2.0, 1002.0]]
            .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
        assert_eq!(
            radius(Geometry::Lathe { profile: &profile }).to_bits(),
            3.0_f64.to_bits()
        );
        let path = [[1000.0, 2000.0, 3000.0], [1001.0, 2003.0, 3003.0]]
            .map(|p| p.map(|v| Finite::try_from(v).expect("finite point")));
        assert_eq!(
            radius(Geometry::Tube {
                path: &path,
                thickness: 0.5
            })
            .to_bits(),
            3.0_f64.to_bits()
        );
    }
}
