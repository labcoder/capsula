//! Detached startup and the lifetime of its process-wide resources.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::{SHELL_EXE, STORE_FILE};
use kumochi_protocol::message::{CAPABILITY_LEN, Capability, Incarnation, RunId, ThreadId};

use crate::config::Config;
use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::connections::model::budget::Budget;
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
use crate::connections::model::text_observation::Observation;
use crate::ipc::{self, Identity, Pipes};
use crate::platform::{self, Job, Mode, SessionLock, native_workers};
use crate::runtime::{Brain, Quit, Selected, Shutdown, Stage, account::Accounts};
use crate::store::{self, SceneState, Store};
use crate::{
    BrainError, STARTUP_LOG_TARGET, StartMode, accounts, credential, engine, serve, start_screen,
};
pub(crate) mod custody;
pub(crate) mod instance;
pub(crate) mod parent_bound;
#[cfg(all(test, windows))]
mod tests;

type VaultHeld = Arc<AtomicBool>;

struct OpenedState {
    /// The profile's writer lock, taken before the store opened.
    profile: Arc<SessionLock>,
    store: Store,
    main_thread: ThreadId,
    high_water: (ThreadId, RunId),
    scenes: Vec<SceneState>,
    config: Config,
    replay: Option<Selected>,
    /// The model requests the account engine may send.
    budget: Budget,
}

/// What the shared startup stages made so far; every mode's root owns one.
#[derive(Default)]
struct Stages {
    // Before publication, retain the old passive unwind order: Job, Brain, accounts, store, the
    // profile lock. The store and credential workers keep their own references to the lock.
    jobs: Option<Arc<Job>>,
    brain: Option<Brain>,
    /// Started accounts until the Brain takes them, so a failure before it can still close them.
    accounts: Option<Accounts>,
    opened: Option<OpenedState>,
    profile: Option<Arc<SessionLock>>,
    /// A store may have opened for these stages, whether or not one is still here.
    acquired: bool,
    /// The issue sessions reserved so far, which the Brain goes on from.
    epochs: crate::runtime::IssueEpochs,
}

struct Lifecycle {
    stages: Stages,
    exe: PathBuf,
    /// After the stages, so the pipes still go last.
    pipes: Pipes,
    session: u32,
    published: bool,
}

pub(super) async fn run_admitted(
    admission: Result<StartMode, BrainError>,
    #[cfg(test)] starting: impl FnOnce(),
) -> Result<(), BrainError> {
    let mode = admission?;
    #[cfg(test)]
    starting();
    let lifecycle = Lifecycle::acquire(platform::session_id()?)?;
    let opened = open_state(mode, &lifecycle.exe, &mut false)?;
    let mut lifecycle = lifecycle.start(Mode::Detached, opened, accounts, |_, _| Ok(()))?;
    let identity = lifecycle.publish_capability()?;
    tracing::info!(target: STARTUP_LOG_TARGET, incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let brain = lifecycle.stages.brain.as_mut().expect("started brain");
    let ended = serve(&mut lifecycle.pipes, &identity, brain).await;
    let settlement = lifecycle
        .settle(&ended, Job::shut_down, credential::delete)
        .await;
    detached_exit(ended, settlement)
}

/// What each lifecycle stage was seen to do after serving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settlement {
    runtime: Shutdown,
    /// Ending the helpers' Job, which has no deadline of its own.
    helpers: Stage,
    /// Deleting the published capability, which has none either; Confirmed when nothing was
    /// published.
    item: Stage,
}

/// Detached keeps the result of serving, whatever settled after it.
fn detached_exit(
    ended: Result<Option<tokio::time::Instant>, ipc::IpcError>,
    settlement: Settlement,
) -> Result<(), BrainError> {
    match (ended, settlement.runtime.quit) {
        (Ok(_), None | Some(Quit::Complete | Quit::Incomplete)) => Ok(()),
        (Err(error), _) => Err(error.into()),
    }
}

impl Lifecycle {
    fn acquire(session: u32) -> Result<Self, BrainError> {
        // The pipes come first: a second brain fails here, before it touches anything else.
        let pipes = Pipes::create(session)?;
        let exe = std::env::current_exe()?;
        Ok(Self {
            stages: Stages::default(),
            exe,
            pipes,
            session,
            published: false,
        })
    }

    /// Each checkpoint admits the work up to the next one; Detached passes a no-op.
    fn start(
        mut self,
        mode: Mode,
        opened: OpenedState,
        accounts: impl FnOnce(
            OAuthTuning,
            TokenTuning,
            Arc<SessionLock>,
        ) -> Result<(Accounts, VaultHeld), BrainError>,
        mut checkpoint: impl FnMut(Checkpoint, &mut Stages) -> Result<(), BrainError>,
    ) -> Result<Self, BrainError> {
        let mut claude_setting = None;
        let vault_held =
            self.stages
                .start(opened, accounts, &mut |point, stages: &mut Stages| {
                    checkpoint(point, stages)?;
                    if point == Checkpoint::Open {
                        let opened = stages.opened.as_ref().expect("opened state");
                        claude_setting = Some(native_workers::claude_setting(&opened.config, mode));
                    }
                    Ok(())
                })?;
        let claude_setting = claude_setting.expect("read after the opened state");
        let stages = &mut self.stages;
        // shortcut: a parent loss during the removal pass doesn't stop the Job from starting; fence
        // the Job's creation when containment lands.
        let (jobs, workers) = native_workers::prepare(
            stages.brain.as_mut().expect("built brain"),
            claude_setting,
            mode,
        )?;
        stages.jobs = Some(jobs);
        checkpoint(Checkpoint::Helpers, stages)?;
        // shortcut: a parent loss after voice starts doesn't stop the Claude lane or the screen
        // worker; fence each start when containment lands.
        let brain = self.stages.brain.as_mut().expect("built brain");
        let jobs = self.stages.jobs.as_ref().expect("started helpers");
        native_workers::start(workers, brain, vault_held, &self.exe, jobs);
        start_screen(brain, self.exe.clone(), Ok(Arc::clone(jobs)));
        Ok(self)
    }

    fn publish_capability(&mut self) -> Result<Identity, BrainError> {
        let mut bytes = [0; CAPABILITY_LEN];
        getrandom::fill(&mut bytes)?;
        let identity = Identity {
            capability: Capability::new(bytes),
            incarnation: Incarnation(getrandom::u64()?),
            shell_image: self.exe.with_file_name(SHELL_EXE),
        };
        credential::store(self.session, &identity.capability)?;
        self.published = true;
        Ok(identity)
    }

    /// Quits or closes the Brain, ends the helpers with `shut_down` and deletes a published
    /// capability with `delete`; tests pass failing ones.
    async fn settle(
        &mut self,
        ended: &Result<Option<tokio::time::Instant>, ipc::IpcError>,
        shut_down: impl FnOnce(&Job) -> std::io::Result<()> + Send + 'static,
        delete: impl FnOnce(u32) -> Result<(), credential::CredentialError>,
    ) -> Settlement {
        let brain = self.stages.brain.take().expect("serving brain");
        let runtime = if let Ok(Some(deadline)) = ended {
            brain.quit(*deadline).await
        } else {
            brain.close().await
        };
        // Ending the job waits on the processes, so it runs off the async thread.
        let jobs = self.stages.jobs.take().expect("serving helpers");
        let helpers = match tokio::task::spawn_blocking(move || shut_down(&jobs)).await {
            Ok(Ok(())) => Stage::Confirmed,
            Ok(Err(error)) => {
                tracing::warn!(target: STARTUP_LOG_TARGET, %error, "couldn't end the helpers; closing the job ends them");
                Stage::Failed
            }
            Err(error) => {
                tracing::warn!(target: STARTUP_LOG_TARGET, %error, "ending the helpers panicked");
                Stage::Failed
            }
        };
        let item = match std::mem::take(&mut self.published).then(|| delete(self.session)) {
            None | Some(Ok(())) => Stage::Confirmed,
            Some(Err(error)) => {
                tracing::warn!(target: STARTUP_LOG_TARGET, %error, "couldn't delete the instance capability");
                Stage::Failed
            }
        };
        Settlement {
            runtime,
            helpers,
            item,
        }
    }
}

impl Stages {
    /// Opens, starts the accounts and builds the Brain, passing `checkpoint` after each unit.
    /// Whatever started stays here on an error, for its owner to drop or close. Returns the flag
    /// the helpers read while the vault is held.
    fn start(
        &mut self,
        opened: OpenedState,
        accounts: impl FnOnce(
            OAuthTuning,
            TokenTuning,
            Arc<SessionLock>,
        ) -> Result<(Accounts, VaultHeld), BrainError>,
        checkpoint: &mut impl FnMut(Checkpoint, &mut Self) -> Result<(), BrainError>,
    ) -> Result<VaultHeld, BrainError> {
        self.acquired = true;
        self.profile = Some(Arc::clone(&opened.profile));
        self.opened = Some(opened);
        checkpoint(Checkpoint::Open, self)?;
        let (account, vault_held) =
            start_accounts(self.opened.as_mut().expect("opened state"), accounts)?;
        self.accounts = Some(account);
        checkpoint(Checkpoint::Accounts, self)?;
        self.build_brain()?;
        checkpoint(Checkpoint::Brain, self)?;
        Ok(vault_held)
    }

    /// Takes the opened state and the accounts only once nothing before `Brain::new` can fail,
    /// and takes the accounts back when it does.
    fn build_brain(&mut self) -> Result<(), BrainError> {
        let opened = self.opened.as_mut().expect("opened state");
        let engine = match opened.replay.take() {
            Some(replay) => replay,
            None => engine(
                opened.config.model_tuning(),
                std::mem::take(&mut opened.budget),
            )?,
        };
        let OpenedState {
            profile,
            store,
            main_thread,
            high_water,
            scenes,
            config,
            replay: _,
            budget: _,
        } = self.opened.take().expect("opened state");
        match Brain::new(
            store,
            profile,
            main_thread,
            high_water,
            scenes,
            engine,
            config,
            self.accounts.take().expect("started accounts"),
            std::mem::take(&mut self.epochs),
        ) {
            Ok(brain) => {
                self.brain = Some(brain);
                Ok(())
            }
            Err((error, accounts)) => {
                self.accounts = Some(*accounts);
                Err(BrainError::StoreThread(error))
            }
        }
    }

    /// Settles what started when no Brain was built: closes the accounts, which waits for the
    /// credential thread to let the vault go, then drops a raw store. No receipt confirms that
    /// drop, so an opened store is Unconfirmed, even one a failed open or `Brain::new` dropped.
    async fn close_unbuilt(&mut self) -> Shutdown {
        let account = match self.accounts.take() {
            Some(accounts) => crate::runtime::close_unbuilt(accounts).await,
            None => Stage::Confirmed,
        };
        drop(self.opened.take());
        let store = if self.acquired {
            Stage::Unconfirmed
        } else {
            Stage::Confirmed
        };
        Shutdown {
            quit: None,
            store,
            settings: Stage::Confirmed,
            claude: Stage::Confirmed,
            account,
        }
    }
}

/// A boundary between startup units, named for the unit before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkpoint {
    Open,
    Accounts,
    Brain,
    Helpers,
}

fn open_state(mode: StartMode, exe: &Path, acquired: &mut bool) -> Result<OpenedState, BrainError> {
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    Observation::prepare(exe);
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    let replay = (mode == StartMode::Draw)
        .then(super::draw_engine)
        .transpose()?;
    #[cfg(not(all(windows, any(debug_assertions, feature = "gate-draw-replay"))))]
    let replay = {
        let _ = (mode, exe);
        None
    };
    OpenedState::load_marking(
        &store::default_path()?,
        platform::config_dir,
        replay,
        acquired,
    )
}

/// Opens the profile under `profile`, custody's loan, with no second lock: the store, the
/// settings and the authority all come from the folder the loan was taken in. The engine sends
/// within `budget`.
fn open_state_held(
    profile: Arc<SessionLock>,
    replay: Option<Selected>,
    budget: Budget,
    acquired: &mut bool,
) -> Result<OpenedState, BrainError> {
    let folder = profile.data_dir().to_owned();
    let store = folder.join(STORE_FILE);
    OpenedState::open_marking(
        || super::open_held(profile, &store),
        || Some(folder),
        replay,
        budget,
        acquired,
    )
}

impl OpenedState {
    #[cfg(all(test, windows))]
    fn load(
        path: &Path,
        config_dir: impl FnOnce() -> Option<PathBuf>,
        replay: Option<Selected>,
    ) -> Result<Self, BrainError> {
        Self::load_marking(path, config_dir, replay, &mut false)
    }

    /// Sets `acquired` once a store may be open, so its owner can tell a failure after that from
    /// one before.
    fn load_marking(
        path: &Path,
        config_dir: impl FnOnce() -> Option<PathBuf>,
        replay: Option<Selected>,
        acquired: &mut bool,
    ) -> Result<Self, BrainError> {
        // Every writer of the store and the credential authority holds this lock for its whole run,
        // so a brain of this user in another session stops here. Before any shell is served, so no
        // run starts ahead of the recovery scan. Blocking is fine: nothing else runs on the runtime yet.
        Self::open_marking(
            || super::open_profile(path),
            config_dir,
            replay,
            Budget::from_env(),
            acquired,
        )
    }

    /// `load_marking` with the profile and store from `open`, and the engine's replay or budget.
    fn open_marking(
        open: impl FnOnce() -> Result<(Arc<SessionLock>, Store), BrainError>,
        config_dir: impl FnOnce() -> Option<PathBuf>,
        replay: Option<Selected>,
        budget: Budget,
        acquired: &mut bool,
    ) -> Result<Self, BrainError> {
        let (profile, mut store) = open().inspect_err(|error| {
            // Past the profile lock, SQLite may already be open: Unconfirmed only costs a 70.
            *acquired = !matches!(error, BrainError::ProfileBusy | BrainError::ProfileLock(_));
        })?;
        *acquired = true;
        let recovered = store.recover()?;
        tracing::info!(
            target: STARTUP_LOG_TARGET,
            crashed_runs = recovered.crashed_runs,
            finished_deletions = recovered.finished_deletions,
            purge_pending = recovered.purge_pending,
            failed_deletions = recovered.failed_deletions,
            failed_sends = recovered.failed_sends,
            unknown_sends = recovered.unknown_sends,
            "store recovered"
        );
        // Recovery created it if it was missing, so its id is below the high water.
        let main_thread = store.main_thread()?;
        let high_water = store.high_water()?;
        let scenes = store.scenes()?;
        let config = Config::load(config_dir(), main_thread);
        #[cfg(all(windows, feature = "gate-draw-replay", not(debug_assertions)))]
        super::require_draw_model(&config)?;
        Ok(Self {
            profile,
            store,
            main_thread,
            high_water,
            scenes,
            config,
            replay,
            budget,
        })
    }
}

fn start_accounts(
    opened: &mut OpenedState,
    accounts: impl FnOnce(
        OAuthTuning,
        TokenTuning,
        Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError>,
) -> Result<(Accounts, VaultHeld), BrainError> {
    // Before the capability is stored, so its write never overlaps the vault's first reads.
    let (account, vault_held) = accounts(
        opened.config.oauth_tuning(),
        opened.config.token_tuning(),
        Arc::clone(&opened.profile),
    )?;
    opened
        .config
        .count_in_use(&account.view(), opened.high_water.1);
    Ok((account, vault_held))
}
