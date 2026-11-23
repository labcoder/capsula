//! Allocation-free triangle generation using caller-owned reusable scratch.

use super::{
    REL, Transform, Triangle, dist_eps,
    size::{MeshError, Retained, area_cutoff, orient, radius, triangles},
};
use crate::{
    geom::Vec3,
    limits::{MAX_CURVED_SEGMENTS, MAX_PATH_POINTS},
    record::{Geometry, Segments, Solid},
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Candidate triangles emitted or dropped at a lathe's axis.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Built {
    /// Appended triangles.
    pub written: u32,
    /// Degenerate candidates at axis-touching lathe bands.
    pub dropped: u32,
}

type Point = [f64; 3];

/// Caller-owned fixed-capacity buffers reused by every solid build.
pub struct MeshScratch {
    keys: Box<[[u32; 4]]>,
    tangents: Box<[Point]>,
    frames: Box<[[Point; 2]]>,
    vertices: Box<[Point]>,
    faces: Box<[[u16; 3]]>,
}
impl std::fmt::Debug for MeshScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeshScratch(..)")
    }
}
impl Default for MeshScratch {
    fn default() -> Self {
        Self::new()
    }
}
impl MeshScratch {
    /// Bytes the reserved buffers hold.
    pub fn bytes(&self) -> usize {
        size_of_val(&*self.keys)
            + size_of_val(&*self.tangents)
            + size_of_val(&*self.frames)
            + size_of_val(&*self.vertices)
            + size_of_val(&*self.faces)
    }
    /// Reserves all buffers once at the decoded shape and subdivision limits.
    pub fn new() -> Self {
        Self {
            keys: vec![[0; 4]; MAX_PATH_POINTS * MAX_CURVED_SEGMENTS].into_boxed_slice(),
            tangents: vec![[0.0; 3]; MAX_PATH_POINTS].into_boxed_slice(),
            frames: vec![[[0.0; 3]; 2]; MAX_PATH_POINTS].into_boxed_slice(),
            vertices: vec![[0.0; 3]; 642].into_boxed_slice(),
            faces: vec![[0; 3]; 1280].into_boxed_slice(),
        }
    }
}

/// Narrows a triangle to world f32 and refuses it when the narrowed points have zero area: the one
/// check the world build and the prepared mesh share, so both accept exactly the same triangles.
pub(super) fn narrowed(world: &Transform, v: [Point; 3]) -> Result<[Vec3; 3], MeshError> {
    let world = [
        world.point64(v[0])?,
        world.point64(v[1])?,
        world.point64(v[2])?,
    ];
    let p = world.map(|p| [f64::from(p.x), f64::from(p.y), f64::from(p.z)]);
    let normal = cross(subtract(p[1], p[0]), subtract(p[2], p[0]));
    if dot(normal, normal).to_bits() == 0 {
        return Err(MeshError::Degenerate("pos"));
    }
    Ok(world)
}

/// Bit-for-bit equality: a construction fact, not a tolerance.
fn same(a: Point, b: Point) -> bool {
    a.map(f64::to_bits) == b.map(f64::to_bits)
}

fn subtract(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] - b[i])
}
fn dot(a: Point, b: Point) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}
fn cross(a: Point, b: Point) -> Point {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn unit(p: Point) -> Result<Point, MeshError> {
    let length = dot(p, p).sqrt();
    if !length.is_finite() || length == 0.0 {
        return Err(MeshError::Degenerate("path"));
    }
    Ok(p.map(|v| v / length))
}
fn poll(cancel: &AtomicBool) -> Result<(), MeshError> {
    if cancel.load(Ordering::Relaxed) {
        Err(MeshError::Cancelled)
    } else {
        Ok(())
    }
}
fn ring(r: f64, y: f64, i: usize, n: usize) -> Point {
    if r == 0.0 {
        return [0.0, y, 0.0];
    }
    let (sin, cos) = (std::f64::consts::TAU
        * f64::from(u32::try_from(i % n).expect("ring index is bounded"))
        / f64::from(u32::try_from(n).expect("segments are bounded")))
    .sin_cos();
    [r * cos, y, r * sin]
}

fn check_vertices(
    count: usize,
    point: &impl Fn(usize) -> Point,
    world: &Transform,
    cancel: &AtomicBool,
    keys: &mut [[u32; 4]],
) -> Result<(), MeshError> {
    for (i, key) in keys[..count].iter_mut().enumerate() {
        poll(cancel)?;
        let p = world.point64(point(i))?;
        *key = [
            p.x.to_bits(),
            p.y.to_bits(),
            p.z.to_bits(),
            u32::try_from(i).expect("vertex index is bounded"),
        ];
    }
    for root in (0..count / 2).rev() {
        sift(&mut keys[..count], root, cancel)?;
    }
    for end in (1..count).rev() {
        keys.swap(0, end);
        sift(&mut keys[..end], 0, cancel)?;
    }
    for pair in keys[..count].windows(2) {
        poll(cancel)?;
        if pair[0][..3] == pair[1][..3]
            && exact_point(point(usize::try_from(pair[0][3]).expect("bounded vertex")))
                != exact_point(point(usize::try_from(pair[1][3]).expect("bounded vertex")))
        {
            return Err(MeshError::Degenerate("pos"));
        }
    }
    poll(cancel)
}

fn exact_point(point: Point) -> [u64; 3] {
    point.map(|v| if v == 0.0 { 0 } else { v.to_bits() })
}

fn sift(keys: &mut [[u32; 4]], mut root: usize, cancel: &AtomicBool) -> Result<(), MeshError> {
    poll(cancel)?;
    while root * 2 + 1 < keys.len() {
        let mut child = root * 2 + 1;
        if child + 1 < keys.len() && keys[child][..3] < keys[child + 1][..3] {
            child += 1;
        }
        if keys[root][..3] >= keys[child][..3] {
            break;
        }
        keys.swap(root, child);
        root = child;
    }
    Ok(())
}

/// Where checked triangles go: world f32 triangles, or the unscaled local f64 points that
/// produced them, with one group per source face.
enum Sink<'a> {
    World(&'a mut Vec<Triangle>),
    Local {
        triangles: &'a mut Vec<[Point; 3]>,
        groups: &'a mut Vec<u8>,
    },
}
impl Sink<'_> {
    fn spare(&self) -> usize {
        match self {
            Self::World(out) => out.capacity() - out.len(),
            Self::Local { triangles, .. } => triangles.capacity() - triangles.len(),
        }
    }
}

struct Emitter<'a> {
    world: &'a Transform,
    sink: Sink<'a>,
    cancel: &'a AtomicBool,
    part: u16,
    cutoff: f64,
    field: &'static str,
    built: Built,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    Surface,
    Axis,
}

impl Emitter<'_> {
    fn triangle(&mut self, v: [Point; 3], face: Face) -> Result<(), MeshError> {
        let before = self.built.written;
        self.emit(v, face)?;
        self.group(before);
        Ok(())
    }
    fn group(&mut self, before: u32) {
        if let Sink::Local { groups, .. } = &mut self.sink
            && self.built.written > before
        {
            groups.push(
                u8::try_from(self.built.written - before).expect("a source face emits at most two"),
            );
        }
    }
    fn emit(&mut self, v: [Point; 3], face: Face) -> Result<(), MeshError> {
        poll(self.cancel)?;
        let normal = cross(subtract(v[1], v[0]), subtract(v[2], v[0]));
        if dot(normal, normal) < 4.0 * self.cutoff * self.cutoff {
            let keys = v.map(exact_point);
            if face == Face::Axis
                && (keys[0] == keys[1] || keys[1] == keys[2] || keys[2] == keys[0])
            {
                self.built.dropped += 1;
                return Ok(());
            }
            return Err(MeshError::Degenerate(self.field));
        }
        let world = narrowed(self.world, v)?;
        match &mut self.sink {
            Sink::World(out) => out.push(Triangle {
                v: world,
                part: self.part,
            }),
            Sink::Local { triangles, .. } => triangles.push(v),
        }
        self.built.written += 1;
        Ok(())
    }
    fn quad(&mut self, v: [Point; 4], face: Face) -> Result<(), MeshError> {
        let before = self.built.written;
        self.emit([v[0], v[1], v[2]], face)?;
        self.emit([v[0], v[2], v[3]], face)?;
        self.group(before);
        Ok(())
    }
}

/// Appends a solid's outward-wound triangles using preallocated output and reusable scratch.
///
/// `Full` preserves the entire existing prefix. `Cancelled` and `Degenerate` may leave appended
/// triangles: callers must save `out.len()` and truncate back to it after either failure.
/// On success, `written + dropped == triangles(solid)`; only lathe axis candidates may be dropped.
/// # Errors
/// Rejects invalid geometry, insufficient spare capacity, cancellation, sub-cutoff source faces,
/// non-finite world vertices, collapsed world faces or distinct local vertices that narrow together.
pub fn build(
    solid: &Solid,
    world: &Transform,
    part: u16,
    out: &mut Vec<Triangle>,
    cancel: &AtomicBool,
    scratch: &mut MeshScratch,
) -> Result<Built, MeshError> {
    emit(solid, world, Sink::World(out), part, cancel, scratch)
}

/// Like [`build`], with every check at the complete rest transform `rest`, but appends each
/// triangle as the solid's own unscaled f64 points, which `rest` places exactly where [`build`]
/// checked them, and one entry per source face to `groups`: how many of the appended triangles
/// that face emitted. On failure, truncate both to their saved lengths.
/// # Errors
/// As [`build`].
pub fn build_local(
    solid: &Solid,
    rest: &Transform,
    triangles: &mut Vec<[[f64; 3]; 3]>,
    groups: &mut Vec<u8>,
    cancel: &AtomicBool,
    scratch: &mut MeshScratch,
) -> Result<Built, MeshError> {
    emit(
        solid,
        rest,
        Sink::Local { triangles, groups },
        0,
        cancel,
        scratch,
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "one dispatch keeps each shape's canonical vertex expression beside its topology"
)]
fn emit(
    solid: &Solid,
    world: &Transform,
    sink: Sink<'_>,
    part: u16,
    cancel: &AtomicBool,
    scratch: &mut MeshScratch,
) -> Result<Built, MeshError> {
    let MeshScratch {
        keys,
        tangents,
        frames,
        vertices,
        faces,
    } = scratch;
    poll(cancel)?;
    let candidates = triangles(solid)?;
    if sink.spare() < usize::try_from(candidates).map_err(|_| MeshError::Full)? {
        return Err(MeshError::Full);
    }
    let shape = solid.geometry()?;
    let n = usize::from(solid.segments.map_or(24, Segments::get));
    let eps = dist_eps(radius(shape));
    let field = match shape {
        Geometry::Box { .. } => "size",
        Geometry::Extrude { .. } | Geometry::Lathe { .. } => "points",
        Geometry::Tube { .. } => "path",
        Geometry::Torus { .. } => "r2",
        _ => "r",
    };
    let mut emit = Emitter {
        world,
        sink,
        cancel,
        part,
        cutoff: area_cutoff(eps),
        field,
        built: Built::default(),
    };
    match shape {
        Geometry::Box { size } => {
            let size = size.map(|s| f64::from(s) / 2.0);
            let point =
                |i| std::array::from_fn(|d| if i & (1 << d) == 0 { -size[d] } else { size[d] });
            check_vertices(8, &point, world, cancel, keys)?;
            for face in [
                [0, 2, 3, 1],
                [4, 5, 7, 6],
                [0, 4, 6, 2],
                [1, 3, 7, 5],
                [0, 1, 5, 4],
                [2, 6, 7, 3],
            ] {
                // A box face is an axis-aligned rectangle.
                emit.quad(face.map(point), Face::Surface)?;
            }
        }
        Geometry::Sphere { r } => sphere(r, candidates, &mut emit, keys, vertices, faces)?,
        Geometry::Cylinder { r, h } => revolved(r, r, h, n, &mut emit, keys)?,
        Geometry::Cone { r, top, h } => revolved(r, top, h, n, &mut emit, keys)?,
        Geometry::Torus { ring: major, tube } => {
            let point = |i: usize| {
                let circle = ring(f64::from(tube), 0.0, i % n, n);
                ring(f64::from(major) + circle[0], circle[2], i / n, n)
            };
            check_vertices(n * n, &point, world, cancel, keys)?;
            for i in 0..n {
                for j in 0..n {
                    // Both rings are circles about one axis, so the band is a mirror-symmetric trapezoid.
                    emit.quad(quad_indices(i, (i + 1) % n, j, n).map(point), Face::Surface)?;
                }
            }
        }
        Geometry::Extrude { outline, depth } => {
            let mut points = Retained::new(outline, eps);
            points.close(eps);
            extrude(&points, depth, &mut emit, keys)?;
        }
        Geometry::Lathe { profile } => {
            let points = Retained::new(profile, eps);
            let point = |i: usize| {
                let [r, y] = points.point(i / n);
                ring(r, y, i % n, n)
            };
            check_vertices(points.len * n, &point, world, cancel, keys)?;
            for i in 0..points.len - 1 {
                for j in 0..n {
                    let face = if points.point(i)[0] == 0.0 || points.point(i + 1)[0] == 0.0 {
                        Face::Axis
                    } else {
                        Face::Surface
                    };
                    let mut corners = quad_indices(i, i + 1, j, n);
                    corners[1..].reverse();
                    // Both rings are circles about one axis, so the band is a mirror-symmetric trapezoid.
                    emit.quad(corners.map(point), face)?;
                }
            }
        }
        Geometry::Tube { path, thickness } => {
            tube(
                &Retained::new(path, eps),
                thickness,
                n,
                &mut emit,
                keys,
                tangents,
                frames,
            )?;
        }
    }
    poll(cancel)?;
    Ok(emit.built)
}

fn quad_indices(row: usize, next: usize, column: usize, n: usize) -> [usize; 4] {
    [
        row * n + column,
        row * n + (column + 1) % n,
        next * n + (column + 1) % n,
        next * n + column,
    ]
}

fn revolved(
    r: f32,
    top: f32,
    h: f32,
    n: usize,
    emit: &mut Emitter<'_>,
    keys: &mut [[u32; 4]],
) -> Result<(), MeshError> {
    let point = |i: usize| {
        ring(
            f64::from(if i < n { r } else { top }),
            f64::from(h) * if i < n { -0.5 } else { 0.5 },
            i % n,
            n,
        )
    };
    check_vertices(2 * n, &point, emit.world, emit.cancel, keys)?;
    for i in 0..n {
        if top == 0.0 {
            emit.triangle([point(i), point(n), point((i + 1) % n)], Face::Surface)?;
        } else {
            // Both rings are circles about one axis, so the side is a mirror-symmetric trapezoid.
            emit.quad(
                [i, n + i, n + (i + 1) % n, (i + 1) % n].map(point),
                Face::Surface,
            )?;
        }
    }
    for i in 1..n - 1 {
        emit.triangle([0, i, i + 1].map(point), Face::Surface)?;
        if top != 0.0 {
            emit.triangle([n, n + i + 1, n + i].map(point), Face::Surface)?;
        }
    }
    Ok(())
}

fn extrude(
    points: &Retained<'_, 2>,
    depth: f32,
    emit: &mut Emitter<'_>,
    keys: &mut [[u32; 4]],
) -> Result<(), MeshError> {
    let n = points.len;
    let point = |i: usize| {
        let [x, y] = points.point(i % n);
        [x, y, f64::from(depth) * if i < n { -0.5 } else { 0.5 }]
    };
    check_vertices(2 * n, &point, emit.world, emit.cancel, keys)?;
    let mut indices = [0_u8; crate::limits::MAX_EXTRUDE_POINTS];
    for (i, index) in indices[..n].iter_mut().enumerate() {
        *index = u8::try_from(i).expect("the outline cap fits u8");
    }
    let area: f64 = (1..n - 1)
        .map(|i| orient(points.point(0), points.point(i), points.point(i + 1)))
        .sum();
    if area < 0.0 {
        indices[..n].reverse();
    }
    for i in 0..n {
        let a = usize::from(indices[i]);
        let b = usize::from(indices[(i + 1) % n]);
        // A side joins one outline edge to its copy translated along z: a rectangle.
        emit.quad([a, b, b + n, a + n].map(point), Face::Surface)?;
    }
    let mut len = n;
    let mut ears = [false; crate::limits::MAX_EXTRUDE_POINTS];
    for (i, ear) in ears[..len].iter_mut().enumerate() {
        poll(emit.cancel)?;
        *ear = is_ear(&indices[..len], i, points);
    }
    while len > 3 {
        poll(emit.cancel)?;
        let i = ears[..len]
            .iter()
            .position(|&ear| ear)
            .ok_or(MeshError::Degenerate("points"))?;
        let face = [
            indices[(i + len - 1) % len],
            indices[i],
            indices[(i + 1) % len],
        ]
        .map(usize::from);
        emit.triangle(face.map(|i| point(i + n)), Face::Surface)?;
        emit.triangle([face[2], face[1], face[0]].map(point), Face::Surface)?;
        indices.copy_within(i + 1..len, i);
        ears.copy_within(i + 1..len, i);
        len -= 1;
        for j in [(i + len - 1) % len, i % len] {
            ears[j] = is_ear(&indices[..len], j, points);
        }
    }
    let face = [indices[0], indices[1], indices[2]].map(usize::from);
    emit.triangle(face.map(|i| point(i + n)), Face::Surface)?;
    emit.triangle([face[2], face[1], face[0]].map(point), Face::Surface)
}

fn is_ear(indices: &[u8], index: usize, points: &Retained<'_, 2>) -> bool {
    let len = indices.len();
    let face = [
        indices[(index + len - 1) % len],
        indices[index],
        indices[(index + 1) % len],
    ];
    let [a, b, c] = face.map(|i| points.point(usize::from(i)));
    orient(a, b, c) > 0.0
        && indices.iter().filter(|i| !face.contains(i)).all(|&i| {
            let p = points.point(usize::from(i));
            orient(a, b, p) < 0.0 || orient(b, c, p) < 0.0 || orient(c, a, p) < 0.0
        })
}

fn tube(
    points: &Retained<'_, 3>,
    thickness: f32,
    n: usize,
    emit: &mut Emitter<'_>,
    keys: &mut [[u32; 4]],
    tangents: &mut [Point],
    frames: &mut [[Point; 2]],
) -> Result<(), MeshError> {
    for (i, tangent) in tangents[..points.len].iter_mut().enumerate() {
        poll(emit.cancel)?;
        *tangent = if i == 0 {
            unit(subtract(points.point(1), points.point(0)))?
        } else if i == points.len - 1 {
            unit(subtract(points.point(i), points.point(i - 1)))?
        } else {
            let a = unit(subtract(points.point(i), points.point(i - 1)))?;
            let b = unit(subtract(points.point(i + 1), points.point(i)))?;
            if same(a, b) {
                a
            } else {
                unit(std::array::from_fn(|d| a[d] + b[d]))?
            }
        };
    }
    let tangent = tangents[0];
    let mut axis = [0.0; 3];
    let least = (0..3)
        .min_by(|&a, &b| tangent[a].abs().total_cmp(&tangent[b].abs()))
        .expect("three axes");
    axis[least] = 1.0;
    let mut normal = unit(cross(tangent, axis))?;
    for i in 0..points.len {
        // An unchanged tangent keeps the frame bit for bit, so a straight run's rings are translations.
        if i > 0 && !same(tangents[i - 1], tangents[i]) {
            let a = tangents[i - 1];
            let b = tangents[i];
            let cosine = dot(a, b);
            if cosine < -1.0 + REL {
                return Err(MeshError::Degenerate("path"));
            }
            let turn_axis = cross(a, b);
            let turn = cross(turn_axis, normal);
            let second = cross(turn_axis, turn);
            normal = std::array::from_fn(|d| normal[d] + turn[d] + second[d] / (1.0 + cosine));
            let projection = dot(normal, b);
            normal = unit(std::array::from_fn(|d| normal[d] - projection * b[d]))?;
        }
        frames[i] = [normal, cross(tangents[i], normal)];
    }
    let point = |i: usize| {
        let p = points.point(i / n);
        let [normal, binormal] = frames[i / n];
        let circle = ring(f64::from(thickness), 0.0, i % n, n);
        std::array::from_fn(|d| p[d] + normal[d] * circle[0] + binormal[d] * circle[2])
    };
    check_vertices(points.len * n, &point, emit.world, emit.cancel, keys)?;
    for i in 0..points.len - 1 {
        // Only rings that are translations of each other bound planar bands; a bend's are split.
        let planar = same(frames[i][0], frames[i + 1][0]) && same(frames[i][1], frames[i + 1][1]);
        for j in 0..n {
            let v = quad_indices(i, i + 1, j, n).map(point);
            if planar {
                emit.quad(v, Face::Surface)?;
            } else {
                emit.triangle([v[0], v[1], v[2]], Face::Surface)?;
                emit.triangle([v[0], v[2], v[3]], Face::Surface)?;
            }
        }
    }
    let end = (points.len - 1) * n;
    for i in 1..n - 1 {
        emit.triangle([0, i + 1, i].map(point), Face::Surface)?;
        emit.triangle([end, end + i, end + i + 1].map(point), Face::Surface)?;
    }
    Ok(())
}

fn sphere(
    radius: f32,
    candidates: u32,
    emit: &mut Emitter<'_>,
    keys: &mut [[u32; 4]],
    vertices: &mut [Point],
    faces: &mut [[u16; 3]],
) -> Result<(), MeshError> {
    let phi = f64::midpoint(1.0, 5.0_f64.sqrt());
    for (i, vertex) in vertices[..12].iter_mut().enumerate() {
        let short = if i % 2 == 0 { -1.0 } else { 1.0 };
        let long = if i % 4 < 2 { phi } else { -phi };
        let mut point = [short, long, 0.0];
        point.rotate_right(i / 4);
        *vertex = unit(point)?;
    }
    faces[..20].copy_from_slice(&[
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ]);
    let mut count = 20;
    let mut used = 12;
    while count < usize::try_from(candidates).expect("sphere count is bounded") {
        for i in (0..count).rev() {
            poll(emit.cancel)?;
            let [a, b, c] = faces[i];
            let mut middle = [0_u16; 3];
            for (slot, (a, b)) in middle.iter_mut().zip([(a, b), (b, c), (c, a)]) {
                let mut ends = [vertices[usize::from(a)], vertices[usize::from(b)]];
                ends.sort_unstable_by_key(|p| local_bits(*p));
                let midpoint = unit(std::array::from_fn(|d| ends[0][d] + ends[1][d]))?;
                let id = if let Some(id) = vertices[..used]
                    .iter()
                    .position(|v| exact_point(*v) == exact_point(midpoint))
                {
                    id
                } else {
                    vertices[used] = midpoint;
                    used += 1;
                    used - 1
                };
                *slot = u16::try_from(id).expect("sphere has at most 642 vertices");
            }
            let [ab, bc, ca] = middle;
            faces[i * 4..i * 4 + 4].copy_from_slice(&[
                [a, ab, ca],
                [b, bc, ab],
                [c, ca, bc],
                [ab, bc, ca],
            ]);
        }
        count *= 4;
    }
    let point = |i: usize| vertices[i].map(|v| v * f64::from(radius));
    check_vertices(used, &point, emit.world, emit.cancel, keys)?;
    for face in &faces[..count] {
        emit.triangle(face.map(|i| point(usize::from(i))), Face::Surface)?;
    }
    Ok(())
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "canonical endpoint order is defined on the local f32 bits"
)]
fn local_bits(point: Point) -> [u32; 3] {
    point.map(|value| (value as f32).to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_area_cutoff_is_inclusive_and_cancellation_stops_the_next_triangle() {
        let world = Transform::local([0.0; 3], [0.0; 3], 1.0);
        for (height, expected, emitted) in [
            (0.25_f32.next_down(), Err(MeshError::Degenerate("r")), 0),
            (0.25, Ok(()), 1),
            (0.25_f32.next_up(), Ok(()), 1),
        ] {
            let cancel = AtomicBool::new(false);
            let mut out = Vec::with_capacity(2);
            let mut emit = Emitter {
                world: &world,
                sink: Sink::World(&mut out),
                cancel: &cancel,
                part: 1,
                cutoff: 0.125,
                field: "r",
                built: Built::default(),
            };
            let face = [
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, f64::from(height), 0.0],
            ];
            assert_eq!(
                emit.triangle(face, Face::Surface),
                expected,
                "height {height}"
            );
            assert_eq!(out.len(), emitted, "height {height}");
            for triangle in out {
                assert_eq!(triangle.part, 1);
                let [a, b, c] = [
                    Vec3::new(0.0, 0.0, 0.0),
                    Vec3::new(1.0, 0.0, 0.0),
                    Vec3::new(0.0, height, 0.0),
                ];
                assert!(
                    [[a, b, c], [b, c, a], [c, a, b]].contains(&triangle.v),
                    "height {height}: wrong vertices or winding, {triangle:?}"
                );
            }
        }
        let cancel = AtomicBool::new(false);
        let mut out = Vec::with_capacity(2);
        let mut emit = Emitter {
            world: &world,
            sink: Sink::World(&mut out),
            cancel: &cancel,
            part: 1,
            cutoff: 1e-12,
            field: "r",
            built: Built::default(),
        };
        let face = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert_eq!(emit.triangle(face, Face::Surface), Ok(()));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            emit.triangle(face, Face::Surface),
            Err(MeshError::Cancelled)
        );
        assert_eq!(
            emit.built,
            Built {
                written: 1,
                dropped: 0
            }
        );
        assert!(matches!(&emit.sink, Sink::World(out) if out.len() == 1));
    }
}
