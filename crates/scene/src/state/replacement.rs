//! One checked transition from a frozen victim set to a new scene.

use super::{Admission, Admitted, Committed, SceneState, StaleAdmission};
use crate::{
    Budget, ErrorCode, GlobalBudget, Step, StepError, budget::Usage, limits::MAX_VISIBLE_SCENES,
};
use std::{fmt, sync::Arc};

/// Whether a durable replacement installs its newcomer after settling the victims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementDisposition {
    /// Install the admitted newcomer.
    Install,
    /// Settle only the victims, discarding the newcomer.
    Discard,
}

/// An owned first-step admission against the surviving global set. Dropping it changes nothing.
pub struct ReplacementAdmission {
    state: SceneState,
    admitted: Admitted,
    victims: Vec<Victim>,
    removed: Usage,
}

struct Victim {
    owner: Arc<()>,
    revision: u32,
    mutation: u64,
    usage: Usage,
}

impl ReplacementAdmission {
    /// Checks a new scene and its first step without changing any victim or global charge.
    /// Repeated victim identities are charged only once.
    /// # Errors
    /// Refuses non-new scenes, foreign victims, excessive victim counts and ordinary admission failures.
    pub fn checked(
        state: SceneState,
        step: &Step,
        victims: &[&SceneState],
        global: &GlobalBudget,
    ) -> Result<Self, StepError> {
        if state.revision != 0 || state.context.is_some() || state.cleared {
            return Err(StepError::new(ErrorCode::BudgetContext));
        }
        if victims.len() > MAX_VISIBLE_SCENES {
            return Err(StepError::new(ErrorCode::OverBudget(Budget::VisibleScenes)));
        }
        let mut frozen: Vec<Victim> = Vec::with_capacity(victims.len());
        let mut removed = Usage::default();
        for victim in victims {
            if victim.cleared
                || victim
                    .context
                    .as_ref()
                    .is_none_or(|id| !Arc::ptr_eq(id, &global.identity))
            {
                return Err(StepError::new(ErrorCode::BudgetContext));
            }
            if frozen
                .iter()
                .any(|old| Arc::ptr_eq(&old.owner, &victim.identity))
            {
                continue;
            }
            removed = removed.add(victim.usage);
            frozen.push(Victim {
                owner: Arc::clone(&victim.identity),
                revision: victim.revision,
                mutation: victim.mutation,
                usage: victim.usage,
            });
        }
        let projected = global.without(removed);
        let Admission::New(admitted) = state.admit_checked(step, &projected)? else {
            return Err(StepError::new(ErrorCode::DuplicateStepConflict));
        };
        Ok(Self {
            state,
            admitted,
            victims: frozen,
            removed,
        })
    }

    /// Consumes a durable replacement's admission, validating every stamp before mutation.
    /// Discarding settles only the victims; the new scene never acquires a charge.
    /// # Errors
    /// A stale or mismatched state/budget leaves all live states and charges unchanged.
    /// # Panics
    /// Panics if a victim exhausts its process-local mutation counter.
    pub fn settle(
        mut self,
        victims: &mut [&mut SceneState],
        global: &mut GlobalBudget,
        disposition: ReplacementDisposition,
    ) -> Result<Option<(SceneState, Committed)>, StaleAdmission> {
        if !self.state.admission_matches(&self.admitted, global)
            || victims.len() != self.victims.len()
            || victims.iter().zip(&self.victims).any(|(state, frozen)| {
                state.cleared
                    || !Arc::ptr_eq(&state.identity, &frozen.owner)
                    || state.revision != frozen.revision
                    || state.mutation != frozen.mutation
                    || state.usage != frozen.usage
                    || state
                        .context
                        .as_ref()
                        .is_none_or(|id| !Arc::ptr_eq(id, &global.identity))
            })
        {
            return Err(StaleAdmission);
        }
        let mut next = global.without(self.removed);
        let committed = if disposition == ReplacementDisposition::Install {
            Some(self.state.commit(self.admitted, &mut next)?)
        } else {
            next.advance();
            None
        };
        for state in victims {
            state.clear_contents();
            state.usage = Usage::default();
            state.mutation = state
                .mutation
                .checked_add(1)
                .expect("scene mutations cannot exhaust u64 within a process lifetime");
        }
        *global = next;
        Ok(committed.map(|committed| (self.state, committed)))
    }
}

impl fmt::Debug for ReplacementAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplacementAdmission")
            .field("victims", &self.victims.len())
            .finish_non_exhaustive()
    }
}
