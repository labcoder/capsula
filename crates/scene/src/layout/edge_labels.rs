use super::{
    LayoutState, PrepareError,
    primitives::{Build, point_bounds},
};
use crate::{
    Op,
    display::Prim,
    geom::{Rect, Vec2},
};
use std::sync::Arc;

/// Quarter-segment candidates can occupy narrow gaps without changing the drawn curve.
const SUBDIVISIONS: usize = 4;

pub(super) fn separate(state: &mut LayoutState, build: &Build<'_>) -> Result<(), PrepareError> {
    let ids: Vec<_> = state
        .objects
        .iter()
        .filter(|(_, object)| matches!(&*object.source, Op::Edge(edge) if edge.label.is_some()))
        .map(|(id, _)| id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut occupied = Vec::<Rect>::new();
    for object in state.objects.values() {
        let ink = if matches!(&*object.source, Op::Node(_)) {
            Some(object.item.bounds)
        } else {
            object.title.and_then(|size| {
                object.item.primitives.iter().find_map(|primitive| {
                    if let Prim::Text {
                        origin, max_width, ..
                    } = primitive
                    {
                        Some(Rect {
                            origin: *origin
                                + Vec2::new((max_width.unwrap_or(size.x) - size.x) * 0.5, 0.0)
                                - Vec2::new(4.0, 2.0),
                            size: size + Vec2::new(8.0, 4.0),
                        })
                    } else {
                        None
                    }
                })
            })
        };
        if let Some(ink) = ink {
            occupied.push(state.resting_transform(object.item.handle)?.bounds(ink));
        }
    }
    for id in ids {
        build.check_cancel()?;
        let object = &state.objects[&id];
        let Some(old) = object
            .item
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                Prim::Rect { rect, .. } => Some(*rect),
                _ => None,
            })
        else {
            continue;
        };
        let Prim::Line { points, .. } = &object.item.primitives[0] else {
            unreachable!("edge starts with its curve");
        };
        let world = state.resting_transform(object.item.handle)?;
        let curve_middle = points.len() / 2;
        let middle = curve_middle * SUBDIVISIONS;
        let candidates = std::iter::once(middle)
            .chain((1..middle).flat_map(|offset| [middle - offset, middle + offset]));
        // shortcut: interior curve samples can run out of room; keep the midpoint label until post-D1 routing can make space.
        let chip = candidates
            .map(|index| {
                let segment = index / SUBDIVISIONS;
                let t = [0.0, 0.25, 0.5, 0.75][index % SUBDIVISIONS];
                Rect::centered(
                    points[segment] * (1.0 - t) + points[segment + 1] * t,
                    old.size,
                )
            })
            .find(|candidate| {
                let bounds = world.bounds(*candidate);
                candidate.within_canvas()
                    && bounds.within_canvas()
                    && occupied.iter().all(|other| !bounds.intersects(*other))
            })
            .unwrap_or_else(|| Rect::centered(points[curve_middle], old.size));
        occupied.push(world.bounds(chip));
        let mut bounds = point_bounds(points)?.union(chip);
        for primitive in &*object.item.primitives {
            if let Prim::Fill { points, .. } = primitive {
                bounds = bounds.union(point_bounds(points)?);
            }
        }
        if chip == old && bounds == object.item.bounds {
            continue;
        }
        let object = state.objects.get_mut(&id).expect("labelled edge exists");
        let delta = chip.origin - old.origin;
        if chip != old {
            for primitive in Arc::make_mut(&mut object.item.primitives) {
                match primitive {
                    Prim::Rect { rect, .. } => *rect = chip,
                    Prim::Text { origin, .. } => *origin = *origin + delta,
                    _ => {}
                }
            }
        }
        object.item.bounds = bounds;
    }
    Ok(())
}
