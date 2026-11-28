//! The owner's copy of a staged view into the build input, made once per host. It counts each
//! rect's corner pieces here, with the scene crate's own count at the scale it charged.

use kumochi_scene::{
    display::{OUTLINE_POINT_LIMIT, Paint, Prim, corner_segments},
    geom::Rect,
    record::Align as Placed,
};

use crate::{
    cache::RenderCache,
    stage::{Align, BuildInput, Color, Failure, Shape},
};

impl BuildInput {
    /// Copies a staged view's primitive headers, points and label text, whole or not at all.
    ///
    /// # Errors
    /// [`Failure::Build`] when the view doesn't fit what is left of the input, or a rect needs
    /// more corner pieces than one outline holds. The input is then as it was.
    pub fn add(&mut self, cache: &RenderCache) -> Result<(), Failure> {
        let mark = self.mark();
        let copied = self.copy(cache);
        if copied.is_err() {
            self.rewind(mark);
        }
        copied
    }

    fn copy(&mut self, cache: &RenderCache) -> Result<(), Failure> {
        self.begin(cache.id())?;
        for (_, paint_scale, prims) in cache.sources() {
            self.object(prims.len())?;
            for prim in prims.iter() {
                let shape = match prim {
                    Prim::Rect {
                        rect,
                        radius,
                        fill,
                        border,
                    } => {
                        let (radius, segments) = if fill.is_none() && border.is_none() {
                            (*radius, 0)
                        } else {
                            corners(*rect, *radius, *paint_scale)?
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
                };
                self.shape(shape)?;
            }
        }
        Ok(())
    }
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
