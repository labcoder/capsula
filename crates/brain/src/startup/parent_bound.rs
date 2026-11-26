//! The parent-bound root: the shared stages, the instance attempt and the process services,
//! settled in one order so nothing that started outlives custody, and its admission. The entry
//! doesn't run admission yet, and the stage driver and serving aren't written.

use std::sync::Arc;

use kumochi_protocol::constants::BRAIN_LOG;

use kumochi_protocol::message::{CAPABILITY_LEN, Incarnation, IssueSession};
use tokio::time::Instant;

use super::custody::{CustodyError, SessionCustody};
use super::instance::{InstanceAttempt, InstanceStore, OwnedKey};
use super::{Checkpoint, OpenedState, Stages, VaultHeld};
use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::platform::shell_process::Committed;
use crate::platform::{SessionLock, ShellProcess};
use crate::runtime::{Shutdown, Stage, account::Accounts};
use crate::{BrainError, ParentBoundError, StartMode};

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
pub(crate) struct Settled {
    pub(in crate::startup) runtime: Shutdown,
    /// The instance attempt's settlement; Confirmed when there was none.
    pub(crate) item: Stage,
}

/// The inherited pair as admission drives it: the platform's own, or a test's.
pub(crate) trait AdmissionPair {
    type Keyed;
    type Greeted;
    /// Keys the responder from `key`; the caller wipes its own copy right after.
    fn key(self, key: &[u8; CAPABILITY_LEN]) -> Option<Self::Keyed>;
    /// READY, the handshake and the greeting by `by`; `reserve` reserves Welcome's session.
    fn admit(
        keyed: Self::Keyed,
        shell: &ShellProcess,
        by: Instant,
        reserve: impl FnOnce() -> IssueSession,
    ) -> impl Future<Output = Option<Self::Greeted>>;
}

/// Owns everything a parent-bound start makes, from its first stage to its settlement.
pub(crate) struct Root<S: InstanceStore> {
    /// Set by the first `admit`, before anything else it does, so admission runs once.
    admitting: bool,
    stages: Stages,
    /// The attempt and the custody it holds, set before its first publish.
    instance: Option<InstanceAttempt<S>>,
    init: Option<ProcessInit>,
}

impl<S: InstanceStore> Root<S> {
    pub(crate) fn new() -> Self {
        Self {
            admitting: false,
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

    /// Admits the parent shell on `pair` by `by`: a fence, custody, a fence again, the item,
    /// the key, READY, the handshake and the greeting, then the commit claim. It runs once: a
    /// later call refuses without touching the root. The attempt is in the root before its first
    /// item call, so settlement finds whatever a refusal or a dropped call left.
    ///
    /// # Errors
    /// `Busy` when another session holds custody, otherwise `Refused`.
    pub(crate) async fn admit<P: AdmissionPair>(
        &mut self,
        pair: P,
        shell: &ShellProcess,
        by: Instant,
        custody: impl FnOnce() -> Result<SessionCustody, CustodyError>,
        store: impl FnOnce() -> Result<S, S::Error>,
    ) -> Result<(P::Greeted, Committed), ParentBoundError> {
        if std::mem::replace(&mut self.admitting, true) {
            return Err(ParentBoundError::Refused);
        }
        let fence = || -> Result<(), ParentBoundError> {
            shell.fence().map_err(|_| ParentBoundError::Refused)?;
            if Instant::now() >= by {
                return Err(ParentBoundError::Refused);
            }
            Ok(())
        };
        fence()?;
        let custody = custody().map_err(|error| match error {
            CustodyError::Busy => ParentBoundError::Busy,
            _ => ParentBoundError::Refused,
        })?;
        fence()?;
        let store = Arc::new(store().map_err(|_| ParentBoundError::Refused)?);
        let attempt = self.instance.insert(InstanceAttempt::new(custody, store));
        let key = OwnedKey::generate().map_err(|_| ParentBoundError::Refused)?;
        attempt
            .publish(key.key(), by.into_std(), shell)
            .await
            .map_err(|_| ParentBoundError::Refused)?;
        let keyed = pair.key(key.key()).ok_or(ParentBoundError::Refused)?;
        drop(key);
        let incarnation = Incarnation(getrandom::u64().map_err(|_| ParentBoundError::Refused)?);
        let epochs = &mut self.stages.epochs;
        let greeted = P::admit(keyed, shell, by, || epochs.reserve(incarnation))
            .await
            .ok_or(ParentBoundError::Refused)?;
        // No await from here to the claim, so a loss either lands before it or loses to it.
        fence()?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_CLAIM.take() {
            hook();
        }
        let committed = shell.claim_commit().ok_or(ParentBoundError::Refused)?;
        Ok((greeted, committed))
    }

    /// Closes the Brain, or whatever started without one; then settles the instance attempt
    /// under custody, drops custody, and only then the process services.
    pub(crate) async fn settle(self) -> Settled {
        let Self {
            admitting: _,
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
thread_local! {
    /// Runs once between admission's last fence and its claim, on the test's own thread.
    pub(crate) static BEFORE_CLAIM: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        std::cell::Cell::new(None);
}

#[cfg(test)]
mod tests;
