//! World-extent bounds for solids down their attachment chains.
//!
//! The bound sums translation lengths, so it ignores cancellation between links and is
//! stricter than the coordinate cube; rotations preserve lengths and need no envelope.

use super::object::{Kind, Object};
use crate::{
    ErrorCode, Handle, Op, Step, WireId,
    limits::MAX_COORD,
    record::Track,
    theme::Theme,
    three::{
        mesh::MeshScratch,
        size::MeshError,
        view::{Ends, Mesh, SolidInput, ViewInput, ViewUpdate, tessellate_view},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Clone, Copy)]
struct Placement {
    origin: f64,
    scale: f64,
}

/// Checks every solid in `after`, using the largest `pos3` envelope any charged version holds.
pub(super) fn bounds<'a>(
    after: &BTreeMap<WireId, Object>,
    versions: impl IntoIterator<Item = (&'a WireId, &'a Object)>,
) -> Result<(), (Handle, ErrorCode)> {
    let mut offsets = BTreeMap::<&WireId, f64>::new();
    for (id, object) in versions {
        if let Some(motion) = object.work.motion.get("pos3") {
            let entry = offsets.entry(id).or_default();
            *entry = entry.max(f64::from(motion.max[0]));
        }
    }
    let mut memo = BTreeMap::new();
    for (id, object) in after {
        if object.kind == Kind::Solid {
            placement(id, after, &offsets, &mut memo).map_err(|code| (object.handle, code))?;
        }
    }
    Ok(())
}

fn placement(
    id: &WireId,
    after: &BTreeMap<WireId, Object>,
    offsets: &BTreeMap<&WireId, f64>,
    memo: &mut BTreeMap<WireId, Placement>,
) -> Result<Placement, ErrorCode> {
    if let Some(known) = memo.get(id) {
        return Ok(*known);
    }
    let object = &after[id];
    let solid = object.work.solid.expect("solids carry placement work");
    let parent = match object
        .references
        .iter()
        .find(|reference| Some(*reference) != object.parent.as_ref())
    {
        Some(attach) => placement(attach, after, offsets, memo)?,
        None => Placement {
            origin: 0.0,
            scale: 1.0,
        },
    };
    let offset = solid.pos + offsets.get(id).copied().unwrap_or_default();
    let value = check(parent, offset, solid.scale, solid.reach)?;
    memo.insert(id.clone(), value);
    Ok(value)
}

fn check(parent: Placement, offset: f64, scale: f64, reach: f64) -> Result<Placement, ErrorCode> {
    let origin = parent.origin + parent.scale * offset;
    let scale = parent.scale * scale;
    let extent = origin + scale * reach;
    if !(origin.is_finite() && scale.is_finite() && extent.is_finite()) {
        return Err(ErrorCode::TransformOverflow);
    }
    if extent > f64::from(MAX_COORD) {
        return Err(ErrorCode::OutOfRange);
    }
    Ok(Placement { origin, scale })
}

/// The views a step puts into or removes from, each composed from `after` or marked removed.
/// Colour sets change no input: preparation applies them from the step.
pub(super) fn view_updates(
    step: &Step,
    before: &BTreeMap<WireId, Object>,
    after: &BTreeMap<WireId, Object>,
) -> Vec<ViewUpdate> {
    let view_of = |object: &Object, id: &WireId| match object.kind {
        Kind::View3d => Some(id.clone()),
        Kind::Solid => object.parent.clone(),
        _ => None,
    };
    let mut views = BTreeSet::new();
    for op in &*step.put {
        match op {
            Op::View3d(view) => _ = views.insert(view.id.clone()),
            Op::Solid(solid) => _ = views.insert(solid.parent.clone()),
            _ => {}
        }
    }
    for id in &*step.remove {
        views.extend(before.get(id).and_then(|object| view_of(object, id)));
    }
    views
        .into_iter()
        .map(|id| ViewUpdate {
            input: after
                .get(&id)
                .map(|view| Arc::new(view_input(&id, view, after))),
            id,
        })
        .collect()
}

/// Builds each view this step changes or reposes at its candidate rest with the shell's own
/// tessellation, and names the first solid whose world mesh fails there, so a step the shell
/// would prepare empty is refused before commit.
pub(super) fn degenerate(
    step: &Step,
    views: &[ViewUpdate],
    after: &BTreeMap<WireId, Object>,
) -> Option<Handle> {
    let mut changed: BTreeMap<&WireId, Option<&Arc<ViewInput>>> = views
        .iter()
        .filter_map(|update| update.input.as_ref().map(|input| (&update.id, Some(input))))
        .collect();
    for op in &*step.put {
        if let Op::Anim(anim) = op
            && matches!(anim.track, Track::Pos3 | Track::Rot3)
            && let Some(view) = after.get(&anim.target).and_then(|o| o.parent.as_ref())
        {
            changed.entry(view).or_insert(None);
        }
    }
    let mut scratch = MeshScratch::new();
    for (id, input) in changed {
        let input = input.map_or_else(|| Arc::new(view_input(id, &after[id], after)), Arc::clone);
        let ends: HashMap<&WireId, Ends> = after
            .iter()
            .filter(|(_, object)| object.parent.as_ref() == Some(id))
            .filter_map(|(id, object)| {
                let [pos3, rot3] = object.work.solid?.end;
                Some((id, Ends { pos3, rot3 }))
            })
            .collect();
        let built = tessellate_view(
            &input,
            None,
            &HashMap::new(),
            &ends,
            &Theme::default(),
            &AtomicBool::new(false),
            &mut scratch,
        );
        let Ok(geometry) = built else {
            return Some(input.handle);
        };
        if let Some(solid) = geometry
            .solids
            .iter()
            .find(|solid| matches!(solid.mesh, Mesh::Failed(MeshError::Degenerate(_))))
        {
            return Some(solid.handle);
        }
    }
    None
}

fn view_input(id: &WireId, view: &Object, after: &BTreeMap<WireId, Object>) -> ViewInput {
    let mut members: Vec<(&WireId, &Object)> = after
        .iter()
        .filter(|(_, object)| object.kind == Kind::Solid && object.parent.as_ref() == Some(id))
        .collect();
    members.sort_by_key(|(_, object)| object.handle);
    let mut solids: Vec<SolidInput> = Vec::with_capacity(members.len());
    for (_, object) in members {
        let source = object.three.as_ref().expect("solids carry their source");
        let Op::Solid(record) = &**source else {
            unreachable!("a solid's source is its solid record");
        };
        // Members run in handle order, so a parent's slot is a binary search away.
        let parent = record.attach.as_ref().map(|attach| {
            solids
                .binary_search_by_key(&after[attach].handle, |solid| solid.handle)
                .expect("an attachment names an earlier solid in the same view")
        });
        solids.push(SolidInput {
            handle: object.handle,
            source: Arc::clone(source),
            parent: parent.map(|p| u16::try_from(p).expect("a view holds at most 160 solids")),
        });
    }
    ViewInput {
        handle: view.handle,
        source: Arc::clone(view.three.as_ref().expect("views carry their source")),
        solids: solids.into_boxed_slice(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Placement = Placement {
        origin: 0.0,
        scale: 1.0,
    };

    #[test]
    fn a_chain_link_refuses_overflow_and_the_coordinate_limit_exactly() {
        let limit = f64::from(MAX_COORD);
        let at = check(ROOT, limit - 2.0, 1.0, 2.0).expect("exactly at the limit");
        assert_eq!((at.origin, at.scale), (limit - 2.0, 1.0));
        assert_eq!(
            check(ROOT, limit.next_up(), 1.0, 0.0).map(|_| ()),
            Err(ErrorCode::OutOfRange)
        );
        let huge = Placement {
            origin: 0.0,
            scale: f64::MAX,
        };
        assert_eq!(
            check(huge, 0.0, 2.0, 0.0).map(|_| ()),
            Err(ErrorCode::TransformOverflow)
        );
        assert_eq!(
            check(ROOT, f64::NAN, 1.0, 0.0).map(|_| ()),
            Err(ErrorCode::TransformOverflow)
        );
        let child = check(
            Placement {
                origin: 100.0,
                scale: 2.0,
            },
            10.0,
            3.0,
            1.0,
        )
        .expect("small chain");
        assert_eq!((child.origin, child.scale), (120.0, 6.0));
    }
}
