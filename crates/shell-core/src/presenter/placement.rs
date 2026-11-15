//! Host geometry changes re-place existing views without restarting their clocks or preparing.
use kumochi_scene::{
    SceneView,
    geom::Rect,
    layout::{LayoutState, SurfaceGeometry},
};

use super::Presenter;

pub(super) fn resting_view(rest: &LayoutState, activated: u32, stage: Option<Rect>) -> SceneView {
    let mut view = SceneView::new(rest, activated);
    if let Some(stage) = stage {
        view.set_stage(stage);
    }
    view
}

impl Presenter {
    pub fn placement_due(&self, now: std::time::Instant) -> bool {
        self.scenes.values().any(|scene| {
            scene.cut == super::Cut::Open
                && !scene.skipping
                && scene.pending_consumption.is_none()
                && scene.ready.front().is_some_and(|head| {
                    self.fenced.is_none_or(|fence| head.run > fence)
                        && scene
                            .activation
                            .due(scene.state.view(), head.step.index)
                            .is_ok_and(|due| due <= scene.activation.time(now))
                })
        })
    }

    pub fn set_stage(&mut self, stage: Rect) -> bool {
        if !matches!(self.surface, SurfaceGeometry::Overlay { .. })
            || self.drawing_stage == Some(stage)
        {
            return false;
        }
        self.drawing_stage = Some(stage);
        let mut frames = self.frames.borrow_mut();
        for (id, scene) in &mut self.scenes {
            scene.view.set_stage(stage);
            frames.reposition(*id, &scene.view);
        }
        true
    }
}
