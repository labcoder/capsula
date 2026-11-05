//! Scene v1 admission and ingress limits (architecture §4.4).

/// Maximum encoded bytes in one record.
pub const MAX_RECORD_BYTES: usize = 32 * 1024;
/// Maximum encoded bytes in one drawing.
pub const MAX_DRAWING_BYTES: usize = 512 * 1024;
/// Maximum JSON nesting depth.
pub const MAX_JSON_DEPTH: usize = 16;
/// Maximum ASCII characters in an id.
pub const MAX_ID_CHARS: usize = 40;
/// Maximum Unicode scalar values in text.
pub const MAX_TEXT_CHARS: usize = 400;
/// Maximum UTF-8 bytes in path data or Mermaid source.
pub const MAX_SOURCE_BYTES: usize = 8 * 1024;
/// Maximum committed steps per scene.
pub const MAX_STEPS: usize = 64;
/// Maximum live objects per scene, including pending removals.
pub const MAX_LIVE_OBJECTS: usize = 600;
/// Maximum live objects across visible scenes.
pub const MAX_GLOBAL_LIVE_OBJECTS: usize = 1_500;
/// Maximum visible scenes.
pub const MAX_VISIBLE_SCENES: usize = 4;
/// Maximum absolute composed canvas coordinate.
pub const MAX_COORD: f32 = 20_000.0;
/// Shared chart title, legend and plot inset, also reserved at admission.
pub(crate) const CHART_LABEL_INSET: f32 = 12.0;
/// Maximum composed scale, including objects whose dimensions are measured later.
pub const MAX_COMPOSED_SCALE: f32 = 64.0;
/// Maximum nodes in one diagram.
pub const MAX_DIAGRAM_NODES: usize = 48;
/// Maximum edges in one diagram.
pub const MAX_DIAGRAM_EDGES: usize = 96;
/// Maximum columns in a table.
pub const MAX_TABLE_COLUMNS: usize = 8;
/// Maximum rows in a table.
pub const MAX_TABLE_ROWS: usize = 40;
/// Maximum cells across visible scenes.
pub const MAX_GLOBAL_TABLE_CELLS: usize = 4_000;
/// Maximum series in one chart.
pub const MAX_CHART_SERIES: usize = 8;
/// Maximum points in one series.
pub const MAX_SERIES_POINTS: usize = 500;
/// Maximum categories in one chart.
pub const MAX_CHART_CATEGORIES: usize = 40;
/// Maximum chart marks across visible scenes.
pub const MAX_GLOBAL_CHART_MARKS: usize = 20_000;
/// Maximum points in one path.
pub const MAX_PATH_POINTS: usize = 512;
/// Maximum flattened segments in one path.
pub const MAX_PATH_SEGMENTS: usize = 4_096;
/// Maximum flattened path segments across visible scenes.
pub const MAX_GLOBAL_PATH_SEGMENTS: usize = 20_000;
/// Maximum tracks per scene.
pub const MAX_TRACKS: usize = 256;
/// Maximum put operations in one step: objects plus tracks.
pub const MAX_STEP_OPS: usize = MAX_LIVE_OBJECTS + MAX_TRACKS;
/// Maximum keys in one track.
pub const MAX_KEYS: usize = 32;
/// Maximum object-graph depth.
pub const MAX_OBJECT_DEPTH: usize = 16;
/// Maximum track duration and scene motion after its last activation.
pub const MAX_MOTION_MS: u32 = 60_000;
/// Maximum prepared residency across visible scenes.
pub const MAX_RESIDENT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum decoded image residency across visible scenes.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Reserved slice 5 cap: solids per scene.
pub const MAX_SOLIDS: usize = 160;
/// Reserved slice 5 cap: triangles per scene and globally.
pub const MAX_TRIANGLES: usize = 24_000;
/// Slice 5 guess: triangles in a view with a `pos3` or `rot3` track.
pub const MAX_MOVING_TRIANGLES: usize = 6_000;
/// D1 restriction: 3D views per scene.
pub const MAX_VIEWS: usize = 1;
/// Reserved slice 5 cap: segments per curved solid.
pub const MAX_CURVED_SEGMENTS: usize = 48;
