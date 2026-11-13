//! The one image a hold hands out: the masked window's JPEG and how to map it back.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::encode::{CaptureTransform, Encoded};

/// A sealed hold's image, with one owner at a time. Its bytes are user content: zeroed on drop,
/// never in diagnostics, and never logged, written to a file or stored.
#[derive(Serialize, Deserialize)]
pub(crate) struct ScreenContext {
    /// Declared first so it drops first: the registration ends before the bytes are freed.
    #[cfg(windows)]
    #[serde(skip)]
    dump: Option<crate::platform::DumpExclusion>,
    jpeg: Zeroizing<Vec<u8>>,
    size: [u32; 2],
    transform: CaptureTransform,
}

impl ScreenContext {
    pub(super) fn new(encoded: Encoded) -> Self {
        let Encoded {
            jpeg,
            size,
            transform,
            ..
        } = encoded;
        Self {
            #[cfg(windows)]
            dump: crate::platform::DumpExclusion::new(jpeg.as_ptr(), jpeg.capacity()),
            jpeg,
            size,
            transform,
        }
    }

    #[cfg(test)]
    pub(super) fn jpeg(&self) -> &[u8] {
        &self.jpeg
    }

    #[cfg(test)]
    pub(super) fn transform(&self) -> CaptureTransform {
        self.transform
    }
}

impl PartialEq for ScreenContext {
    fn eq(&self, other: &Self) -> bool {
        self.jpeg == other.jpeg && self.size == other.size && self.transform == other.transform
    }
}

impl Eq for ScreenContext {}

impl std::fmt::Debug for ScreenContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenContext")
            .field("bytes", &self.jpeg.len())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
