use super::{
    PrepareError,
    state::{LayoutState, Placed},
};
use crate::{
    ErrorCode, Handle, Op, StepError, Text,
    display::{Item, Prim, TextMeasure, TextStyle},
    geom::{Affine, Rect, Vec2},
    path,
    record::{Align, Anchor, Color, Head, ShapeKind},
    state::object,
    theme::Theme,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) fn layout_error(code: ErrorCode) -> PrepareError {
    PrepareError::Layout(StepError::new(code))
}
/// An internal invariant failure: the layout prefix lacks, or holds another kind of, what
/// admission guarantees. It fails the step, where `Stale` would invite a retry.
pub(super) fn invariant() -> PrepareError {
    layout_error(ErrorCode::UnknownRef)
}

pub(super) fn text_origin(ink: Rect, max_width: Option<f32>, align: Align) -> Vec2 {
    let spare = max_width.map_or(0.0, |width| width - ink.size.x);
    let offset = match align {
        Align::Left => 0.0,
        Align::Center => spare * 0.5,
        Align::Right => spare,
    };
    ink.origin - Vec2::new(offset, 0.0)
}

pub(super) struct Build<'a> {
    pub viewport: Rect,
    pub theme: &'a Theme,
    pub measure: &'a mut dyn TextMeasure,
    pub cancel: &'a AtomicBool,
    pub scales: &'a BTreeMap<Handle, f32>,
}
struct Geometry {
    bounds: Rect,
    anchors: [Vec2; 2],
    primitives: Vec<Prim>,
}

impl Build<'_> {
    pub(super) fn check_cancel(&self) -> Result<(), PrepareError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(PrepareError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub(super) fn measure(
        &mut self,
        text: &Text,
        style: TextStyle,
        width: Option<f32>,
    ) -> Result<Vec2, PrepareError> {
        self.check_cancel()?;
        if !style.size.is_finite()
            || style.size <= 0.0
            || style.size > crate::limits::MAX_COORD
            || width.is_some_and(|width| {
                !width.is_finite() || width <= 0.0 || width > crate::limits::MAX_COORD
            })
        {
            return Err(layout_error(ErrorCode::OutOfRange));
        }
        let [x, y] = self
            .measure
            .measure(text.as_str(), style, width)
            .map_err(|_| PrepareError::TextMeasure)?;
        let size = Vec2::new(x, y);
        if !size.within_canvas() || x < 0.0 || y < 0.0 {
            return Err(layout_error(ErrorCode::OutOfRange));
        }
        Ok(size)
    }
    pub(super) fn color(&self, color: Option<&Color>) -> crate::display::Paint {
        color
            .map_or(self.theme.accent, |color| self.theme.resolve(color))
            .into()
    }
    pub(super) fn primitive(
        &mut self,
        source: &Arc<Op>,
        handle: Handle,
        parent: Option<Handle>,
        state: &LayoutState,
        cached: Option<&Placed>,
    ) -> Result<Placed, PrepareError> {
        self.check_cancel()?;
        let visual = object::visual(source).ok_or_else(|| layout_error(ErrorCode::Unsupported))?;
        let geometry = match &**source {
            Op::Text(value) => self.text(value, state, parent, cached)?,
            Op::Path(value) => self.path(value, handle, state, parent, cached)?,
            Op::Line(value) => self.line(value, Head::None, state, parent, cached)?,
            Op::Arrow(value) => self.line(value, Head::End, state, parent, cached)?,
            Op::Shape(value) => self.shape(value, state, parent, cached)?,
            Op::Highlight(value) => self.highlight(value, state, parent, cached)?,
            Op::Callout(value) => {
                return super::callout::build(self, source, value, handle, parent, state, cached);
            }
            _ => return Err(layout_error(ErrorCode::Unsupported)),
        };
        if !geometry.bounds.within_canvas() {
            return Err(layout_error(ErrorCode::OutOfRange));
        }
        let mut placed = Placed::new(
            Item {
                handle,
                parent,
                primitives: geometry.primitives.into(),
                bounds: geometry.bounds,
                transform: Affine::default(),
                opacity: visual.opacity.map_or(1.0, crate::Finite::get),
            },
            Arc::clone(source),
            cached,
        );
        placed.anchors = geometry.anchors;
        Ok(placed)
    }

    fn text(
        &mut self,
        text: &crate::record::TextOp,
        state: &LayoutState,
        parent: Option<Handle>,
        cached: Option<&Placed>,
    ) -> Result<Geometry, PrepareError> {
        let mut anchors = [Vec2::default(); 2];
        let mut output = Vec::new();
        let bounds = {
            let at = if let Some(previous) = cached {
                previous.anchors[0]
            } else {
                anchor(state, &text.at, parent)?.0
            };
            anchors[0] = at;
            let style = TextStyle {
                size: text.size.map_or(self.theme.font_size, crate::Finite::get),
                bold: text.bold,
            };
            let width = text.max_width.map(crate::Finite::get);
            let size = self.measure(&text.text, style, width)?;
            let x = match text.align {
                Align::Left => at.x,
                Align::Center => at.x - size.x * 0.5,
                Align::Right => at.x - size.x,
            };
            let origin = Vec2::new(x, at.y - size.y * 0.5);
            output.push(Prim::Text {
                origin: text_origin(Rect { origin, size }, width, text.align),
                text: text.text.clone(),
                style,
                color: text
                    .color
                    .as_ref()
                    .map_or(self.theme.fg, |color| self.theme.resolve(color))
                    .into(),
                max_width: width,
                align: text.align,
            });
            Rect { origin, size }
        };
        Ok(Geometry {
            bounds,
            anchors,
            primitives: output,
        })
    }

    fn path(
        &mut self,
        path_op: &crate::record::PathOp,
        handle: Handle,
        state: &LayoutState,
        parent: Option<Handle>,
        cached: Option<&Placed>,
    ) -> Result<Geometry, PrepareError> {
        let mut anchors = [Vec2::default(); 2];
        let mut output = Vec::new();
        let bounds = {
            let at = if let Some(previous) = cached {
                previous.anchors[0]
            } else {
                path_op
                    .at
                    .as_ref()
                    .map(|at| anchor(state, at, parent).map(|v| v.0))
                    .transpose()?
                    .unwrap_or_default()
            };
            anchors[0] = at;
            if let Some(&scale) = self.scales.get(&handle) {
                let local_scale = path_op.scale.map_or(1.0, crate::Finite::get);
                let paths = super::path::flatten(path_op.d.as_str(), scale)?;
                let mut all = Vec::new();
                for path in paths {
                    let points: Arc<[Vec2]> = path
                        .points
                        .iter()
                        .map(|point| *point * local_scale + at)
                        .collect();
                    all.extend(points.iter().copied());
                    if let Some(fill) = &path_op.fill {
                        output.push(Prim::Fill {
                            points: Arc::clone(&points),
                            color: self.theme.resolve(fill).into(),
                        });
                    }
                    output.push(Prim::Line {
                        points,
                        closed: path.closed,
                        color: self.color(path_op.color.as_ref()),
                        width: path_op.width.map_or(2.0, crate::Finite::get),
                        dashed: path_op.dash,
                    });
                }
                point_bounds(&all)?
            } else {
                let previous = cached.ok_or_else(invariant)?;
                output.extend(
                    previous
                        .item
                        .primitives
                        .iter()
                        .cloned()
                        .map(|mut primitive| {
                            if let Prim::Line { color, .. } = &mut primitive {
                                *color = self.color(path_op.color.as_ref());
                            }
                            primitive
                        }),
                );
                previous.item.bounds
            }
        };
        Ok(Geometry {
            bounds,
            anchors,
            primitives: output,
        })
    }

    fn line(
        &mut self,
        line: &crate::record::Line,
        default_head: Head,
        state: &LayoutState,
        parent: Option<Handle>,
        cached: Option<&Placed>,
    ) -> Result<Geometry, PrepareError> {
        let anchors;
        let mut output = Vec::new();
        let bounds = {
            anchors = if let Some(previous) = cached {
                previous.anchors
            } else {
                [
                    anchor(state, &line.from, parent)?.0,
                    anchor(state, &line.to, parent)?.0,
                ]
            };
            let mut points = vec![anchors[0]];
            if !line.via.is_empty() {
                points.extend(
                    line.via
                        .iter()
                        .map(|point| Vec2::new(point[0].get(), point[1].get())),
                );
            } else if let Some(bend) = line.bend.filter(|bend| bend.get().abs() > 0.001) {
                let delta = anchors[1] - anchors[0];
                let middle = (anchors[0] + anchors[1]) * 0.5;
                let control = middle + Vec2::new(-delta.y, delta.x) * (bend.get() * 0.5);
                for index in 1..path::PROCEDURAL_CURVE_PIECES {
                    let t =
                        f32::from(u16::try_from(index).expect("procedural curve count fits u16"))
                            / f32::from(
                                u16::try_from(path::PROCEDURAL_CURVE_PIECES)
                                    .expect("procedural curve count fits u16"),
                            );
                    points.push(
                        anchors[0] * (1.0 - t).powi(2)
                            + control * (2.0 * (1.0 - t) * t)
                            + anchors[1] * t.powi(2),
                    );
                }
            }
            points.push(anchors[1]);
            let color = self.color(line.color.as_ref());
            let head = line.head.unwrap_or(default_head);
            let mut extent = point_bounds(&points)?;
            if matches!(head, Head::Start | Head::Both) {
                let head = arrow_head(points[1], points[0]);
                extent = extent.union(point_bounds(&head)?);
                output.push(Prim::Fill {
                    points: head.into(),
                    color,
                });
            }
            if matches!(head, Head::End | Head::Both) {
                let head = arrow_head(points[points.len() - 2], points[points.len() - 1]);
                extent = extent.union(point_bounds(&head)?);
                output.push(Prim::Fill {
                    points: head.into(),
                    color,
                });
            }
            output.push(Prim::Line {
                points: points.into(),
                closed: false,
                color,
                width: line.width.map_or(2.0, crate::Finite::get),
                dashed: line.dash,
            });
            extent
        };
        Ok(Geometry {
            bounds,
            anchors,
            primitives: output,
        })
    }

    fn shape(
        &mut self,
        shape: &crate::record::Shape,
        state: &LayoutState,
        parent: Option<Handle>,
        cached: Option<&Placed>,
    ) -> Result<Geometry, PrepareError> {
        let mut anchors = [Vec2::default(); 2];
        let mut output;
        let bounds = {
            let rect = if let Some(bounds) = shape.bounds {
                rect(bounds)
            } else {
                let at = if let Some(previous) = cached {
                    previous.anchors[0]
                } else {
                    shape
                        .at
                        .as_ref()
                        .map(|at| anchor(state, at, parent).map(|v| v.0))
                        .transpose()?
                        .unwrap_or_default()
                };
                anchors[0] = at;
                let size = shape.size.map_or_else(
                    || {
                        let radius = shape.r.map_or(40.0, crate::Finite::get);
                        Vec2::new(radius * 2.0, radius * 2.0)
                    },
                    |size| Vec2::new(size[0].get(), size[1].get()),
                );
                Rect::centered(at, size)
            };
            let rect = if shape.kind == ShapeKind::Circle {
                let diameter = rect.size.x.min(rect.size.y);
                Rect::centered(rect.center(), Vec2::new(diameter, diameter))
            } else {
                rect
            };
            let (mut bounds, outline) = self.shape_outline(shape, rect)?;
            output = outline;
            if let Some(label) = &shape.label {
                let style = TextStyle {
                    size: self.theme.font_size,
                    bold: false,
                };
                let measured = self.measure(label, style, Some(bounds.size.x))?;
                let origin = bounds.center() - measured * 0.5;
                output.push(Prim::Text {
                    origin: text_origin(
                        Rect {
                            origin,
                            size: measured,
                        },
                        Some(bounds.size.x),
                        Align::Center,
                    ),
                    text: label.clone(),
                    style,
                    color: self.theme.fg.into(),
                    max_width: Some(bounds.size.x),
                    align: Align::Center,
                });
                bounds = bounds.union(Rect {
                    origin,
                    size: measured,
                });
            }
            bounds
        };
        Ok(Geometry {
            bounds,
            anchors,
            primitives: output,
        })
    }

    fn highlight(
        &mut self,
        highlight: &crate::record::Highlight,
        state: &LayoutState,
        parent: Option<Handle>,
        cached: Option<&Placed>,
    ) -> Result<Geometry, PrepareError> {
        let anchors;
        let mut output = Vec::new();
        let bounds = {
            let (_, mut target) = if let Some(old) = cached {
                (
                    old.anchors[0],
                    Rect {
                        origin: old.anchors[0],
                        size: old.anchors[1],
                    },
                )
            } else {
                anchor(state, &highlight.target, parent)?
            };
            if target.size.x < 2.0 || target.size.y < 2.0 {
                target = Rect::centered(target.center(), Vec2::new(24.0, 24.0));
            }
            anchors = [target.origin, target.size];
            let pad = highlight.pad.map_or(6.0, crate::Finite::get);
            let bounds = Rect {
                origin: target.origin - Vec2::new(pad, pad),
                size: target.size + Vec2::new(pad * 2.0, pad * 2.0),
            };
            let color = highlight.color.as_ref().map_or_else(
                || {
                    self.theme
                        .resolve(&Color::Token(crate::record::Token::Amber))
                        .into()
                },
                |color| self.theme.resolve(color).into(),
            );
            match highlight.style {
                crate::record::HighlightStyle::Underline => output.push(Prim::Line {
                    points: Arc::from([Vec2::new(bounds.origin.x, bounds.max().y), bounds.max()]),
                    closed: false,
                    color,
                    width: 3.0,
                    dashed: false,
                }),
                crate::record::HighlightStyle::Marker => output.push(Prim::Rect {
                    rect: bounds,
                    radius: 0.0,
                    fill: Some(color.faded(0.25)),
                    border: None,
                }),
                crate::record::HighlightStyle::Box => output.push(Prim::Rect {
                    rect: bounds,
                    radius: 6.0,
                    fill: Some(color.faded(0.16)),
                    border: Some((color, 2.0)),
                }),
                crate::record::HighlightStyle::Spotlight => {
                    output = self.spotlight(bounds);
                    output.push(Prim::Rect {
                        rect: bounds,
                        radius: 8.0,
                        fill: None,
                        border: Some((color, 3.0)),
                    });
                }
                crate::record::HighlightStyle::Ring => output.push(Prim::Rect {
                    rect: bounds,
                    radius: 6.0,
                    fill: None,
                    border: Some((color, 2.0)),
                }),
            }
            bounds
        };
        Ok(Geometry {
            bounds,
            anchors,
            primitives: output,
        })
    }

    fn shape_outline(
        &mut self,
        shape: &crate::record::Shape,
        rect: Rect,
    ) -> Result<(Rect, Vec<Prim>), PrepareError> {
        let mut output = Vec::new();
        let color = self.color(shape.color.as_ref());
        let fill = shape
            .fill
            .as_ref()
            .map(|fill| self.theme.resolve(fill).into());
        let width = shape.width.map_or(2.0, crate::Finite::get);
        let mut bounds = rect;
        match shape.kind {
            ShapeKind::Rect => {
                let radius = shape
                    .radius
                    .map_or(0.0, crate::Finite::get)
                    .min(rect.size.x.min(rect.size.y) * 0.5);
                output.push(Prim::Rect {
                    rect,
                    radius,
                    fill,
                    border: (!shape.dash).then_some((color, width)),
                });
                if shape.dash {
                    output.push(Prim::Line {
                        points: rounded_rect(rect, radius),
                        closed: true,
                        color,
                        width,
                        dashed: true,
                    });
                }
            }
            ShapeKind::Circle | ShapeKind::Ellipse => {
                let points = ellipse(rect, path::ELLIPSE_SEGMENTS);
                if let Some(color) = fill {
                    output.push(Prim::Fill {
                        points: Arc::clone(&points),
                        color,
                    });
                }
                output.push(Prim::Line {
                    points,
                    closed: true,
                    color,
                    width,
                    dashed: shape.dash,
                });
            }
            ShapeKind::Polygon => {
                let points: Arc<[Vec2]> = shape
                    .points
                    .iter()
                    .map(|point| Vec2::new(point[0].get(), point[1].get()))
                    .collect();
                if points.len() < 3 {
                    return Err(layout_error(ErrorCode::OutOfRange));
                }
                bounds = point_bounds(&points)?;
                if let Some(color) = fill {
                    output.push(Prim::Fill {
                        points: Arc::clone(&points),
                        color,
                    });
                }
                output.push(Prim::Line {
                    points,
                    closed: true,
                    color,
                    width,
                    dashed: shape.dash,
                });
            }
        }

        Ok((bounds, output))
    }

    fn spotlight(&self, bounds: Rect) -> Vec<Prim> {
        let mut output = Vec::new();
        let min = Vec2::new(
            bounds
                .origin
                .x
                .clamp(self.viewport.origin.x, self.viewport.max().x),
            bounds
                .origin
                .y
                .clamp(self.viewport.origin.y, self.viewport.max().y),
        );
        let max = Vec2::new(
            bounds.max().x.clamp(min.x, self.viewport.max().x),
            bounds.max().y.clamp(min.y, self.viewport.max().y),
        );
        let viewport = self.viewport;
        for (origin, size) in [
            (
                viewport.origin,
                Vec2::new(viewport.size.x, min.y - viewport.origin.y),
            ),
            (
                Vec2::new(viewport.origin.x, max.y),
                Vec2::new(viewport.size.x, viewport.max().y - max.y),
            ),
            (
                Vec2::new(viewport.origin.x, min.y),
                Vec2::new(min.x - viewport.origin.x, max.y - min.y),
            ),
            (
                Vec2::new(max.x, min.y),
                Vec2::new(viewport.max().x - max.x, max.y - min.y),
            ),
        ] {
            if size.x > 0.0 && size.y > 0.0 {
                output.push(Prim::Rect {
                    rect: Rect { origin, size },
                    radius: 0.0,
                    fill: Some(crate::display::Paint::from(self.theme.halo).faded(0.5)),
                    border: None,
                });
            }
        }

        output
    }
}

pub(super) fn anchor(
    state: &LayoutState,
    anchor: &Anchor,
    parent: Option<Handle>,
) -> Result<(Vec2, Rect), PrepareError> {
    match anchor {
        Anchor::At(point) => {
            let point = Vec2::new(point[0].get(), point[1].get());
            Ok((
                point,
                Rect {
                    origin: point,
                    size: Vec2::default(),
                },
            ))
        }
        Anchor::Ref(id) => {
            let bounds = reference(state, id, parent)?;
            Ok((bounds.center(), bounds))
        }
    }
}
pub(super) fn reference(
    state: &LayoutState,
    id: &crate::WireId,
    parent: Option<Handle>,
) -> Result<Rect, PrepareError> {
    // shortcut: references resolve at preparation; add following only with motion ownership.
    let object = state
        .objects
        .get(id)
        .ok_or_else(|| layout_error(ErrorCode::UnknownRef))?;
    let bounds = if parent == object.item.parent {
        object
            .item
            .transform
            .compose(object.rest.transform())
            .bounds(object.item.bounds)
    } else {
        let world = state.world_bounds(object)?;
        if let Some(parent) = parent {
            state
                .resting_transform(parent)?
                .inverse()
                .ok_or_else(|| layout_error(ErrorCode::TransformOverflow))?
                .bounds(world)
        } else {
            world
        }
    };
    if !bounds.within_canvas() {
        return Err(layout_error(ErrorCode::OutOfRange));
    }
    Ok(bounds)
}
pub(super) fn rect(bounds: [crate::Finite; 4]) -> Rect {
    Rect {
        origin: Vec2::new(bounds[0].get(), bounds[1].get()),
        size: Vec2::new(bounds[2].get(), bounds[3].get()),
    }
}
pub(super) fn point_bounds(points: &[Vec2]) -> Result<Rect, PrepareError> {
    if points.iter().any(|point| !point.within_canvas()) {
        return Err(layout_error(ErrorCode::OutOfRange));
    }
    let Some(first) = points.first().copied() else {
        return Ok(Rect::default());
    };
    let (mut min, mut max) = (first, first);
    for point in points {
        min = Vec2::new(min.x.min(point.x), min.y.min(point.y));
        max = Vec2::new(max.x.max(point.x), max.y.max(point.y));
    }
    Ok(Rect {
        origin: min,
        size: max - min,
    })
}
pub(super) fn ellipse(rect: Rect, pieces: usize) -> Arc<[Vec2]> {
    (0..pieces)
        .map(|index| {
            let angle = f32::from(u16::try_from(index).expect("procedural segment count fits u16"))
                / f32::from(u16::try_from(pieces).expect("procedural segment count fits u16"))
                * std::f32::consts::TAU;
            let (sin, cos) = angle.sin_cos();
            rect.center() + Vec2::new(cos * rect.size.x * 0.5, sin * rect.size.y * 0.5)
        })
        .collect()
}
fn rounded_rect(rect: Rect, radius: f32) -> Arc<[Vec2]> {
    let origin = rect.origin;
    let max = rect.max();
    if radius <= f32::EPSILON {
        return Arc::from([
            origin,
            Vec2::new(max.x, origin.y),
            max,
            Vec2::new(origin.x, max.y),
        ]);
    }
    let mut points = Vec::with_capacity(28);
    for (center, start) in [
        (
            Vec2::new(origin.x + radius, origin.y + radius),
            std::f32::consts::PI,
        ),
        (
            Vec2::new(max.x - radius, origin.y + radius),
            std::f32::consts::PI * 1.5,
        ),
        (Vec2::new(max.x - radius, max.y - radius), 0.0),
        (
            Vec2::new(origin.x + radius, max.y - radius),
            std::f32::consts::FRAC_PI_2,
        ),
    ] {
        for index in 0..=6_u16 {
            let angle = start + f32::from(index) / 6.0 * std::f32::consts::FRAC_PI_2;
            let (sin, cos) = angle.sin_cos();
            points.push(center + Vec2::new(cos, sin) * radius);
        }
    }
    points.into()
}
pub(super) fn arrow_head(from: Vec2, to: Vec2) -> [Vec2; 3] {
    let delta = to - from;
    let length = delta.x.hypot(delta.y);
    let direction = if length > f32::EPSILON {
        delta * (1.0 / length)
    } else {
        Vec2::new(1.0, 0.0)
    };
    let base = to - direction * 9.0;
    let side = Vec2::new(-direction.y, direction.x) * 4.0;
    [to, base + side, base - side]
}
