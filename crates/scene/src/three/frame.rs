//! Pose, source-face depth, canonical clipping and flat shading into reused raster scratch.

use super::{
    Transform,
    camera::{Camera, Light},
    raster::{self, Fragment, FrameScratch, PushError},
};
use crate::{
    geom::Vec3,
    limits::{MAX_SOLIDS, MAX_TRIANGLES},
};
use std::sync::atomic::{AtomicBool, Ordering};

type Point = [f64; 3];

/// A source face's stable plane anchors and its outward-wound tessellation.
/// Triangles tessellate one source plane, allowing f32 vertex rounding off it; preparation retains
/// the same anchors across triangulations. Indices name vertices within the solid, including edges.
#[derive(Debug, Clone, Copy)]
pub struct Face<'a> {
    /// Three noncollinear original vertices defining the source face, independent of triangulation.
    pub anchors: [u32; 3],
    /// Triangles indexing the solid's vertices.
    pub triangles: &'a [[u32; 3]],
}

/// Borrowed prepared geometry and one already-composed sampled pose; no per-frame mesh rebuild.
#[derive(Clone, Copy)]
pub struct Solid<'a> {
    /// Shared local vertices, indexed identically by every face.
    pub vertices: &'a [Vec3],
    /// Source faces, preserving their stable plane anchors.
    pub faces: &'a [Face<'a>],
    /// Local-to-world sampled transform, including attachment ancestors.
    pub pose: Transform,
    /// Theme-resolved straight colour.
    pub rgb: [u8; 3],
    /// Authored opacity in 0..=1.
    pub opacity: f32,
}
impl std::fmt::Debug for Solid<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Solid")
            .field("vertices", &self.vertices.len())
            .field("faces", &self.faces.len())
            .finish_non_exhaustive()
    }
}

/// Failure invalidates the whole output; the host must keep the previously displayed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// An admitted count or caller-owned output capacity was exceeded.
    #[error("scene.Full")]
    Over,
    /// An input, index, opacity or raster size was invalid.
    #[error("scene.Invalid")]
    Invalid,
    /// Geometry or depth could not be represented finitely and positively.
    #[error("scene.Numeric")]
    Numeric,
    /// The frame was cancelled.
    #[error("scene.Cancelled")]
    Cancelled,
}

#[derive(Clone, Copy, Default)]
struct Vertex {
    p: Point,
    index: u32,
}

fn sub(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] - b[i])
}
fn dot(a: Point, b: Point) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn cross(a: Point, b: Point) -> Point {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn finite(p: Point) -> Result<Point, FrameError> {
    if p.into_iter().all(f64::is_finite) {
        Ok(p)
    } else {
        Err(FrameError::Numeric)
    }
}
fn poll(cancel: &AtomicBool) -> Result<(), FrameError> {
    #[cfg(test)]
    if tests::skip_poll() {
        return Ok(());
    }
    if cancel.load(Ordering::Relaxed) {
        Err(FrameError::Cancelled)
    } else {
        Ok(())
    }
}

fn world(solid: &Solid<'_>, v: Vec3) -> Result<Point, FrameError> {
    let p = [v.x, v.y, v.z].map(f64::from);
    finite(solid.pose.0.map(|r| dot([r[0], r[1], r[2]], p) + r[3]))
}
fn point(
    solid: &Solid<'_>,
    index: u32,
    eye: Point,
    basis: [Point; 3],
) -> Result<Vertex, FrameError> {
    let v = *solid
        .vertices
        .get(usize::try_from(index).map_err(|_| FrameError::Invalid)?)
        .ok_or(FrameError::Invalid)?;
    let relative = sub(world(solid, v)?, eye);
    Ok(Vertex {
        p: finite(basis.map(|axis| dot(relative, axis)))?,
        index,
    })
}

fn intersection(mut a: Vertex, mut b: Vertex, plane: Point, offset: f64) -> Vertex {
    let order = a.index.cmp(&b.index).then_with(|| {
        (0..3)
            .map(|i| a.p[i].total_cmp(&b.p[i]))
            .find(|o| !o.is_eq())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if order.is_gt() {
        std::mem::swap(&mut a, &mut b);
    }
    let da = dot(plane, a.p) + offset;
    let db = dot(plane, b.p) + offset;
    let t = (da / (da - db)).clamp(0.0, 1.0);
    Vertex {
        p: std::array::from_fn(|i| a.p[i] + t * (b.p[i] - a.p[i])),
        index: a.index.min(b.index),
    }
}

#[expect(
    clippy::float_cmp,
    reason = "only exactly identical endpoints are duplicates; on-plane classification has no epsilon"
)]
fn clip(
    poly: &mut [Vertex; 8],
    len: usize,
    plane: Point,
    offset: f64,
) -> Result<usize, FrameError> {
    let source = *poly;
    let mut n = 0;
    let mut emit = |v: Vertex| -> Result<(), FrameError> {
        if n > 0 && poly[n - 1].p == v.p {
            return Ok(());
        }
        if n == poly.len() {
            return Err(FrameError::Over);
        }
        poly[n] = v;
        n += 1;
        Ok(())
    };
    for i in 0..len {
        let a = source[i];
        let b = source[(i + 1) % len];
        let da = dot(plane, a.p) + offset;
        let db = dot(plane, b.p) + offset;
        if da >= 0.0 {
            emit(a)?;
        }
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            emit(intersection(a, b, plane, offset))?;
        }
    }
    if n > 1 && poly[0].p == poly[n - 1].p {
        n -= 1;
    }
    Ok(n)
}

/// Builds one polygon per surviving triangle with no allocation; solid ids are input index + 1.
/// The caller supplies indexed, coplanar source faces and composed transforms from preparation.
/// Every error clears output, so a partial candidate cannot accidentally become pickable.
/// # Errors
/// Returns `Over` above 160 solids/24,000 triangles or output capacity, `Invalid` for bad inputs,
/// `Numeric` for unrepresentable geometry/depth, or `Cancelled` (polled at most 1,024 vertices or
/// triangles apart, and at each source face).
pub fn frame(
    solids: &[Solid<'_>],
    camera: &Camera,
    light: &Light,
    size: [u32; 2],
    out: &mut FrameScratch,
    cancel: &AtomicBool,
) -> Result<(), FrameError> {
    out.clear();
    let result = build(solids, camera, light, size, out, cancel);
    if result.is_err() {
        out.clear();
    }
    result
}

#[expect(
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "one bounded frame pipeline; deliberate checked depth and fixed-point narrowing"
)]
fn build(
    solids: &[Solid<'_>],
    camera: &Camera,
    light: &Light,
    size: [u32; 2],
    out: &mut FrameScratch,
    cancel: &AtomicBool,
) -> Result<(), FrameError> {
    poll(cancel)?;
    if solids.len() > MAX_SOLIDS {
        return Err(FrameError::Over);
    }
    if !size
        .into_iter()
        .all(|v| (1..=raster::MAX_TEXELS).contains(&v))
    {
        return Err(FrameError::Invalid);
    }
    let mut count = 0usize;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for solid in solids {
        if !(0.0..=1.0).contains(&solid.opacity) {
            return Err(FrameError::Invalid);
        }
        for face in solid.faces {
            poll(cancel)?;
            count = count
                .checked_add(face.triangles.len())
                .ok_or(FrameError::Over)?;
            if count > MAX_TRIANGLES {
                return Err(FrameError::Over);
            }
            for (i, index) in face
                .anchors
                .iter()
                .chain(face.triangles.iter().flatten())
                .enumerate()
            {
                if i % 1024 == 0 {
                    poll(cancel)?;
                }
                if usize::try_from(*index).map_or(true, |i| i >= solid.vertices.len()) {
                    return Err(FrameError::Invalid);
                }
            }
        }
        for (i, &v) in solid.vertices.iter().enumerate() {
            if i % 1024 == 0 {
                poll(cancel)?;
            }
            let p = world(solid, v)?;
            for j in 0..3 {
                min[j] = min[j].min(p[j]);
                max[j] = max[j].max(p[j]);
            }
        }
    }
    if count == 0 {
        return Ok(());
    }
    let radius = (0..3)
        .map(|i| ((max[i] - min[i]) / 2.0).powi(2))
        .sum::<f64>()
        .sqrt()
        .max(0.001);
    let largest = min.into_iter().chain(max).map(f64::abs).fold(0.0, f64::max);
    let near = camera.near(radius, largest, size[1])?;
    let focal = camera.focal(size[1]);
    let centre = size.map(|v| f64::from(v) / 2.0);
    let eye = finite(camera.eye())?;
    let basis = camera.basis();
    let guard = f64::from(raster::GUARD_TEXELS);
    let planes = [
        ([0.0, 0.0, 1.0], -near),
        ([focal, 0.0, centre[0] + guard], 0.0),
        ([-focal, 0.0, centre[0] + guard], 0.0),
        ([0.0, -focal, centre[1] + guard], 0.0),
        ([0.0, focal, centre[1] + guard], 0.0),
    ];
    count = 0;
    for (solid_index, solid) in solids.iter().enumerate() {
        for face in solid.faces {
            poll(cancel)?;
            let [a, b, c] = face.anchors.map(|i| point(solid, i, eye, basis));
            let [a, b, c] = [a?, b?, c?].map(|v| v.p);
            let normal = finite(cross(sub(c, a), sub(b, a)))?;
            let distance = dot(normal, a);
            if !distance.is_finite() {
                return Err(FrameError::Numeric);
            }
            if distance >= 0.0 {
                continue;
            }
            let plane = [
                normal[0] / (distance * focal),
                -normal[1] / (distance * focal),
                (normal[2] - normal[0] * centre[0] / focal + normal[1] * centre[1] / focal)
                    / distance,
            ]
            .map(|v| v as f32);
            if !plane.into_iter().all(f32::is_finite) {
                return Err(FrameError::Numeric);
            }
            let world_normal =
                std::array::from_fn(|i| (0..3).map(|j| normal[j] * basis[j][i]).sum());
            let rgb = light.shade(world_normal, solid.rgb);
            for triangle in face.triangles {
                if count.is_multiple_of(1024) {
                    poll(cancel)?;
                }
                count += 1;
                let mut poly = [Vertex::default(); 8];
                for (v, &index) in poly.iter_mut().zip(triangle) {
                    *v = point(solid, index, eye, basis)?;
                }
                let mut len = 3;
                for (plane, offset) in planes {
                    if poly[..len].iter().any(|v| dot(plane, v.p) + offset < 0.0) {
                        // shortcut: clip overflow rejects the frame; drop slivers once a reachable fixture establishes them.
                        len = clip(&mut poly, len, plane, offset)?;
                    }
                    if len < 3 {
                        break;
                    }
                }
                if len < 3 {
                    continue;
                }
                let mut points = [[0; 2]; 8];
                let mut depth_sum = 0.0;
                for (dst, v) in points.iter_mut().zip(&poly[..len]) {
                    let p = finite([
                        centre[0] + focal * v.p[0] / v.p[2],
                        centre[1] - focal * v.p[1] / v.p[2],
                        v.p[2],
                    ])?;
                    for i in 0..2 {
                        let snapped = (p[i] * f64::from(raster::SUBTEXEL)).round();
                        if snapped.abs() > f64::from(raster::COORD_LIMIT) {
                            return Err(FrameError::Numeric);
                        }
                        dst[i] = snapped as i32;
                    }
                    depth_sum += v.p[2];
                }
                let fragment = Fragment {
                    points: &points[..len],
                    plane,
                    rgb,
                    opacity: solid.opacity,
                    solid: u8::try_from(solid_index + 1).map_err(|_| FrameError::Over)?,
                    centroid_z: (depth_sum
                        / f64::from(u32::try_from(len).map_err(|_| FrameError::Over)?))
                        as f32,
                };
                out.push(&fragment).map_err(|e| match e {
                    PushError::Full => FrameError::Over,
                    PushError::Invalid => FrameError::Invalid,
                    PushError::Numeric => FrameError::Numeric,
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! { static SKIP_POLLS: Cell<usize> = const { Cell::new(0) }; }

    pub(super) fn skip_poll() -> bool {
        SKIP_POLLS.with(|remaining| {
            let count = remaining.get();
            remaining.set(count.saturating_sub(1));
            count > 0
        })
    }

    #[test]
    fn triangle_loop_stops_at_exactly_1024_and_frame_clears_partial_output() {
        let vertices = [
            Vec3::new(-1.0, -1.0, 0.0),
            Vec3::new(1.0, -1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ];
        let triangles = vec![[0, 1, 2]; 2049];
        let faces = [Face {
            anchors: [0, 1, 2],
            triangles: &triangles,
        }];
        let solids = [Solid {
            vertices: &vertices,
            faces: &faces,
            pose: Transform::local([0.0; 3], [0.0; 3], 1.0),
            rgb: [255; 3],
            opacity: 1.0,
        }];
        let camera = Camera::new(0.0, 0.0, 2.0, 90.0, [0.0; 3]).unwrap();
        let light = Light::new([0.0, 0.0, -1.0], 0.0).unwrap();
        let mut out = FrameScratch::with_capacity(2049);
        let cancel = AtomicBool::new(true);
        SKIP_POLLS.with(|n| n.set(12));
        assert_eq!(
            build(&solids, &camera, &light, [8, 8], &mut out, &cancel),
            Err(FrameError::Cancelled)
        );
        assert_eq!(
            out.len(),
            1024,
            "entry, preparation and the first triangle poll are bypassed, not the 1024 poll"
        );
        SKIP_POLLS.with(|n| n.set(12));
        assert_eq!(
            frame(&solids, &camera, &light, [8, 8], &mut out, &cancel),
            Err(FrameError::Cancelled)
        );
        assert_eq!(
            out.len(),
            0,
            "a cancelled candidate cannot retain the first quantum"
        );
        SKIP_POLLS.with(|n| n.set(0));
    }

    #[test]
    fn clip_removes_consecutive_and_closing_input_duplicates() {
        for source in [
            vec![
                [0.0, 0.0, 1.0],
                [0.0, 0.0, 1.0],
                [2.0, 0.0, 1.0],
                [0.0, 2.0, 1.0],
            ],
            vec![
                [0.0, 0.0, 1.0],
                [2.0, 0.0, 1.0],
                [0.0, 2.0, 1.0],
                [0.0, 0.0, 1.0],
            ],
        ] {
            let mut poly = polygon(&source);
            assert_eq!(clip(&mut poly, source.len(), [0.0, 0.0, 1.0], 0.0), Ok(3));
            assert_eq!(
                poly[..3]
                    .iter()
                    .map(|v| v.p.map(f64::to_bits))
                    .collect::<Vec<_>>(),
                [[0.0_f64, 0.0, 1.0], [2.0, 0.0, 1.0], [0.0, 2.0, 1.0]]
                    .map(|p| p.map(f64::to_bits))
            );
        }
    }

    #[test]
    fn reversed_shared_edges_remain_bit_identical_after_near_and_guard_clipping() {
        for endpoints in [
            [[-22.345, -12.23, 0.3], [0.1, 0.2, 1.2]],
            [[-22.345, 12.23, 0.3], [0.1, -0.2, 1.2]],
        ] {
            let mut forward = polygon(&endpoints);
            forward[1].index = 2;
            let mut reverse = forward;
            reverse[..2].reverse();
            let mut counts = [2, 2];
            for (plane, offset) in [
                ([0.0, 0.0, 1.0], -0.5),
                ([2048.0, 0.0, 6144.0], 0.0),
                ([-2048.0, 0.0, 6144.0], 0.0),
                ([0.0, -2048.0, 5632.0], 0.0),
                ([0.0, 2048.0, 5632.0], 0.0),
            ] {
                counts[0] = clip(&mut forward, counts[0], plane, offset).unwrap();
                counts[1] = clip(&mut reverse, counts[1], plane, offset).unwrap();
            }
            assert_eq!(counts, [2, 2]);
            let mut a = forward[..2]
                .iter()
                .map(|v| v.p.map(f64::to_bits))
                .collect::<Vec<_>>();
            let mut b = reverse[..2]
                .iter()
                .map(|v| v.p.map(f64::to_bits))
                .collect::<Vec<_>>();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                a, b,
                "a one-ulp clipped endpoint mismatch must not be hidden by texel snapping"
            );
        }
    }

    fn polygon(points: &[Point]) -> [Vertex; 8] {
        let mut out = [Vertex::default(); 8];
        for (i, &p) in points.iter().enumerate() {
            out[i] = Vertex {
                p,
                index: u32::try_from(i).unwrap(),
            };
        }
        out
    }

    #[test]
    fn canonical_intersection_is_bit_identical_in_both_directions() {
        for (a, b) in [(0, 7), (7, 0), (0, 0)] {
            let start = Vertex {
                p: [0.1, 0.2, 0.3],
                index: a,
            };
            let end = Vertex {
                p: [22.345, 12.23, 1.2],
                index: b,
            };
            for (normal, offset) in [([0.0, 0.0, 1.0], -0.5), ([1.0, 0.0, 4097.0], -2500.0)] {
                assert_eq!(
                    intersection(start, end, normal, offset).p.map(f64::to_bits),
                    intersection(end, start, normal, offset).p.map(f64::to_bits)
                );
            }
        }
    }

    #[test]
    fn translated_minimum_camera_projects_target_to_centre_in_f64() {
        let camera = Camera::new(30.0, 20.0, 0.001, 40.0, [20000.0; 3]).unwrap();
        let vertices = [Vec3::new(20000.0, 20000.0, 20000.0)];
        let solid = Solid {
            vertices: &vertices,
            faces: &[],
            pose: Transform::local([0.0; 3], [0.0; 3], 1.0),
            rgb: [0; 3],
            opacity: 1.0,
        };
        let transformed = point(&solid, 0, camera.eye(), camera.basis()).unwrap().p;
        assert!(
            transformed[0].abs() < 2e-12 && transformed[1].abs() < 2e-12,
            "target is on the optical axis: {transformed:?}"
        );
        assert!(
            (transformed[2] - 0.001).abs() < 2e-12,
            "target is one distance in front: {transformed:?}"
        );
        let projected = [
            720.0 + camera.focal(1440) * transformed[0] / transformed[2],
            720.0 - camera.focal(1440) * transformed[1] / transformed[2],
        ];
        assert!(
            (projected[0] - 720.0).abs() < 4e-6 && (projected[1] - 720.0).abs() < 4e-6,
            "analytic centre [720,720]: {projected:?}"
        );
    }

    #[test]
    fn near_boundary_and_snap_tie_are_exact_and_reversal_independent() {
        let camera = Camera::new(0.0, 0.0, 1.0, 90.0, [16384.0; 3]).unwrap();
        let near = camera.near(2.0, 16385.0, 8).unwrap();
        let mut at_floor = polygon(&[[0.0, 0.0, near], [1.0, 0.0, near], [0.0, 1.0, near]]);
        assert_eq!(clip(&mut at_floor, 3, [0.0, 0.0, 1.0], -near), Ok(3));
        for v in &mut at_floor[..3] {
            v.p[2] = near.next_down();
        }
        assert_eq!(clip(&mut at_floor, 3, [0.0, 0.0, 1.0], -near), Ok(0));
        let mut poly = polygon(&[[0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 1.0]]);
        assert_eq!(clip(&mut poly, 3, [0.0, 0.0, 1.0], -1.0), Ok(3));
        assert_eq!(
            clip(&mut poly, 3, [0.0, 0.0, 1.0], -1.0_f64.next_up()),
            Ok(0)
        );
        let start = Vertex {
            p: [1.0 / 512.0, 0.0, 0.0],
            index: 7,
        };
        let end = Vertex {
            p: [1.0 / 512.0, 1.0, 2.0],
            index: 2,
        };
        let forward = intersection(start, end, [0.0, 0.0, 1.0], -1.0);
        let reverse = intersection(end, start, [0.0, 0.0, 1.0], -1.0);
        assert_eq!(
            forward.p.map(f64::to_bits),
            [1.0_f64 / 512.0, 0.5, 1.0].map(f64::to_bits)
        );
        assert_eq!(forward.p.map(f64::to_bits), reverse.p.map(f64::to_bits));
        assert_eq!((forward.p[0] * 256.0).round().to_bits(), 1.0_f64.to_bits());
    }

    #[test]
    fn near_clip_has_exact_area_and_emits_on_plane_once() {
        let mut poly = polygon(&[[0.0, 0.0, 0.0], [4.0, 0.0, 2.0], [0.0, 4.0, 2.0]]);
        assert_eq!(clip(&mut poly, 3, [0.0, 0.0, 1.0], -1.0), Ok(4));
        assert_eq!(
            poly[..4].iter().map(|v| v.p).collect::<Vec<_>>(),
            [
                [2.0, 0.0, 1.0],
                [4.0, 0.0, 2.0],
                [0.0, 4.0, 2.0],
                [0.0, 2.0, 1.0]
            ]
        );
        let twice_area: f64 = (0..4)
            .map(|i| poly[i].p[0] * poly[(i + 1) % 4].p[1] - poly[(i + 1) % 4].p[0] * poly[i].p[1])
            .sum();
        assert_eq!(twice_area.to_bits(), 12.0_f64.to_bits());
        let mut on = polygon(&[[0.0, 0.0, 1.0], [4.0, 0.0, 2.0], [0.0, 4.0, 0.0]]);
        assert_eq!(clip(&mut on, 3, [0.0, 0.0, 1.0], -1.0), Ok(3));
        assert_eq!(
            on[..3].iter().map(|v| v.p).collect::<Vec<_>>(),
            [[0.0, 0.0, 1.0], [4.0, 0.0, 2.0], [2.0, 2.0, 1.0]]
        );
        assert_eq!(clip(&mut on, 3, [0.0, 0.0, 1.0], -1.0), Ok(3));
        assert_eq!(clip(&mut on, 0, [0.0, 0.0, 1.0], -1.0), Ok(0));
    }

    #[test]
    fn five_real_halfspaces_reach_eight_vertices_and_ninth_write_refuses() {
        let mut poly = polygon(&[
            [11784.0, -19376.0, 4.0],
            [9367.0, 10804.0, 1.75],
            [-10216.0, 3140.0, 2.0],
        ]);
        let mut len = 3;
        let mut counts = Vec::new();
        for (normal, offset) in [
            ([0.0, 0.0, 1.0], -1.867_372_756_751_112_7),
            ([1.0, 0.0, 4097.0], 0.0),
            ([-1.0, 0.0, 4097.0], 0.0),
            ([0.0, -1.0, 4097.0], 0.0),
            ([0.0, 1.0, 4097.0], 0.0),
        ] {
            len = clip(&mut poly, len, normal, offset).unwrap();
            counts.push(len);
        }
        assert_eq!(counts, [4, 5, 6, 7, 8]);
        let mut octagon = polygon(&[
            [-2.0, -1.0, 1.0],
            [-1.0, -2.0, 1.0],
            [1.0, -2.0, 1.0],
            [2.0, -1.0, 1.0],
            [2.0, 1.0, 1.0],
            [1.0, 2.0, 1.0],
            [-1.0, 2.0, 1.0],
            [-2.0, 1.0, 1.0],
        ]);
        assert_eq!(
            clip(&mut octagon, 8, [1.0, 0.5, 0.0], 2.25),
            Err(FrameError::Over)
        );
    }
}
