//! Serial scene preparation and pure layout, independent of painting and clocks.
mod callout;
mod chart;
mod compound;
mod diagram;
mod edge_labels;
mod edges;
pub(crate) mod heap;
mod nodes;
mod path;
mod place;
mod prepare;
mod primitives;
mod state;
mod table;

use crate::{
    Preparation, SceneState, StepError,
    deferred::Deferred,
    display::DisplayList,
    geom::{Rect, Vec2},
    theme::Theme,
};
pub use place::place;
pub use prepare::{prepare, recover, recover_all};
pub use state::LayoutState;
pub(crate) use state::Rest;
use std::sync::Arc;

/// Overlay layout dimensions when the drawing does not declare a canvas.
pub const DEFAULT_CANVAS: [f32; 2] = [960.0, 600.0];

/// Host geometry supplied for this rendering, not persisted drawing coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SurfaceGeometry {
    /// Overlay drawing canvas, independent of the display or presentation box.
    Overlay {
        /// Canvas dimensions when the drawing does not declare its own size.
        default_canvas: [f32; 2],
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
            Self::Overlay { default_canvas } => {
                let size = canvas_size.unwrap_or(default_canvas);
                Vec2::new(size[0], size[1])
            }
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
    paint_cap: Option<crate::display::PaintWork>,
}
impl PrepareInput {
    /// Joins commit-time sizing facts with the immediately preceding prepared state.
    /// # Errors
    /// Rejects a foreign, nonconsecutive or pre-cut layout and invalid host dimensions, and
    /// hands the ticket back so the step can still join its right predecessor.
    #[expect(
        clippy::result_large_err,
        reason = "return the original ticket without allocating so serial preparation can retry it"
    )]
    pub fn new(
        ticket: Preparation,
        before: LayoutState,
        surface: SurfaceGeometry,
        theme: Arc<Theme>,
    ) -> Result<Self, (PrepareError, Preparation)> {
        let current = Arc::ptr_eq(&ticket.owner, &before.owner)
            && ticket.cut_revision == before.cut_revision
            && before.revision.checked_add(1) == Some(ticket.revision);
        let checked = if current {
            surface.viewport(ticket.canvas_size)
        } else {
            Err(PrepareError::Stale)
        };
        if let Err(error) = checked {
            // The rejected layout may hold the last references to a prefix's geometry.
            let bytes = before.index_bytes();
            drop(Deferred::new(before, bytes));
            return Err((error, ticket));
        }
        Ok(Self {
            ticket,
            before,
            surface,
            theme,
            paint_cap: None,
        })
    }
    /// Refuses the step with `OverPaintWork` once the scene's own paint work passes `cap`,
    /// counting no further crossings; without one, preparation charges its work exactly.
    #[must_use]
    pub fn with_paint_cap(self, paint_cap: Option<crate::display::PaintWork>) -> Self {
        Self { paint_cap, ..self }
    }
}

/// One 3D view a step prepared, published whole.
#[derive(Debug, Clone)]
pub struct PreparedView {
    /// The view's identity.
    pub view: crate::Handle,
    /// Its solids; empty and `failed` when any solid failed at rest.
    pub geometry: Arc<crate::three::view::ViewGeometry>,
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
    /// New or replaced 3D views; a removed view's handle is in `removed`.
    pub views: Vec<PreparedView>,
    /// Immutable state to retain if this step activates.
    pub after: LayoutState,
    /// Requested bytes this result adds to its predecessor: new geometry, source copies and
    /// motion maps, its own full layout index, its vectors at capacity, and the ticket's
    /// compiled record and private facts. Allocations shared with the predecessor are not
    /// counted again.
    pub resident_bytes: usize,
    retained_bytes: usize,
    /// Temporary mesh scratch the worker allocated, never published or retained.
    pub scratch_bytes: usize,
    ticket: Preparation,
    base: Arc<()>,
    theme: Arc<Theme>,
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
    /// Requested bytes of the full `after` layout as prepared, geometry included, computed once
    /// on the preparing thread: its charge as a rest, and what a view of it may hold last.
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    /// Cached per-holder retirement estimate, including the full layout and original record.
    pub fn owner_bytes(&self) -> usize {
        self.retained_bytes
            .saturating_add(self.resident_bytes)
            .saturating_add(self.original_bytes())
    }
    /// Original committed input, exactly as the model sent it.
    pub fn step(&self) -> &crate::Step {
        &self.ticket.step
    }

    /// Requested bytes of the original record and its Arc, even when the run aliases it.
    /// Admission computes this once before ownership can outlive the scene's log.
    pub fn original_bytes(&self) -> usize {
        self.ticket.original_bytes
    }
    /// Compiled input for entrance and explicit-track compilation in A7; the original unless
    /// it held Mermaid.
    pub(crate) fn run(&self) -> &crate::Step {
        &self.ticket.run
    }
    /// Whether this result was prepared on exactly `before`'s layout version.
    pub(crate) fn is_based_on(&self, before: &LayoutState) -> bool {
        Arc::ptr_eq(&self.base, &before.version)
    }
    /// The palette color keys compile against at activation.
    pub(crate) fn theme(&self) -> &Theme {
        &self.theme
    }
}

/// Content-free preparation failures; the host routes these through its ordinary cut.
/// A `Stale` from [`crate::SceneView::frame`] instead requires clearing the frame
/// outside paint, not cutting the scene.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    /// Cancellation was observed before publication.
    #[error("scene.PrepareCancelled")]
    Cancelled,
    /// The scene or layout changed, or a frame belongs to another view generation.
    #[error("scene.StalePreparation")]
    Stale,
    /// Host text shaping failed.
    #[error("scene.TextMeasure")]
    TextMeasure,
    /// Owned output exceeds the preparation residency limit.
    #[error("scene.OverResidency")]
    OverResidency,
    /// The step's paint work passes the cap the host passes, or one item's fill or rect
    /// outline exceeds a single tessellation build.
    #[error("scene.OverPaintWork")]
    OverPaintWork,
    /// Semantic or measured geometry validation failed.
    #[error(transparent)]
    Layout(#[from] StepError),
}
impl From<crate::ErrorCode> for PrepareError {
    fn from(code: crate::ErrorCode) -> Self {
        primitives::layout_error(code)
    }
}

/// One source scene's canonical state and resting layout, or its isolated recovery failure.
pub type SceneRecovery = Result<(SceneState, LayoutState), RecoveryError>;

/// A kept preparation failure identifies the revision B4 must stop before.
#[derive(Debug, thiserror::Error)]
#[error("scene.RecoveryPreparation revision={revision}: {error}")]
pub struct RecoveryError {
    /// Failing kept revision; zero rejects the whole scene: an invalid header or log, or a
    /// recovered scene that [`recover_all`]'s batch can't also fit under its global caps.
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
