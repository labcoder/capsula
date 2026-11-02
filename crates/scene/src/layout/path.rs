//! SVG flattening using exactly the admission subdivision rule.
use crate::{
    ErrorCode,
    geom::Vec2,
    limits::MAX_COMPOSED_SCALE,
    path::{self, Segment},
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Polyline {
    pub points: Arc<[Vec2]>,
    pub closed: bool,
}
#[derive(Clone, Copy)]
enum End {
    Open,
    Closed,
}

pub(super) fn flatten(source: &str, scale: f32) -> Result<Vec<Polyline>, ErrorCode> {
    if !scale.is_normal() || scale <= 0.0 || scale > MAX_COMPOSED_SCALE {
        return Err(ErrorCode::TransformOverflow);
    }
    path::walk(source, |_, _| Ok(()))?.segments(scale)?;
    let mut output = Vec::new();
    let mut points = Vec::new();
    path::walk(source, |original, segment| {
        if matches!(segment, Segment::Move(_) | Segment::Close) {
            finish(
                &mut output,
                &mut points,
                if segment == Segment::Close {
                    End::Closed
                } else {
                    End::Open
                },
            );
        }
        if let Segment::Move(point) = segment {
            points.push(Vec2::new(point[0], point[1]));
            return Ok(());
        }
        if segment == Segment::Close {
            return Ok(());
        }
        if points.is_empty() {
            points.push(Vec2::new(original[0], original[1]));
        }
        match segment {
            Segment::Line(point) => points.push(Vec2::new(point[0], point[1])),
            Segment::Quad([control, end]) => {
                let curvature = path::second_difference(original, control, end) * 0.25;
                let pieces = path::subdivisions(curvature, scale)?;
                for index in 1..=pieces {
                    let t = f32::from(index) / f32::from(pieces);
                    let a = lerp(original, control, t);
                    let b = lerp(control, end, t);
                    points.push(lerp([a.x, a.y], [b.x, b.y], t));
                }
            }
            Segment::Cubic([first, second, end]) => {
                let curvature = path::second_difference(original, first, second)
                    .max(path::second_difference(first, second, end))
                    * 0.75;
                let pieces = path::subdivisions(curvature, scale)?;
                for index in 1..=pieces {
                    let t = f32::from(index) / f32::from(pieces);
                    let first_mix = lerp(original, first, t);
                    let middle_mix = lerp(first, second, t);
                    let last_mix = lerp(second, end, t);
                    let upper = lerp([first_mix.x, first_mix.y], [middle_mix.x, middle_mix.y], t);
                    let lower = lerp([middle_mix.x, middle_mix.y], [last_mix.x, last_mix.y], t);
                    points.push(lerp([upper.x, upper.y], [lower.x, lower.y], t));
                }
            }
            Segment::Move(_) | Segment::Close => {
                unreachable!("move and close are handled before drawing")
            }
        }
        Ok(())
    })?;
    finish(&mut output, &mut points, End::Open);
    Ok(output)
}

fn lerp(a: [f32; 2], b: [f32; 2], t: f32) -> Vec2 {
    Vec2::new(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)
}
fn finish(output: &mut Vec<Polyline>, points: &mut Vec<Vec2>, end: End) {
    let current = std::mem::take(points);
    if current.len() >= 2 {
        output.push(Polyline {
            points: current.into(),
            closed: matches!(end, End::Closed),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn count_matches_preflight_and_subpaths_never_connect() {
        let source = "M0 0 Q0 4 4 4 Z M10 10 C10 14 14 14 14 10";
        let paths = flatten(source, 4.0).expect("bounded paths");
        assert_eq!(paths.len(), 2);
        assert!(paths[0].closed, "first subpath closes");
        assert!(!paths[1].closed, "second subpath stays open");
        assert_eq!(paths[0].points.len(), 9);
        assert_eq!(paths[1].points.len(), 17);
        assert_eq!(paths[0].points.last(), Some(&Vec2::new(4.0, 4.0)));
        assert_eq!(paths[1].points.first(), Some(&Vec2::new(10.0, 10.0)));
        let actual: usize = paths
            .iter()
            .map(|p| p.points.len() - 1 + usize::from(p.closed))
            .sum();
        assert_eq!(actual, 25);
        assert_eq!(
            path::walk(source, |_, _| Ok(()))
                .expect("same subset")
                .segments(4.0)
                .expect("same scale"),
            actual
        );
        assert!(
            flatten("M0 0 M1 1", 1.0).expect("moves only").is_empty(),
            "moves do not draw"
        );
    }
}
