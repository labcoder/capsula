//! Immutable prepared primitives; painters never read records or perform layout.
use crate::{
    Handle, Text,
    geom::{Affine, Rect, Vec2},
    limits::{
        MAX_COMPOSED_SCALE, MAX_GLOBAL_DASH_PIECES, MAX_GLOBAL_FILL_VERTICES,
        MAX_GLOBAL_OUTLINE_POINTS,
    },
    record::{Align, Rgb},
};
use std::{fmt, sync::Arc};

/// Text shaping parameters shared with painting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// Logical-pixel font size.
    pub size: f32,
    /// Whether to use the bold face.
    pub bold: bool,
}
/// Host text shaping, supplied during preparation rather than painting.
pub trait TextMeasure {
    /// Returns width and height, wrapping at `max_width` when present.
    /// # Errors
    /// Returns `MeasureError` when the host cannot shape this text.
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError>;
}
/// Content-free text-shaping failure; host diagnostics must not contain drawing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("scene.TextMeasure")]
pub struct MeasureError;
/// A resolved RGB color and its independent primitive opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint {
    /// RGB channels.
    pub rgb: Rgb,
    /// Opacity before object and sampled opacity are applied.
    pub opacity: f32,
}
impl From<Rgb> for Paint {
    fn from(rgb: Rgb) -> Self {
        Self { rgb, opacity: 1.0 }
    }
}
impl Paint {
    /// Multiplies this paint's opacity without changing its RGB channels.
    #[must_use]
    pub fn faded(self, opacity: f32) -> Self {
        Self {
            opacity: self.opacity * opacity,
            ..self
        }
    }
}
/// A resolved renderer-independent primitive.
#[derive(Clone, PartialEq)]
pub enum Prim {
    /// Native rounded rectangle, without flattened path storage.
    Rect {
        /// Geometry.
        rect: Rect,
        /// Corner radius.
        radius: f32,
        /// Optional fill.
        fill: Option<Paint>,
        /// Optional border color and width.
        border: Option<(Paint, f32)>,
    },
    /// Filled polygon with immutable vertex storage.
    Fill {
        /// Polygon vertices.
        points: Arc<[Vec2]>,
        /// Fill color.
        color: Paint,
    },
    /// Stroked polyline with immutable vertex storage.
    Line {
        /// Polyline vertices.
        points: Arc<[Vec2]>,
        /// Whether the last vertex connects to the first.
        closed: bool,
        /// Stroke color.
        color: Paint,
        /// Stroke width.
        width: f32,
        /// Whether to paint a dashed stroke.
        dashed: bool,
    },
    /// Text laid out using the host's matching text measurement.
    Text {
        /// Top-left of the wrapping box when `max_width` is set, otherwise of the ink.
        origin: Vec2,
        /// Display content, redacted by Debug.
        text: Text,
        /// Shaping parameters.
        style: TextStyle,
        /// Foreground color.
        color: Paint,
        /// Optional wrapping width.
        max_width: Option<f32>,
        /// Alignment within the wrapping width.
        align: Align,
    },
}
/// Prepared geometry belonging to one deterministic object identity.
#[derive(Clone, PartialEq)]
pub struct Item {
    /// Object identity.
    pub handle: Handle,
    /// Transform parent, if retained.
    pub parent: Option<Handle>,
    /// Shared geometry, never rebuilt by the painter.
    pub primitives: Arc<[Prim]>,
    /// Prepared bounds before sampled motion.
    pub bounds: Rect,
    /// Static transform inherited from detached ancestors.
    pub transform: Affine,
    /// Base opacity.
    pub opacity: f32,
}
/// Prepared object replacements for one step.
#[derive(Default, Clone, PartialEq)]
pub struct DisplayList {
    /// Items in stable paint order.
    pub items: Vec<Item>,
}

/// Prim-unit dash and gap of a dashed stroke, measured before the item transform, so dashes
/// scale with their object and their count never depends on pose or display.
pub const DASH: [f32; 2] = [6.0, 4.0];
/// Vertices one tessellation build may emit: a third of lyon's u16 vertex index space.
pub const BUILD_VERTEX_LIMIT: usize = 21_845;
/// Points plus crossings of one fill, which bound the vertices lyon's fill emits.
pub const FILL_VERTEX_LIMIT: usize = BUILD_VERTEX_LIMIT;
/// Conservative fallback outline cap; a border ring fills at most twice this many points.
pub const OUTLINE_POINT_LIMIT: usize = (BUILD_VERTEX_LIMIT - 4) / 4;
const _: () = assert!(
    3 * BUILD_VERTEX_LIMIT == 65_535,
    "a third of the u16 index space"
);

/// Paint work a prepared item asks of the painter, the same at any display, pose or reveal.
/// Sums saturate, so an overflow can only refuse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintWork {
    /// Dash pieces its dashed lines paint at most.
    pub dash_pieces: usize,
    /// Fill vertices: points plus crossings per fill, and each filled rect's fallback outline.
    pub fill_vertices: usize,
    /// Its largest single fill, against [`FILL_VERTEX_LIMIT`].
    pub largest_fill: usize,
    /// Fallback outline points of its bordered rects.
    pub outline_points: usize,
    /// Its largest single outline, against [`OUTLINE_POINT_LIMIT`].
    pub largest_outline: usize,
}
impl PaintWork {
    /// Charges every primitive once; a fill's crossing test costs O(points²).
    ///
    /// Every `Prim::Rect` is charged as if the painter's path fallback draws it, because
    /// rotation and partial reveals that trigger the fallback change from frame to frame.
    pub fn of(primitives: &[Prim]) -> Self {
        primitives
            .iter()
            .map(|primitive| match primitive {
                Prim::Line {
                    points,
                    closed,
                    dashed: true,
                    ..
                } => Self {
                    dash_pieces: dash_pieces(
                        points,
                        if *closed {
                            Contour::Closed
                        } else {
                            Contour::Open
                        },
                        MAX_GLOBAL_DASH_PIECES,
                    ),
                    ..Self::default()
                },
                Prim::Fill { points, .. } => {
                    let cap = FILL_VERTEX_LIMIT.saturating_sub(points.len());
                    Self::fills(points.len().saturating_add(crossings(points, cap)))
                }
                Prim::Rect {
                    rect,
                    radius,
                    fill,
                    border,
                } => {
                    let points = fallback_corner_segments(*rect, *radius)
                        .saturating_add(1)
                        .saturating_mul(4)
                        .saturating_add(1);
                    let outline = usize::from(border.is_some()) * points;
                    Self {
                        outline_points: outline,
                        largest_outline: outline,
                        ..Self::fills(usize::from(fill.is_some()) * points)
                    }
                }
                Prim::Line { .. } | Prim::Text { .. } => Self::default(),
            })
            .fold(Self::default(), |sum, work| sum + work)
    }
    fn fills(vertices: usize) -> Self {
        Self {
            fill_vertices: vertices,
            largest_fill: vertices,
            ..Self::default()
        }
    }
    /// The componentwise larger charge, for states that may each become the painted one.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            dash_pieces: self.dash_pieces.max(other.dash_pieces),
            fill_vertices: self.fill_vertices.max(other.fill_vertices),
            largest_fill: self.largest_fill.max(other.largest_fill),
            outline_points: self.outline_points.max(other.outline_points),
            largest_outline: self.largest_outline.max(other.largest_outline),
        }
    }
    /// Whether every single fill and outline fits one tessellation build.
    pub fn fits_builds(self) -> bool {
        self.largest_fill <= FILL_VERTEX_LIMIT && self.largest_outline <= OUTLINE_POINT_LIMIT
    }
    /// Whether this work fits the global paint budgets and every build fits one tessellation.
    pub fn within_limits(self) -> bool {
        self.fits(Self::LIMITS)
    }
    /// The global paint budgets and the single-build limits, as one cap.
    pub const LIMITS: Self = Self {
        dash_pieces: MAX_GLOBAL_DASH_PIECES,
        fill_vertices: MAX_GLOBAL_FILL_VERTICES,
        largest_fill: FILL_VERTEX_LIMIT,
        outline_points: MAX_GLOBAL_OUTLINE_POINTS,
        largest_outline: OUTLINE_POINT_LIMIT,
    };
    /// Whether every component is at most `cap`'s.
    pub fn fits(self, cap: Self) -> bool {
        self.dash_pieces <= cap.dash_pieces
            && self.fill_vertices <= cap.fill_vertices
            && self.largest_fill <= cap.largest_fill
            && self.outline_points <= cap.outline_points
            && self.largest_outline <= cap.largest_outline
    }
    /// The work of `primitives` on top of `spent`, or `None` once the sum doesn't fit `cap`.
    /// Each fill counts crossings only up to the room left in `cap`, so work that can never
    /// fit stops after at most `cap.fill_vertices` counted fill points.
    pub fn within(primitives: &[Prim], spent: Self, cap: Self) -> Option<Self> {
        primitives
            .iter()
            .try_fold(Self::default(), |sum, primitive| {
                let work = match primitive {
                    Prim::Fill { points, .. } => {
                        let room = cap.fill_vertices.checked_sub(
                            spent
                                .fill_vertices
                                .saturating_add(sum.fill_vertices)
                                .saturating_add(points.len()),
                        )?;
                        Self::fills(points.len() + crossings(points, room))
                    }
                    other => Self::of(std::slice::from_ref(other)),
                };
                let sum = sum + work;
                (spent + sum).fits(cap).then_some(sum)
            })
    }
}
/// Both charges together: sums saturate and the largest single builds take the larger.
impl std::ops::Add for PaintWork {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            dash_pieces: self.dash_pieces.saturating_add(other.dash_pieces),
            fill_vertices: self.fill_vertices.saturating_add(other.fill_vertices),
            largest_fill: self.largest_fill.max(other.largest_fill),
            outline_points: self.outline_points.saturating_add(other.outline_points),
            largest_outline: self.largest_outline.max(other.largest_outline),
        }
    }
}

/// Whether a polyline's last point joins its first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contour {
    /// The ends stay apart.
    Open,
    /// A closing edge joins the last point to the first.
    Closed,
}

/// Upper bound on the dash pieces a dashed polyline paints at any flow phase and reveal: an
/// edge of length l meets at most floor(l / period) + 2 dashes. It saturates at `cap + 1`
/// without walking a dash, and a non-finite length saturates too.
pub fn dash_pieces(points: &[Vec2], contour: Contour, cap: usize) -> usize {
    edges(points, contour).fold(0, |total, (a, b)| {
        dash_bound(total, distance(*a, *b), 1, cap)
    })
}
/// Length of a polyline, with its closing edge when [`Contour::Closed`].
pub(crate) fn polyline_length(points: &[Vec2], contour: Contour) -> f64 {
    edges(points, contour).map(|(a, b)| distance(*a, *b)).sum()
}
fn edges(points: &[Vec2], contour: Contour) -> impl Iterator<Item = (&Vec2, &Vec2)> {
    let closing = points
        .last()
        .zip(points.first())
        .filter(|_| contour == Contour::Closed && points.len() > 1);
    points
        .windows(2)
        .map(|pair| (&pair[0], &pair[1]))
        .chain(closing)
}
/// `total` plus floor(length / period) + 2 per edge, saturating at `cap + 1`.
pub(crate) fn dash_bound(total: usize, length: f64, edges: usize, cap: usize) -> usize {
    whole(length / f64::from(DASH[0] + DASH[1]))
        .map_or(usize::MAX, |pieces| {
            total
                .saturating_add(pieces)
                .saturating_add(edges.saturating_mul(2))
        })
        .min(cap.saturating_add(1))
}
fn distance(a: Vec2, b: Vec2) -> f64 {
    (f64::from(b.x) - f64::from(a.x)).hypot(f64::from(b.y) - f64::from(a.y))
}
/// A float's floor as a count; `None` when it is negative, NaN or past `u32::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "only a whole number in 0..=u32::MAX is converted"
)]
fn whole(value: f64) -> Option<usize> {
    let value = value.floor();
    (0.0..=f64::from(u32::MAX))
        .contains(&value)
        .then_some(value as usize)
}

/// Pieces per quarter turn that keep a rounded corner of this world radius within the
/// flattening tolerance: 0 without a radius, `usize::MAX` for NaN or radii no build holds.
pub fn corner_segments(world_radius: f64) -> usize {
    if world_radius.is_nan() {
        return usize::MAX;
    }
    if world_radius <= 0.0 {
        return 0;
    }
    // acos(1 − t/R) as 2·asin(√(t/2R)), which stays accurate for large radii.
    let half_angle = 2.0
        * (crate::path::TOLERANCE / (2.0 * world_radius))
            .sqrt()
            .min(1.0)
            .asin();
    whole((std::f64::consts::FRAC_PI_4 / half_angle).ceil()).map_or(usize::MAX, |n| n.max(1))
}
/// Corner pieces a `Prim::Rect` fallback is charged: its radius clamped as painted, at the
/// largest admitted composed scale, so no later pose needs more.
pub fn fallback_corner_segments(rect: Rect, radius: f32) -> usize {
    let radius = radius.clamp(0.0, (rect.size.x.min(rect.size.y) * 0.5).max(0.0));
    corner_segments(f64::from(radius) * f64::from(MAX_COMPOSED_SCALE))
}

/// Intersecting edge pairs of a closed polygon, stopping at `cap + 1`: non-adjacent edges that
/// cross, touch or overlap, and adjacent edges that fold back along each other. Orientations
/// within a relative 1e-6 of zero count as touching, so lyon's f32 sweep meets no more. Edges
/// whose boxes lie more than 3e-6 of the largest coordinate apart never count, and a
/// non-finite point charges `cap + 1`.
///
/// A sweep over edge boxes, sorted on the axis they span least, tests only overlapping pairs:
/// near-linear for convex fills, quadratic only in cheap box checks for combs of long edges.
pub fn crossings(points: &[Vec2], cap: usize) -> usize {
    let n = points.len();
    if n < 3 {
        return 0;
    }
    let at: Vec<[f64; 2]> = points
        .iter()
        .map(|point| [f64::from(point.x), f64::from(point.y)])
        .collect();
    if at.iter().flatten().any(|value| !value.is_finite()) {
        return cap.saturating_add(1);
    }
    let reach = at
        .iter()
        .flatten()
        .fold(0.0_f64, |reach, value| reach.max(value.abs()));
    // Exact meetings share a box point, and a point within the side slack of an edge itself lies
    // within 1e-6 · |ac| ≤ 2√2e-6 · reach of it, so boxes padded by 1.5e-6 · reach keep both;
    // only pairs where each meets the other's line alone, far from its edge, are dropped.
    let pad = 1.5e-6 * reach;
    let ends = |i: usize| [at[i], at[(i + 1) % n]];
    let boxes: Vec<[[f64; 2]; 2]> = (0..n)
        .map(|i| {
            let [a, b] = ends(i);
            [
                [a[0].min(b[0]) - pad, a[1].min(b[1]) - pad],
                [a[0].max(b[0]) + pad, a[1].max(b[1]) + pad],
            ]
        })
        .collect();
    let span = |axis: usize| boxes.iter().map(|b| b[1][axis] - b[0][axis]).sum::<f64>();
    let (sweep, across) = if span(0) <= span(1) { (0, 1) } else { (1, 0) };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_unstable_by(|&i, &j| boxes[i][0][sweep].total_cmp(&boxes[j][0][sweep]));
    let mut count = 0;
    for (rank, &first) in order.iter().enumerate() {
        let [low, high] = boxes[first];
        for &second in &order[rank + 1..] {
            let [other_low, other_high] = boxes[second];
            if other_low[sweep] > high[sweep] {
                break;
            }
            if other_low[across] > high[across] || low[across] > other_high[across] {
                continue;
            }
            let (i, j) = (first.min(second), first.max(second));
            let meets = if j == i + 1 {
                folds(at[j], at[i], at[(j + 1) % n])
            } else if i == 0 && j == n - 1 {
                folds(at[0], at[1], at[j])
            } else {
                intersects(ends(i), ends(j))
            };
            count += usize::from(meets);
            if count > cap {
                return count;
            }
        }
    }
    count
}
/// The side of `a → b` that `c` lies on, or 0 when |cross| ≤ 1e-6 · |ab| · |ac|.
/// The f64 cross product errs by under 1e-15 · |ab| · |ac|, far below the slack, so a nonzero
/// answer is the exact sign for these points: exact meetings always count.
fn side(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> i8 {
    let to_b = [b[0] - a[0], b[1] - a[1]];
    let to_c = [c[0] - a[0], c[1] - a[1]];
    let cross = to_b[0] * to_c[1] - to_b[1] * to_c[0];
    let squared = |v: [f64; 2]| v[0] * v[0] + v[1] * v[1];
    // Squared, without hypot: finite f32 differences keep every term in f64's normal range,
    // and rounding moves the slack's edge by under 1e-15 of itself.
    if cross * cross > 1e-12 * squared(to_b) * squared(to_c) {
        if cross > 0.0 { 1 } else { -1 }
    } else {
        0
    }
}
/// Whether the edges from `shared` to `a` and to `b` run back along each other.
fn folds(shared: [f64; 2], a: [f64; 2], b: [f64; 2]) -> bool {
    side(shared, a, b) == 0
        && (a[0] - shared[0]) * (b[0] - shared[0]) + (a[1] - shared[1]) * (b[1] - shared[1]) > 0.0
}
/// Whether two closed segments meet, counting near-touches.
fn intersects([a, b]: [[f64; 2]; 2], [c, d]: [[f64; 2]; 2]) -> bool {
    let across_ab = [side(a, b, c), side(a, b, d)];
    if across_ab[0] * across_ab[1] > 0 {
        return false;
    }
    let across_cd = [side(c, d, a), side(c, d, b)];
    if across_cd[0] * across_cd[1] > 0 {
        return false;
    }
    across_ab.iter().chain(&across_cd).any(|&value| value != 0)
        || (0..2).all(|axis| {
            a[axis].min(b[axis]) <= c[axis].max(d[axis])
                && c[axis].min(d[axis]) <= a[axis].max(b[axis])
        })
}

impl fmt::Debug for Prim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rect { .. } => f.write_str("Rect { .. }"),
            Self::Fill { points, .. } => f
                .debug_struct("Fill")
                .field("vertices", &points.len())
                .finish_non_exhaustive(),
            Self::Line { points, .. } => f
                .debug_struct("Line")
                .field("vertices", &points.len())
                .finish_non_exhaustive(),
            Self::Text { text, .. } => f
                .debug_struct("Text")
                .field("characters", &text.as_str().chars().count())
                .finish_non_exhaustive(),
        }
    }
}
impl fmt::Debug for DisplayList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayList")
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Item")
            .field("handle", &self.handle)
            .field("primitives", &self.primitives.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(points: &[[f32; 2]]) -> Arc<[Vec2]> {
        points.iter().map(|p| Vec2::new(p[0], p[1])).collect()
    }
    fn paint() -> Paint {
        crate::theme::Theme::default().fg.into()
    }
    fn rect(size: f32, radius: f32, fill: Option<Paint>, border: Option<(Paint, f32)>) -> Prim {
        Prim::Rect {
            rect: Rect {
                origin: Vec2::default(),
                size: Vec2::new(size, size),
            },
            radius,
            fill,
            border,
        }
    }
    /// The regular star polygon {n/k}: n vertices, each edge crossing 2(k − 1) others.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "small fixture indices and bounded coordinates"
    )]
    fn star(n: usize, k: usize) -> Arc<[Vec2]> {
        (0..n)
            .map(|i| {
                let angle = std::f64::consts::TAU * ((i * k) % n) as f64 / n as f64;
                Vec2::new((1000.0 * angle.cos()) as f32, (1000.0 * angle.sin()) as f32)
            })
            .collect()
    }

    #[test]
    fn dash_pieces_bound_each_edge_and_saturate_without_walking() {
        assert_eq!(dash_pieces(&[], Contour::Closed, 1_024), 0);
        assert_eq!(
            dash_pieces(&points(&[[3.0, 4.0]]), Contour::Closed, 1_024),
            0
        );
        assert_eq!(
            dash_pieces(&points(&[[0.0, 0.0], [100.0, 0.0]]), Contour::Open, 1_024),
            12
        );
        let triangle = points(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(dash_pieces(&triangle, Contour::Open, 1_024), 4);
        assert_eq!(
            dash_pieces(&triangle, Contour::Closed, 1_024),
            6,
            "the closing edge counts"
        );
        let at_cap = points(&[[0.0, 0.0], [10_220.0, 0.0]]);
        assert_eq!(dash_pieces(&at_cap, Contour::Open, 1_024), 1_024);
        let over = points(&[[0.0, 0.0], [10_230.0, 0.0]]);
        assert_eq!(dash_pieces(&over, Contour::Open, 1_024), 1_025);
        let huge = points(&[[0.0, 0.0], [1e30, 0.0], [0.0, 0.0]]);
        assert_eq!(dash_pieces(&huge, Contour::Open, 1_024), 1_025);
        let nan = points(&[[0.0, 0.0], [f32::NAN, 0.0]]);
        assert_eq!(dash_pieces(&nan, Contour::Open, 1_024), 1_025);
    }

    #[test]
    fn corner_segments_keep_every_radius_within_tolerance() {
        assert_eq!(corner_segments(0.0), 0);
        assert_eq!(corner_segments(-5.0), 0);
        for radius in [f64::NAN, f64::INFINITY, 1e300] {
            assert_eq!(
                corner_segments(radius),
                usize::MAX,
                "unbuildable radius saturates"
            );
        }
        // Expected from ceil((π/4) / acos(1 − 0.25 / R)), computed separately.
        for (radius, expected) in [
            (0.1, 1),
            (40.0, 8),
            (51.0, 8),
            (53.0, 9),
            (512.0, 26),
            (640.0, 29),
            (10_000.0, 112),
            (20_000.0, 158),
            (1_280_000.0, 1_257),
        ] {
            let n = corner_segments(radius);
            assert_eq!(n, expected, "radius {radius}");
            let error = |pieces: usize| {
                let pieces = f64::from(u32::try_from(pieces).expect("small count"));
                radius * (1.0 - (std::f64::consts::FRAC_PI_4 / pieces).cos())
            };
            assert!(error(n) <= 0.25, "radius {radius} stays within tolerance");
            assert!(
                n == 1 || error(n - 1) > 0.25,
                "radius {radius} uses no extra piece"
            );
        }
    }

    #[test]
    fn crossings_count_every_meeting_pair_up_to_the_cap() {
        let square = points(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        assert_eq!(crossings(&square, 100), 0);
        let bowtie = points(&[[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(crossings(&bowtie, 100), 1);
        assert_eq!(crossings(&star(5, 2), 100), 5);
        assert_eq!(crossings(&star(7, 3), 100), 14);
        assert_eq!(crossings(&star(7, 3), 2), 3, "stops one past the cap");
        let folded = points(&[[0.0, 0.0], [2.0, 0.0], [1.0, 0.0]]);
        assert_eq!(crossings(&folded, 100), 2, "both fold-backs count");
        let touching = points(&[[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [2.0, 0.0]]);
        assert_eq!(crossings(&touching, 100), 2, "a touch and a fold-back");
        assert_eq!(crossings(&points(&[[0.0, 0.0], [1.0, 1.0]]), 100), 0);
    }

    #[test]
    fn side_keeps_its_relative_slack_and_sign_at_both_f32_extremes() {
        let (o, x) = ([0.0, 0.0], [1.0, 0.0]);
        assert_eq!(side(o, x, [1.0, 1.1e-6]), 1);
        assert_eq!(side(o, x, [1.0, 0.9e-6]), 0, "within 1e-6 of the line");
        assert_eq!(side(o, x, [1.0, -1.1e-6]), -1);
        assert_eq!(side(o, o, [1.0, 1.0]), 0, "a point edge has no side");
        let max = f64::from(f32::MAX);
        let (left, right) = ([-max, 0.0], [max, 0.0]);
        assert_eq!(side(left, right, [0.0, max * 3e-6]), 1);
        assert_eq!(side(left, right, [0.0, max * 1e-7]), 0);
        assert_eq!(side([-max, -max], [max, max], [max, -max]), -1);
        let tiny = f64::from(f32::from_bits(1));
        assert_eq!(side(o, [tiny, 0.0], [tiny, tiny]), 1);
        assert_eq!(side(o, [tiny, 0.0], [-tiny, 0.0]), 0);
    }

    #[test]
    fn crossings_sweep_either_axis_and_skip_edges_only_their_lines_meet() {
        let wide = points(&[[0.0, 0.0], [100.0, 1.0], [100.0, 0.0], [0.0, 1.0]]);
        assert_eq!(crossings(&wide, 100), 1, "a crossing found sweeping y");
        // (20, 1.9e-5) is within the slack of the line through (0, 0) and (10, 0), and the line
        // on to (22, 2.245e-5) passes within the slack of both: the edges themselves are 10 apart.
        let apart = points(&[
            [0.0, 0.0],
            [10.0, 0.0],
            [15.0, -10.0],
            [20.0, 1.9e-5],
            [22.0, 2.245e-5],
            [30.0, 10.0],
            [-5.0, 10.0],
        ]);
        assert_eq!(crossings(&apart, 100), 0);
        assert_eq!(
            crossings(&points(&[[0.0, 0.0], [f32::NAN, 1.0], [1.0, 0.0]]), 7),
            8
        );
    }

    #[test]
    fn paint_work_charges_dashes_fills_and_every_rect_as_its_fallback() {
        let segment = [[0.0, 0.0], [100.0, 0.0]];
        let dashed = PaintWork::of(&[
            Prim::Line {
                points: points(&segment),
                closed: false,
                color: paint(),
                width: 2.0,
                dashed: true,
            },
            Prim::Line {
                points: points(&segment),
                closed: false,
                color: paint(),
                width: 2.0,
                dashed: false,
            },
        ]);
        assert_eq!(
            dashed,
            PaintWork {
                dash_pieces: 12,
                ..PaintWork::default()
            }
        );
        let at_limit = PaintWork::of(&[Prim::Fill {
            points: star(257, 85),
            color: paint(),
        }]);
        assert_eq!(
            (at_limit.fill_vertices, at_limit.largest_fill),
            (21_845, 21_845)
        );
        assert!(
            at_limit.fits_builds(),
            "257 points + 21,588 crossings fit one build"
        );
        let over = PaintWork::of(&[Prim::Fill {
            points: star(331, 66),
            color: paint(),
        }]);
        assert_eq!(over.largest_fill, 21_846);
        assert!(
            !over.fits_builds(),
            "331 points + 21,515 crossings exceed one build"
        );

        let square = PaintWork::of(&[rect(100.0, 0.0, Some(paint()), Some((paint(), 2.0)))]);
        assert_eq!(
            square,
            PaintWork {
                fill_vertices: 5,
                largest_fill: 5,
                outline_points: 5,
                largest_outline: 5,
                ..PaintWork::default()
            }
        );
        // Radius 8 at scale 64 needs 26 pieces per corner: 4 · 27 + 1 points.
        let node = PaintWork::of(&[rect(100.0, 8.0, None, Some((paint(), 2.0)))]);
        assert_eq!((node.outline_points, node.fill_vertices), (109, 0));
        // Clamped to half of a 10-unit side: radius 5 at scale 64 needs 20 pieces.
        assert_eq!(
            PaintWork::of(&[rect(10.0, 8.0, Some(paint()), None)]).fill_vertices,
            85
        );
        assert_eq!(
            fallback_corner_segments(Rect::default(), f32::NAN),
            usize::MAX
        );
        let largest = Prim::Rect {
            rect: Rect {
                origin: Vec2::new(-20_000.0, -20_000.0),
                size: Vec2::new(40_000.0, 40_000.0),
            },
            radius: 20_000.0,
            fill: None,
            border: Some((paint(), 2.0)),
        };
        assert_eq!(PaintWork::of(&[largest]).largest_outline, 5_033);
        // Radii 23,510 and 23,545 need 1,363 and 1,364 pieces at scale 64.
        let outline = PaintWork::of(&[rect(50_000.0, 23_510.0, None, Some((paint(), 2.0)))]);
        assert_eq!(outline.largest_outline, 5_457);
        assert!(outline.fits_builds(), "the largest outline under the limit");
        let outline = PaintWork::of(&[rect(
            50_000.0,
            23_545.0,
            Some(paint()),
            Some((paint(), 2.0)),
        )]);
        assert_eq!(
            (outline.largest_outline, outline.largest_fill),
            (5_461, 5_461)
        );
        assert!(
            !outline.fits_builds(),
            "one more corner piece passes the outline limit"
        );
    }

    #[test]
    fn text_adds_no_tessellation_work() {
        let text = Prim::Text {
            origin: Vec2::default(),
            text: "label".to_owned().try_into().expect("bounded fixture text"),
            style: TextStyle {
                size: 12.0,
                bold: false,
            },
            color: paint(),
            max_width: None,
            align: Align::Left,
        };
        assert_eq!(PaintWork::of(&[text]), PaintWork::default());
    }

    #[test]
    fn paint_work_sums_saturate_and_limits_hold_at_each_bound() {
        let full = PaintWork {
            dash_pieces: usize::MAX,
            largest_fill: 7,
            ..PaintWork::default()
        };
        let one = PaintWork {
            dash_pieces: 1,
            fill_vertices: 2,
            largest_fill: 3,
            ..PaintWork::default()
        };
        assert_eq!(
            full + one,
            PaintWork {
                dash_pieces: usize::MAX,
                fill_vertices: 2,
                largest_fill: 7,
                ..PaintWork::default()
            }
        );
        assert_eq!(
            full.max(one),
            PaintWork {
                dash_pieces: usize::MAX,
                fill_vertices: 2,
                largest_fill: 7,
                ..PaintWork::default()
            }
        );
        let only =
            |dash_pieces, fill_vertices, outline_points, largest_fill, largest_outline| PaintWork {
                dash_pieces,
                fill_vertices,
                largest_fill,
                outline_points,
                largest_outline,
            };
        for (at, over) in [
            (
                only(MAX_GLOBAL_DASH_PIECES, 0, 0, 0, 0),
                only(MAX_GLOBAL_DASH_PIECES + 1, 0, 0, 0, 0),
            ),
            (
                only(0, MAX_GLOBAL_FILL_VERTICES, 0, 0, 0),
                only(0, MAX_GLOBAL_FILL_VERTICES + 1, 0, 0, 0),
            ),
            (
                only(0, 0, MAX_GLOBAL_OUTLINE_POINTS, 0, 0),
                only(0, 0, MAX_GLOBAL_OUTLINE_POINTS + 1, 0, 0),
            ),
            (
                only(0, 0, 0, FILL_VERTEX_LIMIT, 0),
                only(0, 0, 0, FILL_VERTEX_LIMIT + 1, 0),
            ),
            (
                only(0, 0, 0, 0, OUTLINE_POINT_LIMIT),
                only(0, 0, 0, 0, OUTLINE_POINT_LIMIT + 1),
            ),
        ] {
            assert!(at.within_limits(), "exactly at the limit fits");
            assert!(!over.within_limits(), "one over is refused");
        }
    }

    #[test]
    fn within_stops_counting_once_a_bounded_scene_passes_its_cap() {
        let spent = |fill_vertices| PaintWork {
            fill_vertices,
            ..PaintWork::default()
        };
        let exact = |fill_vertices, largest_fill| PaintWork {
            fill_vertices,
            largest_fill,
            ..PaintWork::default()
        };
        let fill = |corners: &[[f32; 2]]| Prim::Fill {
            points: points(corners),
            color: paint(),
        };
        let square = fill(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        let bowtie = fill(&[[0.0, 0.0], [10.0, 10.0], [10.0, 0.0], [0.0, 10.0]]);
        let within = |prims: &[Prim], fill_vertices| {
            PaintWork::within(prims, spent(fill_vertices), PaintWork::LIMITS)
        };
        assert_eq!(
            within(std::slice::from_ref(&bowtie), 507),
            Some(exact(5, 5)),
            "4 points and 1 crossing reach 512 exactly"
        );
        assert_eq!(
            within(std::slice::from_ref(&bowtie), 508),
            None,
            "the crossing is one over"
        );
        assert_eq!(
            within(std::slice::from_ref(&square), 508),
            Some(exact(4, 4))
        );
        assert_eq!(
            within(std::slice::from_ref(&square), 509),
            None,
            "the points alone are one over"
        );
        assert_eq!(
            within(&[square.clone(), bowtie.clone()], 503),
            Some(exact(9, 5)),
            "fills in one item share the room left"
        );
        assert_eq!(within(&[square, bowtie], 504), None);
    }
}
