use super::{
    Glide, LayoutState, PrepareError, PrepareInput, PreparedStep, PreparedView, RecoveryError,
    SceneRecovery, SurfaceGeometry, compound, edge_labels, edges, heap, nodes,
    primitives::{Build, invariant, layout_error, rect},
    state::{Placed, Rest},
};
use crate::{
    Begin, ErrorCode, GlobalBudget, Handle, Op, SceneEvent, SceneState, WireId,
    display::{DisplayList, Item, PaintWork, Prim, TextMeasure},
    geom::{Affine, Rect, Vec2},
    record::{Anchor, Change, Color, Enter, KeyValue, Track},
    state::{object, work},
    theme::Theme,
    three::{
        mesh::MeshScratch,
        size::MeshError,
        view::{Ends, tessellate_view},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    mem::size_of,
    sync::{Arc, atomic::AtomicBool},
};

/// Relative allowance for f32 drift between an object's prepared envelope and a frame's composed
/// transform, applied before the 64 clamp; the painter still compares pieces against pieces.
const PAINT_DRIFT: f32 = 1.0 + 1.0 / 65_536.0;

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
        mut ticket,
        before,
        surface,
        theme,
        paint_cap,
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
    after.canvas = viewport;
    if !ticket.extras.is_empty() {
        after
            .extras
            .insert(ticket.revision, Arc::clone(&ticket.extras));
    }
    // 3D targets never enter the 2D maps; the ticket's view updates carry them.
    let three: HashMap<&WireId, usize> = ticket
        .run
        .put
        .iter()
        .enumerate()
        .filter_map(|(slot, op)| match op {
            Op::View3d(view) => Some((&view.id, slot)),
            Op::Solid(solid) => Some((&solid.id, slot)),
            _ => None,
        })
        .collect();
    let is_three = |id: &WireId| three.contains_key(id) || before.is_three(id).is_some();
    let mut sources = BTreeMap::<WireId, (Handle, Arc<Op>)>::new();
    let mut order = Vec::new();
    let mut tables = false;
    let mut charts = false;
    for (slot, op) in ticket.run.put.iter().enumerate() {
        if let Some(visual) = object::visual(op) {
            let handle = Handle::new(ticket.revision, slot)?;
            tables |= matches!(op, Op::Table(_) | Op::Row(_));
            charts |= matches!(op, Op::Chart(_) | Op::Series(_));
            sources.insert(visual.id.clone(), (handle, Arc::new(op.clone())));
            order.push(visual.id.clone());
        }
    }
    for change in ticket
        .run
        .set
        .iter()
        .filter(|change| !is_three(&change.target))
    {
        let (handle, source) = if let Some((handle, source)) = sources.get(&change.target) {
            (*handle, Arc::clone(source))
        } else {
            let previous = after.objects.get(&change.target).ok_or_else(invariant)?;
            (previous.item.handle, Arc::clone(&previous.source))
        };
        tables |= matches!(&*source, Op::Table(_) | Op::Row(_));
        charts |= matches!(&*source, Op::Chart(_) | Op::Series(_));
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
    let mut removed = Vec::new();
    for id in &*ticket.run.remove {
        if let Some(&slot) = three.get(id) {
            removed.push(Handle::new(ticket.revision, slot)?);
        } else if let Some(handle) = before.is_three(id) {
            removed.push(handle);
        } else if let Some(old) = after.objects.remove(id) {
            tables |= matches!(&*old.source, Op::Row(_));
            charts |= matches!(&*old.source, Op::Series(_));
            after.handles.remove(&old.item.handle);
            removed.push(old.item.handle);
        } else if let Some((handle, _)) = sources.get(id) {
            removed.push(*handle);
        } else {
            return Err(invariant());
        }
        sources.remove(id);
    }
    order.retain(|id| sources.contains_key(id));
    let mut rests: BTreeMap<_, _> = after
        .objects
        .iter()
        .map(|(id, object)| (id.clone(), object.rest))
        .collect();
    for change in ticket
        .run
        .set
        .iter()
        .filter(|change| change.color.is_some() && !is_three(&change.target))
    {
        rests
            .entry(change.target.clone())
            .or_insert_with(Rest::default)
            .color = None;
    }
    let mut motions = BTreeMap::new();
    let mut explicit_pos = BTreeSet::new();
    for (slot, op) in ticket.run.put.iter().enumerate() {
        if let Op::Anim(anim) = op
            && !is_three(&anim.target)
        {
            rests
                .entry(anim.target.clone())
                .or_insert_with(Rest::default)
                .set(anim.track, crate::motion::final_key(anim), &theme);
            if anim.track == Track::Pos {
                explicit_pos.insert(anim.target.clone());
            }
            let motion = work::motion(anim, slot, &ticket.run)?;
            motions.insert((anim.target.clone(), work::property(anim.track)), motion);
        }
    }
    for (id, object) in &mut after.objects {
        if let Some(rest) = rests.get(id) {
            object.rest = *rest;
        }
    }
    let mut containers: Vec<_> = order
        .iter()
        .filter(|id| {
            matches!(
                &*sources[*id].1,
                Op::Diagram(_) | Op::Table(_) | Op::Chart(_)
            )
        })
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
        let (bounds, opacity) = match &**source {
            Op::Diagram(value) => (value.bounds, value.opacity),
            Op::Table(value) => (value.bounds, value.opacity),
            Op::Chart(value) => (value.bounds, value.opacity),
            _ => unreachable!("container list contains diagrams, tables and charts"),
        };
        let previous = after.objects.get(&id).cloned();
        let parent = parent(source, &after, previous.as_ref())?;
        // A boxless diagram keeps its measured contents, which a parent's ranking reads.
        let measured = previous
            .as_ref()
            .filter(|_| matches!(&**source, Op::Diagram(_)))
            .map(|old| old.item.bounds);
        let mut object = Placed::new(
            Item {
                handle: *handle,
                parent,
                // A titled cluster keeps the title its last layout drew.
                primitives: previous
                    .as_ref()
                    .filter(|_| matches!(&**source, Op::Diagram(_)))
                    .map_or_else(|| Arc::from([]), |old| Arc::clone(&old.item.primitives)),
                bounds: bounds.map(rect).or(measured).unwrap_or_default(),
                transform: Affine::default(),
                opacity: opacity.map_or(1.0, crate::Finite::get),
            },
            Arc::clone(source),
            previous.as_ref(),
        );
        object.anchors = [Vec2::default(); 2];
        object.rest = rests.get(&id).copied().unwrap_or_default();

        after.handles.insert(*handle, id.clone());
        after.objects.insert(id, object);
    }
    if tables {
        super::table::place(&sources, &before, &mut after, &removed, &mut build)?;
    }
    let chart_bytes = if charts {
        super::chart::place(&sources, &before, &mut after, &removed, &mut build)?
    } else {
        0
    };
    let (mut glides, prebuilt) =
        nodes::place(&sources, &order, &rests, &mut after, &mut build, surface)?;
    glides.retain(|glide| {
        after
            .handles
            .get(&glide.target)
            .is_some_and(|id| !explicit_pos.contains(id))
    });
    for (id, object) in &mut after.objects {
        if let Some(rest) = rests.get(id) {
            object.rest = *rest;
        }
    }
    for Glide { target, to } in &glides {
        let id = after.handles.get(target).ok_or_else(invariant)?;
        let group = after.objects.get_mut(id).ok_or_else(invariant)?;
        let old = group.rest.pos;
        group.rest.pos = *to;
        let glide = work::Motion {
            keys: 2,
            min: [old.x.min(to.x), old.y.min(to.y)],
            max: [old.x.max(to.x), old.y.max(to.y)],
        };
        Arc::make_mut(group.motion.get_or_insert_with(Arc::default))
            .entry("pos")
            .and_modify(|cached| *cached = cached.peak(glide))
            .or_insert(glide);
    }
    update_group_bounds(&mut after);
    let mut pending: Vec<_> = order
        .iter()
        .filter(|id| !prebuilt.contains(*id))
        .filter(|id| {
            !matches!(
                &*sources[*id].1,
                Op::Diagram(_)
                    | Op::Node(_)
                    | Op::Table(_)
                    | Op::Row(_)
                    | Op::Chart(_)
                    | Op::Series(_)
            )
        })
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
            edges::build(
                source,
                *handle,
                parent,
                compound::direction(&after, parent, surface).0,
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
    edges::route(&mut after, &build)?;
    update_group_bounds(&mut after);
    edge_labels::separate(&mut after, &build)?;
    for ((id, property), motion) in motions {
        let object = after.objects.get_mut(&id).ok_or_else(invariant)?;
        Arc::make_mut(object.motion.get_or_insert_with(Arc::default)).insert(property, motion);
    }
    after.prune_extras();
    let (presentation_scale, envelopes) = validate_bounds(&after)?;
    after.presentation_scale = presentation_scale;
    after.grow_content()?;
    for primitive in after
        .objects
        .values()
        .flat_map(|object| &*object.item.primitives)
    {
        if let Prim::Fill { points, .. } = primitive
            && points.len() > crate::limits::MAX_GLOBAL_FILL_VERTICES
        {
            return Err(PrepareError::OverResidency);
        }
    }
    // A capped scene whose own work passes its cap can never be admitted: stop counting there.
    let surface_limit = after.presentation_limit;
    let mut total = PaintWork::default();
    for (id, object) in &mut after.objects {
        let envelope = envelopes
            .get(&object.item.handle)
            .copied()
            .ok_or_else(invariant)?;
        // The fit's largest scale enters once, beside the object's own scale.
        let scale = (envelope * surface_limit * PAINT_DRIFT).min(crate::limits::MAX_COMPOSED_SCALE);
        object.work = match (before.objects.get(id), paint_cap) {
            (Some(old), _)
                if Arc::ptr_eq(&old.item.primitives, &object.item.primitives)
                    && old.paint_scale.to_bits() == scale.to_bits() =>
            {
                old.work
            }
            (_, None) => PaintWork::of(&object.item.primitives, scale),
            (_, Some(cap)) => PaintWork::within(&object.item.primitives, total, cap, scale)
                .ok_or(PrepareError::OverResidency)?,
        };
        object.paint_scale = scale;
        total = total + object.work;
        if !object.work.fits_builds() || paint_cap.is_some_and(|cap| !total.fits(cap)) {
            return Err(PrepareError::OverResidency);
        }
    }
    if after.objects.is_empty() {
        // Removals leave an emptied B-tree's root leaf, which the index charge doesn't count.
        after.objects = BTreeMap::new();
        after.handles = BTreeMap::new();
    }
    let colors: HashMap<&WireId, Color> = ticket
        .run
        .set
        .iter()
        .filter(|change| is_three(&change.target))
        .filter_map(|change| change.color.map(|color| (&change.target, color)))
        .collect();
    // A 3D track's final key settles into its solid's rest, so a cut or recovery lands there.
    let mut ends: HashMap<&WireId, Ends> = HashMap::new();
    for op in &*ticket.run.put {
        if let Op::Anim(anim) = op
            && is_three(&anim.target)
            && let KeyValue::V3(value) = crate::motion::final_key(anim)
        {
            let value = Some(value.map(crate::Finite::get));
            let end = ends.entry(&anim.target).or_default();
            match anim.track {
                Track::Pos3 => end.pos3 = value,
                Track::Rot3 => end.rot3 = value,
                _ => {}
            }
        }
    }
    let mut views = Vec::with_capacity(ticket.views.len());
    let mut scratch = None;
    for update in &ticket.views {
        build.check_cancel()?;
        let Some(input) = &update.input else {
            after.views.remove(&update.id);
            continue;
        };
        let geometry = tessellate_view(
            input,
            before.views.get(&update.id).map(|view| &**view),
            &colors,
            &ends,
            &theme,
            cancel,
            scratch.get_or_insert_with(MeshScratch::new),
        )
        .map_err(|error| match error {
            MeshError::Cancelled => PrepareError::Cancelled,
            _ => PrepareError::OverResidency,
        })?;
        let geometry = Arc::new(geometry);
        after.views.insert(update.id.clone(), Arc::clone(&geometry));
        views.push(PreparedView {
            view: input.handle,
            geometry,
        });
    }
    let scratch_bytes = scratch.as_ref().map_or(0, MeshScratch::bytes);
    if !colors.is_empty() || !ends.is_empty() {
        // A colour- or track-only view keeps its inputs, so it restyles the predecessor's geometry.
        for (id, old) in &before.views {
            if ticket.views.iter().any(|update| update.id == *id)
                || !old
                    .solids
                    .iter()
                    .any(|solid| colors.contains_key(&solid.id) || ends.contains_key(&solid.id))
            {
                continue;
            }
            build.check_cancel()?;
            let geometry = Arc::new(old.restyled(&colors, &ends, &theme));
            after.views.insert(id.clone(), Arc::clone(&geometry));
            views.push(PreparedView {
                view: old.handle,
                geometry,
            });
        }
    }
    if after.views.is_empty() {
        // As for the 2D maps: the last removal leaves a root leaf the index charge doesn't count.
        after.views = BTreeMap::new();
    }
    let view_bytes: usize = views
        .iter()
        .map(|view| {
            let id = after
                .views
                .iter()
                .find(|(_, geometry)| Arc::ptr_eq(geometry, &view.geometry))
                .map(|(id, _)| id);
            view.geometry
                .marginal_bytes(id.and_then(|id| before.views.get(id)).map(|old| &**old))
        })
        .sum();
    let mut items: Vec<_> = after
        .objects
        .iter()
        .filter(|(id, object)| {
            before.objects.get(*id).is_none_or(|old| {
                !Arc::ptr_eq(&old.item.primitives, &object.item.primitives)
                    || old.item.bounds != object.item.bounds
                    || old.item.parent != object.item.parent
                    || old.item.transform != object.item.transform
            })
        })
        .map(|(_, object)| object.item.clone())
        .collect();
    items.sort_by_key(|item| item.handle);
    let resident_bytes = residency(before.objects.values().map(|object| &object.item), &items)
        + heap::shared_bytes(before.objects.values(), after.objects.values())
        + heap::compiled_bytes(&ticket.step, &ticket.run)
        + heap::extras_bytes(
            before.extras.values(),
            after.extras.values().chain([&ticket.extras]),
        )
        + chart_bytes
        + after.index_bytes()
        + items.capacity() * size_of::<Item>()
        + removed.capacity() * size_of::<Handle>()
        + glides.capacity() * size_of::<Glide>()
        + views.capacity() * size_of::<PreparedView>()
        + view_bytes;
    if resident_bytes > crate::limits::MAX_RESIDENT_BYTES {
        return Err(PrepareError::OverResidency);
    }
    after.revision = ticket.revision;
    after.version = Arc::new(());
    build.check_cancel()?;
    // Only preparation reads the sizing facts, so the held result keeps none.
    ticket.path_scales.clear();
    ticket.views = Vec::new();
    Ok(PreparedStep {
        index: ticket.revision - 1,
        display: DisplayList { items },
        removed,
        glides,
        retained_bytes: after.resident_bytes(),
        views,
        after,
        resident_bytes,
        scratch_bytes,
        ticket,
        base: Arc::clone(&before.version),
        theme,
    })
}

/// Rebuilds semantic state and layout together, skipping preparations later cuts drop.
/// Each step prepares under `paint_cap` when one is given, as [`PrepareInput::with_paint_cap`].
/// # Errors
/// A failed kept preparation identifies its revision; invalid semantic input uses zero.
pub fn recover(
    header: Begin,
    log: &[SceneEvent],
    surface: SurfaceGeometry,
    theme: &Arc<Theme>,
    measure: &mut dyn TextMeasure,
    cancel: &AtomicBool,
    paint_cap: Option<PaintWork>,
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
                        let committed = committed.ok_or_else(invariant)?;
                        let input = PrepareInput::new(
                            committed.preparation,
                            before.clone(),
                            surface,
                            Arc::clone(theme),
                        )
                        .map_err(|(error, _)| error)?
                        .with_paint_cap(paint_cap);
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

/// Recovers a visible batch scene by scene and binds the survivors to one new budget.
///
/// Results follow input order. A failed scene charges nothing, and so does a recovered one
/// that would take the batch past a global cap with the scenes before it: that one fails
/// whole, with revision zero.
pub fn recover_all(
    sources: &[(&Begin, &[SceneEvent])],
    surface: SurfaceGeometry,
    theme: &Arc<Theme>,
    measure: &mut dyn TextMeasure,
    cancel: &AtomicBool,
    paint_cap: Option<PaintWork>,
) -> (Vec<SceneRecovery>, GlobalBudget) {
    let mut global = GlobalBudget::default();
    let results = sources
        .iter()
        .map(|(header, log)| -> Result<_, RecoveryError> {
            let (mut scene, layout) = recover(
                (*header).clone(),
                log,
                surface,
                theme,
                measure,
                cancel,
                paint_cap,
            )?;
            scene.bind(&mut global)?;
            Ok((scene, layout))
        })
        .collect();
    (results, global)
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
        Op::Row(row) => {
            if let Some(value) = &change.cells {
                row.cells = value.clone();
            }
            if let Some(value) = change.emphasis {
                row.emphasis = value;
            }
        }
        Op::Series(series) => {
            if let Some(value) = &change.data {
                series.data = value.clone();
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
            Op::Table(value) => color!(value),
            Op::Row(value) => color!(value),
            Op::Chart(value) => color!(value),
            Op::Series(value) => color!(value),
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
            let Some(&contents) = bounds.get(&object.item.handle) else {
                continue;
            };
            let (bounds, title) = match object.title {
                Some(size) => {
                    let (bounds, origin) = compound::banded(contents, size);
                    (bounds, Some(origin))
                }
                None => (contents, None),
            };
            if bounds != object.item.bounds {
                object.item.bounds = bounds;
                if let Some(origin) = title {
                    object.item.primitives =
                        compound::retitle(&object.item.primitives, bounds, origin);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// The presentation scale's cap, and each placed object's composed-scale envelope.
fn validate_bounds(state: &LayoutState) -> Result<(f32, BTreeMap<Handle, f32>), PrepareError> {
    let mut objects = BTreeMap::new();
    for (id, placed) in &state.objects {
        let parent = placed
            .item
            .parent
            .and_then(|handle| state.handles.get(&handle))
            .cloned();
        let matrix = placed.item.transform.0;
        let scale = matrix[0].hypot(matrix[1]);
        let bounds = super::place::local_bounds(&placed.item)?;
        let enter = work::entrance(&placed.source);
        let extent = if matches!(&*placed.source, Op::Diagram(_))
            && enter.is_none_or(|enter| enter == Enter::None)
        {
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
            motion: placed.motion.as_deref().cloned().unwrap_or_default(),
            enter,
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
                three: None,
            },
        );
    }
    let envelopes: BTreeMap<Handle, f32> =
        crate::state::metrics::validate_transforms(&objects)?.collect();
    let maximum = envelopes
        .iter()
        .map(|(handle, scale)| {
            let source = &state.objects[&state.handles[handle]].source;
            scale
                * match &**source {
                    Op::Path(path) => path.scale.map_or(1.0, crate::Finite::get),
                    _ => 1.0,
                }
        })
        .fold(1.0, f32::max);
    for object in state
        .objects
        .values()
        .filter(|object| !object.item.primitives.is_empty())
    {
        state.world_bounds(object)?;
    }
    Ok((
        state
            .presentation_limit
            .min(crate::limits::MAX_COMPOSED_SCALE / maximum),
        envelopes,
    ))
}

/// Bytes of `items`' geometry that `shared` doesn't already hold, each allocation once with its
/// `Arc` header.
pub(super) fn residency<'a>(
    shared: impl IntoIterator<Item = &'a Item>,
    items: impl IntoIterator<Item = &'a Item>,
) -> usize {
    let mut shared_arrays = HashSet::new();
    let mut vertices = HashSet::new();
    for item in shared {
        shared_arrays.insert(item.primitives.as_ptr());
        for primitive in &*item.primitives {
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
        bytes += heap::ARC_HEADER + item.primitives.len() * size_of::<Prim>();
        for primitive in &*item.primitives {
            match primitive {
                Prim::Fill { points, .. } | Prim::Line { points, .. }
                    if vertices.insert(points.as_ptr()) =>
                {
                    bytes += heap::ARC_HEADER + points.len() * size_of::<Vec2>();
                }
                Prim::Text { text, .. } => bytes += text.as_str().len(),
                _ => {}
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn residency_counts_shared_stroke_and_fill_vertices_once() {
        let crate::Record::Begin(header) = crate::parse_record(
            br#"{"begin":{"v":1,"scene":"bytes","surface":"inline","alt":""}}"#,
        )
        .expect("header") else {
            panic!("begin");
        };
        let mut before = LayoutState::new(&SceneState::begin(header).expect("scene"));
        let points: Arc<[Vec2]> =
            Arc::from([Vec2::default(), Vec2::new(1.0, 1.0), Vec2::new(2.0, 0.0)]);
        let color =
            crate::display::Paint::from(crate::record::Rgb::try_from(0x00ff_ffff).expect("RGB"));
        let item = Item {
            handle: Handle::new(1, 0).expect("handle"),
            parent: None,
            bounds: Rect::default(),
            transform: Affine::default(),
            opacity: 1.0,
            primitives: Arc::from([
                Prim::Line {
                    points: Arc::clone(&points),
                    closed: false,
                    color,
                    width: 1.0,
                    dashed: false,
                },
                Prim::Fill { points, color },
            ]),
        };
        assert_eq!(
            residency(
                before.objects.values().map(|o| &o.item),
                std::slice::from_ref(&item)
            ),
            2 * heap::ARC_HEADER + 2 * size_of::<Prim>() + 3 * size_of::<Vec2>(),
            "one primitive array and one vertex array shared by the stroke and the fill"
        );
        let crate::Record::Step(step) = crate::parse_record(
            br#"{"step":{"id":"first","put":[{"shape":{"id":"p","kind":"rect","box":[0,0,1,1]}}]}}"#,
        )
        .expect("step") else {
            panic!("step");
        };
        before.objects.insert(
            "p".to_owned().try_into().expect("id"),
            Placed::new(item.clone(), Arc::new(step.put[0].clone()), None),
        );
        assert_eq!(
            residency(
                before.objects.values().map(|o| &o.item),
                std::slice::from_ref(&item)
            ),
            0
        );
        let recolored = Item {
            primitives: item.primitives.to_vec().into(),
            ..item
        };
        assert_eq!(
            residency(before.objects.values().map(|o| &o.item), &[recolored]),
            heap::ARC_HEADER + 2 * size_of::<Prim>()
        );
    }
}
