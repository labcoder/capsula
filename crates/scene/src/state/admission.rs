//! Atomic admission of authored steps and their compiled operations.
use super::retire::{map_bytes, records_bytes};
use super::{
    Admission, Admitted, Candidate, DetachedCandidate, Object, SceneState, WorldCheck, metrics,
    object, parents, retire, three, work,
};
use crate::{
    Budget, ErrorCode, FieldPath, GlobalBudget, Handle, Step, StepError, WireId,
    deferred::Deferred,
    layout::heap,
    limits::{MAX_LIVE_OBJECTS, MAX_STEP_OUTLINE_POINTS, MAX_STEPS},
    mermaid::{self, Expansion},
    record::{SolidShape, Start, Track},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

impl SceneState {
    /// Checks a closed step without changing identities, state or counters.
    /// # Errors
    /// Rejects invalid identities, references, ownership, values or a cleared scene.
    pub fn admit(&self, step: &Step, global: &GlobalBudget) -> Result<Admission, StepError> {
        self.admit_with(step, global, WorldCheck::Skip)
    }

    /// `admit` as the brain's drawing ingress runs it: every view the step changes or reposes is
    /// also tessellated at its candidate rest, so a world-degenerate solid refuses `Degenerate`.
    /// # Errors
    /// As `admit`, and `Degenerate` for a solid whose world mesh fails.
    pub fn admit_checked(
        &self,
        step: &Step,
        global: &GlobalBudget,
    ) -> Result<Admission, StepError> {
        self.admit_with(step, global, WorldCheck::Run)
    }

    fn admit_with(
        &self,
        step: &Step,
        global: &GlobalBudget,
        world: WorldCheck,
    ) -> Result<Admission, StepError> {
        self.check_context(global)?;
        Ok(match self.admit_local(step, Some(global), world)? {
            Some(local) => Admission::New(Admitted {
                global: Arc::clone(&global.identity),
                generation: global.generation(),
                local,
            }),
            None => Admission::Duplicate,
        })
    }

    /// Admission against this state alone, or `None` for a retransmission. Without a budget,
    /// as on a worker, every global cap waits for the binding that applies the candidate.
    pub(super) fn admit_local(
        &self,
        step: &Step,
        global: Option<&GlobalBudget>,
        world: WorldCheck,
    ) -> Result<Option<DetachedCandidate>, StepError> {
        let fail = |path, code| StepError::in_step(&step.id, path, code);
        if self.cleared {
            return Err(fail(FieldPath::default(), ErrorCode::SceneCleared));
        }
        if let Some(revision) = self.step_ids.get(&step.id) {
            let stored = &self.steps[revision];
            return if self.original(stored.event) == step {
                Ok(None)
            } else {
                Err(fail(
                    FieldPath::default().field("id"),
                    ErrorCode::DuplicateStepConflict,
                ))
            };
        }
        if self.steps.len() == MAX_STEPS {
            return Err(fail(
                FieldPath::default(),
                ErrorCode::OverBudget(Budget::Steps),
            ));
        }
        if let Start::After { step: earlier, .. } = &step.start
            && !self
                .step_ids
                .get(earlier)
                .is_some_and(|r| self.steps[r].kept)
        {
            return Err(fail(
                FieldPath::default().field("start").field("after"),
                ErrorCode::ForwardRef,
            ));
        }
        let revision = self.revision + 1;
        let mut minimum = self.usage;
        minimum.objects += step
            .put
            .iter()
            .filter(|op| object::model(op).is_some())
            .count();
        minimum.scenes = 1;
        if minimum.objects > MAX_LIVE_OBJECTS {
            return Err(fail(
                FieldPath::default(),
                ErrorCode::OverBudget(Budget::LiveObjects),
            ));
        }
        let outline_points: usize = step
            .put
            .iter()
            .map(|op| match op {
                crate::Op::Solid(solid) if solid.shape == SolidShape::Extrude => solid.points.len(),
                _ => 0,
            })
            .sum();
        if outline_points > MAX_STEP_OUTLINE_POINTS {
            return Err(fail(
                FieldPath::default(),
                ErrorCode::OverBudget(Budget::OutlinePoints),
            ));
        }
        if let Some(global) = global {
            global
                .check(self.bound_usage(), minimum)
                .map_err(|code| fail(FieldPath::default(), code))?;
        }
        let original = Deferred::new(
            Arc::new(step.clone()),
            heap::ARC_HEADER + heap::step_bytes(step),
        );
        let mut expansion = mermaid::expand(
            &original,
            revision,
            &self.reserved,
            MAX_LIVE_OBJECTS - minimum.objects,
        )?;
        parents::resolve(&mut expansion.run, &self.objects);
        let bytes = heap::compiled_bytes(&original, &expansion.run)
            + heap::extras_bytes(std::iter::empty(), [&expansion.extras]);
        let expansion = Deferred::new(expansion, bytes);
        self.candidate(&original, &expansion, global, world)
            .map(Some)
            .map_err(|error| expansion.blame(error))
    }

    /// Validates and charges the compiled step as one atomic candidate. Its paths name compiled
    /// slots until `Expansion::blame` moves them onto the original step.
    #[expect(clippy::too_many_lines, reason = "one atomic admission transaction")]
    fn candidate(
        &self,
        original: &Arc<Step>,
        expansion: &Expansion,
        global: Option<&GlobalBudget>,
        world: WorldCheck,
    ) -> Result<DetachedCandidate, StepError> {
        let step = &*expansion.run;
        let fail = |path, code| StepError::in_step(&step.id, path, code);
        let revision = self.revision + 1;
        let mut removal_ids = HashSet::with_capacity(step.remove.len());
        for (index, id) in step.remove.iter().enumerate() {
            if !removal_ids.insert(id) {
                return Err(fail(
                    FieldPath::default().field("remove").index(index),
                    ErrorCode::DuplicateKey,
                ));
            }
        }
        let mut after = Deferred::new(self.objects.clone(), self.objects_bytes);
        let mut owned = self.objects_bytes - heap::btree::<WireId, Object>(self.objects.len());
        let mut created = Vec::new();
        for (index, op) in step.put.iter().enumerate() {
            if let Some(v) = object::model(op) {
                if self.reserved.contains(v.id) || after.contains_key(v.id) {
                    return Err(fail(
                        object::path(index, v.name).field("id"),
                        ErrorCode::IdReused,
                    ));
                }
                let handle = Handle::new(revision, index)
                    .map_err(|code| fail(object::path(index, v.name).field("id"), code))?;
                let mut object = object::object(&v, op, handle, index, step)?;
                if let Some(segments) = expansion
                    .extras
                    .get(&index)
                    .and_then(mermaid::Extra::segments)
                {
                    object.work.usage.segments = segments;
                }
                owned += retire::owned((v.id, &object));
                after.insert(v.id.clone(), object);
                retire::attribute(&mut after, owned);
                created.push((v.id.clone(), handle));
            }
        }
        for (index, op) in step.put.iter().enumerate() {
            object::validate_op(op, index, step, &after, &removal_ids)?;
            expansion.check_target(step, index, &after)?;
        }
        object::acyclic(step, &after)?;
        let behind = if step.set.iter().any(|change| change.behind.is_some()) {
            self.steps
                .values()
                .filter(|stored| stored.kept)
                .flat_map(|stored| self.original(stored.event).set.iter())
                .filter_map(|change| Some((&change.target, change.behind.as_ref()?)))
                .collect()
        } else {
            BTreeMap::new()
        };
        object::validate_changes(step, &after, &removal_ids, behind)?;
        let mut changed = work::changes(step, &mut after, &mut owned, revision)?;
        metrics::validate(step, &mut after, self.header.surface, |handle| {
            expansion.source_slot(handle, revision)
        })?;
        for (id, object) in after.iter() {
            if let Some(previous) = self.objects.get(id)
                && previous.work.usage != object.work.usage
            {
                changed
                    .entry(id.clone())
                    .or_insert_with(|| previous.clone());
            }
        }
        changed.attribute(map_bytes(&changed));
        three::bounds(
            &after,
            after
                .iter()
                .chain(self.pending().map(|(id, object)| (id, object)))
                .chain(changed.iter()),
        )
        .map_err(|(handle, code)| {
            let mut path = FieldPath::default().field("put");
            if let Some(slot) = expansion.source_slot(handle, revision) {
                path = path.index(slot);
            }
            fail(path, code)
        })?;
        let usage = metrics::charge(
            after
                .iter()
                .chain(self.pending().map(|(id, object)| (id, object)))
                .chain(changed.iter()),
            Some(step),
        )
        .map_err(|code| fail(FieldPath::default(), code))?;
        if let Some(global) = global {
            global
                .check(self.bound_usage(), usage)
                .map_err(|code| fail(FieldPath::default(), code))?;
        }
        let removed = object::removals(step, &mut after, &removal_ids)?;
        owned -= removed
            .iter()
            .map(|(id, object)| retire::owned((id, object)))
            .sum::<usize>();
        retire::attribute(&mut after, owned);
        let views = three::view_updates(step, &self.objects, &after);
        if world == WorldCheck::Run
            && let Some(handle) = three::degenerate(step, &views, &after)
        {
            let mut path = FieldPath::default().field("put");
            let slot = (handle.revision() == revision).then(|| handle.slot()).or_else(|| {
                step.put.iter().position(|op| {
                    matches!(op, crate::Op::Anim(anim) if matches!(anim.track, Track::Pos3 | Track::Rot3))
                })
            });
            if let Some(slot) = slot {
                path = path.index(slot);
            }
            return Err(fail(path, ErrorCode::Degenerate));
        }
        let removed_bytes = records_bytes(&removed);
        let mut removed = Deferred::new(removed, removed_bytes);
        for object in after
            .values_mut()
            .chain(removed.iter_mut().map(|(_, object)| object))
        {
            if let Some(data) = &mut object.work.data {
                data.freeze(step);
            }
        }
        let mut candidate = after
            .map(|after| {
                let removed = removed.into_inner();
                let changed: Vec<_> = changed.into_inner().into_iter().collect();
                let ids: usize = created.iter().map(|(id, _)| id.as_str().len()).sum();
                let resized: HashSet<_> = changed
                    .iter()
                    .filter_map(|(id, old)| {
                        after
                            .get(id)
                            .filter(|new| new.work.usage.segments != old.work.usage.segments)
                            .map(|_| id)
                    })
                    .collect();
                let path_scales: BTreeMap<Handle, f32> = after
                    .iter()
                    .filter_map(|(id, object)| {
                        let path = object.work.path.as_ref()?;
                        (object.handle.revision() == revision || resized.contains(id))
                            .then_some((object.handle, path.composed_scale))
                    })
                    .collect();
                let original_bytes = heap::ARC_HEADER + heap::step_bytes(original);
                Candidate {
                    preparation_bytes: heap::ARC_HEADER
                        + original_bytes
                        + heap::compiled_bytes(original, &expansion.run)
                        + heap::extras_bytes(std::iter::empty(), [&expansion.extras])
                        + heap::btree::<Handle, f32>(path_scales.len())
                        + retire::views_bytes(&views),
                    after_bytes: heap::btree::<WireId, Object>(after.len()) + owned,
                    history_bytes: original.id.as_str().len()
                        + ids
                        + records_bytes(&removed)
                        + records_bytes(&changed)
                        + original_bytes,
                    original_bytes,
                    step: Arc::clone(original),
                    run: Arc::clone(&expansion.run),
                    extras: Arc::clone(&expansion.extras),
                    after,
                    created,
                    removed,
                    changed,
                    path_scales,
                    views,
                }
            })
            .map(Box::new);
        candidate.attribute(candidate.attributed_bytes());
        Ok(DetachedCandidate {
            owner: Arc::clone(&self.identity),
            revision: self.revision,
            mutation: self.mutation,
            usage,
            candidate,
        })
    }
}
