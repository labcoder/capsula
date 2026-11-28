//! The mesh build: one geometry's primitives, from one or more build input batches in order,
//! into vertex and index buffers reserved once for the format's global caps, tessellated with
//! main's lyon settings. A vertex carries its distance along its stroke's source points, so
//! reveal and dashes change per frame without a rebuild.

use std::{
    f64::consts::{FRAC_PI_2, PI},
    fmt,
    ops::Range,
};

use lyon::{
    math::{Point, point},
    path::{ControlPointId, EndpointId, IdEvent, PathEvent, PositionStore},
    tessellation::{
        FillGeometryBuilder, FillOptions, FillRule, FillTessellator, FillVertex, GeometryBuilder,
        GeometryBuilderError, StrokeGeometryBuilder, StrokeOptions, StrokeTessellator,
        StrokeVertex, TessellationResult, VertexId, VertexSource,
    },
};

use super::{BuildInput, Color, Cursor, Failure, GeometryId, Part, Record, Shape, index, push};
use crate::limits::{
    INPUT_OBJECTS, INPUT_PRIMS, MESH_INDICES, MESH_PIECES, MESH_VERTICES, MITER_LIMIT, STROKE_CAP,
    STROKE_JOIN, TOLERANCE,
};

/// One mesh vertex in primitive space.
#[derive(Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Position.
    pub at: [f32; 2],
    /// Distance along its stroke in primitive units, for dashes; 0 off a stroke.
    pub along: f32,
    /// Where it lies in its primitive's reveal, 0 to 1, times [`Vertex::weight`]; 0 for what
    /// is never revealed.
    pub reveal: f32,
    /// What the interpolated reveal is divided by per pixel: 1, except on a border band's inner
    /// edge, where it is that edge's length over the outer edge's, so the cut stays straight.
    pub weight: f32,
    /// Its piece, for paint.
    pub piece: u32,
}

/// Positions are drawing content: nothing.
impl fmt::Debug for Vertex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vertex")
    }
}

/// One painted part of a primitive: a rect has up to two, a fill or a line one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Piece {
    /// Its prepared paint.
    pub color: Color,
    /// Painted in dashes.
    pub dashed: bool,
    /// Drawn in by the record's reveal: strokes and borders.
    pub revealed: bool,
}

/// One geometry's mesh, built into buffers reserved once and reused for the next geometry.
pub struct Mesh {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    pieces: Vec<Piece>,
    /// The first index of each primitive, then the end.
    starts: Vec<u32>,
    /// The first primitive of each object.
    objects: Vec<u32>,
    /// The geometry being built and where its next batch starts; none once its last is in.
    building: Option<(GeometryId, Option<Cursor>)>,
    fill: FillTessellator,
    stroke: StrokeTessellator,
}

impl Mesh {
    /// Bytes its buffers reserve, boxed; lyon's own working memory is `TESSELLATOR_BYTES`.
    pub const BYTES: usize = size_of::<Self>()
        + MESH_VERTICES * size_of::<Vertex>()
        + MESH_INDICES * size_of::<u32>()
        + MESH_PIECES * size_of::<Piece>()
        + (INPUT_PRIMS + 1) * size_of::<u32>()
        + INPUT_OBJECTS * size_of::<u32>();

    /// Reserves every buffer at its cap.
    pub fn new() -> Self {
        Self {
            vertices: Vec::with_capacity(MESH_VERTICES),
            indices: Vec::with_capacity(MESH_INDICES),
            pieces: Vec::with_capacity(MESH_PIECES),
            starts: Vec::with_capacity(INPUT_PRIMS + 1),
            objects: Vec::with_capacity(INPUT_OBJECTS),
            building: None,
            fill: FillTessellator::new(),
            stroke: StrokeTessellator::new(),
        }
    }

    /// Starts building the geometry `id` in place of whatever this mesh held.
    pub fn begin(&mut self, id: GeometryId) {
        self.vertices.clear();
        self.indices.clear();
        self.pieces.clear();
        self.starts.clear();
        self.objects.clear();
        self.building = Some((id, Some(Cursor::START)));
    }

    /// Builds the part of the geometry being built that `input` holds. The batch isn't wiped
    /// here: that is its owner's, on every path.
    ///
    /// # Errors
    /// [`Failure::Order`] when `input` holds no part of that geometry or not the next one;
    /// [`Failure::Build`] when lyon refuses a primitive or the mesh would pass its reservation.
    /// The mesh is then as it was before this batch.
    pub fn extend(&mut self, input: &BuildInput) -> Result<(), Failure> {
        let Some((id, Some(next))) = self.building else {
            return Err(Failure::Order);
        };
        let part = input
            .part(id)
            .filter(|part| part.from == next)
            .ok_or(Failure::Order)?;
        let mark = [
            self.vertices.len(),
            self.indices.len(),
            self.pieces.len(),
            self.starts.len(),
            self.objects.len(),
        ];
        let built = self.part(input, part);
        if built.is_err() {
            self.vertices.truncate(mark[0]);
            self.indices.truncate(mark[1]);
            self.pieces.truncate(mark[2]);
            self.starts.truncate(mark[3]);
            self.objects.truncate(mark[4]);
            return built;
        }
        self.building = Some((id, part.to));
        Ok(())
    }

    /// Ends the build once its last batch is in.
    ///
    /// # Errors
    /// [`Failure::Order`] when no build is open or a batch is still to come; the mesh is then
    /// as it was.
    pub fn finish(&mut self) -> Result<(), Failure> {
        let Some((_, None)) = self.building else {
            return Err(Failure::Order);
        };
        push(
            &mut self.starts,
            INPUT_PRIMS + 1,
            index(self.indices.len())?,
        )?;
        self.building = None;
        Ok(())
    }

    /// Every vertex, in primitive space.
    pub fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }

    /// Triangle corners, three per triangle.
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// The pieces vertices name.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// The indices a record draws, if it names primitives of one object of this mesh.
    pub fn range(&self, record: &Record) -> Option<Range<u32>> {
        let object = usize::try_from(record.object).ok()?;
        let first = self.objects.get(object)?.checked_add(record.first)?;
        let end = first.checked_add(record.count)?;
        let next = match self.objects.get(object + 1) {
            Some(next) => *next,
            None => index(self.starts.len().checked_sub(1)?).ok()?,
        };
        if end > next {
            return None;
        }
        let start = *self.starts.get(usize::try_from(first).ok()?)?;
        Some(start..*self.starts.get(usize::try_from(end).ok()?)?)
    }

    fn part(&mut self, input: &BuildInput, part: Part<'_>) -> Result<(), Failure> {
        for (n, object) in part.objects.iter().enumerate() {
            if n > 0 || part.from.prim == 0 {
                push(&mut self.objects, INPUT_OBJECTS, index(self.starts.len())?)?;
            }
            for shape in input.shapes(object).ok_or(Failure::Build)? {
                push(&mut self.starts, INPUT_PRIMS, index(self.indices.len())?)?;
                self.shape(input, *shape)?;
            }
        }
        Ok(())
    }

    fn shape(&mut self, input: &BuildInput, shape: Shape) -> Result<(), Failure> {
        match shape {
            Shape::Rect {
                rect,
                radius,
                segments,
                fill,
                border,
            } => {
                let outer = Rounded {
                    rect,
                    radius,
                    segments,
                };
                if let Some(color) = fill {
                    self.fill(outer.points(), color)?;
                }
                if let Some((color, width)) = border {
                    let [x, y, w, h] = rect;
                    let width = width.min(w.min(h) / 2.0);
                    if width > 0.0 {
                        let inner = Rounded {
                            rect: [x + width, y + width, w - width * 2.0, h - width * 2.0],
                            radius: (radius - width).max(0.0),
                            segments,
                        };
                        self.ring(outer, inner, color)?;
                    }
                }
                Ok(())
            }
            Shape::Fill { points, color } => {
                let points = input.points(points).ok_or(Failure::Build)?;
                self.fill(points.iter().copied(), color)
            }
            Shape::Line {
                points,
                closed,
                color,
                width,
                dashed,
            } => {
                let points = input.points(points).ok_or(Failure::Build)?;
                if points.len() < 2 {
                    return Ok(());
                }
                let piece = self.piece(Piece {
                    color,
                    dashed,
                    revealed: true,
                })?;
                // shortcut: a reveal's moving end bends through a join's miter, not main's butt
                // cut (render.md); emit our own joins when that difference has to go.
                let close = closed && points.len() > 2;
                let line = Polyline {
                    points,
                    ends: points.len() + usize::from(closed && !close),
                    close,
                };
                let options = StrokeOptions::default()
                    .with_line_width(width)
                    .with_line_join(STROKE_JOIN)
                    .with_line_cap(STROKE_CAP)
                    .with_miter_limit(MITER_LIMIT)
                    .with_tolerance(TOLERANCE);
                let mut walk = Walk {
                    line,
                    merge: (TOLERANCE * TOLERANCE * 0.5)
                        .min(width * width * 0.05)
                        .max(1e-8),
                    total: 0.0,
                    arc: (0, 0.0),
                };
                walk.total = walk.arc(line.ends - usize::from(!close));
                let mut sink = Sink::new(&mut self.vertices, &mut self.indices, piece, Some(walk));
                let result = self.stroke.tessellate_with_ids(
                    line.events()?,
                    &line,
                    None,
                    &options,
                    &mut sink,
                );
                finished(&result, &sink)
            }
            Shape::Text { .. } => Ok(()),
        }
    }

    fn piece(&mut self, piece: Piece) -> Result<u32, Failure> {
        let id = index(self.pieces.len())?;
        push(&mut self.pieces, MESH_PIECES, piece)?;
        Ok(id)
    }

    /// A polygon filled nonzero, as main fills polygons and rect bodies.
    fn fill(
        &mut self,
        points: impl Iterator<Item = [f32; 2]> + Clone,
        color: Color,
    ) -> Result<(), Failure> {
        let piece = self.piece(Piece {
            color,
            dashed: false,
            revealed: false,
        })?;
        let options = FillOptions::default()
            .with_fill_rule(FillRule::NonZero)
            .with_tolerance(TOLERANCE);
        let mut sink = Sink::new(&mut self.vertices, &mut self.indices, piece, None);
        let result = self.fill.tessellate(events(points), &options, &mut sink);
        finished(&result, &sink)
    }

    /// The band between a rect's outline and its inset, a quad of its own per outline edge,
    /// revealed along the outer outline from the right end of the top edge as main's border
    /// sector is: its cut joins the same fraction of the outer and the inner edge. The inner
    /// edge is parallel to the outer one, so that fraction is a ratio of two quantities linear
    /// across the quad, carried as `reveal` and `weight`; a corner the inset collapses has an
    /// inner edge of length 0 and weight 0, and every cut there meets at the inner corner.
    fn ring(&mut self, outer: Rounded, inner: Rounded, color: Color) -> Result<(), Failure> {
        let piece = self.piece(Piece {
            color,
            dashed: false,
            revealed: true,
        })?;
        let points = outer.len();
        if self.vertices.len() + 4 * points > MESH_VERTICES
            || self.indices.len() + 6 * points > MESH_INDICES
        {
            return Err(Failure::Build);
        }
        let edge = |at: usize| length(outer.at(at), outer.at((at + 1) % points));
        let total: f64 = (0..points).map(edge).sum();
        let mut along = 0.0;
        for at in 0..points {
            let next = (at + 1) % points;
            let span = edge(at);
            let [a, b, c, d] = [outer.at(at), outer.at(next), inner.at(at), inner.at(next)];
            let outward = [b[0] - a[0], b[1] - a[1]];
            let square = outward[0] * outward[0] + outward[1] * outward[1];
            if span <= 0.0 || square <= 0.0 {
                continue;
            }
            let weight =
                ((outward[0] * (d[0] - c[0]) + outward[1] * (d[1] - c[1])) / square).max(0.0);
            let [start, end] = [along, along + span].map(|at| narrow(at / total));
            let base = index(self.vertices.len())?;
            for (corner, reveal, weight) in [
                (a, start, 1.0),
                (b, end, 1.0),
                (d, end * weight, weight),
                (c, start * weight, weight),
            ] {
                self.vertices.push(Vertex {
                    at: corner,
                    along: 0.0,
                    reveal,
                    weight,
                    piece,
                });
            }
            self.indices
                .extend([0, 1, 2, 0, 2, 3].map(|corner| base + corner));
            along += span;
        }
        Ok(())
    }

    /// The bytes its buffers really reserve.
    #[cfg(test)]
    pub(crate) fn reserved(&self) -> usize {
        size_of::<Self>()
            + self.vertices.capacity() * size_of::<Vertex>()
            + self.indices.capacity() * size_of::<u32>()
            + self.pieces.capacity() * size_of::<Piece>()
            + self.starts.capacity() * size_of::<u32>()
            + self.objects.capacity() * size_of::<u32>()
    }
}

impl Default for Mesh {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts only: the vertices are the drawing's geometry.
impl fmt::Debug for Mesh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mesh")
            .field("vertices", &self.vertices.len())
            .field("indices", &self.indices.len())
            .field("pieces", &self.pieces.len())
            .field("objects", &self.objects.len())
            .finish_non_exhaustive()
    }
}

fn finished(result: &TessellationResult, sink: &Sink<'_>) -> Result<(), Failure> {
    if result.is_err() || sink.full {
        return Err(Failure::Build);
    }
    Ok(())
}

/// A stroke's source points as lyon endpoints. A closed line of two points goes out and back as
/// two open lines, so its return leg keeps its own distance and full width: endpoint 2 is the
/// first point again.
#[derive(Clone, Copy)]
struct Polyline<'a> {
    points: &'a [[f32; 2]],
    /// Endpoints, the return one included.
    ends: usize,
    /// Whether lyon closes it, joining the last point to the first.
    close: bool,
}

impl Polyline<'_> {
    fn at(self, id: usize) -> [f32; 2] {
        self.points[id % self.points.len()]
    }

    fn events(self) -> Result<impl Iterator<Item = IdEvent>, Failure> {
        let ends = index(self.ends)?;
        let back = self.ends > self.points.len();
        let run = |first: u32, last: u32, close: bool| {
            std::iter::once(IdEvent::Begin {
                at: EndpointId(first),
            })
            .chain((first + 1..=last).map(|to| IdEvent::Line {
                from: EndpointId(to - 1),
                to: EndpointId(to),
            }))
            .chain(std::iter::once(IdEvent::End {
                last: EndpointId(last),
                first: EndpointId(first),
                close,
            }))
        };
        let out = run(0, if back { 1 } else { ends - 1 }, self.close);
        Ok(out.chain(back.then(|| run(1, 2, false)).into_iter().flatten()))
    }
}

impl PositionStore for Polyline<'_> {
    fn get_endpoint(&self, id: EndpointId) -> Point {
        let [x, y] = self.at(id.to_usize());
        point(x, y)
    }

    fn get_control_point(&self, _: ControlPointId) -> Point {
        self.get_endpoint(EndpointId(0))
    }
}

/// A stroke's distances along its source points, for the vertices lyon emits at them.
struct Walk<'a> {
    line: Polyline<'a>,
    /// lyon's squared merge distance: a point this close to the last one kept is dropped.
    merge: f32,
    /// The source length, closing edge included.
    total: f64,
    /// The last distance asked for, `(endpoint, distance)`, so a walk in order sums each edge once.
    arc: (usize, f64),
}

impl Walk<'_> {
    /// The distance of a vertex at endpoint `id`. A run of points lyon merged into `id` takes
    /// the run's last distance, as main's walk reaches it, and lyon's closing join at the first
    /// point is past the whole length, which its own advancement tells apart from the start.
    fn along(&mut self, id: usize, advancement: f32) -> f64 {
        if self.line.close && id == 0 && advancement > 0.0 {
            return self.total;
        }
        let kept = self.line.at(id);
        let mut last = id;
        while last + 1 < self.line.ends {
            let [x, y] = self.line.at(last + 1);
            let [dx, dy] = [kept[0] - x, kept[1] - y];
            if dx * dx + dy * dy >= self.merge {
                break;
            }
            last += 1;
        }
        self.arc(last)
    }

    fn arc(&mut self, to: usize) -> f64 {
        if to < self.arc.0 {
            self.arc = (0, 0.0);
        }
        while self.arc.0 < to {
            let from = self.arc.0;
            self.arc = (
                from + 1,
                self.arc.1 + length(self.line.at(from), self.line.at(from + 1)),
            );
        }
        self.arc.1
    }
}

/// A rounded rectangle's outline, clockwise from the right end of the top edge: four corners
/// of `segments + 1` points each, as main's painter samples them.
#[derive(Clone, Copy)]
struct Rounded {
    rect: [f32; 4],
    radius: f32,
    segments: u32,
}

impl Rounded {
    fn per_corner(self) -> usize {
        usize::try_from(self.segments).map_or(usize::MAX, |segments| segments.saturating_add(1))
    }

    fn len(self) -> usize {
        self.per_corner().saturating_mul(4)
    }

    fn at(self, at: usize) -> [f32; 2] {
        let [x, y, width, height] = self.rect;
        let radius = self.radius;
        let near = [x + radius, y + radius];
        let far = [x + width - radius, y + height - radius];
        let per = self.per_corner();
        let (center, start) = [
            ([far[0], near[1]], -FRAC_PI_2),
            (far, 0.0),
            ([near[0], far[1]], FRAC_PI_2),
            (near, PI),
        ][at / per];
        let step = FRAC_PI_2 / f64::from(self.segments.max(1));
        let angle = start + step * f64::from(u32::try_from(at % per).unwrap_or(u32::MAX));
        [
            center[0] + narrow(angle.cos()) * radius,
            center[1] + narrow(angle.sin()) * radius,
        ]
    }

    fn points(self) -> impl Iterator<Item = [f32; 2]> + Clone {
        (0..self.len()).map(move |at| self.at(at))
    }
}

/// Writes lyon's output straight into the mesh's reserved buffers, refusing what wouldn't fit.
struct Sink<'a> {
    vertices: &'a mut Vec<Vertex>,
    indices: &'a mut Vec<u32>,
    piece: u32,
    /// A stroke's distances; none for a fill.
    walk: Option<Walk<'a>>,
    mark: [usize; 2],
    full: bool,
}

impl<'a> Sink<'a> {
    fn new(
        vertices: &'a mut Vec<Vertex>,
        indices: &'a mut Vec<u32>,
        piece: u32,
        walk: Option<Walk<'a>>,
    ) -> Self {
        let mark = [vertices.len(), indices.len()];
        Self {
            vertices,
            indices,
            piece,
            walk,
            mark,
            full: false,
        }
    }

    fn vertex(&mut self, at: [f32; 2], along: f64) -> Result<VertexId, GeometryBuilderError> {
        if self.vertices.len() >= MESH_VERTICES {
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let id = u32::try_from(self.vertices.len())
            .map_err(|_| GeometryBuilderError::TooManyVertices)?;
        let reveal = match &self.walk {
            Some(walk) if walk.total > 0.0 => narrow(along / walk.total),
            _ => 0.0,
        };
        self.vertices.push(Vertex {
            at,
            along: narrow(along),
            reveal,
            weight: 1.0,
            piece: self.piece,
        });
        Ok(VertexId(id))
    }
}

impl GeometryBuilder for Sink<'_> {
    fn begin_geometry(&mut self) {
        self.mark = [self.vertices.len(), self.indices.len()];
    }

    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        if self.indices.len() + 3 > MESH_INDICES {
            self.full = true;
            return;
        }
        self.indices.extend([a.0, b.0, c.0]);
    }

    fn abort_geometry(&mut self) {
        self.vertices.truncate(self.mark[0]);
        self.indices.truncate(self.mark[1]);
    }
}

impl FillGeometryBuilder for Sink<'_> {
    fn add_fill_vertex(
        &mut self,
        vertex: FillVertex<'_>,
    ) -> Result<VertexId, GeometryBuilderError> {
        self.vertex(vertex.position().to_array(), 0.0)
    }
}

impl StrokeGeometryBuilder for Sink<'_> {
    fn add_stroke_vertex(
        &mut self,
        vertex: StrokeVertex<'_, '_>,
    ) -> Result<VertexId, GeometryBuilderError> {
        let id = match vertex.source() {
            VertexSource::Endpoint { id } | VertexSource::Edge { from: id, .. } => id,
        };
        let along = match &mut self.walk {
            Some(walk) => walk.along(id.to_usize(), vertex.advancement()),
            None => 0.0,
        };
        self.vertex(vertex.position().to_array(), along)
    }
}

/// A closed polygon as lyon path events without allocating.
fn events(points: impl Iterator<Item = [f32; 2]> + Clone) -> impl Iterator<Item = PathEvent> {
    let points = points.map(|[x, y]| point(x, y));
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

fn length(a: [f32; 2], b: [f32; 2]) -> f64 {
    (f64::from(b[0]) - f64::from(a[0])).hypot(f64::from(b[1]) - f64::from(a[1]))
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "primitive coordinates and their fractions are f32"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}
