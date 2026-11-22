use super::{Prim, Rect, Rgb, Vec2};

pub(super) fn rectangle_outline(primitives: &[Prim], rect: Rect, rgb: Rgb) -> ([f32; 4], f32) {
    let middle = rect.center();
    let far = rect.max();
    let same_point = |a: Vec2, b: Vec2| {
        let delta = a - b;
        delta.x.abs() < 0.001 && delta.y.abs() < 0.001
    };
    let widths = [
        Vec2::new(middle.x, rect.origin.y),
        Vec2::new(middle.x, far.y),
        Vec2::new(rect.origin.x, middle.y),
        Vec2::new(far.x, middle.y),
        rect.origin,
    ]
    .map(|probe| {
        primitives
            .iter()
            .filter_map(|primitive| match primitive {
                Prim::Rect {
                    rect: outline,
                    radius,
                    border: Some((paint, width)),
                    ..
                } if same_point(outline.origin, rect.origin)
                    && same_point(outline.max(), far)
                    && paint.rgb == rgb
                    && paint.opacity > 0.0
                    && (probe != rect.origin || *radius == 0.0) =>
                {
                    Some(*width)
                }
                Prim::Line {
                    points,
                    color,
                    width,
                    closed,
                    dashed: false,
                } if color.rgb == rgb
                    && color.opacity > 0.0
                    && points
                        .iter()
                        .copied()
                        .zip(points.iter().copied().cycle().skip(1))
                        .take(points.len().saturating_sub(usize::from(!closed)))
                        .any(|(a, b)| {
                            let d = b - a;
                            let q = probe - a;
                            let length_sq = d.x * d.x + d.y * d.y;
                            if length_sq == 0.0 {
                                return false;
                            }
                            let t = ((q.x * d.x + q.y * d.y) / length_sq).clamp(0.0, 1.0);
                            let offset = q - d * t;
                            offset.x.hypot(offset.y) <= *width * 0.5 + 0.001
                        }) =>
                {
                    Some(*width)
                }
                _ => None,
            })
            .fold(0.0_f32, f32::max)
    });
    let [top, bottom, left, right, corner] = widths;
    ([top, bottom, left, right], corner)
}
