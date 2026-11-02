use super::{
    Glide, LayoutState, PrepareError, PrepareInput, PreparedStep, RecoveryError, SurfaceGeometry,
    nodes,
    primitives::{Build, layout_error, rect},
    state::{Placed, Rest},
};
use crate::{
    Begin, ErrorCode, Handle, Op, SceneEvent, SceneState, WireId,
    display::{DisplayList, Item, Prim, TextMeasure},
    geom::{Affine, Rect, Vec2},
    record::{Anchor, Change, Track},
    state::{object, work},
    theme::Theme,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    mem::size_of,
    sync::{Arc, atomic::AtomicBool},
};

/// Builds one immutable picture update from its immediately preceding prepared state.
/// # Errors
/// Rejects cancellation, stale inputs, text shaping failures and unsafe generated work.
/// # Panics
/// Panics only if an opaque commit ticket violates admission's checked invariants.
#[expect(
    clippy::too_many_lines,
    reason = "keep the ordered preparation phases and atomic publication together"
)]
pub fn prepare(
    input: PrepareInput,
    measure: &mut dyn TextMeasure,
    cancel: &AtomicBool,
) -> Result<PreparedStep, PrepareError> {
    let PrepareInput {
        ticket,
        before,
        surface,
        theme,
    } = input;
    let viewport = surface.viewport(ticket.canvas_size)?;
    let mut build = Build {
        viewport,
        theme: &theme,
        measure,
        cancel,
        scales: &ticket.path_scales,
    };
    build.check_cancel()?;
    let mut after = before.clone();
    let mut sources = BTreeMap::<WireId, (Handle, Arc<Op>)>::new();
    let mut order = Vec::new();
    for (slot, op) in ticket.step.put.iter().enumerate() {
        if let Some(visual) = object::visual(op) {
            let handle = Handle::new(ticket.revision, slot)?;
            sources.insert(visual.id.clone(), (handle, Arc::new(op.clone())));
            order.push(visual.id.clone());
        }
    }
    for change in &*ticket.step.set {
        let (handle, source) = if let Some((handle, source)) = sources.get(&change.target) {
            (*handle, Arc::clone(source))
        } else {
            let previous = after
                .objects
                .get(&change.target)
                .ok_or(PrepareError::Stale)?;
            (previous.item.handle, Arc::clone(&previous.source))
        };
        if !sources.contains_key(&change.target) {
            order.push(change.target.clone());
        }
        sources.insert(
            change.target.clone(),
            (handle, Arc::new(patch(&source, change))),
        );
    }
    for handle in ticket.path_scales.keys() {
        if let Some(id) = after.handles.get(handle)
            && !sources.contains_key(id)
        {
            let object = &after.objects[id];
            sources.insert(id.clone(), (*handle, Arc::clone(&object.source)));
            order.push(id.clone());
        }
    }
    if sources.values().any(|(_, op)| {
        matches!(
            &**op,
            Op::Table(_) | Op::Row(_) | Op::Chart(_) | Op::Series(_)
        )
    }) {
        return Err(layout_error(ErrorCode::Unsupported));
    }
    let mut removed = Vec::new();
    for id in &*ticket.step.remove {
        if let Some(old) = after.objects.remove(id) {
            after.handles.remove(&old.item.handle);
            removed.push(old.item.handle);
        } else if let Some((handle, _)) = sources.get(id) {
            removed.push(*handle);
        } else {
            return Err(PrepareError::Stale);
        }
        sources.remove(id);
    }
    order.retain(|id| sources.contains_key(id));
    let mut rests: BTreeMap<_, _> = after
        .objects
        .iter()
        .map(|(id, object)| (id.clone(), object.rest))
        .collect();
    let mut motions = BTreeMap::new();
    let mut explicit_pos = BTreeSet::new();
    for (slot, op) in ticket.step.put.iter().enumerate() {
        if let Op::Anim(anim) = op {
            let value = &anim.keys.last().expect("admission checked nonempty keys").1;
            rests
                .entry(anim.target.clone())
                .or_insert_with(Rest::default)
                .set(anim.track, value);
            if anim.track == Track::Pos {
                explicit_pos.insert(anim.target.clone());
            }
            let motion = work::motion(anim, slot, &ticket.step)?;
            motions
                .entry((anim.target.clone(), work::property(anim.track)))
                .and_modify(|previous: &mut work::Motion| *previous = previous.peak(motion))
                .or_insert(motion);
        }
    }
    for (id, object) in &mut after.objects {
        if let Some(rest) = rests.get(id) {
            object.rest = *rest;
        }
    }
    let mut containers: Vec<_> = order
        .iter()
        .filter(|id| matches!(&*sources[*id].1, Op::Diagram(_)))
        .cloned()
        .collect();
    while !containers.is_empty() {
        let Some(index) = containers
            .iter()
            .position(|id| after.objects.contains_key(id) || ready(&sources[id].1, &after))
        else {
            return Err(layout_error(ErrorCode::UnknownRef));
        };
        let id = containers.remove(index);
        let (handle, source) = &sources[&id];
        let Op::Diagram(diagram) = &**source else {
            unreachable!("container list contains diagrams");
        };
        let previous = after.objects.get(&id).cloned();
        let parent = parent(source, &after, previous.as_ref())?;
        let references = previous.as_ref().map_or_else(
            || object::reference_ids(source),
            |old| old.references.clone(),
        );
        let object = Placed {
            item: Item {
                handle: *handle,
                parent,
                primitives: Arc::from([]),
                bounds: diagram.bounds.map_or(viewport, rect),
                transform: previous
                    .as_ref()
                    .map_or_else(Affine::default, |old| old.item.transform),
                opacity: diagram.opacity.map_or(1.0, crate::Finite::get),
            },
            source: Arc::clone(source),
            anchors: [Vec2::default(); 2],
            rest: rests.get(&id).copied().unwrap_or_default(),
            rank: None,
            initialized: previous.as_ref().is_some_and(|old| old.initialized),
            motion: previous.map_or_else(BTreeMap::new, |old| old.motion),
            references,
        };
        after.handles.insert(*handle, id.clone());
        after.objects.insert(id, object);
    }
    let mut glides = nodes::place(&sources, &order, &mut after, &mut build, surface)?;
    glides.retain(|glide| {
        after
            .handles
            .get(&glide.target)
            .is_some_and(|id| !explicit_pos.contains(id))
    });
    let mut pending: Vec<_> = order
        .iter()
        .filter(|id| !matches!(&*sources[*id].1, Op::Diagram(_) | Op::Node(_)))
        .cloned()
        .collect();
    while !pending.is_empty() {
        build.check_cancel()?;
        let Some(index) = pending
            .iter()
            .position(|id| after.objects.contains_key(id) || ready(&sources[id].1, &after))
        else {
            return Err(layout_error(ErrorCode::UnknownRef));
        };
        let id = pending.remove(index);
        let (handle, source) = &sources[&id];
        let previous = after.objects.get(&id).cloned();
        let parent = parent(source, &after, previous.as_ref())?;
        let mut object = if matches!(&**source, Op::Edge(_)) {
            nodes::edge(
                source,
                *handle,
                parent,
                &after,
                &mut build,
                previous.as_ref(),
            )?
        } else {
            build.primitive(source, *handle, parent, &after, previous.as_ref())?
        };
        object.rest = rests.get(&id).copied().unwrap_or_default();
        after.handles.insert(*handle, id.clone());
        after.objects.insert(id, object);
    }
    for (id, object) in &mut after.objects {
        if let Some(rest) = rests.get(id) {
            object.rest = *rest;
        }
    }
    for ((id, property), motion) in motions {
        after
            .objects
            .get_mut(&id)
            .ok_or(PrepareError::Stale)?
            .motion
            .insert(property, motion);
    }
    for Glide { target, to } in &glides {
        let id = after.handles.get(target).ok_or(PrepareError::Stale)?;
        let group = after.objects.get_mut(id).ok_or(PrepareError::Stale)?;
        let old = group.rest.pos;
        group.rest.pos = *to;
        group.motion.insert(
            "pos",
            work::Motion {
                keys: 2,
                min: [old.x.min(to.x), old.y.min(to.y)],
                max: [old.x.max(to.x), old.y.max(to.y)],
            },
        );
    }
    update_group_bounds(&mut after);
    validate_bounds(&after)?;
    let mut items: Vec<_> = after
        .objects
        .iter()
        .filter(|(id, object)| {
            before.objects.get(*id).is_none_or(|old| {
                !Arc::ptr_eq(&old.item.primitives, &object.item.primitives)
                    || old.item.bounds != object.item.bounds
                    || old.item.parent != object.item.parent
            })
        })
        .map(|(_, object)| object.item.clone())
        .collect();
    items.sort_by_key(|item| item.handle);
    let resident_bytes = residency(&items, &before)
        + after.objects.len()
            * (size_of::<Placed>() + size_of::<WireId>() + size_of::<usize>() * 3);
    if resident_bytes > crate::limits::MAX_RESIDENT_BYTES {
        return Err(PrepareError::OverResidency);
    }
    after.revision = ticket.revision;
    after.version = Arc::new(());
    build.check_cancel()?;
    Ok(PreparedStep {
        index: ticket.revision - 1,
        display: DisplayList { items },
        removed,
        glides,
        after,
        resident_bytes,
        ticket,
        base: Arc::clone(&before.version),
    })
}

/// Rebuilds semantic state and layout together, skipping preparations later cuts drop.
/// # Errors
/// A failed kept preparation identifies its revision; invalid semantic input uses zero.
pub fn recover(
    header: Begin,
    log: &[SceneEvent],
    surface: SurfaceGeometry,
    theme: &Arc<Theme>,
    measure: &mut dyn TextMeasure,
    cancel: &AtomicBool,
) -> Result<(SceneState, LayoutState), RecoveryError> {
    if log.len() > crate::limits::MAX_STEPS * 2 {
        return Err(crate::StepError::new(ErrorCode::TooLong).into());
    }
    let mut skipped = BTreeSet::new();
    let mut next_cut = None;
    for event in log.iter().rev() {
        match event {
            SceneEvent::Stop(cut) => next_cut = Some(cut.activated),
            SceneEvent::Step { revision, .. }
                if next_cut.is_some_and(|activated| *revision > activated) =>
            {
                skipped.insert(*revision);
            }
            SceneEvent::Step { .. } => {}
        }
    }
    let mut layout = None;
    let scene = SceneState::replay_observe(
        header,
        log,
        |scene, event, committed| -> Result<(), RecoveryError> {
            let before = layout.get_or_insert_with(|| LayoutState::new(scene));
            match event {
                SceneEvent::Step { revision, .. } if !skipped.contains(revision) => {
                    let result = (|| {
                        let committed = committed.ok_or(PrepareError::Stale)?;
                        let input = PrepareInput::new(
                            committed.preparation,
                            before.clone(),
                            surface,
                            Arc::clone(theme),
                        )?;
                        let prepared = prepare(input, measure, cancel)?;
                        prepared.validate(scene, before)?;
                        Ok::<_, PrepareError>(prepared.after)
                    })();
                    *before = result.map_err(|error| RecoveryError {
                        revision: *revision,
                        error,
                    })?;
                }
                SceneEvent::Stop(_) => {
                    *before = before.cut(scene).map_err(|error| RecoveryError {
                        revision: scene.revision(),
                        error,
                    })?;
                }
                SceneEvent::Step { .. } => {}
            }
            Ok(())
        },
    )?;
    let layout = layout.unwrap_or_else(|| LayoutState::new(&scene));
    Ok((scene, layout))
}

fn parent(
    source: &Op,
    state: &LayoutState,
    previous: Option<&Placed>,
) -> Result<Option<Handle>, PrepareError> {
    if let Some(old) = previous {
        return Ok(old.item.parent);
    }
    object::visual(source)
        .and_then(|visual| visual.parent)
        .map(|id| {
            state
                .handle(id)
                .ok_or_else(|| layout_error(ErrorCode::UnknownRef))
        })
        .transpose()
}
fn ready(source: &Op, state: &LayoutState) -> bool {
    let has = |anchor: &Anchor| match anchor {
        Anchor::At(_) => true,
        Anchor::Ref(id) => state.objects.contains_key(id),
    };
    let attached = object::visual(source)
        .and_then(|visual| visual.parent)
        .is_none_or(|parent| state.objects.contains_key(parent));
    attached
        && match source {
            Op::Line(line) | Op::Arrow(line) => has(&line.from) && has(&line.to),
            Op::Path(path) => path.at.as_ref().is_none_or(has),
            Op::Shape(shape) => shape.at.as_ref().is_none_or(has),
            Op::Text(text) => has(&text.at),
            Op::Callout(callout) => has(&callout.target) && callout.at.as_ref().is_none_or(has),
            Op::Highlight(highlight) => has(&highlight.target),
            Op::Edge(edge) => {
                state.objects.contains_key(&edge.from) && state.objects.contains_key(&edge.to)
            }
            _ => true,
        }
}

fn patch(source: &Op, change: &Change) -> Op {
    let mut source = source.clone();
    match &mut source {
        Op::Text(text) => {
            if let Some(value) = &change.text {
                text.text = value.clone();
            }
        }
        Op::Callout(callout) => {
            if let Some(value) = &change.text {
                callout.text = value.clone();
            }
        }
        Op::Shape(shape) => {
            if let Some(value) = &change.label {
                shape.label = Some(value.clone());
            }
        }
        Op::Node(node) => {
            if let Some(value) = &change.label {
                node.label = value.clone();
            }
        }
        Op::Edge(edge) => {
            if let Some(value) = &change.label {
                edge.label = Some(value.clone());
            }
        }
        _ => {}
    }
    if let Some(color) = &change.color {
        macro_rules! color {
            ($value:expr) => {
                $value.color = Some(color.clone())
            };
        }
        match &mut source {
            Op::Line(value) | Op::Arrow(value) => color!(value),
            Op::Path(value) => color!(value),
            Op::Shape(value) => color!(value),
            Op::Text(value) => color!(value),
            Op::Callout(value) => color!(value),
            Op::Highlight(value) => color!(value),
            Op::Diagram(value) => color!(value),
            Op::Node(value) => color!(value),
            Op::Edge(value) => color!(value),
            _ => {}
        }
    }
    source
}

fn update_group_bounds(state: &mut LayoutState) {
    for _ in 0..crate::limits::MAX_OBJECT_DEPTH {
        let mut changed = false;
        let bounds: BTreeMap<_, _> = state
            .objects
            .values()
            .filter_map(|object| {
                Some((
                    object.item.parent?,
                    object
                        .item
                        .transform
                        .compose(object.rest.transform())
                        .bounds(object.item.bounds),
                ))
            })
            .fold(BTreeMap::new(), |mut bounds, (parent, rect)| {
                bounds
                    .entry(parent)
                    .and_modify(|old: &mut Rect| *old = old.union(rect))
                    .or_insert(rect);
                bounds
            });
        for object in state
            .objects
            .values_mut()
            .filter(|object| matches!(&*object.source, Op::Diagram(_)))
        {
            if let Some(bounds) = bounds.get(&object.item.handle)
                && *bounds != object.item.bounds
            {
                object.item.bounds = *bounds;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn validate_bounds(state: &LayoutState) -> Result<(), PrepareError> {
    let mut objects = BTreeMap::new();
    for (id, placed) in &state.objects {
        let parent = placed
            .item
            .parent
            .and_then(|handle| state.handles.get(&handle))
            .cloned();
        let matrix = placed.item.transform.0;
        let scale = matrix[0].hypot(matrix[1]);
        let mut bounds = placed.item.bounds;
        for primitive in &*placed.item.primitives {
            match primitive {
                Prim::Rect { rect, .. } => bounds = bounds.union(*rect),
                Prim::Line { points, .. } | Prim::Fill { points, .. } => {
                    bounds = bounds.union(super::primitives::point_bounds(points)?);
                }
                Prim::Text { .. } => {}
            }
        }
        let extent = if matches!(&*placed.source, Op::Diagram(_)) {
            0.0
        } else {
            bounds
                .origin
                .x
                .abs()
                .max(bounds.origin.y.abs())
                .max(bounds.max().x.abs())
                .max(bounds.max().y.abs())
        };
        let work = work::Work {
            extent,
            scale: 1.0,
            inherited_scale: scale,
            inherited_min_scale: scale,
            inherited_translation: matrix[4].abs().max(matrix[5].abs()),
            inherited_rotation: matrix[1] != 0.0 || matrix[2] != 0.0,
            motion: placed.motion.clone(),
            ..work::Work::default()
        };
        objects.insert(
            id.clone(),
            object::Object {
                handle: placed.item.handle,
                kind: object::visual(&placed.source)
                    .expect("placed objects have visual sources")
                    .kind,
                references: parent.iter().cloned().collect(),
                parent,
                work,
            },
        );
    }
    crate::state::metrics::validate_transforms(&objects)?;
    for object in state
        .objects
        .values()
        .filter(|object| !object.item.primitives.is_empty())
    {
        state.world_bounds(object)?;
    }
    Ok(())
}

fn residency(items: &[Item], before: &LayoutState) -> usize {
    let mut shared_arrays = HashSet::new();
    let mut vertices = HashSet::new();
    for object in before.objects.values() {
        shared_arrays.insert(object.item.primitives.as_ptr());
        for primitive in &*object.item.primitives {
            if let Prim::Fill { points, .. } | Prim::Line { points, .. } = primitive {
                vertices.insert(points.as_ptr());
            }
        }
    }
    let mut bytes = 0;
    for item in items {
        if !shared_arrays.insert(item.primitives.as_ptr()) {
            continue;
        }
        bytes += item.primitives.len() * size_of::<Prim>();
        for primitive in &*item.primitives {
            match primitive {
                Prim::Fill { points, .. } | Prim::Line { points, .. }
                    if vertices.insert(points.as_ptr()) =>
                {
                    bytes += points.len() * size_of::<Vec2>();
                }
                Prim::Text { text, .. } => bytes += text.as_str().len(),
                _ => {}
            }
        }
    }
    bytes
}
