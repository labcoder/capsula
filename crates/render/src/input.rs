//! The owner's copy of a staged view into build input batches, made once per host. It counts
//! each rect's corner pieces here, with the scene crate's own count at the scale it charged.

use kumochi_scene::{
    display::{OUTLINE_POINT_LIMIT, Paint, Prim, corner_segments},
    geom::Rect,
    record::Align as Placed,
};

use crate::{
    cache::RenderCache,
    stage::{Align, BuildInput, Color, Cursor, Failure, Shape, index},
};

impl BuildInput {
    /// Copies a staged view's primitives from `from` on, in order and each whole, until the
    /// view ends or a part of this batch is full. Returns where the next batch must start, or
    /// none once the view is whole. A batch whose view table, headers or other parts can't take
    /// the next primitive returns `from` and keeps nothing of this view. Whoever owns the batch
    /// wipes it once it is built, on every path.
    ///
    /// # Errors
    /// [`Failure::Order`] for a cursor no copy of this view returns, or a view the batch
    /// already holds a part of; [`Failure::Build`] when a primitive doesn't fit an empty
    /// batch, or a rect needs more corner pieces than one outline holds. The batch is then as
    /// it was.
    pub fn add(&mut self, cache: &RenderCache, from: Cursor) -> Result<Option<Cursor>, Failure> {
        let mark = self.mark();
        let empty = self.is_empty();
        let copied = self.copy(cache, from);
        match copied {
            Ok(Some(to)) if to == from => {
                self.rewind(mark);
                if empty {
                    return Err(Failure::Build);
                }
            }
            Err(_) => self.rewind(mark),
            Ok(_) => {}
        }
        copied
    }

    fn copy(&mut self, cache: &RenderCache, from: Cursor) -> Result<Option<Cursor>, Failure> {
        let sources = cache.sources();
        let first = usize::try_from(from.object).map_err(|_| Failure::Order)?;
        let start = usize::try_from(from.prim).map_err(|_| Failure::Order)?;
        let issued = match sources.get(first) {
            Some((_, _, prims)) => start == 0 || start < prims.len(),
            None => sources.is_empty() && from == Cursor::START,
        };
        if !issued || self.holds(cache.id()) {
            return Err(Failure::Order);
        }
        if !self.has_room() {
            return Ok(Some(from));
        }
        self.begin(cache.id(), from)?;
        for (object, (_, paint_scale, prims)) in sources.iter().enumerate().skip(first) {
            let start = if object == first { start } else { 0 };
            if prims.is_empty() {
                if !self.fits(0, 0, 1) {
                    let to = at(object, 0)?;
                    self.end(Some(to))?;
                    return Ok(Some(to));
                }
                self.object()?;
                continue;
            }
            for (prim_index, prim) in prims.iter().enumerate().skip(start) {
                let (points, text) = match prim {
                    Prim::Fill { points, .. } | Prim::Line { points, .. } => (points.len(), 0),
                    Prim::Text { text, .. } => (0, text.as_str().len()),
                    Prim::Rect { .. } => (0, 0),
                };
                if !self.fits(points, text, usize::from(prim_index == start)) {
                    let to = at(object, prim_index)?;
                    self.end(Some(to))?;
                    return Ok(Some(to));
                }
                if prim_index == start {
                    self.object()?;
                }
                let shape = self.shape_of(prim, *paint_scale)?;
                self.shape(shape)?;
            }
        }
        Ok(None)
    }

    fn shape_of(&mut self, prim: &Prim, paint_scale: f32) -> Result<Shape, Failure> {
        Ok(match prim {
            Prim::Rect {
                rect,
                radius,
                fill,
                border,
            } => {
                let (radius, segments) = if fill.is_none() && border.is_none() {
                    (*radius, 0)
                } else {
                    corners(*rect, *radius, paint_scale)?
                };
                Shape::Rect {
                    rect: [rect.origin.x, rect.origin.y, rect.size.x, rect.size.y],
                    radius,
                    segments,
                    fill: fill.map(color),
                    border: border.map(|(paint, width)| (color(paint), width)),
                }
            }
            Prim::Fill {
                points,
                color: paint,
            } => Shape::Fill {
                points: self.push_points(points.iter().map(|at| [at.x, at.y]))?,
                color: color(*paint),
            },
            Prim::Line {
                points,
                closed,
                color: paint,
                width,
                dashed,
            } => Shape::Line {
                points: self.push_points(points.iter().map(|at| [at.x, at.y]))?,
                closed: *closed,
                color: color(*paint),
                width: *width,
                dashed: *dashed,
            },
            Prim::Text {
                origin,
                text,
                style,
                color: paint,
                max_width,
                align,
            } => Shape::Text {
                origin: [origin.x, origin.y],
                text: self.push_text(text.as_str())?,
                size: style.size,
                bold: style.bold,
                color: color(*paint),
                max_width: *max_width,
                align: match align {
                    Placed::Left => Align::Left,
                    Placed::Center => Align::Center,
                    Placed::Right => Align::Right,
                },
            },
        })
    }
}

fn at(object: usize, prim: usize) -> Result<Cursor, Failure> {
    Ok(Cursor {
        object: index(object)?,
        prim: index(prim)?,
    })
}

pub(crate) fn color(paint: Paint) -> Color {
    Color {
        rgb: paint.rgb.get(),
        opacity: paint.opacity,
    }
}

/// The radius as painted and its pieces per corner at the charged scale. Main counts a pose's
/// own scale and refuses past the charge; this count holds for every pose.
fn corners(rect: Rect, radius: f32, paint_scale: f32) -> Result<(f32, u32), Failure> {
    let radius = radius.clamp(0.0, (rect.size.x.min(rect.size.y) * 0.5).max(0.0));
    let segments = corner_segments(f64::from(radius) * f64::from(paint_scale));
    let points = segments
        .saturating_add(1)
        .saturating_mul(4)
        .saturating_add(1);
    if points > OUTLINE_POINT_LIMIT {
        return Err(Failure::Build);
    }
    Ok((radius, u32::try_from(segments).map_err(|_| Failure::Build)?))
}
