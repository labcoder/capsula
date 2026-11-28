//! Every size the renderer reserves and every tessellation setting, in one place. Each buffer
//! is reserved once at these sizes, whether or not a drawing shows, and [`FIXED_BYTES`] is
//! what `Stager::fixed_bytes` charges for all of them.

use kumochi_scene::limits::{
    MAX_GLOBAL_FILL_VERTICES, MAX_GLOBAL_LIVE_OBJECTS, MAX_GLOBAL_OUTLINE_POINTS,
    MAX_GLOBAL_PATH_SEGMENTS, MAX_TEXT_CHARS, MAX_VISIBLE_SCENES,
};
use lyon::tessellation::{FillOptions, LineCap, LineJoin, StrokeOptions};

use crate::stage::{BuildInput, Built, FrameBuf, Mesh};

/// Pictures in one frame, live and frozen together: main's combined limit.
pub const PICTURES: usize = MAX_VISIBLE_SCENES;
/// Records in one frame: both paint passes of every object, and the global object cap holds
/// across every listed picture, so 2 × 1,500.
pub const RECORDS: usize = 2 * MAX_GLOBAL_LIVE_OBJECTS;
/// Frame buffers handed back and forth; with all of them out, the owner skips a sample.
pub const FRAME_BUFFERS: usize = 3;
/// Geometries uploaded at once: one per picture of a frame, and one more uploading in place of
/// a picture's while that one still draws. Each slot's range tables hold a geometry at the caps.
pub const UPLOADED: usize = PICTURES + 1;

/// Objects in one batch: the global object cap holds across every listed picture.
pub const INPUT_OBJECTS: usize = MAX_GLOBAL_LIVE_OBJECTS;
/// The fewest fallback outline points the scene charges a rect, `(0 + 1) * 4 + 1`.
const RECT_POINTS: usize = 5;
/// Rects with a fill: each charges at least [`RECT_POINTS`] fill vertices.
const FILLED_RECTS: usize = MAX_GLOBAL_FILL_VERTICES / RECT_POINTS;
/// Primitive headers in one batch, and in one mesh: the most the global caps admit. Each
/// producer charges every line it emits at least one path segment, and gives an object no more
/// texts, lines and unfilled rects than its segment charge plus two, except a pie chart's title
/// and eight legend names, which charge none; every chart paints a filled background. Filled
/// and bordered rects and fills are paint work: a rect charges at least five points, a fill at
/// least two. So 20,000 segments + 2 × 1,500 objects + 7 × 409 pie charts + 409 filled rects
/// + 153 bordered rects + 1,024 fills = 27,449.
pub const INPUT_PRIMS: usize = MAX_GLOBAL_PATH_SEGMENTS
    + 2 * MAX_GLOBAL_LIVE_OBJECTS
    + 8 * FILLED_RECTS
    + MAX_GLOBAL_OUTLINE_POINTS / RECT_POINTS
    + MAX_GLOBAL_FILL_VERTICES / 2;
/// Line and fill points in one batch. Fill points are charged against the fill-vertex cap; a
/// polyline of n points is charged at least n − 1 path segments, so n ≤ 2(n − 1). Every
/// primitive's points are part of that whole, so any one of them fits an empty batch.
pub const INPUT_POINTS: usize = 2 * MAX_GLOBAL_PATH_SEGMENTS + MAX_GLOBAL_FILL_VERTICES;
/// Label text in one batch [proposal]: room for ordinary drawings in one batch. No fixed size
/// holds every view's labels, since a continued scene keeps labels from many calls, so a view
/// past it copies in several batches.
pub const BATCH_TEXT_BYTES: usize = 256 * 1024;
const _: () = assert!(
    BATCH_TEXT_BYTES >= 4 * MAX_TEXT_CHARS,
    "the longest label fits an empty batch"
);

/// Mesh vertices. lyon 1.0.19's fixed-width stroke of lines, with miter joins and butt caps,
/// emits two vertices at each end and at each join at most two per side, one where that side
/// meets in a point. An open line of n kept points emits at most 4 + 4(n − 2) = 4(n − 1), a
/// closed one of n ≥ 3 at most 4n + 2, closing re-emits two, and the mesh copies at most one
/// seam vertex for its opening edge, since lyon folds only one side of a join: 4n + 3. A closed
/// line of two points goes out and back as two open lines of two, 8. Every line of n points
/// charges at least n − 1 segments, so 8 per segment is the most: 4n + 3 ≤ 8(n − 1) from n = 3.
/// lyon's fill sweep
/// emits one vertex per event, and the crossings it splits edges at can add events past the
/// points and crossings the scene charges, so no bound follows from its source: the fill cap
/// here is a reservation, and the mesh refuses a fill past what is left. A border band takes
/// four per outline edge.
pub const MESH_VERTICES: usize =
    8 * MAX_GLOBAL_PATH_SEGMENTS + MAX_GLOBAL_FILL_VERTICES + 4 * MAX_GLOBAL_OUTLINE_POINTS;
/// Mesh indices. A stroke emits at most two triangles per edge and two per join: an open line
/// 2(n − 1) + 2(n − 2) = 4n − 6, a closed one 4n, the out-and-back line 4, so at most 6 per
/// segment, 4n ≤ 6(n − 1) from n = 3. A fill of v vertices has no bound from lyon's source
/// either; it reserves 2v triangles. A band takes two per outline edge.
pub const MESH_INDICES: usize = 3
    * (6 * MAX_GLOBAL_PATH_SEGMENTS + 2 * MAX_GLOBAL_FILL_VERTICES + 2 * MAX_GLOBAL_OUTLINE_POINTS);
/// Mesh pieces: one per drawn primitive, a second for a rect with both a fill and a border,
/// whose border charges at least five outline points, and a second for a closed line of two
/// kept points, whose return leg is its own piece; every line charges at least one segment.
pub const MESH_PIECES: usize =
    INPUT_PRIMS + MAX_GLOBAL_OUTLINE_POINTS / RECT_POINTS + MAX_GLOBAL_PATH_SEGMENTS;
/// What lyon's tessellators may keep between builds: an allowance, main's for the same
/// tessellator, not a bound. lyon grows its own buffers, which the mesh can't cap; the
/// allocation tests measure what they keep against it.
pub const TESSELLATOR_BYTES: usize = 2 * 1024 * 1024;

/// Flattening tolerance, lyon's default as main's painter uses it; polygons don't flatten.
pub const TOLERANCE: f32 = FillOptions::DEFAULT_TOLERANCE;
/// Stroke joins, as main's `StrokeOptions::default()`.
pub const STROKE_JOIN: LineJoin = StrokeOptions::DEFAULT_LINE_JOIN;
/// Stroke ends, as main's `StrokeOptions::default()`.
pub const STROKE_CAP: LineCap = StrokeOptions::DEFAULT_LINE_CAP;
/// Miter limit, as main's `StrokeOptions::default()`.
pub const MITER_LIMIT: f32 = StrokeOptions::DEFAULT_MITER_LIMIT;

/// Everything the renderer holds on the CPU outside the presenter's per-scene charge: its
/// reserved buffers, the uploaded geometries' range tables and lyon's allowance.
pub const FIXED_BYTES: usize = FRAME_BUFFERS * FrameBuf::BYTES
    + BuildInput::BYTES
    + Mesh::BYTES
    + UPLOADED * Built::TABLE_BYTES
    + TESSELLATOR_BYTES;
/// The ceiling on [`FIXED_BYTES`] [proposal]: label raster scratch must fit beside it.
pub const FIXED_CEILING_BYTES: usize = 16 * 1024 * 1024;
const _: () = assert!(
    FIXED_BYTES <= FIXED_CEILING_BYTES,
    "the fixed scratch fits its proposed ceiling"
);
