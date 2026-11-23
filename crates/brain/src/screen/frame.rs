//! The worker's one frame: a reused CPU buffer and, separately, whether it holds a valid image.

use zeroize::Zeroize as _;

use super::wire::{FrameMeta, Refusal};

/// The longest display side the worker captures, so the buffer stays under 256 MiB.
pub(crate) const MAX_SIDE: u32 = 8192;

/// BGRA pixels of the last capture. A replacement invalidates the image before it writes, and
/// only a complete, checked copy becomes valid again. Windows and macOS share it.
#[derive(Default)]
pub(crate) struct RawFrame {
    /// Registered while the allocation holds pixels; dropped once it's scrubbed, before it's freed.
    #[cfg(windows)]
    dump: Option<crate::platform::DumpExclusion>,
    bgra: Vec<u8>,
    filled: [u32; 2],
    valid: Option<FrameMeta>,
    #[cfg(test)]
    audit: Option<std::sync::mpsc::Sender<Vec<u8>>>,
}

impl RawFrame {
    #[cfg(test)]
    pub(super) fn audit(&mut self, audit: std::sync::mpsc::Sender<Vec<u8>>) {
        self.audit = Some(audit);
    }
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
        if len > self.bgra.capacity() {
            self.release();
        }
        self.bgra.resize(len, 0);
        #[cfg(windows)]
        if self.dump.is_none() {
            self.dump =
                crate::platform::DumpExclusion::new(self.bgra.as_ptr(), self.bgra.capacity());
        }
        self.filled = [width, height];
        Ok(&mut self.bgra)
    }

    /// Runs one capture into this frame, then zeroes and frees it if the capture failed, so a
    /// refused or partial copy never stays in the buffer. Windows and macOS share it.
    #[cfg(any(test, windows))]
    pub(crate) fn fill<T, E>(
        &mut self,
        capture: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        let result = capture(self);
        if result.is_err() {
            self.release();
        }
        result
    }

    pub(super) fn invalidate(&mut self) {
        self.valid = None;
    }

    /// Zeroes and frees the current allocation. Replacement may reuse equal or smaller storage.
    pub(super) fn release(&mut self) {
        drop(self.scrubbed());
    }

    /// Takes the whole allocation out, every byte of its capacity zeroed with volatile writes the
    /// optimizer keeps.
    fn scrubbed(&mut self) -> Vec<u8> {
        self.valid = None;
        self.filled = [0; 2];
        let capacity = self.bgra.capacity();
        self.bgra.resize(capacity, 0);
        self.bgra.as_mut_slice().zeroize();
        #[cfg(windows)]
        drop(self.dump.take());
        #[cfg(test)]
        if let Some(audit) = &self.audit {
            let _sent = audit.send(self.bgra.clone());
        }
        std::mem::take(&mut self.bgra)
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

    pub(super) fn meta(&self) -> Option<FrameMeta> {
        self.valid
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

impl Drop for RawFrame {
    fn drop(&mut self) {
        self.release();
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

fn byte_len([width, height]: [u32; 2]) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}

#[cfg(test)]
mod tests;
