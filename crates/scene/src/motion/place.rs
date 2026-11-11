//! Presentation state survives activation independently of the animation clock.
use super::SceneView;
use crate::{
    geom::{Affine, Rect},
    layout::{self, LayoutState},
};

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Placement {
    stage: Option<Rect>,
    pub(super) transform: Affine,
}

impl Placement {
    pub(super) fn for_layout(self, layout: &LayoutState) -> Self {
        Self {
            stage: self.stage,
            transform: self.stage.map_or_else(Affine::default, |stage| {
                layout::place(layout.fit_extent(), stage, layout.presentation_scale())
            }),
        }
    }
}

impl SceneView {
    /// Snaps the drawing into a host-selected box without preparing it again.
    pub fn set_stage(&mut self, stage: Rect) -> bool {
        if self.placement.stage == Some(stage) {
            return false;
        }
        self.placement.stage = Some(stage);
        self.placement = self.placement.for_layout(&self.layout);
        true
    }
}
