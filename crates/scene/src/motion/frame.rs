//! Allocation-free sampling of an activated scene into its reserved frame.
use super::{Frame, FrameItem, HIDDEN, SceneView, Value, ViewFrame, World, effect, fraction, rgb};
use crate::{display::Paint, geom::Affine, layout::PrepareError, record::Track};
use std::sync::Arc;

impl SceneView {
    /// Samples every object at `now` into `out` without allocating, freeing or locking.
    ///
    /// A fresh or cleared frame binds to this view's current generation. Reusing a bound frame
    /// drops only references this view's objects still hold, so nothing is freed here.
    /// # Errors
    /// Returns `Stale`, leaving `out` untouched, when `out` is bound to an earlier view or
    /// generation; clear it outside paint first.
    pub fn frame(&self, now: u64, out: &mut Frame) -> Result<(), PrepareError> {
        let bound = out
            .generation
            .get_or_insert_with(|| Arc::clone(&self.generation));
        if !Arc::ptr_eq(bound, &self.generation) {
            return Err(PrepareError::Stale);
        }
        out.items.clear();
        out.world.clear();
        let settled = now >= self.deadline;
        for node in &self.nodes {
            let parent = node.parent.map(|parent| out.world[parent]);
            let timing = node.timing;
            if parent.is_some_and(|parent| !parent.visible) || (!settled && now < timing.start) {
                out.world.push(HIDDEN);
                continue;
            }
            let progress = if settled || now >= timing.end {
                1.0
            } else {
                fraction(now - timing.start, timing.end - timing.start)
            };
            let entering = effect(timing.enter, progress);
            let mut pose = node.rest;
            if !settled {
                for owner in node.owners.iter().flatten() {
                    let live = &self.tracks[*owner];
                    match (live.track, live.sample(now)) {
                        (Track::X, Value::Num(value)) => pose.x = value,
                        (Track::Y, Value::Num(value)) => pose.y = value,
                        (Track::Pos, Value::V2(value)) => pose.pos = value,
                        (Track::Scale, Value::Num(value)) => pose.scale = value,
                        (Track::Rotate, Value::Num(value)) => pose.rotate = value,
                        (Track::Opacity, Value::Num(value)) => {
                            pose.opacity = Some(value.clamp(0.0, 1.0));
                        }
                        (Track::Draw, Value::Num(value)) => pose.draw = Some(value.clamp(0.0, 1.0)),
                        (Track::Color, Value::Color(value)) => pose.color = Some(rgb(value)),
                        _ => {}
                    }
                }
            }
            let (pivot, scale) = (node.pivot, entering.scale);
            let entrance = Affine([
                scale,
                0.0,
                0.0,
                scale,
                pivot.x - scale * pivot.x,
                pivot.y - scale * pivot.y + entering.offset,
            ]);
            out.world.push(World {
                transform: parent
                    .map_or_else(Affine::default, |parent| parent.transform)
                    .compose(node.transform)
                    .compose(pose.transform())
                    .compose(entrance),
                opacity: parent.map_or(1.0, |parent| parent.opacity)
                    * node.inherited
                    * pose.opacity.unwrap_or(node.opacity)
                    * entering.alpha,
                draw: pose.draw.unwrap_or(1.0) * entering.draw,
                color: pose.color.map(Paint::from),
                flow: if timing.flow == 0 || settled {
                    0.0
                } else {
                    fraction(
                        (now - timing.start) % u64::from(timing.flow),
                        u64::from(timing.flow),
                    )
                },
                visible: true,
            });
        }
        if let Some(motion) = &self.view {
            let spare = &mut out.poses;
            let slot = out.view.get_or_insert_with(|| ViewFrame {
                geometry: Arc::clone(&motion.geometry),
                camera: None,
                poses: std::mem::take(spare),
            });
            motion.sample(&self.tracks, now, self.deadline, slot);
        }
        for &index in &self.paint {
            let world = out.world[index];
            if !world.visible {
                continue;
            }
            let node = &self.nodes[index];
            out.items.push(FrameItem {
                handle: node.handle,
                primitives: Arc::clone(&node.primitives),
                label_start: node.label_start,
                transform: world.transform,
                opacity: world.opacity,
                draw: world.draw,
                color: world.color,
                flow: world.flow,
            });
        }
        Ok(())
    }
}
