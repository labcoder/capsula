use gpui_kit::{Path, Pixels, Point};
use kumochi_scene::{
    display::{BUILD_VERTEX_LIMIT, Contour, DASH},
    geom::{Affine, Vec2},
};
use lyon::tessellation::{
    GeometryBuilder, GeometryBuilderError, StrokeGeometryBuilder, StrokeOptions, StrokeTessellator,
    StrokeVertex, VertexId,
};

use super::paint_paths::{BuildError, bound, length, narrow, pixel, vertex};

pub(super) const RUN_POINTS: usize = (BUILD_VERTEX_LIMIT - 4) / 4;
pub(super) const VERTICES: usize = 65_536;

pub(super) struct Scratch {
    verts: Vec<lyon::math::Point>,
    run: Vec<Point<Pixels>>,
    pub(super) fonts: super::paint_cache::Fonts,
}

impl Scratch {
    pub(super) fn new() -> Self {
        Self {
            verts: Vec::with_capacity(VERTICES),
            run: Vec::with_capacity(RUN_POINTS),
            fonts: super::paint_cache::Fonts::new(),
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.verts.capacity() * size_of::<lyon::math::Point>()
            + self.run.capacity() * size_of::<Point<Pixels>>()
            + super::paint_cache::FONT_FEATURE_BYTES
    }

    pub(super) fn stroke(
        &mut self,
        points: &[Vec2],
        contour: Contour,
        draw: f32,
        flow: Option<f32>,
        width: f32,
        transform: Affine,
    ) -> Result<Option<Path<Pixels>>, BuildError> {
        let pass = |scratch: &mut Self, path: Option<&mut Path<Pixels>>| {
            let mut walk = Runs::new(points, contour, draw, flow);
            let mut next = walk.next(&mut scratch.run)?;
            let mut output = Output {
                verts: &mut scratch.verts,
                path,
                transform,
                count: 0,
                mark: 0,
                overflow: false,
            };
            let options = StrokeOptions::default().with_line_width(width);
            let mut tessellator = StrokeTessellator::new();
            while next.is_some() {
                let mut builder = tessellator.builder(&options, &mut output);
                let mut charged = 0;
                while let Some(contour) = next {
                    let cost = scratch.run.len() * 4 + 4;
                    if charged + cost > BUILD_VERTEX_LIMIT {
                        break;
                    }
                    let [first, rest @ ..] = scratch.run.as_slice() else {
                        unreachable!("the walk returns only nonempty runs");
                    };
                    let close = matches!(contour, Contour::Closed);
                    let rest = match rest.split_last() {
                        Some((_, open)) if close => open,
                        _ => rest,
                    };
                    builder.begin((*first).into());
                    for to in rest {
                        builder.line_to((*to).into());
                    }
                    builder.end(close);
                    charged += cost;
                    next = walk.next(&mut scratch.run)?;
                }
                builder.build().map_err(|_| BuildError::Stroke)?;
            }
            if output.overflow {
                return Err(BuildError::Stroke);
            }
            Ok(output.count)
        };
        let count = pass(self, None)?;
        if count == 0 {
            return Ok(None);
        }
        let mut path = Path::new(Point::default());
        path.vertices = Vec::with_capacity(count);
        if pass(self, Some(&mut path))? != count || path.vertices.len() != count {
            return Err(BuildError::Stroke);
        }
        bound(&mut path);
        super::paint_phase::handoff(&path);
        Ok(Some(path))
    }
}

struct Output<'a> {
    verts: &'a mut Vec<lyon::math::Point>,
    path: Option<&'a mut Path<Pixels>>,
    transform: Affine,
    count: usize,
    mark: usize,
    overflow: bool,
}

impl GeometryBuilder for Output<'_> {
    fn begin_geometry(&mut self) {
        self.verts.clear();
        self.mark = self.count;
    }

    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        self.count += 3;
        if let Some(path) = &mut self.path {
            if path.vertices.capacity() - path.vertices.len() < 3 {
                self.overflow = true;
                return;
            }
            for id in [a, b, c] {
                let at = self.verts[id.to_usize()];
                path.vertices
                    .push(vertex(Vec2::new(at.x, at.y), self.transform));
            }
        }
    }

    fn abort_geometry(&mut self) {
        self.verts.clear();
        self.count = self.mark;
        if let Some(path) = &mut self.path {
            path.vertices.truncate(self.mark);
        }
    }
}

impl StrokeGeometryBuilder for Output<'_> {
    fn add_stroke_vertex(
        &mut self,
        vertex: StrokeVertex<'_, '_>,
    ) -> Result<VertexId, GeometryBuilderError> {
        self.position(vertex.position())
    }
}

impl Output<'_> {
    fn position(&mut self, position: lyon::math::Point) -> Result<VertexId, GeometryBuilderError> {
        if self.verts.len() == VERTICES {
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let id = u32::try_from(self.verts.len()).expect("a u16-bounded vertex id");
        self.verts.push(position);
        Ok(VertexId(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_u16_vertex_boundary_refuses_before_growing_scratch() {
        let mut vertices = Vec::with_capacity(VERTICES);
        let mut output = Output {
            verts: &mut vertices,
            path: None,
            transform: Affine::default(),
            count: 0,
            mark: 0,
            overflow: false,
        };
        let ((), counts) = crate::allocation::activity(|| {
            for id in 0..65_536_u32 {
                assert_eq!(
                    output.position(lyon::math::point(0.0, 0.0)),
                    Ok(VertexId(id))
                );
            }
            assert_eq!(
                output.position(lyon::math::point(0.0, 0.0)),
                Err(GeometryBuilderError::TooManyVertices)
            );
        });
        assert_eq!(counts, [0, 0]);
        assert_eq!(vertices.len(), 65_536);
        assert_eq!(vertices.capacity(), 65_536);
    }
}

struct Runs<'a> {
    points: &'a [Vec2],
    closed: bool,
    final_contour: Contour,
    reveal: f64,
    on: f64,
    period: f64,
    phase: f64,
    segment: usize,
    start: f64,
    span: f64,
    end: f64,
    dash: f64,
    from: f64,
    active: bool,
    open: bool,
    continues: bool,
    pending: Option<[Point<Pixels>; 2]>,
}

impl<'a> Runs<'a> {
    fn new(points: &'a [Vec2], contour: Contour, draw: f32, flow: Option<f32>) -> Self {
        let closed = matches!(contour, Contour::Closed);
        let ends = points.iter().chain(points.first().filter(|_| closed));
        let total: f64 = ends
            .clone()
            .zip(ends.skip(1))
            .map(|(a, b)| length(*a, *b))
            .sum();
        let (on, period, phase) = flow.map_or((total + 1.0, total + 1.0, 0.0), |flow| {
            let period = f64::from(DASH[0] + DASH[1]);
            let phase = (1.0 - f64::from(flow).rem_euclid(1.0)) * period;
            (f64::from(DASH[0]), period, phase)
        });
        Self {
            points,
            closed,
            final_contour: if closed && flow.is_none() && draw >= 1.0 {
                Contour::Closed
            } else {
                Contour::Open
            },
            reveal: total * f64::from(draw.clamp(0.0, 1.0)),
            on,
            period,
            phase,
            segment: 0,
            start: 0.0,
            span: 0.0,
            end: 0.0,
            dash: 0.0,
            from: 0.0,
            active: false,
            open: false,
            continues: false,
            pending: None,
        }
    }

    fn next(&mut self, run: &mut Vec<Point<Pixels>>) -> Result<Option<Contour>, BuildError> {
        run.clear();
        if let Some(points) = self.pending.take() {
            run.extend(points);
        }
        let segments = self.points.len().saturating_sub(usize::from(!self.closed));
        while self.segment < segments {
            let a = self.points[self.segment];
            let b = self.points[(self.segment + 1) % self.points.len()];
            if !self.active {
                if self.start >= self.reveal {
                    break;
                }
                self.span = length(a, b);
                if self.span.is_nan() || self.span <= 0.0 {
                    self.segment += 1;
                    continue;
                }
                self.end = (self.start + self.span).min(self.reveal);
                self.continues = self.open;
                self.open = false;
                self.dash = ((self.start + self.phase) / self.period).floor();
                self.from = self.dash * self.period - self.phase;
                self.active = true;
            }
            while self.from < self.end {
                let from = self.from;
                let to = (from + self.on).min(self.end);
                let from_here = from.max(self.start);
                self.dash += 1.0;
                self.from = self.dash * self.period - self.phase;
                if to <= from_here {
                    continue;
                }
                let along = |offset: f64| {
                    pixel(if offset >= self.start + self.span {
                        b
                    } else {
                        a + (b - a) * narrow((offset - self.start) / self.span)
                    })
                };
                self.open = to >= self.start + self.span;
                if !self.continues || from > self.start {
                    let points = [along(from_here), along(to)];
                    if !run.is_empty() {
                        self.pending = Some(points);
                        return Ok(Some(Contour::Open));
                    }
                    run.extend(points);
                } else {
                    if run.len() == RUN_POINTS {
                        return Err(BuildError::OverCharge);
                    }
                    run.push(along(to));
                }
            }
            self.start += self.span;
            self.segment += 1;
            self.active = false;
        }
        Ok((!run.is_empty()).then_some(self.final_contour))
    }
}
