//! The mesh build: one geometry's primitives into vertex and index buffers reserved once for
//! the format's global caps, tessellated with main's lyon settings. A vertex carries its
//! distance along its stroke, so reveal and dashes change per frame without a rebuild.

use std::{
    f64::consts::{FRAC_PI_2, PI},
    fmt,
    ops::Range,
};

use lyon::{
    math::point,
    path::PathEvent,
    tessellation::{
        FillGeometryBuilder, FillOptions, FillRule, FillTessellator, FillVertex, GeometryBuilder,
        GeometryBuilderError, StrokeGeometryBuilder, StrokeOptions, StrokeTessellator,
        StrokeVertex, TessellationResult, VertexId,
    },
};

use super::{BuildInput, Color, Failure, GeometryId, Record, Shape, index, push};
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
    /// Where it lies in its primitive's reveal, 0 to 1; 0 for what is never revealed.
    pub reveal: f32,
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
            fill: FillTessellator::new(),
            stroke: StrokeTessellator::new(),
        }
    }

    /// Builds the geometry `id` from `input` in place of whatever this mesh held.
    ///
    /// # Errors
    /// [`Failure::Stale`] when `input` doesn't hold `id`; [`Failure::Build`] when lyon refuses
    /// a primitive or the mesh would pass its reservation. The mesh is then empty.
    pub fn build(&mut self, input: &BuildInput, id: GeometryId) -> Result<(), Failure> {
        self.clear();
        let built = self.build_all(input, id);
        if built.is_err() {
            self.clear();
        }
        built
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

    /// The indices a record draws, if it names primitives of this mesh.
    pub fn range(&self, record: &Record) -> Option<Range<u32>> {
        let object = *self.objects.get(usize::try_from(record.object).ok()?)?;
        let first = object.checked_add(record.first)?;
        let end = first.checked_add(record.count)?;
        let start = *self.starts.get(usize::try_from(first).ok()?)?;
        Some(start..*self.starts.get(usize::try_from(end).ok()?)?)
    }

    fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.pieces.clear();
        self.starts.clear();
        self.objects.clear();
    }

    fn build_all(&mut self, input: &BuildInput, id: GeometryId) -> Result<(), Failure> {
        for object in input.objects(id).ok_or(Failure::Stale)? {
            push(&mut self.objects, INPUT_OBJECTS, index(self.starts.len())?)?;
            for shape in input.shapes(object).ok_or(Failure::Build)? {
                push(
                    &mut self.starts,
                    INPUT_PRIMS + 1,
                    index(self.indices.len())?,
                )?;
                self.shape(input, *shape)?;
            }
        }
        push(
            &mut self.starts,
            INPUT_PRIMS + 1,
            index(self.indices.len())?,
        )
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
                let ends = points.iter().chain(points.first().filter(|_| closed));
                let length = ends
                    .clone()
                    .zip(ends.skip(1))
                    .map(|(a, b)| length(*a, *b))
                    .sum();
                let options = StrokeOptions::default()
                    .with_line_width(width)
                    .with_line_join(STROKE_JOIN)
                    .with_line_cap(STROKE_CAP)
                    .with_miter_limit(MITER_LIMIT)
                    .with_tolerance(TOLERANCE);
                let mut sink = Sink::new(&mut self.vertices, &mut self.indices, piece, length);
                let result = self.stroke.tessellate(
                    events(
                        points.iter().copied(),
                        if closed {
                            Contour::Closed
                        } else {
                            Contour::Open
                        },
                    ),
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
        let mut sink = Sink::new(&mut self.vertices, &mut self.indices, piece, 0.0);
        let result = self
            .fill
            .tessellate(events(points, Contour::Closed), &options, &mut sink);
        finished(&result, &sink)
    }

    /// The band between a rect's outline and its inset, a quad per outline edge, revealed
    /// along the outer outline from the right end of the top edge, as main's border sector is.
    fn ring(&mut self, outer: Rounded, inner: Rounded, color: Color) -> Result<(), Failure> {
        let piece = self.piece(Piece {
            color,
            dashed: false,
            revealed: true,
        })?;
        let points = outer.len();
        let base = self.vertices.len();
        if base + 2 * (points + 1) > MESH_VERTICES || self.indices.len() + 6 * points > MESH_INDICES
        {
            return Err(Failure::Build);
        }
        let edge = |at: usize| length(outer.at(at % points), outer.at((at + 1) % points));
        let total: f64 = (0..points).map(edge).sum();
        let mut along = 0.0;
        for at in 0..=points {
            let reveal = if total > 0.0 {
                narrow(along / total)
            } else {
                0.0
            };
            for corner in [outer.at(at % points), inner.at(at % points)] {
                self.vertices.push(Vertex {
                    at: corner,
                    along: 0.0,
                    reveal,
                    piece,
                });
            }
            along += edge(at);
        }
        for at in 0..points {
            let [o, i, next_o, next_i] = [0, 1, 2, 3].map(|offset| base + 2 * at + offset);
            for corner in [o, next_o, next_i, o, next_i, i] {
                self.indices.push(index(corner)?);
            }
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

/// Whether a polyline's last point joins its first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Contour {
    Open,
    Closed,
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
    /// The stroke's whole length, which reveal divides; 0 for a fill.
    length: f64,
    mark: [usize; 2],
    full: bool,
}

impl<'a> Sink<'a> {
    fn new(
        vertices: &'a mut Vec<Vertex>,
        indices: &'a mut Vec<u32>,
        piece: u32,
        length: f64,
    ) -> Self {
        let mark = [vertices.len(), indices.len()];
        Self {
            vertices,
            indices,
            piece,
            length,
            mark,
            full: false,
        }
    }

    fn vertex(&mut self, at: [f32; 2], along: f32) -> Result<VertexId, GeometryBuilderError> {
        if self.vertices.len() >= MESH_VERTICES {
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let id = u32::try_from(self.vertices.len())
            .map_err(|_| GeometryBuilderError::TooManyVertices)?;
        let reveal = if self.length > 0.0 {
            narrow(f64::from(along) / self.length)
        } else {
            0.0
        };
        self.vertices.push(Vertex {
            at,
            along,
            reveal,
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
        self.vertex(vertex.position().to_array(), vertex.advancement())
    }
}

/// A polyline as lyon path events without allocating, closed or open.
fn events(
    points: impl Iterator<Item = [f32; 2]> + Clone,
    contour: Contour,
) -> impl Iterator<Item = PathEvent> {
    let close = contour == Contour::Closed;
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
        .chain(
            first
                .zip(last)
                .map(move |(first, last)| PathEvent::End { first, last, close }),
        )
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
