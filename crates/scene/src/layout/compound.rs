//! Nested diagram clusters: membership, endpoint ancestry and inherited direction.
//!
//! A diagram without a `box` inside another diagram is one member of its parent's ranking,
//! placed by its own transform. Boxed diagrams keep their explicit placement.

use super::{
    SurfaceGeometry,
    state::{LayoutState, Placed},
};
use crate::{
    Handle, Op, WireId,
    display::Prim,
    geom::{Rect, Vec2},
    limits::MAX_OBJECT_DEPTH,
    mermaid::Extra,
    record::Direction,
};
use std::{collections::BTreeMap, sync::Arc};

/// Space between a cluster's contents and the rectangle its parent ranks.
pub(super) const PAD: f32 = 16.0;

fn get(state: &LayoutState, handle: Handle) -> Option<&Placed> {
    state
        .handles
        .get(&handle)
        .and_then(|id| state.objects.get(id))
}

/// Whether the parent diagram's ranking places this object.
pub(super) fn is_cluster(state: &LayoutState, object: &Placed) -> bool {
    matches!(&*object.source, Op::Diagram(diagram) if diagram.bounds.is_none())
        && object
            .item
            .parent
            .and_then(|parent| get(state, parent))
            .is_some_and(|parent| matches!(&*parent.source, Op::Diagram(_)))
}

/// The nearest declared direction among diagram ancestors, else the surface default, and
/// whether a compiled flow runs it backwards.
pub(super) fn direction(
    state: &LayoutState,
    mut group: Option<Handle>,
    surface: SurfaceGeometry,
) -> (Direction, bool) {
    for _ in 0..=MAX_OBJECT_DEPTH {
        let Some(object) = group.and_then(|handle| get(state, handle)) else {
            break;
        };
        let Op::Diagram(diagram) = &*object.source else {
            break;
        };
        if let Some(dir) = diagram.dir {
            let reverse = matches!(
                state.extra(object.item.handle),
                Some(Extra::Group { reverse: true, .. })
            );
            return (dir, reverse);
        }
        group = object.item.parent;
    }
    let dir = match surface {
        SurfaceGeometry::Inline { .. } => Direction::Down,
        SurfaceGeometry::Overlay { .. } => Direction::Right,
    };
    (dir, false)
}

/// Number of ancestors, so deeper groups are placed before the groups that rank them.
pub(super) fn depth(state: &LayoutState, mut group: Option<Handle>) -> usize {
    let mut depth = 0;
    while depth <= MAX_OBJECT_DEPTH
        && let Some(handle) = group
    {
        group = get(state, handle).and_then(|object| object.item.parent);
        depth += 1;
    }
    depth
}

/// Each member holding `id` with the group it sits in, from `id` up to its root and bounded
/// by the depth limit; `parents` covers nodes not yet placed.
pub(super) fn ancestry(
    state: &LayoutState,
    parents: &BTreeMap<WireId, (Handle, Option<Handle>)>,
    id: &WireId,
) -> Vec<(Handle, Option<Handle>)> {
    let mut chain = Vec::new();
    let mut current = parents.get(id).copied().or_else(|| {
        state
            .objects
            .get(id)
            .map(|object| (object.item.handle, object.item.parent))
    });
    while chain.len() <= MAX_OBJECT_DEPTH
        && let Some((handle, parent)) = current
    {
        chain.push((handle, parent));
        current = parent
            .and_then(|parent| get(state, parent))
            .map(|object| (object.item.handle, object.item.parent));
    }
    chain
}

/// Whether `id` is strictly below `group` without crossing a non-cluster diagram.
pub(super) fn inside(state: &LayoutState, id: &WireId, group: Handle) -> bool {
    let mut current = state.objects.get(id);
    for _ in 0..=MAX_OBJECT_DEPTH {
        let Some(object) = current else {
            return false;
        };
        if matches!(&*object.source, Op::Diagram(_)) && !is_cluster(state, object) {
            return false;
        }
        if object.item.parent == Some(group) {
            return true;
        }
        current = object.item.parent.and_then(|parent| get(state, parent));
    }
    false
}

/// The rectangle a member occupies in its parent's space: a node's bounds, or a cluster's
/// transformed contents with padding.
pub(super) fn member_rect(object: &Placed) -> Rect {
    if !matches!(&*object.source, Op::Diagram(_)) {
        return object.item.bounds;
    }
    let contents = object.item.transform.bounds(object.item.bounds);
    Rect {
        origin: contents.origin - Vec2::new(PAD, PAD),
        size: contents.size + Vec2::new(2.0 * PAD, 2.0 * PAD),
    }
}

/// A titled cluster's rectangle around its children's `contents`, and its title's origin: a band
/// as tall as the title plus the padding sits above the contents, and the rectangle widens to fit
/// a wider title. It is computed from the contents alone, so recomputing never adds a second band.
pub(super) fn banded(contents: Rect, title: Vec2) -> (Rect, Vec2) {
    let band = title.y + PAD;
    let width = contents.size.x.max(title.x);
    let bounds = Rect {
        origin: Vec2::new(contents.center().x - width * 0.5, contents.origin.y - band),
        size: Vec2::new(width, contents.size.y + band),
    };
    (
        bounds,
        Vec2::new(bounds.center().x - title.x * 0.5, bounds.origin.y),
    )
}

/// A group's primitives with its title moved to `origin`.
pub(super) fn retitle(primitives: &[Prim], origin: Vec2) -> Arc<[Prim]> {
    primitives
        .iter()
        .map(|primitive| match primitive {
            Prim::Text {
                text,
                style,
                color,
                max_width,
                align,
                ..
            } => Prim::Text {
                origin,
                text: text.clone(),
                style: *style,
                color: *color,
                max_width: *max_width,
                align: *align,
            },
            other => other.clone(),
        })
        .collect()
}

/// Union of a diagram's children in its own space, as the final bounds pass computes it when
/// they come in id order.
pub(super) fn contents<'a>(children: impl IntoIterator<Item = &'a Placed>) -> Option<Rect> {
    children
        .into_iter()
        .map(|object| {
            object
                .item
                .transform
                .compose(object.rest.transform())
                .bounds(object.item.bounds)
        })
        .reduce(Rect::union)
}
