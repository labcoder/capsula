//! Canonical semantic state, atomic admission and replayable Stop cuts.
mod metrics;
mod object;
mod work;

use crate::{
    Begin, Budget, ErrorCode, FieldPath, GlobalBudget, Handle, Record, Step, StepError, WireId,
    budget::Usage,
    limits::{MAX_LIVE_OBJECTS, MAX_STEPS},
    record::Start,
    text::Bounded,
};
use object::Object;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    sync::Arc,
};

/// An authoritative cut through the activated prefix, retaining finished objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopCut {
    /// One-based revision activated through; zero means none ever activated.
    pub activated: u32,
    /// Finished object handles, without duplicates.
    pub revealed: Bounded<Handle, MAX_LIVE_OBJECTS>,
}

/// One entry in the bounded recovery log. Enum tags are append only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SceneEvent {
    /// An original committed step, including any later dropped work.
    Step {
        /// Original one-based commit revision.
        revision: u32,
        /// Original typed input for idempotency and deterministic replay.
        step: Step,
    },
    /// An effective cut, ordered between the steps it affected.
    Stop(StopCut),
}

/// Whether a Stop retains a scene or clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Activated content remains.
    Kept,
    /// Nothing had activated.
    Cleared,
}

/// Validation either creates an owned candidate or recognizes a retransmission.
#[derive(Debug)]
pub enum Admission {
    /// A checked candidate, bound to its state and budget generations.
    New(Admitted),
    /// Typed-identical step id; no transition or charge.
    Duplicate,
}

/// An opaque candidate. Creating it never mutates the scene or global context.
pub struct Admitted {
    owner: Arc<()>,
    global: Arc<()>,
    revision: u32,
    generation: u64,
    mutation: u64,
    usage: Usage,
    step: Box<Step>,
    after: BTreeMap<WireId, Object>,
    created: Vec<(WireId, Handle)>,
    removed: Vec<(WireId, Object)>,
    changed: Vec<(WireId, Object)>,
}
impl fmt::Debug for Admitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admitted")
            .field("revision", &self.revision)
            .field("objects", &self.after.len())
            .finish_non_exhaustive()
    }
}

/// The installed lifetime index and deterministic identities of a committed step.
#[derive(Debug, PartialEq, Eq)]
pub struct Committed {
    /// Zero-based lifetime commit index, not a dense index into the kept steps.
    pub index: u32,
    /// One-based commit revision.
    pub revision: u32,
    /// Handles in expanded put order.
    pub handles: Vec<(WireId, Handle)>,
}

/// The owner, scene revision or global generation changed after admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stale scene admission")]
pub struct StaleAdmission;

struct StoredStep {
    event: usize,
    kept: bool,
    retired: bool,
    removed: Vec<(WireId, Object)>,
    changed: Vec<(WireId, Object)>,
}

/// A scene's prospective committed state, separate from its activation-time view.
pub struct SceneState {
    header: Begin,
    identity: Arc<()>,
    context: Option<Arc<()>>,
    mutation: u64,
    usage: Usage,
    revision: u32,
    objects: BTreeMap<WireId, Object>,
    reserved: BTreeSet<WireId>,
    step_ids: BTreeMap<WireId, u32>,
    steps: BTreeMap<u32, StoredStep>,
    log: Vec<SceneEvent>,
    last_cut_revision: u32,
    activated: u32,
    cleared: bool,
}
impl fmt::Debug for SceneState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneState")
            .field("revision", &self.revision)
            .field("objects", &self.objects.len())
            .field("steps", &self.steps.len())
            .field("cleared", &self.cleared)
            .finish_non_exhaustive()
    }
}
impl SceneState {
    /// Creates an empty scene from a typed header with positive canvas dimensions.
    /// # Errors
    /// Returns `OutOfRange` for a non-positive canvas dimension.
    pub fn begin(header: Begin) -> Result<Self, StepError> {
        if let Some(size) = header.size {
            for (index, value) in size.into_iter().enumerate() {
                if value.get() <= 0.0 {
                    return Err(StepError {
                        step: None,
                        path: FieldPath::default().field("size").index(index),
                        code: ErrorCode::OutOfRange,
                    });
                }
            }
        }
        Ok(Self {
            header,
            identity: Arc::new(()),
            context: None,
            mutation: 0,
            usage: Usage::default(),
            revision: 0,
            objects: BTreeMap::new(),
            reserved: BTreeSet::new(),
            step_ids: BTreeMap::new(),
            steps: BTreeMap::new(),
            log: Vec::new(),
            last_cut_revision: 0,
            activated: 0,
            cleared: false,
        })
    }

    /// Returns the immutable header used by every continuing call.
    pub fn header(&self) -> &Begin {
        &self.header
    }
    /// Returns the highest committed revision, including dropped steps.
    pub fn revision(&self) -> u32 {
        self.revision
    }
    /// Returns the number of prospectively surviving objects.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Returns logical work still charged, including pending removals.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    fn check_context(&self, global: &GlobalBudget) -> Result<(), StepError> {
        if self
            .context
            .as_ref()
            .is_some_and(|context| !Arc::ptr_eq(context, &global.identity))
        {
            return Err(StepError::new(ErrorCode::BudgetContext));
        }
        Ok(())
    }

    fn bound_usage(&self) -> Usage {
        if self.context.is_some() {
            self.usage
        } else {
            Usage::default()
        }
    }

    fn pending(&self) -> impl Iterator<Item = &(WireId, Object)> {
        self.steps
            .values()
            .filter(|step| step.kept && !step.retired)
            .flat_map(|step| step.removed.iter().chain(&step.changed))
    }

    fn current_usage(&self) -> Usage {
        if self.cleared {
            return Usage::default();
        }
        metrics::charge(
            self.objects
                .iter()
                .chain(self.pending().map(|(id, object)| (id, object))),
        )
        .expect("retirement and cuts only reduce previously checked charges")
    }

    fn apply_usage(&mut self, global: &mut GlobalBudget, usage: Usage) {
        global.replace(self.bound_usage(), usage);
        self.context = Some(Arc::clone(&global.identity));
        self.usage = usage;
        self.mutation = self
            .mutation
            .checked_add(1)
            .expect("scene mutations cannot exhaust u64 within a process lifetime");
    }
    /// Returns the number of retained steps, which may be less than revision.
    pub fn committed(&self) -> usize {
        self.steps.values().filter(|step| step.kept).count()
    }
    /// Returns whether the host cleared the scene.
    pub fn is_cleared(&self) -> bool {
        self.cleared
    }
    /// Returns a surviving object's handle.
    pub fn handle(&self, id: &WireId) -> Option<Handle> {
        self.objects.get(id).map(|o| o.handle)
    }
    /// Returns a kept step by zero-based lifetime commit index.
    pub fn step(&self, index: u32) -> Option<&Step> {
        let stored = self.steps.get(&index.checked_add(1)?)?;
        stored.kept.then(|| self.original(stored.event))
    }
    /// Returns original committed steps interleaved with every effective cut.
    pub fn recovery_log(&self) -> &[SceneEvent] {
        &self.log
    }

    fn original(&self, event: usize) -> &Step {
        match &self.log[event] {
            SceneEvent::Step { step, .. } => step,
            SceneEvent::Stop(_) => unreachable!("step metadata always indexes a step event"),
        }
    }

    /// Checks a closed step without changing identities, state or counters.
    /// # Errors
    /// Rejects invalid identities, references, ownership, values or a cleared scene.
    #[expect(
        clippy::too_many_lines,
        reason = "keep the ordered preflight checks and owned candidate construction visible at the trust boundary"
    )]
    pub fn admit(&self, step: &Step, global: &GlobalBudget) -> Result<Admission, StepError> {
        self.check_context(global)?;
        let fail = |path, code| StepError::in_step(&step.id, path, code);
        if self.cleared {
            return Err(fail(FieldPath::default(), ErrorCode::SceneCleared));
        }
        if let Some(revision) = self.step_ids.get(&step.id) {
            let stored = &self.steps[revision];
            return if self.original(stored.event) == step {
                Ok(Admission::Duplicate)
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
            .filter(|op| object::visual(op).is_some())
            .count();
        minimum.scenes = 1;
        if minimum.objects > MAX_LIVE_OBJECTS {
            return Err(fail(
                FieldPath::default(),
                ErrorCode::OverBudget(Budget::LiveObjects),
            ));
        }
        global
            .check(self.bound_usage(), minimum)
            .map_err(|code| fail(FieldPath::default(), code))?;
        let mut removal_ids = HashSet::with_capacity(step.remove.len());
        for (index, id) in step.remove.iter().enumerate() {
            if !removal_ids.insert(id) {
                return Err(fail(
                    FieldPath::default().field("remove").index(index),
                    ErrorCode::DuplicateKey,
                ));
            }
        }
        let mut after = self.objects.clone();
        let mut created = Vec::new();
        for (index, op) in step.put.iter().enumerate() {
            if let Some(v) = object::visual(op) {
                if self.reserved.contains(v.id) || after.contains_key(v.id) {
                    return Err(fail(
                        object::path(index, v.name).field("id"),
                        ErrorCode::IdReused,
                    ));
                }
                let handle = Handle::new(revision, index)
                    .map_err(|code| fail(object::path(index, v.name).field("id"), code))?;
                let object = object::object(&v, op, handle, index, step)?;
                after.insert(v.id.clone(), object);
                created.push((v.id.clone(), handle));
            }
        }
        for (index, op) in step.put.iter().enumerate() {
            object::validate_op(op, index, step, &after, &removal_ids)?;
        }
        object::acyclic(step, &after)?;
        object::validate_changes(step, &after, &removal_ids)?;
        let mut changed = work::changes(step, &mut after, revision)?;
        metrics::validate(step, &mut after)?;
        for (id, object) in &after {
            if let Some(previous) = self.objects.get(id)
                && previous.work.usage != object.work.usage
            {
                changed
                    .entry(id.clone())
                    .or_insert_with(|| previous.clone());
            }
        }
        let usage = metrics::charge(
            after
                .iter()
                .chain(self.pending().map(|(id, object)| (id, object)))
                .chain(changed.iter()),
        )
        .map_err(|code| fail(FieldPath::default(), code))?;
        global
            .check(self.bound_usage(), usage)
            .map_err(|code| fail(FieldPath::default(), code))?;
        let mut removed = object::removals(step, &mut after, &removal_ids)?;
        for object in after
            .values_mut()
            .chain(removed.iter_mut().map(|(_, object)| object))
        {
            if let Some(data) = &mut object.work.data {
                data.freeze(step);
            }
        }
        Ok(Admission::New(Admitted {
            owner: Arc::clone(&self.identity),
            global: Arc::clone(&global.identity),
            revision: self.revision,
            generation: global.generation(),
            mutation: self.mutation,
            usage,
            step: Box::new(step.clone()),
            after,
            created,
            removed,
            changed: changed.into_iter().collect(),
        }))
    }

    /// Installs a candidate after O(1) stamp checks, with no fallible revalidation.
    /// # Errors
    /// Returns only `StaleAdmission` and leaves both states unchanged on failure.
    pub fn commit(
        &mut self,
        admitted: Admitted,
        global: &mut GlobalBudget,
    ) -> Result<Committed, StaleAdmission> {
        if self.cleared
            || admitted.mutation != self.mutation
            || admitted.revision != self.revision
            || admitted.generation != global.generation()
            || !Arc::ptr_eq(&admitted.owner, &self.identity)
            || !Arc::ptr_eq(&admitted.global, &global.identity)
        {
            return Err(StaleAdmission);
        }
        let revision = self.revision + 1;
        self.reserved
            .extend(admitted.created.iter().map(|(id, _)| id.clone()));
        self.step_ids.insert(admitted.step.id.clone(), revision);
        self.steps.insert(
            revision,
            StoredStep {
                event: self.log.len(),
                kept: true,
                retired: false,
                removed: admitted.removed,
                changed: admitted.changed,
            },
        );
        self.log.push(SceneEvent::Step {
            revision,
            step: *admitted.step,
        });
        self.objects = admitted.after;
        self.revision = revision;
        self.apply_usage(global, admitted.usage);
        Ok(Committed {
            index: revision - 1,
            revision,
            handles: admitted.created,
        })
    }

    /// Marks a kept step's removals retired. Repeated notifications are harmless.
    /// # Errors
    /// Returns `OutOfRange` for an index that was never committed.
    pub fn retire(&mut self, index: u32, global: &mut GlobalBudget) -> Result<(), StepError> {
        self.check_context(global)?;
        let revision = index
            .checked_add(1)
            .ok_or_else(|| StepError::new(ErrorCode::OutOfRange))?;
        let stored = self
            .steps
            .get(&revision)
            .ok_or_else(|| StepError::new(ErrorCode::OutOfRange))?;
        if stored.kept && !stored.retired {
            self.retire_through(revision);
            self.activated = self.activated.max(revision);
            self.forget_activated();
            let usage = self.current_usage();
            self.apply_usage(global, usage);
        }
        Ok(())
    }

    fn retire_through(&mut self, revision: u32) -> bool {
        let mut changed = false;
        for (index, stored) in &mut self.steps {
            if *index <= revision && stored.kept && !stored.retired {
                stored.retired = true;
                changed = true;
            }
        }
        changed
    }

    fn forget_activated(&mut self) {
        for (_, stored) in self.steps.range_mut(..=self.activated) {
            stored.removed.clear();
            stored.changed.clear();
        }
    }

    /// Drops unactivated work and retains precisely the finished handles.
    ///
    /// A Stop never reduces revision or frees a reserved model id. Finished objects
    /// retain their placed geometry; references to discarded objects are detached.
    /// # Errors
    /// Rejects invalid bounds, duplicate/unknown handles or more than one effective
    /// cut without an intervening commit. A rejected cut changes nothing.
    #[expect(
        clippy::too_many_lines,
        reason = "Stop validates a candidate then installs it atomically; keep those two phases together"
    )]
    pub fn stop(
        &mut self,
        cut: &StopCut,
        global: &mut GlobalBudget,
    ) -> Result<StopOutcome, StepError> {
        self.check_context(global)?;
        if self.cleared {
            return Ok(StopOutcome::Cleared);
        }
        let at = |field, code| StepError {
            step: None,
            path: FieldPath::default().field("stop").field(field),
            code,
        };
        if cut.activated > self.revision
            || cut.activated < self.activated
            || (cut.activated != 0 && !self.steps.get(&cut.activated).is_some_and(|s| s.kept))
        {
            return Err(at("activated", ErrorCode::OutOfRange));
        }
        let mut after = self.objects.clone();
        let dropped: Vec<u32> = self
            .steps
            .iter()
            .filter_map(|(r, s)| (s.kept && *r > cut.activated).then_some(*r))
            .collect();
        for revision in dropped.iter().rev() {
            let stored = &self.steps[revision];
            for (id, object) in &stored.removed {
                after.insert(id.clone(), object.clone());
            }
            for (id, object) in &stored.changed {
                after.insert(id.clone(), object.clone());
            }
            after.retain(|_, object| object.handle.revision() != *revision);
        }
        let present: BTreeSet<Handle> = after.values().map(|o| o.handle).collect();
        let mut revealed = BTreeSet::new();
        for (index, handle) in cut.revealed.iter().enumerate() {
            if !revealed.insert(*handle) {
                return Err(StepError {
                    step: None,
                    path: FieldPath::default()
                        .field("stop")
                        .field("revealed")
                        .index(index),
                    code: ErrorCode::DuplicateKey,
                });
            }
            if !present.contains(handle) {
                return Err(StepError {
                    step: None,
                    path: FieldPath::default()
                        .field("stop")
                        .field("revealed")
                        .index(index),
                    code: ErrorCode::UnknownRef,
                });
            }
        }
        metrics::freeze_detached(&mut after, &revealed).map_err(StepError::new)?;
        after.retain(|_, object| revealed.contains(&object.handle));
        let effective =
            !dropped.is_empty() || after.len() != self.objects.len() || cut.activated == 0;
        let usage = if cut.activated == 0 {
            Usage::default()
        } else {
            metrics::charge(after.iter()).map_err(StepError::new)?
        };
        global
            .check(self.bound_usage(), usage)
            .map_err(StepError::new)?;
        if effective && self.revision > 0 && self.last_cut_revision == self.revision {
            return Err(at("activated", ErrorCode::OutOfRange));
        }
        if !effective {
            let changed = self.retire_through(cut.activated) || self.activated != cut.activated;
            self.activated = cut.activated;
            self.forget_activated();
            if changed {
                self.apply_usage(global, usage);
            }
            return Ok(StopOutcome::Kept);
        }
        let surviving: BTreeSet<WireId> = after.keys().cloned().collect();
        for object in after.values_mut() {
            object.references.retain(|id| surviving.contains(id));
            if object
                .parent
                .as_ref()
                .is_some_and(|id| !surviving.contains(id))
            {
                object.parent = None;
            }
        }
        for (_, stored) in self.steps.range_mut(cut.activated + 1..) {
            stored.kept = false;
            stored.removed.clear();
            stored.changed.clear();
        }
        self.objects = after;
        self.activated = cut.activated;
        self.retire_through(cut.activated);
        self.forget_activated();
        if self.revision > 0 {
            self.log.push(SceneEvent::Stop(cut.clone()));
            self.last_cut_revision = self.revision;
        }
        if cut.activated == 0 {
            self.cleared = true;
            for stored in self.steps.values_mut() {
                stored.kept = false;
                stored.removed.clear();
            }
        }
        self.apply_usage(global, usage);
        Ok(if self.cleared {
            StopOutcome::Cleared
        } else {
            StopOutcome::Kept
        })
    }

    /// Clears visible state once; historical log and revision remain intact.
    /// # Errors
    /// Returns `BudgetContext` for a foreign budget instance.
    pub fn clear(&mut self, global: &mut GlobalBudget) -> Result<(), StepError> {
        self.check_context(global)?;
        if self.cleared {
            return Ok(());
        }
        self.objects.clear();
        for step in self.steps.values_mut() {
            step.kept = false;
            step.removed.clear();
            step.changed.clear();
        }
        self.cleared = true;
        self.apply_usage(global, Usage::default());
        Ok(())
    }

    /// Rebuilds a scene in an isolated context from original steps and ordered cuts.
    ///
    /// Global installation must check the combined final replayed charges, not
    /// intermediate peaks from independently replaying scenes in a different order.
    /// # Errors
    /// Rejects invalid log order, duplicate commits, redundant cuts or invalid steps.
    /// # Panics
    /// Panics if the crate violates its synchronous admission/commit invariant.
    pub fn replay(header: Begin, log: &[SceneEvent]) -> Result<Self, StepError> {
        if log.len() > MAX_STEPS * 2 {
            return Err(StepError::new(ErrorCode::TooLong));
        }
        let mut state = Self::begin(header)?;
        let mut global = GlobalBudget::replay();
        for event in log {
            match event {
                SceneEvent::Step { revision, step } => {
                    if *revision != state.revision + 1 {
                        return Err(StepError {
                            step: Some(step.id.clone()),
                            path: FieldPath::default().field("revision"),
                            code: ErrorCode::OutOfRange,
                        });
                    }
                    let Admission::New(admitted) = state.admit(step, &global)? else {
                        return Err(StepError::in_step(
                            &step.id,
                            FieldPath::default().field("id"),
                            ErrorCode::DuplicateStepConflict,
                        ));
                    };
                    state
                        .commit(admitted, &mut global)
                        .expect("isolated replay has no intervening transition");
                    state.retire_through(*revision);
                    let usage = state.current_usage();
                    state.apply_usage(&mut global, usage);
                }
                SceneEvent::Stop(cut) => {
                    let before = state.log.len();
                    state.stop(cut, &mut global)?;
                    if before == state.log.len() {
                        return Err(StepError::new(ErrorCode::Malformed));
                    }
                }
            }
        }
        state.retire_through(state.revision);
        state.usage = state.current_usage();
        state.context = None;
        if !state.cleared {
            state.activated = state
                .steps
                .iter()
                .rev()
                .find_map(|(revision, step)| step.kept.then_some(*revision))
                .unwrap_or_default();
        }
        state.forget_activated();
        Ok(state)
    }

    /// Rebuilds and atomically charges an entire visible snapshot batch.
    /// # Errors
    /// Rejects invalid histories, more than four scenes or excessive final totals.
    pub fn replay_all(
        sources: &[(&Begin, &[SceneEvent])],
    ) -> Result<(Vec<Self>, GlobalBudget), StepError> {
        if sources.len() > crate::limits::MAX_VISIBLE_SCENES {
            return Err(StepError::new(ErrorCode::OverBudget(Budget::VisibleScenes)));
        }
        let mut scenes = Vec::with_capacity(sources.len());
        let mut usage = Usage::default();
        for (header, log) in sources {
            let state = Self::replay((*header).clone(), log)?;
            usage = usage.add(state.usage);
            scenes.push(state);
        }
        usage.validate_global().map_err(StepError::new)?;
        let mut global = GlobalBudget::default();
        global.install(usage);
        for state in &mut scenes {
            state.context = Some(Arc::clone(&global.identity));
        }
        Ok((scenes, global))
    }
}

/// A per-call begin/step/end guard; scenes themselves can span many calls.
#[derive(Debug, Default)]
pub struct CallState {
    phase: CallPhase,
}
#[derive(Debug, Default, PartialEq, Eq)]
enum CallPhase {
    #[default]
    Before,
    Open,
    Ended,
}
impl CallState {
    /// Checks record order and typed-equal continuation headers.
    /// # Errors
    /// Returns an ordering or header error without changing phase.
    pub fn accept(&mut self, record: &Record, header: &Begin) -> Result<(), StepError> {
        let code = match (&self.phase, record) {
            (CallPhase::Ended, _) => Some(ErrorCode::AfterEnd),
            (CallPhase::Before, Record::Begin(begin)) if begin != header => {
                Some(ErrorCode::HeaderMismatch)
            }
            (CallPhase::Before, Record::Begin(_)) => {
                self.phase = CallPhase::Open;
                None
            }
            (CallPhase::Open, Record::Begin(_)) => Some(ErrorCode::Malformed),
            (CallPhase::Before, _) => Some(ErrorCode::BeforeBegin),
            (CallPhase::Open, Record::End(_)) => {
                self.phase = CallPhase::Ended;
                None
            }
            (CallPhase::Open, Record::Step(_)) => None,
        };
        if let Some(code) = code {
            let mut error = StepError::new(code);
            if let Record::Step(step) = record {
                error.step = Some(step.id.clone());
            }
            return Err(error);
        }
        Ok(())
    }

    /// Checks that a complete call included its end record.
    /// # Errors
    /// Returns `BeforeBegin` or `MissingField` if the call is incomplete.
    pub fn finish(&self) -> Result<(), StepError> {
        match self.phase {
            CallPhase::Ended => Ok(()),
            CallPhase::Before => Err(StepError::new(ErrorCode::BeforeBegin)),
            CallPhase::Open => Err(StepError {
                step: None,
                path: FieldPath::default().field("end"),
                code: ErrorCode::MissingField,
            }),
        }
    }
}
