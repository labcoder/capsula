//! The parent-bound start: admission, the stage driver, serving and settlement, through one root
//! that owns the stages, the instance attempt and the process services and settles them in one
//! order, so nothing that started outlives custody. This build's prerequisites are never ready,
//! so production refuses at `ready` after a real admission.

use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::constants::BRAIN_LOG;

use kumochi_protocol::message::{CAPABILITY_LEN, Incarnation, IssueSession};
use tokio::sync::watch;
use tokio::time::Instant;

use super::custody::{CustodyError, SessionCustody};
use super::instance::{InstanceAttempt, InstanceStore, OwnedKey};
use super::{Checkpoint, OpenedState, Stages, VaultHeld};
use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::platform::shell_process::Committed;
use crate::platform::{self, Job, SessionLock, ShellProcess};
use crate::runtime::{Brain, Ended, Quit, Shutdown, Stage, Terminal, account::Accounts};
use crate::{BrainError, ParentBoundError, StartMode};

/// How long admission and the stages have, from before the first bootstrap action. Settlement
/// has no admission deadline.
pub(crate) const ADMISSION: Duration = Duration::from_secs(10);

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
    /// The helpers' containment, or None where there's none; workers that need it don't start.
    fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError>;
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

    fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError> {
        Err(BrainError::Helpers(std::io::ErrorKind::Unsupported.into()))
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
    /// The greeted pair's cell, set once either leg refuses.
    fn cell(greeted: &Self::Greeted) -> watch::Receiver<bool>;
    /// `ShellTuning` by `by`, written only while the parent and the clock both still pass, then
    /// the runtime on both legs, lent by `greeted`, with `terminal`, until it ends. The legs stay
    /// open until the root drops `greeted`.
    fn serve(
        greeted: &mut Self::Greeted,
        brain: &mut Brain,
        terminal: Terminal,
        shell: &ShellProcess,
        by: Instant,
    ) -> impl Future<Output = Ended>;
}

/// Starts a parent-bound brain on `pair` with this build's prerequisites, which refuse at
/// `ready`, so production stops after a real admission.
pub(crate) async fn run_production(
    pair: platform::InheritedPair,
    shell: &ShellProcess,
    by: Instant,
) -> Result<(), ParentBoundError> {
    Box::pin(run::<platform::InstanceItem, _>(
        pair,
        shell,
        by,
        &mut Pending,
        SessionCustody::bootstrap,
        platform::InstanceItem::admit,
    ))
    .await
}

/// Admits, stages and serves on one root, then settles it whatever happened, and maps the two.
async fn run<S: InstanceStore, P: AdmissionPair>(
    pair: P,
    shell: &ShellProcess,
    by: Instant,
    pre: &mut impl Prerequisites,
    custody: impl FnOnce() -> Result<SessionCustody, CustodyError>,
    store: impl FnOnce() -> Result<S, S::Error>,
) -> Result<(), ParentBoundError> {
    let mut root = Root::<S>::new();
    let (served, greeted) = root.serve(pair, shell, by, pre, custody, store).await;
    let quit = match served {
        Ok(Ended::Quit { deadline }) => Some(deadline),
        _ => None,
    };
    exit(&served, root.settle(quit, greeted).await)
}

/// 0 only for an authenticated, latched Quit that completed with every stage Confirmed. Any
/// stage short of Confirmed, or an incomplete Quit, is Incomplete over every other result; any
/// other end of serving, a lost shell included, is Refused.
fn exit(
    served: &Result<Ended, ParentBoundError>,
    settled: Settled,
) -> Result<(), ParentBoundError> {
    let Shutdown {
        quit,
        store,
        settings,
        claude,
        account,
    } = settled.runtime;
    let confirmed = [store, settings, claude, account, settled.item]
        .iter()
        .all(|stage| *stage == Stage::Confirmed);
    if !confirmed || quit == Some(Quit::Incomplete) {
        return Err(ParentBoundError::Incomplete);
    }
    match served {
        Ok(Ended::Quit { .. }) if quit == Some(Quit::Complete) => Ok(()),
        Ok(_) => Err(ParentBoundError::Refused),
        Err(refused) => Err(*refused),
    }
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

    /// Admits, then runs the stages, then serves until the runtime ends. Returns the greeted pair
    /// too, once there is one, so its legs close only at settlement.
    async fn serve<P: AdmissionPair>(
        &mut self,
        pair: P,
        shell: &ShellProcess,
        by: Instant,
        pre: &mut impl Prerequisites,
        custody: impl FnOnce() -> Result<SessionCustody, CustodyError>,
        store: impl FnOnce() -> Result<S, S::Error>,
    ) -> (Result<Ended, ParentBoundError>, Option<P::Greeted>) {
        let (mut greeted, committed) = match self.admit(pair, shell, by, custody, store).await {
            Ok(admitted) => admitted,
            Err(refused) => return (Err(refused), None),
        };
        if let Err(refused) = self.stage(pre, shell, by, &committed) {
            return (Err(refused), Some(greeted));
        }
        let terminal = Terminal::new(P::cell(&greeted), shell.loss());
        let brain = self
            .stages
            .brain
            .as_mut()
            .expect("the stages built the brain");
        let ended = P::serve(&mut greeted, brain, terminal, shell, by).await;
        (Ok(ended), Some(greeted))
    }

    /// Runs the stages for a committed admission, each behind a fence and the clock: on entry,
    /// again once the prerequisites are ready and before anything starts, and at each
    /// checkpoint. A refusal keeps what started for `settle`. Voice never starts here: this mode
    /// builds no models, and the helpers start only what their containment allows.
    ///
    /// # Errors
    /// `Unsupported` when the prerequisites aren't ready, otherwise `Refused`.
    fn stage(
        &mut self,
        pre: &mut impl Prerequisites,
        shell: &ShellProcess,
        by: Instant,
        _: &Committed,
    ) -> Result<(), ParentBoundError> {
        let fence = || -> Result<(), BrainError> {
            let refused =
                || BrainError::NativeRuntime(std::io::ErrorKind::ConnectionAborted.into());
            shell.fence().map_err(|_| refused())?;
            if Instant::now() >= by {
                return Err(refused());
            }
            Ok(())
        };
        fence().map_err(|_| ParentBoundError::Refused)?;
        pre.ready().map_err(|_| ParentBoundError::Unsupported)?;
        fence().map_err(|_| ParentBoundError::Refused)?;
        self.start(pre, |_, _| fence())
            .map_err(|_| ParentBoundError::Refused)?;
        self.stages.jobs = pre.containment().map_err(|_| ParentBoundError::Refused)?;
        fence().map_err(|_| ParentBoundError::Refused)
    }

    /// Quits the Brain by `quit` after an accepted Quit, closes it otherwise, or closes whatever
    /// started without one; drops the helpers, then `pair`, so the shell sees EOF only after
    /// those; then settles the instance attempt under custody, drops custody, and only then the
    /// process services.
    pub(crate) async fn settle(self, quit: Option<Instant>, pair: impl Sized) -> Settled {
        let Self {
            admitting: _,
            mut stages,
            mut instance,
            init: _init,
        } = self;
        let runtime = match (stages.brain.take(), quit) {
            (Some(brain), Some(deadline)) => brain.quit(deadline).await,
            (Some(brain), None) => brain.close().await,
            (None, _) => stages.close_unbuilt().await,
        };
        drop(stages);
        drop(pair);
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
pub(crate) mod tests;
