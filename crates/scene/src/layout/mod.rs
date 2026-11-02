//! Serial scene preparation and pure layout, independent of painting and clocks.
mod callout;
mod diagram;
mod nodes;
mod path;
mod prepare;
mod primitives;
mod state;

use crate::{
    Preparation, SceneState, StepError,
    display::DisplayList,
    geom::{Rect, Vec2},
    theme::Theme,
};
pub use prepare::{prepare, recover};
pub use state::LayoutState;
use std::sync::Arc;

/// Host geometry supplied for this rendering, not persisted drawing coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SurfaceGeometry {
    /// Overlay display dimensions in logical pixels.
    Overlay {
        /// Logical width and height.
        logical_size: [f32; 2],
    },
    /// Inline layout with the host's available width.
    Inline {
        /// Logical available width.
        width: f32,
    },
}
impl Default for SurfaceGeometry {
    fn default() -> Self {
        Self::Inline { width: 360.0 }
    }
}
impl SurfaceGeometry {
    fn viewport(self, canvas_size: Option<[f32; 2]>) -> Result<Rect, PrepareError> {
        let size = match self {
            Self::Overlay { logical_size } => Vec2::new(logical_size[0], logical_size[1]),
            Self::Inline { width } => canvas_size
                .map_or(Vec2::new(width, crate::limits::MAX_COORD), |size| {
                    Vec2::new(size[0], size[1])
                }),
        };
        let rect = Rect {
            origin: Vec2::default(),
            size,
        };
        if size.x <= 0.0 || size.y <= 0.0 || !rect.within_canvas() {
            return Err(primitives::layout_error(crate::ErrorCode::OutOfRange));
        }
        Ok(rect)
    }
}

/// Serial preparation inputs, joined only after the preceding layout is available.
#[derive(Debug)]
pub struct PrepareInput {
    ticket: Preparation,
    before: LayoutState,
    surface: SurfaceGeometry,
    theme: Arc<Theme>,
}
impl PrepareInput {
    /// Joins commit-time sizing facts with the immediately preceding prepared state.
    /// # Errors
    /// Rejects a foreign, nonconsecutive or pre-cut layout and invalid host dimensions.
    pub fn new(
        ticket: Preparation,
        before: LayoutState,
        surface: SurfaceGeometry,
        theme: Arc<Theme>,
    ) -> Result<Self, PrepareError> {
        if !Arc::ptr_eq(&ticket.owner, &before.owner)
            || ticket.cut_revision != before.cut_revision
            || before.revision.checked_add(1) != Some(ticket.revision)
        {
            return Err(PrepareError::Stale);
        }
        surface.viewport(ticket.canvas_size)?;
        Ok(Self {
            ticket,
            before,
            surface,
            theme,
        })
    }
}

/// One generated position owner; A7 samples its start at activation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glide {
    /// Diagram group identity.
    pub target: crate::Handle,
    /// Resting position after the shift.
    pub to: Vec2,
}

/// Owned changes for activation, separate from the immutable preceding picture.
#[derive(Debug)]
pub struct PreparedStep {
    /// Lifetime commit index.
    pub index: u32,
    /// New or replaced prepared items.
    pub display: DisplayList,
    /// Objects removed at this step's activation, never during earlier preparation.
    pub removed: Vec<crate::Handle>,
    /// At most one generated position glide per diagram.
    pub glides: Vec<Glide>,
    /// Immutable state to retain if this step activates.
    pub after: LayoutState,
    /// New geometry plus a full per-result layout-index estimate; older geometry is shared.
    pub resident_bytes: usize,
    ticket: Preparation,
    base: Arc<()>,
}
impl PreparedStep {
    /// Rejects results superseded by a cut or prepared against another tip.
    /// # Errors
    /// Returns `Stale` without changing either state.
    pub fn validate(&self, scene: &SceneState, before: &LayoutState) -> Result<(), PrepareError> {
        if !scene.preparation_is_current(&self.ticket)
            || !Arc::ptr_eq(&self.base, &before.version)
            || before.cut_revision != scene.preparation_cut()
        {
            return Err(PrepareError::Stale);
        }
        Ok(())
    }
    /// Original committed input for entrance and explicit-track compilation in A7.
    pub fn step(&self) -> &crate::Step {
        &self.ticket.step
    }
}

/// Content-free preparation failures; the host routes these through its ordinary cut.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    /// Cancellation was observed before publication.
    #[error("scene.PrepareCancelled")]
    Cancelled,
    /// The scene or layout changed across preparation.
    #[error("scene.StalePreparation")]
    Stale,
    /// Host text shaping failed.
    #[error("scene.TextMeasure")]
    TextMeasure,
    /// Owned output exceeds the preparation residency limit.
    #[error("scene.OverResidency")]
    OverResidency,
    /// Semantic or measured geometry validation failed.
    #[error(transparent)]
    Layout(#[from] StepError),
}
impl From<crate::ErrorCode> for PrepareError {
    fn from(code: crate::ErrorCode) -> Self {
        primitives::layout_error(code)
    }
}

/// A kept preparation failure identifies the revision B4 must stop before.
#[derive(Debug, thiserror::Error)]
#[error("scene.RecoveryPreparation revision={revision}: {error}")]
pub struct RecoveryError {
    /// Failing kept revision; zero denotes invalid header/log rather than a kept step.
    pub revision: u32,
    /// Content-free cause.
    pub error: PrepareError,
}
impl From<StepError> for RecoveryError {
    fn from(error: StepError) -> Self {
        Self {
            revision: 0,
            error: PrepareError::Layout(error),
        }
    }
}
