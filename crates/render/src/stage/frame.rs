//! A frame's records: a header per picture and one record per paint item, in buffers reserved
//! once for four pictures of a full scene each. Writing them never allocates.

use std::fmt;

use kumochi_protocol::message::SceneId;

use super::{Color, Failure, GeometryId, index, push};
use crate::limits::{PICTURES, RECORDS};

/// One drawn range of one object, with its sampled pose.
#[derive(Clone, Copy, PartialEq)]
pub struct Record {
    /// The object's index in its geometry's sorted objects.
    pub object: u32,
    /// The first primitive this range draws.
    pub first: u32,
    /// Primitives in the range.
    pub count: u32,
    /// World transform `[a, b, c, d, e, f]`, placement included.
    pub transform: [f32; 6],
    /// World opacity.
    pub opacity: f32,
    /// Stroke reveal fraction.
    pub draw: f32,
    /// Colour override; none keeps the prepared paint.
    pub color: Option<Color>,
    /// Dash phase, in periods.
    pub flow: f32,
    /// The scale its rect fallbacks were charged at.
    pub paint_scale: f32,
}

/// The pose is the drawing's: only which range it draws.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("object", &self.object)
            .field("first", &self.first)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

/// One picture in a frame: its scene, the geometry it draws and its records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    /// The scene it shows.
    pub scene: SceneId,
    /// The geometry its records index.
    pub geometry: GeometryId,
    /// Its first record's place in the frame.
    pub(super) first: u32,
    count: u32,
}

/// One frame's pictures and records, reserved once.
pub struct FrameBuf {
    pictures: Vec<Picture>,
    records: Vec<Record>,
}

impl FrameBuf {
    /// Bytes one buffer reserves, boxed.
    pub const BYTES: usize =
        size_of::<Self>() + PICTURES * size_of::<Picture>() + RECORDS * size_of::<Record>();

    /// Reserves room for every picture of a frame and all their records.
    pub fn new() -> Self {
        Self {
            pictures: Vec::with_capacity(PICTURES),
            records: Vec::with_capacity(RECORDS),
        }
    }

    /// The pictures listed so far, in the order they were written.
    pub fn pictures(&self) -> &[Picture] {
        &self.pictures
    }

    /// A listed picture's records, in paint order.
    pub fn records(&self, picture: &Picture) -> &[Record] {
        let start = usize::try_from(picture.first).unwrap_or(usize::MAX);
        let len = usize::try_from(picture.count).unwrap_or(0);
        self.records
            .get(start..start.saturating_add(len))
            .unwrap_or(&[])
    }

    /// The records of `pictures`, consecutive listed pictures, and the first one's place.
    pub(super) fn span(&self, pictures: &[Picture]) -> (u32, &[Record]) {
        let (Some(first), Some(last)) = (pictures.first(), pictures.last()) else {
            return (0, &[]);
        };
        let start = usize::try_from(first.first).unwrap_or(usize::MAX);
        let end = usize::try_from(last.first.saturating_add(last.count)).unwrap_or(0);
        (first.first, self.records.get(start..end).unwrap_or(&[]))
    }

    /// Empties the buffer for the next frame, keeping its reservations.
    pub fn clear(&mut self) {
        self.pictures.clear();
        self.records.clear();
    }

    /// Appends a picture's records with `write`, and lists the picture if it wrote any. On an
    /// error, nothing `write` appended stays.
    pub(crate) fn picture(
        &mut self,
        scene: SceneId,
        geometry: GeometryId,
        write: impl FnOnce(&mut Records<'_>) -> Result<(), Failure>,
    ) -> Result<bool, Failure> {
        let start = self.records.len();
        let written = self.write_picture(scene, geometry, start, write);
        if written.is_err() {
            self.records.truncate(start);
        }
        written
    }

    fn write_picture(
        &mut self,
        scene: SceneId,
        geometry: GeometryId,
        start: usize,
        write: impl FnOnce(&mut Records<'_>) -> Result<(), Failure>,
    ) -> Result<bool, Failure> {
        write(&mut Records(&mut self.records))?;
        let count = self.records.len() - start;
        if count == 0 {
            return Ok(false);
        }
        if self.pictures.len() >= PICTURES {
            return Err(Failure::Build);
        }
        let picture = Picture {
            scene,
            geometry,
            first: index(start)?,
            count: index(count)?,
        };
        push(&mut self.pictures, PICTURES, picture)?;
        Ok(true)
    }

    /// The buffers' addresses and capacities, which a write must leave as they were.
    #[cfg(test)]
    pub(crate) fn reservation(&self) -> [usize; 4] {
        [
            self.pictures.as_ptr().addr(),
            self.pictures.capacity(),
            self.records.as_ptr().addr(),
            self.records.capacity(),
        ]
    }

    /// Fills the records with copies of `record` up to `len`, as if earlier pictures held them.
    #[cfg(test)]
    pub(crate) fn fill_records(&mut self, record: Record, len: usize) {
        self.records.resize(len, record);
    }

    /// The bytes its buffers really reserve.
    #[cfg(test)]
    pub(crate) fn reserved(&self) -> usize {
        size_of::<Self>()
            + self.pictures.capacity() * size_of::<Picture>()
            + self.records.capacity() * size_of::<Record>()
    }
}

impl Default for FrameBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts only: records carry the drawing's poses.
impl fmt::Debug for FrameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameBuf")
            .field("pictures", &self.pictures.len())
            .field("records", &self.records.len())
            .finish()
    }
}

/// The record list of the picture being written; it can't grow past its reservation.
pub(crate) struct Records<'a>(&'a mut Vec<Record>);

impl Records<'_> {
    pub(crate) fn push(&mut self, record: Record) -> Result<(), Failure> {
        push(self.0, RECORDS, record)
    }
}
