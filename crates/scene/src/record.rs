//! The scene vocabulary. Defaults are applied at decode, never by truncation.
mod mixed;
mod three;
use crate::{
    id::{Ms, WireId},
    limits::{
        MAX_CHART_CATEGORIES, MAX_KEYS, MAX_LIVE_OBJECTS, MAX_PATH_POINTS, MAX_SERIES_POINTS,
        MAX_STEP_OPS, MAX_TABLE_COLUMNS,
    },
    text::{Bounded, Finite, Number, Source, Text},
};
pub use mixed::{Anchor, Cell, Color, KeyValue, Point, Rgb, Start, Token};
use serde::{Deserialize, Serialize};
use std::fmt;
pub use three::{
    Camera, Fov, Geometry, Length, Light, LightDir, Pitch, Segments, ShapeError, Size, Solid,
    SolidShape, Spin, Unit, View3d,
};

/// One externally tagged scene record.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Record {
    /// Opens a call for a scene.
    Begin(Begin),
    /// An atomic scene update.
    Step(Step),
    /// Closes the call.
    End(End),
}
/// The supported scene format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub enum Version {
    /// Version one.
    V1,
}
impl TryFrom<u32> for Version {
    type Error = crate::ErrorCode;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self::V1)
        } else {
            Err(crate::ErrorCode::WrongVersion)
        }
    }
}
impl From<Version> for u32 {
    fn from(_: Version) -> Self {
        1
    }
}
/// Surface vocabulary.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// Overlay.
    Overlay,
    /// Inline.
    Inline,
}
/// What a new drawing does with this thread's other visible drawings.
#[derive(
    schemars::JsonSchema, Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Others {
    /// Replace this thread's other drawings after the first step commits.
    #[default]
    Clear,
    /// Keep other drawings, subject to the global four-scene limit.
    Keep,
}
/// Begin record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Begin {
    /// v.
    pub v: Version,
    /// scene.
    pub scene: WireId,
    /// surface.
    pub surface: Surface,
    /// size.
    pub size: Option<[Finite; 2]>,
    /// title.
    pub title: Option<Text>,
    /// alt.
    pub alt: Text,
    /// Whether a new scene replaces or keeps this thread's other drawings.
    #[serde(default)]
    pub others: Others,
}
impl Begin {
    /// Whether this header continues a scene stored under `stored`. Title and alt describe the
    /// drawing and may be restated differently; every other field identifies it.
    #[must_use]
    pub fn continues(&self, stored: &Self) -> bool {
        // shortcut: the stored title and alt keep their first text and can go stale after an edit; store the latest once a stale description shows.
        let Self {
            v,
            scene,
            surface,
            size,
            title: _,
            alt: _,
            others,
        } = self;
        *v == stored.v
            && *scene == stored.scene
            && *surface == stored.surface
            && *size == stored.size
            && *others == stored.others
    }
}
/// A strict empty call terminator.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct End {}
/// Step record fields.
#[derive(schemars::JsonSchema, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// id.
    pub id: WireId,
    /// say.
    pub say: Option<Text>,
    /// put.
    #[serde(default)]
    pub put: Bounded<Op, MAX_STEP_OPS>,
    /// set.
    #[serde(default)]
    pub set: Bounded<Change, MAX_LIVE_OBJECTS>,
    /// remove.
    #[serde(default)]
    pub remove: Bounded<WireId, MAX_LIVE_OBJECTS>,
    /// start.
    #[serde(default)]
    pub start: Start,
}
impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Step")
            .field("put", &self.put.len())
            .field("set", &self.set.len())
            .field("remove", &self.remove.len())
            .finish_non_exhaustive()
    }
}
/// An externally tagged scene operation.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Line operation.
    Line(Line),
    /// Arrow operation.
    Arrow(Line),
    /// Path operation.
    Path(PathOp),
    /// Shape operation.
    Shape(Shape),
    /// Text operation.
    Text(TextOp),
    /// Callout operation.
    Callout(Callout),
    /// Highlight operation.
    Highlight(Highlight),
    /// Diagram operation.
    Diagram(Diagram),
    /// Node operation.
    Node(Node),
    /// Edge operation.
    Edge(Edge),
    /// Table operation.
    Table(Table),
    /// Row operation.
    Row(Row),
    /// Chart operation.
    Chart(Chart),
    /// Series operation.
    Series(Series),
    /// Anim operation.
    Anim(Anim),
    /// Mermaid operation.
    Mermaid(Mermaid),
    /// 3D viewport operation.
    #[schemars(skip)]
    View3d(View3d),
    /// 3D solid operation.
    #[schemars(skip)]
    Solid(Solid),
}
/// Enter vocabulary.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enter {
    /// None.
    None,
    /// Fade.
    Fade,
    /// Draw.
    Draw,
    /// Pop.
    Pop,
    /// Slide.
    Slide,
    /// Grow.
    Grow,
    /// Fly.
    Fly,
}
/// Head vocabulary.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Head {
    /// None.
    None,
    /// End.
    End,
    /// Start.
    Start,
    /// Both.
    Both,
}
/// `ShapeKind` vocabulary.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    /// Rect.
    Rect,
    /// Ellipse.
    Ellipse,
    /// Circle.
    Circle,
    /// Polygon.
    Polygon,
}
/// Align vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    /// Left.
    Left,
    #[default]
    /// Center.
    Center,
    /// Right.
    Right,
}
/// Side vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    #[default]
    /// Center.
    Center,
    /// Top.
    Top,
    /// Bottom.
    Bottom,
    /// Left.
    Left,
    /// Right.
    Right,
    /// `TopLeft`.
    TopLeft,
    /// `TopRight`.
    TopRight,
    /// `BottomLeft`.
    BottomLeft,
    /// `BottomRight`.
    BottomRight,
}
/// `HighlightStyle` vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HighlightStyle {
    #[default]
    /// Ring.
    Ring,
    /// Box.
    Box,
    /// Spotlight.
    Spotlight,
    /// Underline.
    Underline,
    /// Marker.
    Marker,
}
/// Direction vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    /// Right.
    Right,
    /// Down.
    Down,
}
/// `NodeShape` vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeShape {
    #[default]
    /// Box.
    Box,
    /// Round.
    Round,
    /// Pill.
    Pill,
    /// Diamond.
    Diamond,
    /// Circle.
    Circle,
    /// Cylinder.
    Cylinder,
    /// Note.
    Note,
}
/// `ChartKind` vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    #[default]
    /// Bar.
    Bar,
    /// Line.
    Line,
    /// Area.
    Area,
    /// Scatter.
    Scatter,
    /// Pie.
    Pie,
}
/// `AxisScale` vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AxisScale {
    #[default]
    /// Linear.
    Linear,
    /// Log.
    Log,
}
/// Track vocabulary.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Track {
    /// X.
    X,
    /// Y.
    Y,
    /// Pos.
    Pos,
    /// Opacity.
    Opacity,
    /// Scale.
    Scale,
    /// Rotate.
    Rotate,
    /// Draw.
    Draw,
    /// Color.
    Color,
    /// 3D position.
    #[schemars(skip)]
    Pos3,
    /// 3D rotation.
    #[schemars(skip)]
    Rot3,
}
/// Ease vocabulary.
#[derive(
    schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Ease {
    /// Linear.
    Linear,
    /// In.
    In,
    /// Out.
    Out,
    #[default]
    /// `InOut`.
    #[serde(rename = "inout")]
    InOut,
    /// Spring.
    Spring,
}
/// Change record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    /// target.
    pub target: WireId,
    /// text.
    pub text: Option<Text>,
    /// label.
    pub label: Option<Text>,
    /// color.
    pub color: Option<Color>,
    /// cells.
    pub cells: Option<Bounded<Cell, MAX_TABLE_COLUMNS>>,
    /// data.
    pub data: Option<Bounded<Point, MAX_SERIES_POINTS>>,
    /// emphasis.
    pub emphasis: Option<bool>,
    /// Descriptive text of a solid.
    #[schemars(skip)]
    pub info: Option<Text>,
}
/// Line record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// from.
    pub from: Anchor,
    /// to.
    pub to: Anchor,
    /// via.
    #[serde(default)]
    pub via: Bounded<[Finite; 2], MAX_PATH_POINTS>,
    /// Stroke width, above 0.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub width: Option<Finite>,
    /// dash.
    #[serde(default)]
    pub dash: bool,
    /// head.
    pub head: Option<Head>,
    /// bend.
    pub bend: Option<Finite>,
}
/// `PathOp` record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathOp {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// d.
    pub d: Source,
    /// at.
    pub at: Option<Anchor>,
    /// scale.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub scale: Option<Finite>,
    /// fill.
    pub fill: Option<Color>,
    /// Stroke width, above 0.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub width: Option<Finite>,
    /// dash.
    #[serde(default)]
    pub dash: bool,
}
/// Shape record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// kind.
    pub kind: ShapeKind,
    /// bounds.
    #[serde(rename = "box")]
    pub bounds: Option<[Finite; 4]>,
    /// at.
    pub at: Option<Anchor>,
    /// size.
    pub size: Option<[Finite; 2]>,
    /// r.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub r: Option<Finite>,
    /// points.
    #[serde(default)]
    pub points: Bounded<[Finite; 2], MAX_PATH_POINTS>,
    /// radius.
    #[schemars(extend("minimum" = 0))]
    pub radius: Option<Finite>,
    /// fill.
    pub fill: Option<Color>,
    /// Outline width; 0 draws no outline.
    #[schemars(extend("minimum" = 0))]
    pub width: Option<Finite>,
    /// dash.
    #[serde(default)]
    pub dash: bool,
    /// label.
    pub label: Option<Text>,
}
/// `TextOp` record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextOp {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// at.
    pub at: Anchor,
    /// text.
    pub text: Text,
    /// size.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub size: Option<Finite>,
    /// bold.
    #[serde(default)]
    pub bold: bool,
    /// align.
    #[serde(default)]
    pub align: Align,
    /// max width.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub max_width: Option<Finite>,
}
/// Callout record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Callout {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// target.
    pub target: Anchor,
    /// text.
    pub text: Text,
    /// side.
    pub side: Option<Side>,
    /// at.
    pub at: Option<Anchor>,
    /// max width.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub max_width: Option<Finite>,
}
/// Highlight record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Highlight {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// target.
    pub target: Anchor,
    /// style.
    #[serde(default)]
    pub style: HighlightStyle,
    /// pad.
    #[schemars(extend("exclusiveMinimum" = 0))]
    pub pad: Option<Finite>,
}
/// Diagram record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagram {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// bounds.
    #[serde(rename = "box")]
    pub bounds: Option<[Finite; 4]>,
    /// dir.
    pub dir: Option<Direction>,
}
/// Node record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// id.
    pub id: WireId,
    /// Diagram id; omit for the scene root, which lays out its nodes as one diagram.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// label.
    pub label: Text,
    /// sub.
    pub sub: Option<Text>,
    /// shape.
    #[serde(default)]
    pub shape: NodeShape,
}
/// Edge record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// id.
    pub id: WireId,
    /// Diagram id; omit to derive the endpoints' nearest common diagram, or the scene root.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// from.
    pub from: WireId,
    /// to.
    pub to: WireId,
    /// label.
    pub label: Option<Text>,
    /// dash.
    #[serde(default)]
    pub dash: bool,
    /// flow.
    pub flow: Option<Ms>,
}
/// Table record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// bounds.
    #[serde(rename = "box")]
    pub bounds: Option<[Finite; 4]>,
    /// cols.
    pub cols: Bounded<Text, MAX_TABLE_COLUMNS>,
    /// align.
    #[serde(default)]
    pub align: Bounded<Align, MAX_TABLE_COLUMNS>,
    /// title.
    pub title: Option<Text>,
}
/// Row record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// id.
    pub id: WireId,
    /// Required table id; the table defines this row's columns.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// cells.
    pub cells: Bounded<Cell, MAX_TABLE_COLUMNS>,
    /// emphasis.
    #[serde(default)]
    pub emphasis: bool,
}
/// Axis record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    /// label.
    pub label: Option<Text>,
    /// min.
    pub min: Option<Number>,
    /// max.
    pub max: Option<Number>,
    /// unit.
    pub unit: Option<Text>,
    /// scale.
    #[serde(default)]
    pub scale: AxisScale,
}
impl Default for Axis {
    fn default() -> Self {
        Self {
            label: None,
            min: None,
            max: None,
            unit: None,
            scale: AxisScale::Linear,
        }
    }
}
/// Chart record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chart {
    /// id.
    pub id: WireId,
    /// parent.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// bounds.
    #[serde(rename = "box")]
    pub bounds: Option<[Finite; 4]>,
    /// kind.
    #[serde(default)]
    pub kind: ChartKind,
    /// title.
    pub title: Option<Text>,
    /// categories.
    #[serde(default)]
    pub categories: Bounded<Text, MAX_CHART_CATEGORIES>,
    /// x.
    #[serde(default)]
    pub x: Axis,
    /// y.
    #[serde(default)]
    pub y: Axis,
    /// stacked.
    #[serde(default)]
    pub stacked: bool,
}
/// Series record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Series {
    /// id.
    pub id: WireId,
    /// Required chart id; the chart defines this series' axes.
    #[serde(rename = "in")]
    pub parent: Option<WireId>,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
    /// enter.
    pub enter: Option<Enter>,
    /// dur.
    pub dur: Option<Ms>,
    /// color.
    pub color: Option<Color>,
    /// opacity.
    pub opacity: Option<Finite>,
    /// name.
    pub name: Option<Text>,
    /// data.
    #[serde(default)]
    pub data: Bounded<Point, MAX_SERIES_POINTS>,
}
/// Anim record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anim {
    /// target.
    pub target: WireId,
    /// track.
    pub track: Track,
    /// keys.
    pub keys: Bounded<(Ms, KeyValue), MAX_KEYS>,
    /// ease.
    #[serde(default)]
    pub ease: Ease,
    /// repeat.
    #[serde(default)]
    pub repeat: i32,
    /// yoyo.
    #[serde(default)]
    pub yoyo: bool,
    /// Delay after activation, in milliseconds.
    pub delay: Option<Ms>,
    /// Estimated word offset within the step's synthetic segment.
    pub at_word: Option<u16>,
}
/// Mermaid record fields.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mermaid {
    /// Required diagram id containing the compiled flowchart.
    #[serde(rename = "in")]
    pub parent: WireId,
    /// src.
    pub src: Source,
}
