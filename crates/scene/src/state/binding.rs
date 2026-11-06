//! The presenter's sole mutable owner of a mirrored scene, and the read-only snapshot its
//! admission worker checks a step against without a budget.

use super::{DetachedCandidate, Preparation, SceneState, StopCut, StopOutcome};
use crate::{
    ErrorCode, FieldPath, GlobalBudget, Step, StepError, budget::Usage, deferred::Deferred,
    layout::heap,
};
use std::sync::Arc;

/// Why a bound operation changed nothing: neither the scene, its charge nor the budget.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    /// The scene is charged to another budget instance.
    #[error("scene bound to another budget")]
    WrongContext,
    /// The snapshot or candidate came from another scene.
    #[error("candidate from another scene")]
    WrongOwner,
    /// The scene committed since the candidate's snapshot.
    #[error("candidate for another revision")]
    WrongRevision,
    /// The scene changed since the candidate's snapshot.
    #[error("candidate for an older scene state")]
    WrongMutation,
    /// Installing would pass a global cap.
    #[error("global budget: {0}")]
    OverBudget(ErrorCode),
    /// A snapshot of the scene is still alive.
    #[error("scene state still shared")]
    NotUnique,
    /// The scene rejected the operation.
    #[error(transparent)]
    Rejected(StepError),
}

/// The only mutable owner of a mirrored scene and of the usage it charged. It never clones;
/// dropping it without [`release`](Self::release) keeps its charge, which is right only when
/// that whole budget is being replaced.
#[derive(Debug)]
pub struct SceneBinding {
    state: Deferred<Arc<SceneState>>,
}

/// A read-only snapshot for one admission worker. It can neither mutate, charge nor release.
#[derive(Debug)]
pub struct SceneRead {
    state: Deferred<Arc<SceneState>>,
}

impl SceneBinding {
    /// Adopts a state already charged to `global`, or charges an unbound one.
    /// # Errors
    /// `WrongContext` for a state bound to another budget, or `OverBudget` with `global` unchanged.
    pub fn bind(
        mut state: Deferred<SceneState>,
        global: &mut GlobalBudget,
    ) -> Result<Self, BindingError> {
        state
            .check_context(global)
            .map_err(|_| BindingError::WrongContext)?;
        if state.context.is_none() {
            let usage = state.usage;
            global
                .check(Usage::default(), usage)
                .map_err(BindingError::OverBudget)?;
            if usage == Usage::default() {
                state.context = Some(Arc::clone(&global.identity));
            } else {
                state.apply_usage(global, usage);
            }
        }
        let mut binding = Self {
            state: state.map(Arc::new),
        };
        binding.attribute();
        Ok(binding)
    }

    /// The bound state, read-only.
    pub fn view(&self) -> &SceneState {
        &self.state
    }

    /// Cached attribution of the protected semantic owner when it retires inside a container.
    pub fn attributed_bytes(&self) -> usize {
        self.state.attribution()
    }

    /// A snapshot for one worker, attributed the whole state since it may become its last owner.
    pub fn read(&self) -> SceneRead {
        SceneRead {
            state: Deferred::new(Arc::clone(&*self.state), self.state.attribution()),
        }
    }

    /// Installs a worker's candidate after every check passes, giving back its snapshot.
    /// # Errors
    /// In order: `WrongOwner` for another scene's snapshot, `WrongContext`, `WrongOwner`,
    /// `WrongRevision` or `WrongMutation` for a stale candidate, `OverBudget` against the current
    /// budget, and `NotUnique` while another snapshot lives. Each leaves everything unchanged.
    pub fn apply_detached(
        &mut self,
        read: SceneRead,
        candidate: DetachedCandidate,
        global: &mut GlobalBudget,
    ) -> Result<Preparation, BindingError> {
        if !Arc::ptr_eq(&*read.state, &*self.state) {
            return Err(BindingError::WrongOwner);
        }
        // Only a decrement: this binding still owns the state.
        drop(read.state.into_inner());
        self.state
            .check_context(global)
            .map_err(|_| BindingError::WrongContext)?;
        if !Arc::ptr_eq(&candidate.owner, &self.state.identity) {
            return Err(BindingError::WrongOwner);
        }
        if candidate.revision != self.state.revision {
            return Err(BindingError::WrongRevision);
        }
        if candidate.mutation != self.state.mutation {
            return Err(BindingError::WrongMutation);
        }
        global
            .check(self.state.bound_usage(), candidate.usage)
            .map_err(BindingError::OverBudget)?;
        let state = Arc::get_mut(&mut *self.state).ok_or(BindingError::NotUnique)?;
        let (preparation, created) = state.install(candidate, global);
        state.reserved.extend(created.into_iter().map(|(id, _)| id));
        self.attribute();
        Ok(preparation)
    }

    /// Applies an acknowledged cut through the real budget.
    /// # Errors
    /// `WrongContext`, `NotUnique`, or the cut `Rejected` with nothing changed.
    pub fn stop(
        &mut self,
        cut: &StopCut,
        global: &mut GlobalBudget,
    ) -> Result<StopOutcome, BindingError> {
        let outcome = self
            .unique(global)?
            .stop(cut, global)
            .map_err(BindingError::Rejected)?;
        self.attribute();
        Ok(outcome)
    }

    /// Retires a kept step by zero-based lifetime index through the real budget.
    /// # Errors
    /// `WrongContext`, `NotUnique`, or an unknown index `Rejected` with nothing changed.
    pub fn retire(&mut self, index: u32, global: &mut GlobalBudget) -> Result<(), BindingError> {
        self.unique(global)?
            .retire(index, global)
            .map_err(BindingError::Rejected)?;
        self.attribute();
        Ok(())
    }

    /// Subtracts exactly the usage this binding charged, then retires the state on the dropper.
    /// # Errors
    /// `WrongContext` with the budget unchanged.
    pub fn release(self, global: &mut GlobalBudget) -> Result<(), BindingError> {
        self.state
            .check_context(global)
            .map_err(|_| BindingError::WrongContext)?;
        global.replace(self.state.bound_usage(), Usage::default());
        Ok(())
    }

    fn unique(&mut self, global: &GlobalBudget) -> Result<&mut SceneState, BindingError> {
        self.state
            .check_context(global)
            .map_err(|_| BindingError::WrongContext)?;
        Arc::get_mut(&mut *self.state).ok_or(BindingError::NotUnique)
    }

    fn attribute(&mut self) {
        let bytes = heap::ARC_HEADER + size_of::<SceneState>() + self.state.attributed_bytes();
        self.state.attribute(bytes);
    }
}

impl SceneRead {
    /// Admits a step against this snapshot alone; every global cap waits for the binding.
    /// # Errors
    /// The ordinary admission errors, and `DuplicateStepConflict` for a known step id, since a
    /// mirror never receives retransmissions.
    pub fn admit_detached(&self, step: &Step) -> Result<DetachedCandidate, StepError> {
        self.state
            .admit_local(step, None, super::WorldCheck::Skip)?
            .ok_or_else(|| {
                StepError::in_step(
                    &step.id,
                    FieldPath::default().field("id"),
                    ErrorCode::DuplicateStepConflict,
                )
            })
    }
}
