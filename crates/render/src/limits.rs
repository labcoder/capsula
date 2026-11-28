//! Every size the renderer reserves and every tessellation setting, in one place. Each buffer
//! is reserved once at these sizes, whether or not a drawing shows, and [`FIXED_BYTES`] is
//! what `Stager::fixed_bytes` charges for all of them.

use kumochi_scene::limits::{
    MAX_DRAWING_BYTES, MAX_GLOBAL_FILL_VERTICES, MAX_GLOBAL_LIVE_OBJECTS,
    MAX_GLOBAL_OUTLINE_POINTS, MAX_GLOBAL_PATH_SEGMENTS, MAX_GLOBAL_TABLE_CELLS, MAX_LIVE_OBJECTS,
    MAX_TEXT_CHARS, MAX_VISIBLE_SCENES,
};
use lyon::tessellation::{FillOptions, LineCap, LineJoin, StrokeOptions};

use crate::stage::{BuildInput, FrameBuf, Mesh};

/// Pictures in one frame, live and frozen together: main's combined limit.
pub const PICTURES: usize = MAX_VISIBLE_SCENES;
/// Records in one frame: both paint passes of every object of every picture.
pub const RECORDS: usize = PICTURES * 2 * MAX_LIVE_OBJECTS;
/// Frame buffers handed back and forth; with all of them out, the owner skips a sample.
pub const FRAME_BUFFERS: usize = 3;

/// Objects in one build input: the global object cap holds across every listed picture.
pub const INPUT_OBJECTS: usize = MAX_GLOBAL_LIVE_OBJECTS;
/// The fewest fallback outline points the scene charges a rect, `(0 + 1) * 4 + 1`.
const RECT_POINTS: usize = 5;
/// Rects with a fill: each charges at least [`RECT_POINTS`] fill vertices.
const FILLED_RECTS: usize = MAX_GLOBAL_FILL_VERTICES / RECT_POINTS;
/// Primitive headers in one build input, the most the global caps admit. Each producer charges
/// every line it emits at least one path segment, and gives an object no more texts, lines and
/// unfilled rects than its segment charge plus two, except a pie chart's title and eight legend
/// names, which charge none; every chart paints a filled background. Filled and bordered rects
/// and fills are paint work: a rect charges at least five points, a fill at least two. So
/// 20,000 segments + 2 × 1,500 objects + 7 × 409 pie charts + 409 filled rects + 153 bordered
/// rects + 1,024 fills = 27,449.
pub const INPUT_PRIMS: usize = MAX_GLOBAL_PATH_SEGMENTS
    + 2 * MAX_GLOBAL_LIVE_OBJECTS
    + 8 * FILLED_RECTS
    + MAX_GLOBAL_OUTLINE_POINTS / RECT_POINTS
    + MAX_GLOBAL_FILL_VERTICES / 2;
/// Line and fill points in one build input. Fill points are charged against the fill-vertex
/// cap; a polyline of n points is charged at least n − 1 path segments, so n ≤ 2(n − 1).
pub const INPUT_POINTS: usize = 2 * MAX_GLOBAL_PATH_SEGMENTS + MAX_GLOBAL_FILL_VERTICES;
/// The scene's charge for a line, area, bar or scatter chart's axes: no more than
/// `MAX_GLOBAL_PATH_SEGMENTS / AXIS_SEGMENTS` such charts show at once.
/// shortcut: restates the scene's crate-private `CHART_AXES_SEGMENTS`, export it if it changes.
const AXIS_SEGMENTS: usize = 64;
/// The points of one pie slice's fill, centre and arc, so at most 16 slices, and labels, show.
/// shortcut: restates the scene's crate-private `PIE_ARC_SEGMENTS` + 2, export it if it changes.
const PIE_SLICE_POINTS: usize = 122;
/// UTF-8 bytes of the longest label.
const LABEL_BYTES: usize = 4 * MAX_TEXT_CHARS;
/// Label text in one build input. Layout copies each source string into at most one label but
/// generates three kinds: a table number cell formats to at most 400 ASCII bytes, an axis tick
/// is a number of at most 10 bytes (`-999999.9T`) and its axis unit, on up to seven ticks per
/// axis, and a pie label adds five bytes to its category. So 4 × 512 KiB of drawings
/// + 4,000 cells × 400 + 312 charts × (14 ticks × 10 + 2 units × 6 more copies × 1,600)
/// + 16 slices × (1,600 + 5) = 9,756,912.
pub const INPUT_TEXT_BYTES: usize = PICTURES * MAX_DRAWING_BYTES
    + MAX_GLOBAL_TABLE_CELLS * MAX_TEXT_CHARS
    + MAX_GLOBAL_PATH_SEGMENTS / AXIS_SEGMENTS * (14 * 10 + 2 * 6 * LABEL_BYTES)
    + MAX_GLOBAL_FILL_VERTICES / PIE_SLICE_POINTS * (LABEL_BYTES + 5);

/// Mesh vertices. A butt-capped, miter-joined lyon stroke emits at most 4n + 4 vertices for n
/// points, at most 12 per charged segment; fills emit at most their charged points and
/// crossings; a border ring takes two per outline point.
pub const MESH_VERTICES: usize =
    12 * MAX_GLOBAL_PATH_SEGMENTS + MAX_GLOBAL_FILL_VERTICES + 2 * MAX_GLOBAL_OUTLINE_POINTS;
/// Mesh indices: a stroke emits at most one triangle per vertex, a fill of v points at most
/// 2v, and a ring two per outline point.
pub const MESH_INDICES: usize = 3
    * (12 * MAX_GLOBAL_PATH_SEGMENTS
        + 2 * MAX_GLOBAL_FILL_VERTICES
        + 2 * MAX_GLOBAL_OUTLINE_POINTS);
/// Mesh pieces: one per drawn primitive, and a second for a rect with both a fill and a
/// border, which each charge at least five outline points.
pub const MESH_PIECES: usize = INPUT_PRIMS + MAX_GLOBAL_OUTLINE_POINTS;
/// What lyon's tessellators may keep between builds: main's accounting allowance for the same
/// tessellator, checked on its measured fixtures, not a lyon capacity bound.
pub const TESSELLATOR_BYTES: usize = 2 * 1024 * 1024;

/// Flattening tolerance, lyon's default as main's painter uses it; polygons don't flatten.
pub const TOLERANCE: f32 = FillOptions::DEFAULT_TOLERANCE;
/// Stroke joins, as main's `StrokeOptions::default()`.
pub const STROKE_JOIN: LineJoin = StrokeOptions::DEFAULT_LINE_JOIN;
/// Stroke ends, as main's `StrokeOptions::default()`.
pub const STROKE_CAP: LineCap = StrokeOptions::DEFAULT_LINE_CAP;
/// Miter limit, as main's `StrokeOptions::default()`.
pub const MITER_LIMIT: f32 = StrokeOptions::DEFAULT_MITER_LIMIT;

/// Everything the renderer holds on the CPU outside the presenter's per-scene charge.
pub const FIXED_BYTES: usize =
    FRAME_BUFFERS * FrameBuf::BYTES + BuildInput::BYTES + Mesh::BYTES + TESSELLATOR_BYTES;
/// The ceiling on [`FIXED_BYTES`] [proposal]: label raster scratch must
/// fit beside it. The derived input sizes need 22,826,192 bytes, past the proposed 16 MiB.
pub const FIXED_CEILING_BYTES: usize = 22 * 1024 * 1024;
const _: () = assert!(
    FIXED_BYTES <= FIXED_CEILING_BYTES,
    "the fixed scratch fits its proposed ceiling"
);
