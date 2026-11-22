use std::f64::consts::{FRAC_PI_2, PI};

use gpui_kit::{FillOptions, FillRule, Path, Pixels};
use kumochi_scene::{
    display::{Contour, Paint, Prim, fallback_corner_segments},
    geom::{Affine, Rect, Vec2},
    limits::MAX_GLOBAL_FILL_VERTICES,
};
use lyon::{
    path::PathEvent,
    tessellation::{BuffersBuilder, FillTessellator, FillVertex, VertexBuffers},
};

use super::{
    paint_paths::{BuildError, corners, length, narrow, scale},
    paint_phase::{self, Phase},
    paint_stroke::Scratch,
};

/// A primitive's fill and stroke paths, each with its paint.
pub(super) type PrimPaths = [Option<(Path<Pixels>, Paint)>; 2];

/// Fixed residency allowance for the reusable Lyon sweep and output below the paint caps.
pub(super) const FILL_SCRATCH_BYTES: usize = MAX_GLOBAL_FILL_VERTICES * 256;

pub(super) struct FillScratch {
    tessellator: FillTessellator,
    buffers: VertexBuffers<lyon::math::Point, u16>,
}

impl FillScratch {
    pub(super) fn new() -> Self {
        Self {
            tessellator: FillTessellator::new(),
            buffers: VertexBuffers::with_capacity(0, 0),
        }
    }
}

pub(super) fn paths(
    scratch: &mut Scratch,
    prim: &Prim,
    transform: Affine,
    draw: f32,
    flow: f32,
    paint_scale: f32,
) -> Result<PrimPaths, BuildError> {
    let mut paths = [None, None];
    match prim {
        Prim::Line {
            points,
            closed,
            color,
            width,
            dashed,
        } => {
            let contour = if *closed {
                Contour::Closed
            } else {
                Contour::Open
            };
            paths[0] = scratch
                .stroke(
                    points,
                    contour,
                    draw,
                    dashed.then_some(flow),
                    *width,
                    transform,
                )?
                .map(|path| (path, *color));
        }
        Prim::Rect {
            fill: None,
            border: None,
            ..
        } => {}
        Prim::Rect {
            rect,
            radius,
            fill,
            border,
        } => {
            let charge = fallback_corner_segments(*radius, paint_scale);
            let (radius, segments) = corners(*rect, *radius, scale(transform), charge)?;
            let outer = Rounded::new(*rect, radius, segments);
            if let Some(paint) = fill {
                paths[0] = filled(
                    &mut scratch.fill,
                    outer.iter(),
                    FillRule::NonZero,
                    transform,
                )?
                .map(|path| (path, *paint));
            }
            if let Some((paint, width)) = border {
                let half = rect.size.x.min(rect.size.y) / 2.0;
                let width = width.min(half);
                if width > 0.0 {
                    let inset = Vec2::new(width, width);
                    let inner = Rounded::new(
                        Rect {
                            origin: rect.origin + inset,
                            size: rect.size - inset * 2.0,
                        },
                        (radius - width).max(0.0),
                        segments,
                    );
                    let path = if draw < 1.0 {
                        sector(&mut scratch.fill, outer, inner, draw, transform)?
                    } else if width < half {
                        tessellated(
                            &mut scratch.fill,
                            events(outer.iter()).chain(events(inner.iter())),
                            FillRule::EvenOdd,
                            transform,
                        )?
                    } else {
                        filled(
                            &mut scratch.fill,
                            outer.iter(),
                            FillRule::NonZero,
                            transform,
                        )?
                    };
                    paths[1] = path.map(|path| (path, *paint));
                }
            }
        }
        Prim::Text { .. } => return Err(BuildError::Text),
        Prim::Fill { .. } => return Err(BuildError::Stale),
    }
    Ok(paths)
}

#[derive(Clone, Copy)]
struct Rounded {
    rect: Rect,
    radius: f32,
    segments: u32,
}

impl Rounded {
    fn new(rect: Rect, radius: f32, segments: u32) -> Self {
        Self {
            rect,
            radius,
            segments,
        }
    }

    fn len(self) -> usize {
        usize::try_from(self.segments + 1).expect("bounded corner segments") * 4
    }

    fn at(self, index: usize) -> Vec2 {
        let near = self.rect.origin + Vec2::new(self.radius, self.radius);
        let far = self.rect.max() - Vec2::new(self.radius, self.radius);
        let per = usize::try_from(self.segments + 1).expect("bounded corner segments");
        let (center, start) = [
            (Vec2::new(far.x, near.y), -FRAC_PI_2),
            (far, 0.0),
            (Vec2::new(near.x, far.y), FRAC_PI_2),
            (near, PI),
        ][index / per];
        let step = FRAC_PI_2 / f64::from(self.segments.max(1));
        let index = u32::try_from(index % per).expect("bounded corner index");
        let angle = start + step * f64::from(index);
        center + Vec2::new(narrow(angle.cos()), narrow(angle.sin())) * self.radius
    }

    fn iter(self) -> impl DoubleEndedIterator<Item = Vec2> + ExactSizeIterator + Clone {
        (0..self.len()).map(move |index| self.at(index))
    }
}

#[expect(
    clippy::many_single_char_names,
    reason = "a and b bound the outer edge, c and d the inner edge, and t the revealed fraction"
)]
fn sector(
    scratch: &mut FillScratch,
    outer: Rounded,
    inner: Rounded,
    draw: f32,
    transform: Affine,
) -> Result<Option<Path<Pixels>>, BuildError> {
    let total: f64 = (0..outer.len())
        .map(|index| length(outer.at(index), outer.at((index + 1) % outer.len())))
        .sum();
    let mut left = total * f64::from(draw.clamp(0.0, 1.0));
    if left.is_nan() || left <= 0.0 {
        return Ok(None);
    }
    for index in 0..outer.len() {
        let a = outer.at(index);
        let b = outer.at((index + 1) % outer.len());
        let span = length(a, b);
        if left <= span || index == outer.len() - 1 {
            let c = inner.at(index);
            let d = inner.at((index + 1) % inner.len());
            let t = narrow((left / span).min(1.0));
            let contour = outer
                .iter()
                .take(index + 1)
                .chain([a + (b - a) * t, c + (d - c) * t])
                .chain(inner.iter().take(index + 1).rev());
            return filled(scratch, contour, FillRule::NonZero, transform);
        }
        left -= span;
    }
    Ok(None)
}

fn events(points: impl Iterator<Item = Vec2> + Clone) -> impl Iterator<Item = PathEvent> {
    let points = points.map(|at| lyon::math::point(at.x, at.y));
    let first = points.clone().next();
    let last = points.clone().last();
    let lines = points.skip(1).scan(first, |from, to| {
        Some(PathEvent::Line {
            from: from.replace(to)?,
            to,
        })
    });
    first
        .map(|at| PathEvent::Begin { at })
        .into_iter()
        .chain(lines)
        .chain(first.zip(last).map(|(first, last)| PathEvent::End {
            first,
            last,
            close: true,
        }))
}

fn filled(
    scratch: &mut FillScratch,
    points: impl Iterator<Item = Vec2> + Clone,
    rule: FillRule,
    transform: Affine,
) -> Result<Option<Path<Pixels>>, BuildError> {
    tessellated(scratch, events(points), rule, transform)
}

fn tessellated(
    scratch: &mut FillScratch,
    events: impl Iterator<Item = PathEvent>,
    rule: FillRule,
    transform: Affine,
) -> Result<Option<Path<Pixels>>, BuildError> {
    let fill = paint_phase::enter(Phase::LyonFill);
    let buffers = &mut scratch.buffers;
    buffers.clear();
    scratch
        .tessellator
        .tessellate(
            events,
            &FillOptions::default().with_fill_rule(rule),
            &mut BuffersBuilder::new(buffers, |vertex: FillVertex<'_>| vertex.position()),
        )
        .map_err(|_| BuildError::Fill)?;
    drop(fill);
    let _app = paint_phase::enter(Phase::App);
    let triangles = buffers.indices.iter().map(|index| {
        let at = buffers.vertices[usize::from(*index)];
        Vec2::new(at.x, at.y)
    });
    Ok(super::paint_paths::placed(triangles, transform))
}

#[cfg(test)]
#[path = "paint_geometry_tests.rs"]
mod tests;
