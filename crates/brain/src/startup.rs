//! Detached startup and the lifetime of its process-wide resources.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::SHELL_EXE;
use kumochi_protocol::message::{CAPABILITY_LEN, Capability, Incarnation, RunId, ThreadId};

use crate::config::Config;
use crate::config::tuning::{OAuthTuning, TokenTuning};
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
use crate::connections::model::text_observation::Observation;
use crate::ipc::{self, Identity, Pipes};
use crate::platform::{self, Job, Mode, SessionLock, native_workers};
use crate::runtime::{Brain, Quit, Selected, Shutdown, Stage, account::Accounts};
use crate::store::{self, SceneState, Store};
use crate::{
    BrainError, STARTUP_LOG_TARGET, StartMode, accounts, credential, engine, serve, start_screen,
};
use instance::InstanceAttempt;
#[expect(dead_code, reason = "parent-bound startup is not wired yet")]
pub(crate) mod custody;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound startup is not wired yet")
)]
pub(crate) mod instance;
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
}

struct Lifecycle {
    // Before publication, retain the old passive unwind order: Job, Brain, store, the profile
    // lock, then pipes. The store and credential workers keep their own references to the lock.
    jobs: Option<Arc<Job>>,
    brain: Option<Brain>,
    opened: Option<OpenedState>,
    profile: Option<Arc<SessionLock>>,
    exe: PathBuf,
    pipes: Pipes,
    session: u32,
    published: bool,
    /// The parent-bound item and the custody it holds, settled by `finish` before custody drops.
    #[expect(dead_code, reason = "parent-bound startup is not wired yet")]
    instance: Option<InstanceAttempt<platform::InstanceItem>>,
}

pub(super) async fn run_admitted(
    admission: Result<StartMode, BrainError>,
    #[cfg(test)] starting: impl FnOnce(),
) -> Result<(), BrainError> {
    let mode = admission?;
    #[cfg(test)]
    starting();
    let lifecycle = Lifecycle::acquire(platform::session_id()?)?;
    let opened = open_state(mode, &lifecycle.exe)?;
    let mut lifecycle = lifecycle.start(Mode::Detached, opened, accounts, |_, _| Ok(()))?;
    let identity = lifecycle.publish_capability()?;
    tracing::info!(target: STARTUP_LOG_TARGET, incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let brain = lifecycle.brain.as_mut().expect("started brain");
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
            jobs: None,
            brain: None,
            opened: None,
            profile: None,
            exe,
            pipes,
            session,
            published: false,
            instance: None,
        })
    }

    /// Settles the instance attempt on every exit; custody drops with it, after.
    #[expect(dead_code, reason = "parent-bound startup is not wired yet")]
    async fn finish(mut self) -> Stage {
        match self.instance.as_mut() {
            Some(attempt) => attempt.settle().await,
            None => Stage::Confirmed,
        }
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
        mut checkpoint: impl FnMut(Checkpoint, &mut Self) -> Result<(), BrainError>,
    ) -> Result<Self, BrainError> {
        self.profile = Some(Arc::clone(&opened.profile));
        self.opened = Some(opened);
        checkpoint(Checkpoint::Open, &mut self)?;
        let opened = self.opened.as_mut().expect("opened state");
        let claude_setting = native_workers::claude_setting(&opened.config, mode);
        let (account, vault_held) = start_accounts(opened, accounts)?;
        checkpoint(Checkpoint::Accounts, &mut self)?;
        self.brain = Some(build_brain(
            self.opened.take().expect("opened state"),
            account,
        )?);
        checkpoint(Checkpoint::Brain, &mut self)?;
        // shortcut: a parent loss during the removal pass doesn't stop the Job from starting; fence
        // the Job's creation when containment lands.
        let (jobs, workers) = native_workers::prepare(
            self.brain.as_mut().expect("built brain"),
            claude_setting,
            mode,
        )?;
        self.jobs = Some(jobs);
        checkpoint(Checkpoint::Helpers, &mut self)?;
        // shortcut: a parent loss after voice starts doesn't stop the Claude lane or the screen
        // worker; fence each start when containment lands.
        let brain = self.brain.as_mut().expect("built brain");
        let jobs = self.jobs.as_ref().expect("started helpers");
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
        let brain = self.brain.take().expect("serving brain");
        let runtime = if let Ok(Some(deadline)) = ended {
            brain.quit(*deadline).await
        } else {
            brain.close().await
        };
        // Ending the job waits on the processes, so it runs off the async thread.
        let jobs = self.jobs.take().expect("serving helpers");
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

/// A boundary between startup units, named for the unit before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkpoint {
    Open,
    Accounts,
    Brain,
    Helpers,
}

fn open_state(mode: StartMode, exe: &Path) -> Result<OpenedState, BrainError> {
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
    OpenedState::load(&store::default_path()?, platform::config_dir, replay)
}

impl OpenedState {
    fn load(
        path: &Path,
        config_dir: impl FnOnce() -> Option<PathBuf>,
        replay: Option<Selected>,
    ) -> Result<Self, BrainError> {
        // Every writer of the store and the credential authority holds this lock for its whole run,
        // so a brain of this user in another session stops here. Before any shell is served, so no
        // run starts ahead of the recovery scan. Blocking is fine: nothing else runs on the runtime yet.
        let (profile, mut store) = super::open_profile(path)?;
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

fn build_brain(opened: OpenedState, account: Accounts) -> Result<Brain, BrainError> {
    let OpenedState {
        profile,
        store,
        main_thread,
        high_water,
        scenes,
        config,
        replay,
    } = opened;
    Brain::new(
        store,
        profile,
        main_thread,
        high_water,
        scenes,
        match replay {
            Some(replay) => replay,
            None => engine(config.model_tuning())?,
        },
        config,
        account,
        crate::runtime::IssueEpochs::default(),
    )
    .map_err(BrainError::StoreThread)
}
