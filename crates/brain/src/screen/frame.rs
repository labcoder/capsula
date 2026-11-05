//! The worker's one frame: a reused CPU buffer and, separately, whether it holds a valid image.

use kumochi_protocol::geometry::DisplayRef;

use super::wire::{FrameMeta, Refusal};

/// The longest display side the worker captures, so the buffer stays under 256 MiB.
pub(crate) const MAX_SIDE: u32 = 8192;

/// BGRA pixels of the last capture. A replacement invalidates the image before it writes, and
/// only a complete, checked copy becomes valid again. Windows and macOS share it.
#[derive(Default)]
pub(crate) struct RawFrame {
    bgra: Vec<u8>,
    filled: [u32; 2],
    valid: Option<FrameMeta>,
}

impl RawFrame {
    /// Drops the held image and returns exactly `width * height * 4` bytes to fill.
    /// This write-only capture seam invalidates the image before lending its buffer.
    ///
    /// # Errors
    /// `Invalid` for an empty side, a side over [`MAX_SIDE`] or an overflowing length.
    #[cfg_attr(
        not(any(test, windows)),
        expect(dead_code, reason = "only the Windows adapter fills it")
    )]
    pub(crate) fn begin(&mut self, width: u32, height: u32) -> Result<&mut [u8], Refusal> {
        self.valid = None;
        self.filled = [0; 2];
        let len = byte_len([width, height]).ok_or(Refusal::Invalid)?;
        self.bgra.resize(len, 0);
        self.filled = [width, height];
        Ok(&mut self.bgra)
    }

    pub(super) fn invalidate(&mut self) {
        self.valid = None;
    }

    /// Publishes the image only when the copy filled exactly the requested size.
    pub(crate) fn commit(&mut self, meta: FrameMeta) -> Result<(), Refusal> {
        if self.filled != meta.display.size {
            self.invalidate();
            return Err(Refusal::DisplayChanged);
        }
        self.valid = Some(meta);
        Ok(())
    }

    pub(super) fn image_mut(&mut self) -> Option<(FrameMeta, &mut [u8])> {
        self.valid.map(|meta| (meta, self.bgra.as_mut_slice()))
    }

    #[cfg(test)]
    pub(crate) fn image(&self) -> Option<(FrameMeta, &[u8])> {
        self.valid.map(|meta| (meta, self.bgra.as_slice()))
    }

    #[cfg(test)]
    pub(super) fn allocation(&self) -> (*const u8, usize) {
        (self.bgra.as_ptr(), self.bgra.capacity())
    }
}

impl std::fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawFrame")
            .field("bytes", &self.bgra.len())
            .field("valid", &self.valid)
            .finish_non_exhaustive()
    }
}

/// The display's `[left, top, right, bottom]` in physical pixels, once its size is capturable.
///
/// # Errors
/// `Invalid` when a side is empty or over the cap, or an edge leaves the `i32` range.
pub(crate) fn display_edges(display: &DisplayRef) -> Result<[i32; 4], Refusal> {
    let rect = display.0;
    byte_len(rect.size).ok_or(Refusal::Invalid)?;
    let edge = |origin: i32, size: u32| {
        i32::try_from(i64::from(origin) + i64::from(size)).map_err(|_| Refusal::Invalid)
    };
    Ok([
        rect.origin[0],
        rect.origin[1],
        edge(rect.origin[0], rect.size[0])?,
        edge(rect.origin[1], rect.size[1])?,
    ])
}

fn byte_len([width, height]: [u32; 2]) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}
