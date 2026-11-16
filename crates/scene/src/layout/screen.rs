//! Dressing on the screen surface: marks drawn around a thing's box, never over it, and labels
//! beside them, with every chrome size in logical pixels.
use super::{
    PrepareError,
    primitives::{Build, Geometry, layout_error, target},
    state::{LayoutState, Placed},
};
use crate::{
    ErrorCode, Handle, Op,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    limits::{MAX_MARKER_OPACITY, MAX_SPOTLIGHT_DIM},
    record::{Align, Callout, Highlight, HighlightStyle, Rgb, Side},
    screen::{
        LABEL_GAP, LABEL_PAD, MARK_EDGE, MARK_EDGE_RGB, MARK_PAD, MARK_RADIUS, MARK_RGB,
        MARK_STROKE, RING_LOOP_MAX, RING_RADIUS,
    },
};
use std::{f32::consts::FRAC_1_SQRT_2, sync::Arc};

fn paint(rgb: u32) -> Paint {
    Paint::from(Rgb::try_from(rgb).expect("screen mark colours are RGB"))
}

fn grow(rect: Rect, by: f32) -> Rect {
    Rect {
        origin: rect.origin - Vec2::new(by, by),
        size: rect.size + Vec2::new(by * 2.0, by * 2.0),
    }
}

/// Lays out a highlight on the screen surface, where `unit` is canvas units per logical pixel.
/// Its box, ring and underline lie wholly outside the target's padded box: a rounded outline's
/// inner edge is grown by `r(1 - 1/√2)` so its corner arc of radius `r` clears the padded corner.
pub(super) fn highlight(
    build: &Build<'_>,
    highlight: &Highlight,
    state: &LayoutState,
    parent: Option<Handle>,
    cached: Option<&Placed>,
    unit: f32,
) -> Result<Geometry, PrepareError> {
    let target = match cached {
        Some(old) => Rect {
            origin: old.anchors[0],
            size: old.anchors[1],
        },
        None => target(state, &highlight.target, parent)?.1,
    };
    let pad = highlight.pad.map_or(MARK_PAD * unit, crate::Finite::get);
    let (stroke, edge) = (MARK_STROKE * unit, MARK_EDGE * unit);
    let outline = stroke + edge * 2.0;
    let color = highlight.color.as_ref().map_or_else(
        || paint(MARK_RGB),
        |color| build.theme.resolve(color).into(),
    );
    let padded = grow(target, pad);
    let (long, short) = (
        padded.size.x.max(padded.size.y),
        padded.size.x.min(padded.size.y),
    );
    let looped = short * 0.5 / FRAC_1_SQRT_2;
    let radius = match highlight.style {
        HighlightStyle::Ring if long <= RING_LOOP_MAX * unit => looped,
        HighlightStyle::Ring => RING_RADIUS * unit,
        _ if long >= short * 3.0 => looped,
        _ => MARK_RADIUS * unit,
    };
    let outer = grow(grow(padded, radius * (1.0 - FRAC_1_SQRT_2)), outline);
    let mut primitives = Vec::with_capacity(6);
    match highlight.style {
        HighlightStyle::Underline => {
            let bar = Rect {
                origin: Vec2::new(target.origin.x, target.max().y + pad),
                size: Vec2::new(target.size.x, outline),
            };
            primitives.push(Prim::Rect {
                rect: bar,
                radius: outline * 0.5,
                fill: Some(paint(MARK_EDGE_RGB)),
                border: None,
            });
            primitives.push(Prim::Rect {
                rect: Rect {
                    origin: bar.origin + Vec2::new(edge, edge),
                    size: Vec2::new((bar.size.x - edge * 2.0).max(0.0), stroke),
                },
                radius: stroke * 0.5,
                fill: Some(color),
                border: None,
            });
            return Ok(Geometry {
                bounds: bar,
                anchors: [target.origin, target.size],
                primitives,
            });
        }
        HighlightStyle::Marker => {
            primitives.push(Prim::Rect {
                rect: padded,
                radius: MARK_RADIUS * unit,
                fill: Some(color.faded(MAX_MARKER_OPACITY)),
                border: None,
            });
            return Ok(Geometry {
                bounds: padded,
                anchors: [target.origin, target.size],
                primitives,
            });
        }
        HighlightStyle::Spotlight => {
            primitives = build.spotlight(outer, paint(0).faded(MAX_SPOTLIGHT_DIM));
        }
        HighlightStyle::Box | HighlightStyle::Ring => {}
    }
    primitives.push(Prim::Rect {
        rect: outer,
        radius: radius + outline,
        fill: None,
        border: Some((paint(MARK_EDGE_RGB), outline)),
    });
    primitives.push(Prim::Rect {
        rect: grow(outer, -edge),
        radius: radius + stroke + edge,
        fill: None,
        border: Some((color, stroke)),
    });
    Ok(Geometry {
        bounds: outer,
        anchors: [target.origin, target.size],
        primitives,
    })
}

/// Lays out a label on the screen surface: one line, outside its target's marked box and every
/// other object's, on the side named first and then the others.
/// # Errors
/// `NoRoom` when no side fits on the canvas.
pub(super) fn callout(
    build: &mut Build<'_>,
    op: &Arc<Op>,
    callout: &Callout,
    handle: Handle,
    state: &LayoutState,
    cached: Option<&Placed>,
    unit: f32,
) -> Result<Placed, PrepareError> {
    let style = TextStyle {
        size: build.theme.font_size * unit,
        bold: false,
    };
    let pad = Vec2::new(LABEL_PAD[0] * unit, LABEL_PAD[1] * unit);
    let size = build.measure(&callout.text, style, None)? + pad * 2.0;
    let center = if let Some(old) = cached {
        old.anchors[1]
    } else {
        let reach = MARK_PAD + MARK_STROKE + MARK_EDGE * 2.0 + LABEL_GAP;
        let keep_out = grow(target(state, &callout.target, None)?.1, reach * unit);
        let taken = state
            .objects
            .values()
            .map(|object| state.world_bounds(object))
            .collect::<Result<Vec<_>, _>>()?;
        let named = callout.side.filter(|side| *side != Side::Center);
        named
            .into_iter()
            .chain(SIDES.into_iter().filter(|side| Some(*side) != named))
            .map(|side| beside(keep_out, size, side))
            .find(|rect| {
                rect.origin.x >= build.viewport.origin.x
                    && rect.origin.y >= build.viewport.origin.y
                    && rect.max().x <= build.viewport.max().x
                    && rect.max().y <= build.viewport.max().y
                    && !rect.intersects(keep_out)
                    && taken.iter().all(|other| !rect.intersects(*other))
            })
            .ok_or_else(|| layout_error(ErrorCode::NoRoom))?
            .center()
    };
    let rect = Rect::centered(center, size);
    let color = callout.color.as_ref().map_or_else(
        || paint(MARK_RGB),
        |color| build.theme.resolve(color).into(),
    );
    let primitives = [
        Prim::Rect {
            rect,
            radius: MARK_RADIUS * unit,
            fill: Some(Paint::from(build.theme.surface_raised).faded(0.96)),
            border: Some((color, MARK_STROKE * unit * 0.75)),
        },
        Prim::Text {
            origin: rect.origin + pad,
            text: callout.text.clone(),
            style,
            color: build.theme.fg.into(),
            max_width: None,
            align: Align::Left,
        },
    ];
    let mut placed = Placed::new(
        Item {
            handle,
            parent: None,
            primitives: primitives.into(),
            bounds: rect,
            transform: Affine::default(),
            opacity: callout.opacity.map_or(1.0, crate::Finite::get),
        },
        Arc::clone(op),
        cached,
    );
    placed.anchors = [center, center];
    Ok(placed)
}

const SIDES: [Side; 8] = [
    Side::Top,
    Side::Bottom,
    Side::Right,
    Side::Left,
    Side::TopRight,
    Side::BottomRight,
    Side::TopLeft,
    Side::BottomLeft,
];

fn beside(keep_out: Rect, label: Vec2, side: Side) -> Rect {
    let (near, far, middle) = (keep_out.origin, keep_out.max(), keep_out.center());
    let origin = match side {
        Side::Top | Side::Center => Vec2::new(middle.x - label.x * 0.5, near.y - label.y),
        Side::Bottom => Vec2::new(middle.x - label.x * 0.5, far.y),
        Side::Right => Vec2::new(far.x, middle.y - label.y * 0.5),
        Side::Left => Vec2::new(near.x - label.x, middle.y - label.y * 0.5),
        Side::TopRight => Vec2::new(far.x, near.y - label.y),
        Side::BottomRight => far,
        Side::TopLeft => near - label,
        Side::BottomLeft => Vec2::new(near.x - label.x, far.y),
    };
    Rect {
        origin,
        size: label,
    }
}
