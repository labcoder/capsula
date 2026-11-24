//! Host geometry changes re-place existing views without restarting their clocks or preparing.
use std::ops::Bound::{Excluded, Unbounded};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    Begin, SceneView,
    geom::Rect,
    layout::{LayoutState, SurfaceGeometry, place},
    record::{Others, ScreenDisplay, Surface},
};

use crate::presence::stage::machine::free_bands;

use super::Presenter;

/// A scene's view at rest, placed where its surface goes: an ordinary scene in the host's
/// stage, a screen scene on the overlay's display. Whether a screen scene could be placed is
/// judged at its first mark.
pub(super) fn resting_view(
    rest: &LayoutState,
    activated: u32,
    stage: Option<Rect>,
    begin: &Begin,
    display: Option<ScreenDisplay>,
) -> SceneView {
    let mut view = SceneView::new(rest, activated);
    if let Some(stage) = stage {
        view.set_stage(stage);
    }
    view.set_screen(begin, display);
    view
}

fn keeps_others(begin: &Begin) -> bool {
    begin.surface == Surface::Overlay && begin.others == Others::Keep
}

fn kept_stage(stage: Rect, occupied: Rect, view: &SceneView) -> Rect {
    let Some(current) = view.staged_2d_bounds() else {
        return stage;
    };
    let extent = view.layout().fit_extent();
    let fit = |band| place(extent, band, view.layout().presentation_scale());
    let preferred = fit(stage);
    if let Some(current_stage) = view.stage()
        && !current.intersects(occupied)
        && fit(current_stage).0[0] >= preferred.0[0]
    {
        return current_stage;
    }
    if !preferred.bounds(extent).intersects(occupied) {
        return stage;
    }
    free_bands(stage, occupied)
        .into_iter()
        .find(|band| fit(*band).0[0] >= preferred.0[0])
        .unwrap_or(stage)
}

impl Presenter {
    fn kept_obstacle(&self, id: SceneId) -> Option<Rect> {
        let mut older = self
            .scenes
            .range(..id)
            .filter_map(|(_, scene)| scene.view.staged_2d_bounds());
        let first = older.next()?;
        older.next().is_none().then_some(first)
    }

    pub(super) fn stage_for(&self, id: SceneId, begin: &Begin, view: &SceneView) -> Option<Rect> {
        let stage = self.drawing_stage.filter(|_| keeps_others(begin))?;
        Some(
            self.kept_obstacle(id)
                .map_or(stage, |occupied| kept_stage(stage, occupied, view)),
        )
    }

    pub(super) fn refresh_kept_after(
        &mut self,
        id: SceneId,
        before: Option<Rect>,
        after: Option<Rect>,
    ) {
        if after.is_none() || before == after {
            return;
        }
        let Some((&peer, scene)) = self
            .scenes
            .range((Excluded(id), Unbounded))
            .find(|(_, scene)| scene.view.staged_2d_bounds().is_some())
        else {
            return;
        };
        let Some(stage) = self.stage_for(peer, scene.state.view().header(), &scene.view) else {
            return;
        };
        let view = &mut self
            .scenes
            .get_mut(&peer)
            .expect("the peer was just read")
            .view;
        if view.set_stage(stage) {
            self.frames.borrow_mut().reposition(peer, view);
        }
    }

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
        let mut first = None;
        let mut placed = 0;
        for (id, scene) in &mut self.scenes {
            scene.view.set_stage(stage);
            if placed == 1
                && keeps_others(scene.state.view().header())
                && let Some(occupied) = first
            {
                scene
                    .view
                    .set_stage(kept_stage(stage, occupied, &scene.view));
            }
            if let Some(bounds) = scene.view.staged_2d_bounds() {
                first.get_or_insert(bounds);
                placed += 1;
            }
            frames.reposition(*id, &scene.view);
        }
        true
    }
}
