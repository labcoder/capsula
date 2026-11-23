use super::{Prim, Rect, Rgb, Vec2};

pub(super) fn visible_fill(primitive: &Prim) -> Option<(Rgb, Rect)> {
    match primitive {
        Prim::Rect {
            rect,
            fill: Some(paint),
            ..
        } if paint.opacity > 0.0 => Some((paint.rgb, *rect)),
        Prim::Fill { points, color } if color.opacity > 0.0 => {
            let mut points = points.iter().copied();
            let point = |p| Rect::centered(p, Vec2::default());
            let start = point(points.next()?);
            Some((
                color.rgb,
                points.fold(start, |rect, p| rect.union(point(p))),
            ))
        }
        _ => None,
    }
}
