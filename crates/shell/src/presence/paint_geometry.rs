use std::f64::consts::{FRAC_PI_2, PI};

use gpui_kit::{FillOptions, FillRule, Path, Pixels};
use kumochi_scene::{
    display::{Contour, Paint, Prim, fallback_corner_segments},
    geom::{Affine, Rect, Vec2},
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

pub(super) fn paths(
    scratch: &mut Scratch,
    prim: &Prim,
    transform: Affine,
    draw: f32,
    flow: f32,
) -> Result<[Option<(Path<Pixels>, Paint)>; 2], BuildError> {
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
            let charge = fallback_corner_segments(*rect, *radius);
            let (radius, segments) = corners(*rect, *radius, scale(transform), charge)?;
            let outer = Rounded::new(*rect, radius, segments);
            if let Some(paint) = fill {
                paths[0] =
                    filled(outer.iter(), FillRule::NonZero, transform)?.map(|path| (path, *paint));
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
                        sector(outer, inner, draw, transform)?
                    } else if width < half {
                        tessellated(
                            events(outer.iter()).chain(events(inner.iter())),
                            FillRule::EvenOdd,
                            transform,
                        )?
                    } else {
                        filled(outer.iter(), FillRule::NonZero, transform)?
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

fn sector(
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
            return filled(contour, FillRule::NonZero, transform);
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
    points: impl Iterator<Item = Vec2> + Clone,
    rule: FillRule,
    transform: Affine,
) -> Result<Option<Path<Pixels>>, BuildError> {
    tessellated(events(points), rule, transform)
}

fn tessellated(
    events: impl Iterator<Item = PathEvent>,
    rule: FillRule,
    transform: Affine,
) -> Result<Option<Path<Pixels>>, BuildError> {
    // shortcut: lyon fill allocates per build (OSC-249 exception, counted and reported); revisit after D1 (OSC-174)
    let _fill = paint_phase::enter(Phase::LyonFill);
    let buffers = {
        let mut buffers = VertexBuffers::<lyon::math::Point, u16>::new();
        FillTessellator::new()
            .tessellate(
                events,
                &FillOptions::default().with_fill_rule(rule),
                &mut BuffersBuilder::new(&mut buffers, |vertex: FillVertex<'_>| vertex.position()),
            )
            .map_err(|_| BuildError::Fill)?;
        buffers
    };
    let app = paint_phase::enter(Phase::App);
    let triangles = buffers.indices.iter().map(|index| {
        let at = buffers.vertices[usize::from(*index)];
        Vec2::new(at.x, at.y)
    });
    let path = super::paint_paths::placed(triangles, transform);
    drop(app);
    drop(buffers);
    Ok(path)
}

#[cfg(test)]
#[path = "paint_geometry_tests.rs"]
mod tests;
