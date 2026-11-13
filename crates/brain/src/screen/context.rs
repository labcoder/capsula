//! The one image a hold hands out: the masked window's JPEG and how to map it back.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize as _, Zeroizing};

use super::encode::{CaptureTransform, Encoded};

#[cfg(test)]
mod tests;

/// A sealed hold's image, with one owner at a time. Its bytes are user content: zeroed on drop,
/// never in diagnostics, and never logged, written to a file or stored. A received one is
/// registered for dump exclusion as the worker's is.
#[derive(Serialize, Deserialize)]
#[serde(from = "Received")]
pub(crate) struct ScreenContext {
    /// Declared first so it drops first: the registration ends after `Drop` zeroes the bytes and
    /// before they're freed.
    #[cfg(windows)]
    #[serde(skip)]
    _dump: Option<crate::platform::DumpExclusion>,
    #[serde(serialize_with = "jpeg_bytes")]
    jpeg: Zeroizing<Vec<u8>>,
    size: [u32; 2],
    transform: CaptureTransform,
}

/// A [`ScreenContext`] as it arrives, before it's registered.
#[derive(Deserialize)]
struct Received {
    #[serde(deserialize_with = "exact_jpeg")]
    jpeg: Zeroizing<Vec<u8>>,
    size: [u32; 2],
    transform: CaptureTransform,
}

impl From<Received> for ScreenContext {
    fn from(received: Received) -> Self {
        Self::sealed(received.jpeg, received.size, received.transform)
    }
}

/// The JPEG as one byte string: the same bytes postcard writes for a list of bytes, so the wire
/// doesn't change, read in one piece by [`exact_jpeg`].
fn jpeg_bytes<S: Serializer>(jpeg: &Zeroizing<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(jpeg)
}

/// The JPEG copied once into a zeroing buffer of exactly its length; a list of bytes, which would
/// grow its buffer as it went, is refused.
fn exact_jpeg<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Zeroizing<Vec<u8>>, D::Error> {
    struct Exact;

    impl Visitor<'_> for Exact {
        type Value = Zeroizing<Vec<u8>>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JPEG's bytes")
        }

        fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
            let mut jpeg = Zeroizing::new(Vec::with_capacity(bytes.len()));
            jpeg.extend_from_slice(bytes);
            Ok(jpeg)
        }
    }

    deserializer.deserialize_bytes(Exact)
}

impl ScreenContext {
    pub(super) fn new(encoded: Encoded) -> Self {
        let Encoded {
            jpeg,
            size,
            transform,
            ..
        } = encoded;
        Self::sealed(jpeg, size, transform)
    }

    fn sealed(jpeg: Zeroizing<Vec<u8>>, size: [u32; 2], transform: CaptureTransform) -> Self {
        Self {
            #[cfg(windows)]
            _dump: crate::platform::DumpExclusion::new(jpeg.as_ptr(), jpeg.capacity()),
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

impl Drop for ScreenContext {
    fn drop(&mut self) {
        self.jpeg.zeroize();
    }
}

impl PartialEq for ScreenContext {
    fn eq(&self, other: &Self) -> bool {
        self.jpeg == other.jpeg && self.size == other.size && self.transform == other.transform
    }
}

impl Eq for ScreenContext {}

impl fmt::Debug for ScreenContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScreenContext")
            .field("bytes", &self.jpeg.len())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
