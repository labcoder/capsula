//! The parent-bound root: the shared stages, the instance attempt and the process services,
//! settled in one order so nothing that started outlives custody. Admission, the stage driver
//! and serving aren't wired yet.

use std::sync::Arc;

use kumochi_protocol::constants::BRAIN_LOG;

use super::instance::{InstanceAttempt, InstanceStore};
use super::{Checkpoint, OpenedState, Stages, VaultHeld};
use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::platform::SessionLock;
use crate::runtime::{Shutdown, Stage, account::Accounts};
use crate::{BrainError, StartMode};

/// A parent-bound prerequisite this build lacks. Known to be missing: the vault's credential
/// store, containment, custody lent to the opened state and R2's own list; that list may not be
/// complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("parent-bound startup isn't ready")]
pub(in crate::startup) struct NotReady;

/// What parent-bound startup takes from outside the shared stages.
pub(in crate::startup) trait Prerequisites {
    /// Refuses until every prerequisite is in place, before any process effect.
    fn ready(&self) -> Result<(), NotReady>;
    /// Starts the process-wide services once the claim is committed.
    fn init(&mut self) -> Result<ProcessInit, BrainError>;
    /// Sets `acquired` once it holds a store, even if it fails after.
    fn open(&mut self, acquired: &mut bool) -> Result<OpenedState, BrainError>;
    fn accounts(
        &mut self,
        oauth: OAuthTuning,
        tokens: TokenTuning,
        profile: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError>;
}

/// What `init` started, held until the root has settled.
#[derive(Default)]
pub(in crate::startup) struct ProcessInit {
    /// A test's thread-scoped subscriber; production installs the global one and holds nothing.
    #[cfg(test)]
    pub(in crate::startup) _scoped: Option<tracing::subscriber::DefaultGuard>,
}

/// This build's prerequisites: not ready on any platform, so nothing after `ready` runs.
pub(in crate::startup) struct Pending;

impl Prerequisites for Pending {
    fn ready(&self) -> Result<(), NotReady> {
        Err(NotReady)
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        crate::logging::init(BRAIN_LOG)
            .map_err(|_| BrainError::NativeRuntime(std::io::ErrorKind::Unsupported.into()))?;
        kumochi_scene::deferred::start()?;
        Ok(ProcessInit::default())
    }

    fn open(&mut self, acquired: &mut bool) -> Result<OpenedState, BrainError> {
        super::open_state(StartMode::Normal, &std::env::current_exe()?, acquired)
    }

    fn accounts(
        &mut self,
        oauth: OAuthTuning,
        tokens: TokenTuning,
        profile: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        crate::accounts(oauth, tokens, profile)
    }
}

/// What each stage was seen to do when the root settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::startup) struct Settled {
    pub(in crate::startup) runtime: Shutdown,
    /// The instance attempt's settlement; Confirmed when there was none.
    pub(in crate::startup) item: Stage,
}

/// Owns everything a parent-bound start makes, from its first stage to its settlement.
pub(in crate::startup) struct Root<S: InstanceStore> {
    stages: Stages,
    /// The attempt and the custody it holds, set before its first publish.
    instance: Option<InstanceAttempt<S>>,
    init: Option<ProcessInit>,
}

impl<S: InstanceStore> Root<S> {
    pub(in crate::startup) fn new() -> Self {
        Self {
            stages: Stages::default(),
            instance: None,
            init: None,
        }
    }

    /// Starts the process services, then the shared stages, passing `checkpoint` after each.
    /// Whatever started stays here on an error, for `settle`.
    pub(in crate::startup) fn start(
        &mut self,
        pre: &mut impl Prerequisites,
        mut checkpoint: impl FnMut(Checkpoint, &mut Stages) -> Result<(), BrainError>,
    ) -> Result<(), BrainError> {
        self.init = Some(pre.init()?);
        let opened = pre.open(&mut self.stages.acquired)?;
        self.stages
            .start(
                opened,
                |oauth, tokens, profile| pre.accounts(oauth, tokens, profile),
                &mut checkpoint,
            )
            .map(drop)
    }

    /// Closes the Brain, or whatever started without one; then settles the instance attempt
    /// under custody, drops custody, and only then the process services.
    pub(in crate::startup) async fn settle(self) -> Settled {
        let Self {
            mut stages,
            mut instance,
            init: _init,
        } = self;
        let runtime = match stages.brain.take() {
            Some(brain) => brain.close().await,
            None => stages.close_unbuilt().await,
        };
        drop(stages);
        let item = match instance.as_mut() {
            Some(attempt) => attempt.settle().await,
            None => Stage::Confirmed,
        };
        drop(instance);
        Settled { runtime, item }
    }
}

#[cfg(test)]
mod tests;
