//! Bounded tessellation of one prepared primitive into GPUI paths. lyon sees only prim
//! coordinates, the ones the paint guard counted, and the finished triangles are then placed by
//! the item transform. Dashes are walked in prim units with f64 arc length, every lyon build
//! stays under `BUILD_VERTEX_LIMIT`, and a stroke's chunks merge into one path.

use std::f64::consts::{FRAC_PI_2, PI};

use gpui_kit::{
    Bounds, FillOptions, FillRule, Path, PathBuilder, PathStyle, Pixels, Point, point, px,
};
use kumochi_scene::{
    display::{
        BUILD_VERTEX_LIMIT, Contour, DASH, FILL_VERTEX_LIMIT, OUTLINE_POINT_LIMIT, Paint, Prim,
        corner_segments, fallback_corner_segments,
    },
    geom::{Affine, Rect, Vec2},
};

/// Why a primitive has no paths; carries no geometry or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum BuildError {
    /// Text is shaped and painted by its caller.
    #[error("text isn't built as geometry")]
    Text,
    /// The geometry needs more than its paint work was charged for.
    #[error("drawing geometry exceeds its paint charge")]
    OverCharge,
    /// lyon refused a bounded fill.
    #[error("a drawing fill couldn't be tessellated")]
    Fill,
    /// lyon refused a bounded stroke chunk.
    #[error("a drawing stroke couldn't be tessellated")]
    Stroke,
    /// GPUI couldn't shape or paint a label.
    #[error("a drawing label couldn't be shaped or painted")]
    Label,
}

/// One primitive's paths in world logical pixels, each with its prepared paint, built in full
/// before anything paints. A `Rect` always takes its rounded-outline form; callers keep the quad
/// for axis-aligned, fully drawn rects. `draw` reveals strokes and `flow` moves dashes, in periods.
///
/// A border covers what its quad's would: the rect's rounded outline minus the outline inset by
/// the border width at radius `max(radius − width, 0)`, sampled at the same corner angles and
/// filled even-odd. A border at least half the short side fills the whole outline. A partial
/// `draw` reveals that fraction of the outer outline's length, clockwise from the right end of
/// the top edge, as one sector closed through the inner outline at the same fraction of the
/// matching chord, so it folds onto an inner outline collapsed to a line or a point.
///
/// # Errors
/// [`BuildError`] when the primitive is text, exceeds its charge, or fails to tessellate.
pub(super) fn paths(
    prim: &Prim,
    transform: Affine,
    draw: f32,
    flow: f32,
) -> Result<Vec<(Path<Pixels>, Paint)>, BuildError> {
    let mut built = Vec::new();
    match prim {
        Prim::Fill { points, color } => {
            built.extend(fill(&[points], FillRule::NonZero)?.map(|path| (path, *color)));
        }
        Prim::Line {
            points,
            closed,
            color,
            width,
            dashed,
        } => {
            let flow = dashed.then_some(flow);
            let contour = if *closed {
                Contour::Closed
            } else {
                Contour::Open
            };
            let path = stroke(points, contour, draw, flow, px(*width))?;
            built.extend(path.map(|path| (path, *color)));
        }
        Prim::Rect {
            fill: None,
            border: None,
            ..
        } => {}
        Prim::Rect {
            rect,
            radius,
            fill: background,
            border,
        } => {
            let zoom = scale(transform);
            let charge = fallback_corner_segments(*rect, *radius);
            let (radius, segments) = corners(*rect, *radius, zoom, charge)?;
            let outer = rounded(*rect, radius, segments);
            if let Some(paint) = background {
                built.extend(fill(&[&outer], FillRule::NonZero)?.map(|path| (path, *paint)));
            }
            if let Some((paint, width)) = border {
                let half = rect.size.x.min(rect.size.y) / 2.0;
                let width = width.min(half);
                if width > 0.0 {
                    let inset = Vec2::new(width, width);
                    let inner = Rect {
                        origin: rect.origin + inset,
                        size: rect.size - inset * 2.0,
                    };
                    let inner = rounded(inner, (radius - width).max(0.0), segments);
                    let ring = if draw < 1.0 {
                        sector(&outer, &inner, draw)
                    } else if width < half {
                        fill(&[&outer, &inner], FillRule::EvenOdd)
                    } else {
                        fill(&[&outer], FillRule::NonZero)
                    }?;
                    built.extend(ring.map(|path| (path, *paint)));
                }
            }
        }
        Prim::Text { .. } => return Err(BuildError::Text),
    }
    for (path, _) in &mut built {
        place(path, transform);
    }
    Ok(built)
}

/// Maps a prim-space path's triangles into world space and bounds them exactly. The renderer
/// reads only their positions; their texture coordinates are the constant `build_path` set.
fn place(path: &mut Path<Pixels>, transform: Affine) {
    for vertex in &mut path.vertices {
        let local = Vec2::new(
            f32::from(vertex.xy_position.x),
            f32::from(vertex.xy_position.y),
        );
        vertex.xy_position = pixel(transform.point(local));
    }
    let Some(first) = path.vertices.first().map(|vertex| vertex.xy_position) else {
        return;
    };
    let (min, max) = path
        .vertices
        .iter()
        .fold((first, first), |(min, max), vertex| {
            (min.min(&vertex.xy_position), max.max(&vertex.xy_position))
        });
    path.bounds = Bounds::from_corners(min, max);
}

/// Closed contours of at most `FILL_VERTEX_LIMIT` points in all, filled by `rule`: polygons
/// nonzero, as SVG and Canvas do, and a border's nested outlines even-odd so the inner is a hole.
/// A fill's crossings are the guard's charge; a border's outlines never cross.
fn fill(contours: &[&[Vec2]], rule: FillRule) -> Result<Option<Path<Pixels>>, BuildError> {
    if contours.iter().map(|points| points.len()).sum::<usize>() > FILL_VERTEX_LIMIT {
        return Err(BuildError::OverCharge);
    }
    let options = FillOptions::default().with_fill_rule(rule);
    let mut path = PathBuilder::fill().with_style(PathStyle::Fill(options));
    for points in contours {
        let mut points = points.iter().map(|local| pixel(*local));
        let Some(first) = points.next() else {
            continue;
        };
        path.move_to(first);
        points.for_each(|to| path.line_to(to));
        path.close();
    }
    let path = path.build().map_err(|_| BuildError::Fill)?;
    Ok((!path.vertices.is_empty()).then_some(path))
}

/// Strokes the first `draw` of a polyline's length; with `flow`, as `DASH` runs moved that many
/// periods along it. Each segment visits at most `floor(length / period) + 2` dashes, the count
/// the paint guard charged. A solid stroke walks as one dash longer than the whole line.
fn stroke(
    points: &[Vec2],
    contour: Contour,
    draw: f32,
    flow: Option<f32>,
    width: Pixels,
) -> Result<Option<Path<Pixels>>, BuildError> {
    let closed = matches!(contour, Contour::Closed);
    let ends = points.iter().chain(points.first().filter(|_| closed));
    let segments = || {
        ends.clone()
            .zip(ends.clone().skip(1))
            .map(|(a, b)| (*a, *b, length(*a, *b)))
    };
    let total: f64 = segments().map(|(.., span)| span).sum();
    let reveal = total * f64::from(draw.clamp(0.0, 1.0));
    let (on, period, phase) = match flow {
        Some(flow) => {
            let period = f64::from(DASH[0] + DASH[1]);
            let phase = (1.0 - f64::from(flow).rem_euclid(1.0)) * period;
            (f64::from(DASH[0]), period, phase)
        }
        None => (total + 1.0, total + 1.0, 0.0),
    };
    let mut strokes = Strokes::new(width);
    let mut run = Vec::new();
    let mut open = false;
    let mut start = 0.0_f64;
    for (a, b, span) in segments() {
        if start >= reveal {
            break;
        }
        if span.is_nan() || span <= 0.0 {
            continue;
        }
        let end = (start + span).min(reveal);
        let along = |offset: f64| {
            pixel(if offset >= start + span {
                b
            } else {
                a + (b - a) * narrow((offset - start) / span)
            })
        };
        let continues = open;
        open = false;
        let mut dash = ((start + phase) / period).floor();
        let mut from = dash * period - phase;
        while from < end {
            let to = (from + on).min(end);
            let from_here = from.max(start);
            if to > from_here {
                if !continues || from > start {
                    strokes.add(&run, Contour::Open)?;
                    run.clear();
                    run.push(along(from_here));
                }
                run.push(along(to));
                open = to >= start + span;
            }
            dash += 1.0;
            from = dash * period - phase;
        }
        start += span;
    }
    let last = if closed && flow.is_none() && draw >= 1.0 {
        Contour::Closed
    } else {
        Contour::Open
    };
    strokes.add(&run, last)?;
    strokes.finish()
}

/// A stroke's runs, built in lyon chunks of at most `BUILD_VERTEX_LIMIT` vertices and merged.
struct Strokes {
    width: Pixels,
    builder: PathBuilder,
    bound: usize,
    merged: Option<Path<Pixels>>,
}

impl Strokes {
    fn new(width: Pixels) -> Self {
        Self {
            width,
            builder: PathBuilder::stroke(width),
            bound: 0,
            merged: None,
        }
    }

    /// Appends one run, building the current chunk first when the run would overflow it. A
    /// closed run repeats its first point last, which the closing join replaces.
    /// A butt-capped, bevel-or-miter lyon stroke emits at most 4 vertices per point plus 4.
    fn add(&mut self, run: &[Point<Pixels>], contour: Contour) -> Result<(), BuildError> {
        let [first, rest @ ..] = run else {
            return Ok(());
        };
        let close = matches!(contour, Contour::Closed);
        let cost = run.len().saturating_mul(4).saturating_add(4);
        if cost > BUILD_VERTEX_LIMIT {
            return Err(BuildError::OverCharge);
        }
        if self.bound + cost > BUILD_VERTEX_LIMIT {
            self.build()?;
        }
        let rest = match rest.split_last() {
            Some((_, open)) if close => open,
            _ => rest,
        };
        self.builder.move_to(*first);
        for to in rest {
            self.builder.line_to(*to);
        }
        if close {
            self.builder.close();
        }
        self.bound += cost;
        Ok(())
    }

    /// Builds the current chunk into the merged path, starting from the first non-empty chunk
    /// rather than `Path::new`, whose origin would join the bounds.
    fn build(&mut self) -> Result<(), BuildError> {
        let builder = std::mem::replace(&mut self.builder, PathBuilder::stroke(self.width));
        self.bound = 0;
        let chunk = builder.build().map_err(|_| BuildError::Stroke)?;
        if chunk.vertices.is_empty() {
            return Ok(());
        }
        if let Some(merged) = &mut self.merged {
            merged.bounds = merged.bounds.union(&chunk.bounds);
            merged.vertices.extend(chunk.vertices);
        } else {
            self.merged = Some(chunk);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Option<Path<Pixels>>, BuildError> {
        self.build()?;
        Ok(self.merged)
    }
}

/// The first `draw` of the band between two outlines of matching points, by the outer outline's
/// length: one contour along the outer outline to the cut, across to the inner outline at the
/// same fraction of the matching chord, and back. At most twice the outline points plus two.
fn sector(outer: &[Vec2], inner: &[Vec2], draw: f32) -> Result<Option<Path<Pixels>>, BuildError> {
    let outer_edges = outer.iter().zip(outer.iter().cycle().skip(1));
    let inner_edges = inner.iter().zip(inner.iter().cycle().skip(1));
    let total: f64 = outer_edges.clone().map(|(a, b)| length(*a, *b)).sum();
    let mut left = total * f64::from(draw.clamp(0.0, 1.0));
    if left.is_nan() || left <= 0.0 {
        return Ok(None);
    }
    let last = outer.len().saturating_sub(1);
    let mut contour = Vec::with_capacity(outer.len() * 2 + 2);
    for (index, ((a, b), (c, d))) in outer_edges.zip(inner_edges).enumerate() {
        contour.push(*a);
        let span = length(*a, *b);
        if left <= span || index == last {
            let t = narrow((left / span).min(1.0));
            contour.extend([*a + (*b - *a) * t, *c + (*d - *c) * t]);
            contour.extend(inner.iter().take(index + 1).rev());
            break;
        }
        left -= span;
    }
    fill(&[&contour], FillRule::NonZero)
}

/// A rounded rectangle's radius, clamped as painted, and its chords per corner, flattened from
/// the world radius. A scale past the charged one fails instead of coarsening a corner. Its
/// outline is four corners of `segments + 1` points; a border's sector takes at most twice
/// that plus two, the charged outline's `points` doubled.
fn corners(rect: Rect, radius: f32, zoom: f32, charge: usize) -> Result<(f32, u32), BuildError> {
    let radius = radius.clamp(0.0, (rect.size.x.min(rect.size.y) * 0.5).max(0.0));
    let segments = corner_segments(f64::from(radius) * f64::from(zoom));
    let points = segments
        .saturating_add(1)
        .saturating_mul(4)
        .saturating_add(1);
    if segments > charge || points > OUTLINE_POINT_LIMIT {
        return Err(BuildError::OverCharge);
    }
    let segments = u32::try_from(segments).expect("bounded by OUTLINE_POINT_LIMIT");
    Ok((radius, segments))
}

/// Clockwise from the top edge, `segments` chords per corner.
fn rounded(rect: Rect, radius: f32, segments: u32) -> Vec<Vec2> {
    let near = rect.origin + Vec2::new(radius, radius);
    let far = rect.max() - Vec2::new(radius, radius);
    let step = FRAC_PI_2 / f64::from(segments.max(1));
    [
        (Vec2::new(far.x, near.y), -FRAC_PI_2),
        (far, 0.0),
        (Vec2::new(near.x, far.y), FRAC_PI_2),
        (near, PI),
    ]
    .into_iter()
    .flat_map(|(center, start)| {
        (0..=segments).map(move |index| {
            let angle = start + step * f64::from(index);
            center + Vec2::new(narrow(angle.cos()), narrow(angle.sin())) * radius
        })
    })
    .collect()
}

fn length(a: Vec2, b: Vec2) -> f64 {
    (f64::from(b.x) - f64::from(a.x)).hypot(f64::from(b.y) - f64::from(a.y))
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "prim coordinates and their fractions are f32"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}

fn pixel(at: Vec2) -> Point<Pixels> {
    point(px(at.x), px(at.y))
}

/// The uniform scale of a transform, which corner radii are flattened at.
pub(super) fn scale(transform: Affine) -> f32 {
    let [a, b, c, d, ..] = transform.0;
    (a * d - b * c).abs().sqrt()
}

#[cfg(test)]
#[path = "paint_paths_tests.rs"]
mod tests;
