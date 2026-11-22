//! Pose, source-face depth, canonical clipping and flat shading into reused raster scratch.

use super::{
    Transform,
    camera::{Camera, Light},
    raster::{self, Fragment, FrameScratch, PushError},
    view::FaceRange,
};
use crate::limits::{MAX_SOLIDS, MAX_TRIANGLES};
use std::sync::atomic::{AtomicBool, Ordering};

type Point = [f64; 3];

/// Borrowed prepared geometry and one already-composed sampled pose; no per-frame mesh rebuild.
///
/// Each local f64 vertex and face anchor is transformed once by `pose`, narrowed to world f32
/// exactly as preparation's rest check narrows it, then widened for camera-space work.
#[derive(Clone, Copy)]
pub struct Solid<'a> {
    /// Shared unscaled local vertices, indexed identically by every face.
    pub vertices: &'a [[f64; 3]],
    /// Outward-wound triangles indexing `vertices`.
    pub triangles: &'a [[u32; 3]],
    /// Source faces: stable plane anchors and their run of `triangles`. Triangles tessellate one
    /// source plane, allowing f32 vertex rounding off it.
    pub faces: &'a [FaceRange],
    /// Local-to-world sampled transform, including attachment ancestors.
    pub pose: Transform,
    /// Theme-resolved straight colour.
    pub rgb: [u8; 3],
    /// Authored opacity in 0..=1.
    pub opacity: f32,
}
impl Solid<'_> {
    /// No geometry at the identity pose: filler for caller-owned solid arrays.
    pub const EMPTY: Solid<'static> = Solid {
        vertices: &[],
        triangles: &[],
        faces: &[],
        pose: Transform::IDENTITY,
        rgb: [0; 3],
        opacity: 1.0,
    };
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

/// The world point preparation certified at rest: transformed once, narrowed in world space.
fn world(solid: &Solid<'_>, v: Point) -> Result<Point, FrameError> {
    let p = solid.pose.point64(v).map_err(|_| FrameError::Numeric)?;
    Ok([p.x, p.y, p.z].map(f64::from))
}
fn triangles<'a>(solid: &Solid<'a>, face: &FaceRange) -> Result<&'a [[u32; 3]], FrameError> {
    let start = usize::try_from(face.start).map_err(|_| FrameError::Invalid)?;
    let len = usize::try_from(face.len).map_err(|_| FrameError::Invalid)?;
    let end = start.checked_add(len).ok_or(FrameError::Invalid)?;
    solid.triangles.get(start..end).ok_or(FrameError::Invalid)
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
            let face_triangles = triangles(solid, face)?;
            count = count
                .checked_add(face_triangles.len())
                .ok_or(FrameError::Over)?;
            if count > MAX_TRIANGLES {
                return Err(FrameError::Over);
            }
            for (i, index) in face
                .anchors
                .iter()
                .chain(face_triangles.iter().flatten())
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
            for triangle in triangles(solid, face)? {
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

    fn prepared(put: &str) -> crate::three::view::ViewGeometry {
        use crate::{Admission, GlobalBudget, Record, SceneState, parse_record, theme::Theme};
        let Ok(Record::Begin(header)) =
            parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":""}}"#)
        else {
            panic!("begin fixture");
        };
        let record = format!(r#"{{"step":{{"id":"a","put":[{{"view3d":{{"id":"v"}}}},{put}]}}}}"#);
        let Ok(Record::Step(step)) = parse_record(record.as_bytes()) else {
            panic!("step fixture");
        };
        let (mut scene, mut budget) = (SceneState::begin(header).unwrap(), GlobalBudget::default());
        let Admission::New(candidate) = scene.admit(&step, &budget).unwrap() else {
            panic!("new step");
        };
        let committed = scene.commit(candidate, &mut budget).unwrap();
        let input = committed.preparation.views[0].input.clone().unwrap();
        super::super::view::tessellate_view(
            &input,
            None,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &Theme::default(),
            &AtomicBool::new(false),
            &mut super::super::mesh::MeshScratch::new(),
        )
        .unwrap()
    }

    #[test]
    #[expect(
        clippy::large_stack_arrays,
        clippy::as_conversions,
        clippy::float_cmp,
        reason = "the caller-owned 160-solid array design-6 §7 puts on the stack; u32 mesh indices; exact rest literals"
    )]
    fn frame_world_points_are_the_rest_points_preparation_narrowed() {
        let six = r#"{"solid":{"in":"v","id":"p0","shape":"box","size":[1,1,1],"pos":[10,10.000000953674316,0],"scale":0.5}},
            {"solid":{"in":"v","id":"p1","shape":"box","size":[1,1,1],"attach":"p0","pos":[0.00000095367431640625,0.00000095367431640625,0],"scale":0.001}},
            {"solid":{"in":"v","id":"p2","shape":"box","size":[1,1,1],"attach":"p1","pos":[0,0,0],"scale":0.001}},
            {"solid":{"in":"v","id":"p3","shape":"box","size":[1,1,1],"attach":"p2","pos":[0,0,0],"scale":0.001}},
            {"solid":{"in":"v","id":"p4","shape":"box","size":[1,1,1],"attach":"p3","pos":[0,0,0],"scale":0.001}},
            {"solid":{"in":"v","id":"s","shape":"extrude","attach":"p4","scale":0.001,"points":[[-4,-4],[0,0.001],[4,4]],"depth":1}}"#;
        let tube = r#"{"solid":{"in":"v","id":"s","shape":"tube","path":[[9000,0,0],[9000,1,0]],"r2":0.001,"segments":4,"pos":[-9000,0,0]}}"#;
        let extrusion = r#"{"solid":{"in":"v","id":"s","shape":"extrude","points":[[-6000,-5999],[1.001,2.001],[0,1]],"depth":1}}"#;
        let scaled = r#"{"solid":{"in":"v","id":"p","shape":"box","size":[2,2,2],"pos":[10,0,0],"scale":2}},
            {"solid":{"in":"v","id":"c","shape":"box","size":[2,2,2],"attach":"p","pos":[1,0,0],"scale":1.5}}"#;
        for (name, put, visible) in [
            ("six", six, 12),
            ("tube", tube, 6),
            ("extrusion", extrusion, 3),
            ("scaled", scaled, 12),
        ] {
            let geometry = prepared(put);
            assert!(!geometry.failed, "{name}");
            let poses: Vec<Transform> = geometry.solids.iter().map(|solid| solid.rest).collect();
            let mut storage = [Solid::EMPTY; MAX_SOLIDS];
            let solids = geometry.solids_at(&poses, &mut storage);
            assert_eq!(solids.len(), geometry.solids.len(), "{name}");
            assert!(!solids.is_empty(), "{name}");
            let mut corner = [f64::NEG_INFINITY; 3];
            for (solid, prepared) in solids.iter().zip(&*geometry.solids) {
                let world_bits = |index: u32| {
                    world(solid, solid.vertices[index as usize])
                        .unwrap()
                        .map(f64::to_bits)
                };
                for &triangle in solid.triangles {
                    let narrowed = super::super::mesh::narrowed(
                        &prepared.rest,
                        triangle.map(|i| solid.vertices[i as usize]),
                    )
                    .unwrap();
                    for (index, point) in triangle.into_iter().zip(narrowed) {
                        assert_eq!(
                            world_bits(index),
                            [point.x, point.y, point.z].map(|c| f64::from(c).to_bits()),
                            "{name}"
                        );
                    }
                }
                for face in solid.faces {
                    for index in face.anchors {
                        let point = prepared
                            .rest
                            .point64(solid.vertices[index as usize])
                            .unwrap();
                        assert_eq!(
                            world_bits(index),
                            [point.x, point.y, point.z].map(|c| f64::from(c).to_bits()),
                            "{name} anchor"
                        );
                    }
                }
                if prepared.id.as_str() == "c" {
                    for &v in solid.vertices {
                        let p = world(solid, v).unwrap();
                        corner = std::array::from_fn(|i| corner[i].max(p[i]));
                    }
                }
            }
            if name == "scaled" {
                assert_eq!(corner, [15.0, 3.0, 3.0], "scale applies once");
            }
            let mut out = FrameScratch::with_capacity(64);
            frame(
                solids,
                &geometry.camera.unwrap(),
                &geometry.light,
                [128, 128],
                &mut out,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                out.len(),
                visible,
                "{name} projects all surviving triangles"
            );
        }
    }

    #[test]
    fn triangle_loop_polls_within_1024_and_frame_clears_partial_output() {
        let vertices = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [0.0, 1.0, 0.0]];
        let triangles = vec![[0, 1, 2]; 2049];
        let faces = [FaceRange {
            anchors: [0, 1, 2],
            start: 0,
            len: 2049,
        }];
        let solids = [Solid {
            vertices: &vertices,
            triangles: &triangles,
            faces: &faces,
            pose: Transform::local([0.0; 3], [0.0; 3], 1.0),
            rgb: [255; 3],
            opacity: 1.0,
        }];
        let camera = Camera::new(0.0, 0.0, 2.0, 90.0, [0.0; 3]).unwrap();
        let light = Light::new([0.0, 0.0, -1.0], 0.0).unwrap();
        let mut out = FrameScratch::with_capacity(2049);
        let cancel = AtomicBool::new(true);
        let mut first_partial = None;
        let mut previous = 0;
        let mut completed = false;
        let preamble_allowance = 16 + vertices.len() + 3 * triangles.len();
        for skip in 0..=preamble_allowance + triangles.len() {
            out.clear();
            SKIP_POLLS.with(|n| n.set(skip));
            let result = build(&solids, &camera, &light, [8, 8], &mut out, &cancel);
            SKIP_POLLS.with(|n| n.set(0));
            let emitted = out.len();
            assert!(
                (previous..=previous + 1024).contains(&emitted),
                "triangle work advanced from {previous} to {emitted} without polling"
            );
            previous = emitted;
            if result.is_ok() {
                assert_eq!(
                    emitted,
                    triangles.len(),
                    "every fixture triangle is visible"
                );
                completed = true;
                break;
            }
            assert_eq!(
                result,
                Err(FrameError::Cancelled),
                "cancellation must reach the triangle loop"
            );
            if emitted != 0 {
                first_partial.get_or_insert(skip);
            }
        }
        assert!(
            completed,
            "bypassing all frame polls must finish the fixture"
        );
        SKIP_POLLS.with(|n| n.set(first_partial.expect("cancellation after triangle work")));
        let result = frame(&solids, &camera, &light, [8, 8], &mut out, &cancel);
        SKIP_POLLS.with(|n| n.set(0));
        assert_eq!(result, Err(FrameError::Cancelled));
        assert_eq!(
            out.len(),
            0,
            "a cancelled candidate cannot retain the first quantum"
        );
    }

    fn assert_winding(vertices: &[Vertex], expected: &[Point]) {
        assert_eq!(vertices.len(), expected.len());
        assert!(
            (0..vertices.len()).any(|start| {
                expected.iter().enumerate().all(|(i, point)| {
                    vertices[(start + i) % vertices.len()].p.map(f64::to_bits)
                        == point.map(f64::to_bits)
                })
            }),
            "same vertices and winding, from any starting point"
        );
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
            assert_winding(
                &poly[..3],
                &[[0.0, 0.0, 1.0], [2.0, 0.0, 1.0], [0.0, 2.0, 1.0]],
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
                for poly in [&forward[..counts[0]], &reverse[..counts[1]]] {
                    for vertex in poly {
                        assert!(
                            dot(vertex.p, plane) + offset >= -1e-10,
                            "clipped endpoint is inside {plane:?}"
                        );
                    }
                }
            }
            assert_eq!(counts, [2, 2]);
            assert!(
                forward[..2]
                    .iter()
                    .all(|vertex| vertex.p.map(f64::to_bits) != endpoints[0].map(f64::to_bits))
            );
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

        let [start, mut end, ..] = polygon(&[[0.1, 0.2, 0.3], [22.345, 12.23, 1.2]]);
        end.index = start.index;
        for (normal, offset) in [([0.0, 0.0, 1.0], -0.5), ([1.0, 0.0, 4097.0], -2500.0)] {
            assert_eq!(
                intersection(start, end, normal, offset).p.map(f64::to_bits),
                intersection(end, start, normal, offset).p.map(f64::to_bits)
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
    fn translated_minimum_camera_places_the_target_on_its_axis_in_f64() {
        let camera = Camera::new(30.0, 20.0, 0.001, 40.0, [20000.0; 3]).unwrap();
        let vertices = [[20000.0; 3]];
        let solid = Solid {
            vertices: &vertices,
            triangles: &[],
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
        let camera = Camera::new(0.0, 0.0, 2.0, 90.0, [0.0; 3]).unwrap();
        let light = Light::new([0.0, 0.0, -1.0], 0.0).unwrap();
        let cancel = AtomicBool::new(false);
        let mut out = FrameScratch::with_capacity(1);
        let mut band = raster::BandScratch::new(8).unwrap();
        for (x, expected) in [(0.25, 1), (0.250_488_281_25, 1), (0.250_976_562_5, 0)] {
            let vertices = [[x, -1.0, 0.0], [1.5, -1.0, 0.0], [x, 1.0, 0.0]];
            let solid = Solid {
                vertices: &vertices,
                triangles: &[[0, 1, 2]],
                faces: &[FaceRange {
                    anchors: [0, 1, 2],
                    start: 0,
                    len: 1,
                }],
                pose: Transform::local([0.0; 3], [0.0; 3], 1.0),
                rgb: [255; 3],
                opacity: 1.0,
            };
            frame(&[solid], &camera, &light, [8, 8], &mut out, &cancel).unwrap();
            let (mut color, mut ids) = ([0; 64], [0; 64]);
            raster::raster(
                &mut out,
                raster::Quality::Motion,
                &mut band,
                raster::Target {
                    width: 8,
                    height: 8,
                    color: &mut color,
                    ids: &mut ids,
                },
                &cancel,
            )
            .unwrap();
            assert_eq!(ids[4 * 8 + 4], expected, "sample on the moving left edge");
            assert_eq!(ids[4 * 8 + 5], 1, "the triangle paints past the left edge");
        }
    }

    #[test]
    fn near_clip_has_exact_area_and_emits_on_plane_once() {
        let mut poly = polygon(&[[0.0, 0.0, 0.0], [4.0, 0.0, 2.0], [0.0, 4.0, 2.0]]);
        assert_eq!(clip(&mut poly, 3, [0.0, 0.0, 1.0], -1.0), Ok(4));
        assert_winding(
            &poly[..4],
            &[
                [2.0, 0.0, 1.0],
                [4.0, 0.0, 2.0],
                [0.0, 4.0, 2.0],
                [0.0, 2.0, 1.0],
            ],
        );
        let twice_area: f64 = (0..4)
            .map(|i| poly[i].p[0] * poly[(i + 1) % 4].p[1] - poly[(i + 1) % 4].p[0] * poly[i].p[1])
            .sum();
        assert_eq!(twice_area.to_bits(), 12.0_f64.to_bits());
        let mut on = polygon(&[[0.0, 0.0, 1.0], [4.0, 0.0, 2.0], [0.0, 4.0, 0.0]]);
        assert_eq!(clip(&mut on, 3, [0.0, 0.0, 1.0], -1.0), Ok(3));
        assert_winding(
            &on[..3],
            &[[0.0, 0.0, 1.0], [4.0, 0.0, 2.0], [2.0, 2.0, 1.0]],
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
