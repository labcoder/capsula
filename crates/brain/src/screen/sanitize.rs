//! The only entrance to pixel consumers: mask the held full-resolution frame in place.

use kumochi_protocol::geometry::PhysicalRect;

use super::frame::RawFrame;
use super::mask::{Completeness, MaskSet};
use super::wire::{FrameMeta, Refusal};

/// A borrowed image with every known password rectangle blacked out.
pub(crate) struct SanitizedFrame<'a> {
    bgra: &'a [u8],
    meta: FrameMeta,
    generation: u32,
    completeness: Completeness,
    painted: Vec<PhysicalRect>,
}

impl SanitizedFrame<'_> {
    pub(crate) fn image(&self) -> (FrameMeta, &[u8]) {
        (self.meta, self.bgra)
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    pub(crate) fn completeness(&self) -> Completeness {
        self.completeness
    }

    pub(crate) fn painted(&self) -> &[PhysicalRect] {
        &self.painted
    }
}

impl std::fmt::Debug for SanitizedFrame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SanitizedFrame")
            .field("size", &self.meta.display.size)
            .field("bytes", &self.bgra.len())
            .field("generation", &self.generation)
            .finish()
    }
}

/// Blacks out known masks before lending the image to any pixel consumer.
///
/// # Errors
/// `Stale` when the raw frame has no committed image or the masks belong to another capture.
pub(crate) fn sanitize<'a>(
    frame: &'a mut RawFrame,
    masks: &MaskSet,
) -> Result<SanitizedFrame<'a>, Refusal> {
    let (meta, bgra) = frame.image_mut().ok_or(Refusal::Stale)?;
    if masks.capture() != meta.capture {
        return Err(Refusal::Stale);
    }
    let width = usize::try_from(meta.display.size[0]).expect("capturable width fits usize");
    let mut painted = Vec::new();
    for mask in masks.rects() {
        let Some(rect) = intersection(*mask, meta.display) else {
            continue;
        };
        painted.push(rect);
        let [left, top] = std::array::from_fn(|axis| {
            usize::try_from(i64::from(rect.origin[axis]) - i64::from(meta.display.origin[axis]))
                .expect("intersection lies inside the display")
        });
        let [w, h] = rect
            .size
            .map(|side| usize::try_from(side).expect("capturable side fits usize"));
        for y in top..top + h {
            for pixel in bgra[(y * width + left) * 4..(y * width + left + w) * 4]
                .as_chunks_mut::<4>()
                .0
            {
                *pixel = [0, 0, 0, 255];
            }
        }
    }
    Ok(SanitizedFrame {
        bgra,
        meta,
        generation: masks.generation(),
        completeness: masks.completeness(),
        painted,
    })
}

pub(super) fn intersection(rect: PhysicalRect, display: PhysicalRect) -> Option<PhysicalRect> {
    let mut origin = [0; 2];
    let mut size = [0; 2];
    for axis in 0..2 {
        let left = i64::from(rect.origin[axis]).max(i64::from(display.origin[axis]));
        let right = (i64::from(rect.origin[axis]) + i64::from(rect.size[axis]))
            .min(i64::from(display.origin[axis]) + i64::from(display.size[axis]));
        if right <= left {
            return None;
        }
        origin[axis] = i32::try_from(left).expect("intersection origin is an input origin");
        size[axis] = u32::try_from(right - left).expect("intersection fits an input size");
    }
    Some(PhysicalRect { origin, size })
}
