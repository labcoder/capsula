//! Picking from immutable raster ids and the exact mapping of their painted image.

use super::{Transform, camera::Camera, raster::MAX_TEXELS};
use crate::{Handle, limits::MAX_SOLIDS};

/// A content-free refusal to freeze inconsistent frame data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    /// The destination, DPI or raster dimensions cannot describe a displayed image.
    #[error("scene.Size")]
    Size,
    /// Pose/part counts disagree, exceed the solid bound, or an id has no source solid.
    #[error("scene.Invalid")]
    Invalid,
}

/// Frozen logical destination, effective DPI and exact internal raster dimensions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    origin: [f64; 2],
    extent: [f64; 2],
    dpi: f64,
    texels: [u32; 2],
}

impl Mapping {
    /// Snapshots a logical rectangle and the raster dimensions actually painted into it.
    ///
    /// `dpi` is physical pixels per logical pixel. Pointers use the same logical coordinate
    /// space as `origin`; DPI is recorded, not applied a second time during picking.
    /// # Errors
    /// Refuses non-finite values, non-positive extents/DPI, unrepresentable rectangle ends
    /// and dimensions outside 1..=4,096 texels per axis.
    pub fn new(
        origin: [f64; 2],
        extent: [f64; 2],
        dpi: f64,
        texels: [u32; 2],
    ) -> Result<Self, PickError> {
        if !dpi.is_finite()
            || dpi <= 0.0
            || (0..2).any(|i| {
                !origin[i].is_finite()
                    || !extent[i].is_finite()
                    || extent[i] <= 0.0
                    || !(origin[i] + extent[i]).is_finite()
                    || origin[i] + extent[i] <= origin[i]
                    || !(1..=MAX_TEXELS).contains(&texels[i])
            })
        {
            return Err(PickError::Size);
        }
        Ok(Self {
            origin,
            extent,
            dpi,
            texels,
        })
    }

    /// Returns logical origin and extent, in that order.
    pub fn destination(self) -> [[f64; 2]; 2] {
        [self.origin, self.extent]
    }

    /// Returns the physical-pixels-per-logical-pixel ratio at the paint snapshot.
    pub fn dpi(self) -> f64 {
        self.dpi
    }

    /// Returns the exact integer raster width and height, not a rounded scalar scale.
    pub fn texels(self) -> [u32; 2] {
        self.texels
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounds checks keep floored coordinates nonnegative and below 4,096"
    )]
    fn index(self, at: [f64; 2]) -> Option<usize> {
        if (0..2).any(|i| {
            !at[i].is_finite() || at[i] < self.origin[i] || at[i] >= self.origin[i] + self.extent[i]
        }) {
            return None;
        }
        let [x, y] = std::array::from_fn(|i| {
            let coordinate = (at[i] - self.origin[i]) / self.extent[i] * f64::from(self.texels[i]);
            (coordinate.floor() as usize).min(self.texels[i] as usize - 1)
        });
        Some(y * self.texels[0] as usize + x)
    }
}

/// Numeric and identity state used to produce one raster, owned by that frame.
#[derive(Debug)]
pub struct FrameState {
    /// Geometry revision, independent of the parts' original commit revisions.
    pub revision: u32,
    /// The camera used for this raster, not the latest requested camera.
    pub camera: Camera,
    /// Sampled, attachment-composed transforms, in zero-based solid order.
    pub pose: Vec<Transform>,
    /// Destination and raster mapping sampled for this frame.
    pub mapping: Mapping,
    /// Zero-based solid-to-part map; `None` means geometry blocks without a label.
    pub parts: Vec<Option<Handle>>,
}

/// The ids and metadata belonging to one displayed image, frozen under unique ownership.
///
/// The host pairs this with its image only at accepted paint. A completed, skipped, stale
/// or failed candidate must not replace the displayed pair. A worker owns the other id
/// buffer; it cannot borrow mutable ids from this value. No user labels are stored here.
pub struct DisplayedFrame {
    state: FrameState,
    ids: Vec<u8>,
}

impl std::fmt::Debug for DisplayedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayedFrame")
            .field("revision", &self.state.revision)
            .field("texels", &self.state.mapping.texels)
            .field("solids", &self.state.parts.len())
            .field("ids", &self.ids.len())
            .finish_non_exhaustive()
    }
}

impl DisplayedFrame {
    /// Freezes a completed raster and its corresponding geometry snapshot without copying.
    ///
    /// Id 0 is background; id `n` refers to `parts[n - 1]`. Validation scans every id once,
    /// so construct it on the raster worker, never on the UI thread.
    /// # Errors
    /// Refuses an id length other than width × height, more than 160 solids, mismatched
    /// pose/part counts or an id outside the supplied part map.
    pub fn new(state: FrameState, ids: Vec<u8>) -> Result<Self, PickError> {
        let [width, height] = state.mapping.texels;
        if ids.len() != usize::try_from(width * height).map_err(|_| PickError::Size)? {
            return Err(PickError::Size);
        }
        if state.parts.len() > MAX_SOLIDS
            || state.pose.len() != state.parts.len()
            || ids.iter().any(|id| usize::from(*id) > state.parts.len())
        {
            return Err(PickError::Invalid);
        }
        Ok(Self { state, ids })
    }

    /// Borrows the immutable camera, pose, revision, mapping and part snapshot.
    pub fn state(&self) -> &FrameState {
        &self.state
    }

    /// Borrows the immutable raster ids belonging to the displayed image.
    pub fn ids(&self) -> &[u8] {
        &self.ids
    }

    /// Retires this snapshot, returning its metadata and id allocation for reuse.
    pub fn into_parts(self) -> (FrameState, Vec<u8>) {
        (self.state, self.ids)
    }
}

/// The discrete picking policy of the painted raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// A labelled solid, with the handle from the frozen geometry revision.
    Part(Handle),
    /// Visible geometry with no part label blocks what is behind it.
    Blocked,
    /// Background, an outside pointer or a non-finite pointer.
    Miss,
}

/// Reads one id through the displayed image's frozen per-axis mapping, without allocating.
///
/// Bounds are half-open. An edge neighbourhood of one internal texel, mapped separately
/// on each axis (including bilinear filtering), may disagree with an exact geometric ray.
/// Rest ids use the most frequent sample id including background, ties by first sample.
/// shortcut: translucent ids follow centroid paint order, not nearest-ray order; revisit
/// when OSC-242's unsplit transparency approximation is replaced.
pub fn pick(displayed: &DisplayedFrame, at: [f64; 2]) -> Pick {
    let Some(index) = displayed.state.mapping.index(at) else {
        return Pick::Miss;
    };
    let id = displayed.ids[index];
    if id == 0 {
        return Pick::Miss;
    }
    displayed.state.parts[usize::from(id) - 1].map_or(Pick::Blocked, Pick::Part)
}
