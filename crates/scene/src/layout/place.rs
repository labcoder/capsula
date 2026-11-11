//! Canvas extents and presentation transforms, computed only at lifecycle transitions.
use super::{LayoutState, PrepareError};
use crate::{
    display::{Item, Prim},
    geom::{Affine, Rect, Vec2},
};

const CONTENT_PADDING: f32 = 24.0;

/// Fits a scene extent into a host-selected box without changing its layout.
pub fn place(extent: Rect, stage: Rect, max_scale: f32) -> Affine {
    if extent.size.x <= 0.0 || extent.size.y <= 0.0 {
        return Affine::default();
    }
    let scale = (stage.size.x / extent.size.x)
        .min(stage.size.y / extent.size.y)
        .min(max_scale);
    Affine::pose(scale, 0.0, stage.center() - extent.center() * scale)
}

impl LayoutState {
    /// Maximum presentation scale the prepared geometry and paint envelope can support.
    pub fn presentation_scale(&self) -> f32 {
        self.presentation_scale
    }

    /// Canvas or growing world-space content bounds used for presentation.
    pub fn fit_extent(&self) -> Rect {
        let Some(content) = self
            .content
            .filter(|rect| rect.size.x > 0.0 && rect.size.y > 0.0)
        else {
            return self.canvas;
        };
        if self.declared_canvas {
            self.canvas.union(content)
        } else {
            Rect {
                origin: content.origin - Vec2::new(CONTENT_PADDING, CONTENT_PADDING),
                size: content.size + Vec2::new(2.0 * CONTENT_PADDING, 2.0 * CONTENT_PADDING),
            }
        }
    }

    pub(super) fn grow_content(&mut self) -> Result<(), PrepareError> {
        for object in self
            .objects
            .values()
            .filter(|object| !object.item.primitives.is_empty())
        {
            let bounds = self
                .resting_transform(object.item.handle)?
                .bounds(local_bounds(&object.item)?);
            self.content = Some(self.content.map_or(bounds, |content| content.union(bounds)));
        }
        Ok(())
    }
}

pub(super) fn local_bounds(item: &Item) -> Result<Rect, PrepareError> {
    let mut bounds = item.bounds;
    for primitive in &*item.primitives {
        match primitive {
            Prim::Rect { rect, .. } => bounds = bounds.union(*rect),
            Prim::Line { points, .. } | Prim::Fill { points, .. } => {
                bounds = bounds.union(super::primitives::point_bounds(points)?);
            }
            Prim::Text { .. } => {}
        }
    }
    Ok(bounds)
}
