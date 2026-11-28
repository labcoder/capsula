//! The build input: one batch of primitive headers, points and label text from one or more
//! views, copied by the owner into buffers reserved once. A view too large for one batch
//! copies in several, each primitive whole. It holds no reference to scene memory, and
//! [`BuildInput::wipe`] overwrites the text once it has been used.

use std::fmt;

use super::{Color, Cursor, Failure, GeometryId, index, push};
use crate::limits::{BATCH_TEXT_BYTES, INPUT_OBJECTS, INPUT_POINTS, INPUT_PRIMS, PICTURES};

/// A run of points or text bytes in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// First element.
    pub start: u32,
    /// Elements.
    pub len: u32,
}

/// One object's run of shapes in a batch: all of them, or those this batch took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    /// Its first shape in the input.
    pub first: u32,
    /// Its shapes, one per primitive in order.
    pub count: u32,
}

/// Where a label sits in its wrapping box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Left.
    Left,
    /// Centre.
    Center,
    /// Right.
    Right,
}

/// One primitive's header; points and text live beside it in the input.
#[derive(Clone, Copy, PartialEq)]
pub enum Shape {
    /// A rounded rectangle with its corner pieces already counted.
    Rect {
        /// Origin and size, `[x, y, width, height]`.
        rect: [f32; 4],
        /// Corner radius, clamped to half the shorter side as painted.
        radius: f32,
        /// Pieces per corner, counted at the scale the scene crate charged it at.
        segments: u32,
        /// Fill colour.
        fill: Option<Color>,
        /// Border colour and width.
        border: Option<(Color, f32)>,
    },
    /// A polygon, filled nonzero.
    Fill {
        /// Its points.
        points: Span,
        /// Fill colour.
        color: Color,
    },
    /// A stroked polyline.
    Line {
        /// Its points.
        points: Span,
        /// Whether the last point joins the first.
        closed: bool,
        /// Stroke colour.
        color: Color,
        /// Stroke width.
        width: f32,
        /// Whether it is painted dashed.
        dashed: bool,
    },
    /// A label, rastered from its text.
    Text {
        /// Top-left of its wrapping box or its ink.
        origin: [f32; 2],
        /// Its UTF-8 bytes.
        text: Span,
        /// Logical-pixel size.
        size: f32,
        /// Bold face.
        bold: bool,
        /// Foreground colour.
        color: Color,
        /// Wrapping width.
        max_width: Option<f32>,
        /// Alignment in the wrapping width.
        align: Align,
    },
}

/// Geometry is drawing content: the kind only.
impl fmt::Debug for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rect { .. } => "Rect",
            Self::Fill { .. } => "Fill",
            Self::Line { .. } => "Line",
            Self::Text { .. } => "Text",
        })
    }
}

#[derive(Clone, Copy)]
struct View {
    id: GeometryId,
    from: Cursor,
    to: Option<Cursor>,
    first: u32,
    count: u32,
}

/// One view's part of a batch: where it starts and stops, and its objects' shapes. When `from`
/// is partway through an object, the first object continues it.
#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    /// The view's first primitive in this batch.
    pub from: Cursor,
    /// The first primitive a later batch holds; none once the view is whole.
    pub to: Option<Cursor>,
    /// Its objects, in order.
    pub objects: &'a [Object],
}

/// How full each part of the input is, to put it back after a copy that didn't fit.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    views: usize,
    objects: usize,
    shapes: usize,
    points: usize,
    text: usize,
}

/// One batch of build input in buffers reserved once.
pub struct BuildInput {
    views: Vec<View>,
    objects: Vec<Object>,
    shapes: Vec<Shape>,
    points: Vec<[f32; 2]>,
    text: Box<[u8]>,
    text_len: usize,
}

impl BuildInput {
    /// Bytes this input reserves, boxed.
    pub const BYTES: usize = size_of::<Self>()
        + PICTURES * size_of::<View>()
        + INPUT_OBJECTS * size_of::<Object>()
        + INPUT_PRIMS * size_of::<Shape>()
        + INPUT_POINTS * size_of::<[f32; 2]>()
        + BATCH_TEXT_BYTES;

    /// Reserves every buffer at its cap.
    pub fn new() -> Self {
        Self {
            views: Vec::with_capacity(PICTURES),
            objects: Vec::with_capacity(INPUT_OBJECTS),
            shapes: Vec::with_capacity(INPUT_PRIMS),
            points: Vec::with_capacity(INPUT_POINTS),
            text: vec![0; BATCH_TEXT_BYTES].into_boxed_slice(),
            text_len: 0,
        }
    }

    /// The part of the view staged as `id` this batch holds, if any.
    pub fn part(&self, id: GeometryId) -> Option<Part<'_>> {
        let view = self.views.iter().find(|view| view.id == id)?;
        let first = usize::try_from(view.first).ok()?;
        let count = usize::try_from(view.count).ok()?;
        Some(Part {
            from: view.from,
            to: view.to,
            objects: self.objects.get(first..first + count)?,
        })
    }

    /// An object's shapes, in primitive order.
    pub fn shapes(&self, object: &Object) -> Option<&[Shape]> {
        let first = usize::try_from(object.first).ok()?;
        let count = usize::try_from(object.count).ok()?;
        self.shapes.get(first..first + count)
    }

    /// A shape's points.
    pub fn points(&self, span: Span) -> Option<&[[f32; 2]]> {
        let start = usize::try_from(span.start).ok()?;
        let len = usize::try_from(span.len).ok()?;
        self.points.get(start..start + len)
    }

    /// A label's UTF-8 bytes. Drawing content: never log it.
    pub fn text(&self, span: Span) -> Option<&[u8]> {
        let start = usize::try_from(span.start).ok()?;
        let len = usize::try_from(span.len).ok()?;
        self.text[..self.text_len].get(start..start + len)
    }

    /// Overwrites the copied points and text and empties the input, keeping its reservations.
    pub fn wipe(&mut self) {
        self.rewind(Mark {
            views: 0,
            objects: 0,
            shapes: 0,
            points: 0,
            text: 0,
        });
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            views: self.views.len(),
            objects: self.objects.len(),
            shapes: self.shapes.len(),
            points: self.points.len(),
            text: self.text_len,
        }
    }

    /// Puts the input back to `mark`, overwriting what was copied after it.
    pub(crate) fn rewind(&mut self, mark: Mark) {
        self.points[mark.points..].fill([0.0; 2]);
        self.points.truncate(mark.points);
        self.text[mark.text..self.text_len].fill(0);
        self.text_len = mark.text;
        self.shapes.truncate(mark.shapes);
        self.objects.truncate(mark.objects);
        self.views.truncate(mark.views);
    }

    /// Whether the batch holds nothing yet.
    pub(crate) fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// Whether the batch holds a part of the view staged as `id`.
    pub(crate) fn holds(&self, id: GeometryId) -> bool {
        self.views.iter().any(|view| view.id == id)
    }

    /// Whether the view table and the headers have room for one more view's first primitive.
    pub(crate) fn has_room(&self) -> bool {
        self.views.len() < PICTURES && self.shapes.len() < INPUT_PRIMS
    }

    /// Whether one more shape with `points` points and `text` label bytes fits, with `objects`
    /// new object entries, 0 or 1.
    pub(crate) fn fits(&self, points: usize, text: usize, objects: usize) -> bool {
        self.shapes.len() < INPUT_PRIMS
            && points <= INPUT_POINTS - self.points.len()
            && text <= BATCH_TEXT_BYTES - self.text_len
            && objects <= INPUT_OBJECTS - self.objects.len()
    }

    /// Starts a view's part at `from`; the objects pushed next are its own.
    pub(crate) fn begin(&mut self, id: GeometryId, from: Cursor) -> Result<(), Failure> {
        let first = index(self.objects.len())?;
        push(
            &mut self.views,
            PICTURES,
            View {
                id,
                from,
                to: None,
                first,
                count: 0,
            },
        )
    }

    /// Ends the current part where the next batch must start, or as the view's last.
    pub(crate) fn end(&mut self, to: Option<Cursor>) -> Result<(), Failure> {
        self.views.last_mut().ok_or(Failure::Build)?.to = to;
        Ok(())
    }

    /// Starts an object of the current part; the shapes pushed next are its own.
    pub(crate) fn object(&mut self) -> Result<(), Failure> {
        let object = Object {
            first: index(self.shapes.len())?,
            count: 0,
        };
        push(&mut self.objects, INPUT_OBJECTS, object)?;
        let view = self.views.last_mut().ok_or(Failure::Build)?;
        view.count += 1;
        Ok(())
    }

    pub(crate) fn shape(&mut self, shape: Shape) -> Result<(), Failure> {
        push(&mut self.shapes, INPUT_PRIMS, shape)?;
        self.objects.last_mut().ok_or(Failure::Build)?.count += 1;
        Ok(())
    }

    pub(crate) fn push_points(
        &mut self,
        points: impl ExactSizeIterator<Item = [f32; 2]>,
    ) -> Result<Span, Failure> {
        let start = self.points.len();
        if points.len() > INPUT_POINTS - start {
            return Err(Failure::Build);
        }
        self.points.extend(points);
        Ok(Span {
            start: index(start)?,
            len: index(self.points.len() - start)?,
        })
    }

    pub(crate) fn push_text(&mut self, text: &str) -> Result<Span, Failure> {
        let start = self.text_len;
        let end = start + text.len();
        let room = self.text.get_mut(start..end).ok_or(Failure::Build)?;
        room.copy_from_slice(text.as_bytes());
        self.text_len = end;
        Ok(Span {
            start: index(start)?,
            len: index(text.len())?,
        })
    }

    /// Every text byte the input reserves, copied or not.
    #[cfg(test)]
    pub(crate) fn text_storage(&self) -> &[u8] {
        &self.text
    }

    /// The bytes its buffers really reserve.
    #[cfg(test)]
    pub(crate) fn reserved(&self) -> usize {
        size_of::<Self>()
            + self.views.capacity() * size_of::<View>()
            + self.objects.capacity() * size_of::<Object>()
            + self.shapes.capacity() * size_of::<Shape>()
            + self.points.capacity() * size_of::<[f32; 2]>()
            + self.text.len()
    }
}

impl Default for BuildInput {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts only: the points and text are drawing content.
impl fmt::Debug for BuildInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildInput")
            .field("views", &self.views.len())
            .field("objects", &self.objects.len())
            .field("shapes", &self.shapes.len())
            .field("points", &self.points.len())
            .field("text_bytes", &self.text_len)
            .finish_non_exhaustive()
    }
}
