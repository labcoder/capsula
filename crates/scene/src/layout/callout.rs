use super::{
    PrepareError,
    primitives::{Build, anchor, layout_error},
    state::{LayoutState, Placed},
};
use crate::{
    ErrorCode, Handle, Op,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    record::{Align, Callout, Side},
};
use std::sync::Arc;

#[expect(
    clippy::too_many_lines,
    reason = "keep callout fitting and its leader geometry in the same placement operation"
)]
pub(super) fn build(
    build: &mut Build<'_>,
    op: &Arc<Op>,
    callout: &Callout,
    handle: Handle,
    parent: Option<Handle>,
    state: &LayoutState,
    cached: Option<&Placed>,
) -> Result<Placed, PrepareError> {
    let style = TextStyle {
        size: build.theme.font_size,
        bold: false,
    };
    let width = callout.max_width.map_or(220.0, crate::Finite::get);
    let text_size = build.measure(&callout.text, style, Some(width))?;
    let pad = Vec2::new(12.0, 9.0);
    let size = text_size + pad * 2.0;
    if size.x > build.viewport.size.x || size.y > build.viewport.size.y {
        return Err(layout_error(ErrorCode::OutOfRange));
    }
    let (target, target_rect) = if let Some(previous) = cached {
        (
            previous.anchors[0],
            Rect {
                origin: previous.anchors[0],
                size: Vec2::default(),
            },
        )
    } else {
        anchor(state, &callout.target, parent)?
    };
    let mut center = if let Some(previous) = cached {
        previous.anchors[1]
    } else if let Some(at) = &callout.at {
        anchor(state, at, parent)?.0
    } else {
        let at_side = |placement| side_center(target_rect, size, placement);
        match callout.side.unwrap_or_default() {
            Side::Center => [Side::Right, Side::Top, Side::Left, Side::Bottom]
                .into_iter()
                .map(at_side)
                .find(|center| {
                    let bounds = Rect::centered(*center, size);
                    bounds.origin.x >= build.viewport.origin.x
                        && bounds.origin.y >= build.viewport.origin.y
                        && bounds.max().x <= build.viewport.max().x
                        && bounds.max().y <= build.viewport.max().y
                })
                .unwrap_or_else(|| at_side(Side::Right)),
            placement => at_side(placement),
        }
    };
    let half = size * 0.5;
    center.x = center
        .x
        .min(build.viewport.max().x - half.x)
        .max(build.viewport.origin.x + half.x);
    center.y = center
        .y
        .min(build.viewport.max().y - half.y)
        .max(build.viewport.origin.y + half.y);
    let rect = Rect::centered(center, size);
    let tip = if target_rect.size.x > 1.0 && target_rect.size.y > 1.0 {
        target_rect.edge_toward(center)
    } else {
        target
    };
    let from = rect.edge_toward(tip);
    let color = build.color(callout.color.as_ref());
    let mut primitives = vec![
        Prim::Line {
            points: Arc::from([from, tip]),
            closed: false,
            color,
            width: 2.0,
            dashed: false,
        },
        Prim::Rect {
            rect: Rect::centered(tip, Vec2::new(8.0, 8.0)),
            radius: 4.0,
            fill: Some(color),
            border: None,
        },
        Prim::Rect {
            rect,
            radius: 8.0,
            fill: Some(Paint::from(build.theme.surface_raised).faded(0.96)),
            border: Some((color, 1.5)),
        },
    ];
    primitives.push(Prim::Text {
        origin: rect.origin + pad,
        text: callout.text.clone(),
        style,
        color: build.theme.fg.into(),
        max_width: Some(width),
        align: Align::Left,
    });
    let bounds = rect.union(Rect::centered(tip, Vec2::new(8.0, 8.0)));
    if !bounds.within_canvas() {
        return Err(layout_error(ErrorCode::OutOfRange));
    }
    let mut placed = Placed::new(
        Item {
            handle,
            parent,
            primitives: primitives.into(),
            bounds,
            transform: Affine::default(),
            opacity: callout.opacity.map_or(1.0, crate::Finite::get),
        },
        Arc::clone(op),
        cached,
    );
    placed.anchors = [tip, center];
    Ok(placed)
}

fn side_center(target: Rect, size: Vec2, placement: Side) -> Vec2 {
    let center = target.center();
    let half = (target.size + size) * 0.5 + Vec2::new(40.0, 40.0);
    match placement {
        Side::Right | Side::Center => center + Vec2::new(half.x, -20.0),
        Side::Left => center + Vec2::new(-half.x, -20.0),
        Side::Top => center + Vec2::new(20.0, -half.y),
        Side::Bottom => center + Vec2::new(20.0, half.y),
        Side::TopLeft => center + Vec2::new(-half.x, -half.y),
        Side::TopRight => center + Vec2::new(half.x, -half.y),
        Side::BottomLeft => center + Vec2::new(-half.x, half.y),
        Side::BottomRight => center + Vec2::new(half.x, half.y),
    }
}
