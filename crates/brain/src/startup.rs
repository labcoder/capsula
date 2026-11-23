//! Detached startup and the lifetime of its process-wide resources.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::SHELL_EXE;
use kumochi_protocol::message::{CAPABILITY_LEN, Capability, Incarnation, RunId, ThreadId};
use kumochi_protocol::voice::VoicePart;

use crate::config::Config;
use crate::config::tuning::{OAuthTuning, TokenTuning};
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
use crate::connections::model::text_observation::Observation;
use crate::ipc::{self, Identity, Pipes};
use crate::platform::{self, Job};
use crate::runtime::{Brain, Quit, Selected, account::Accounts};
use crate::store::{self, SceneState, Store};
use crate::voice::installer::Folder;
use crate::{
    BrainError, STARTUP_LOG_TARGET, StartMode, accounts, credential, engine, serve, start_screen,
};
#[cfg(all(test, windows))]
mod tests;

type VaultHeld = Arc<AtomicBool>;

struct OpenedState {
    store: Store,
    main_thread: ThreadId,
    high_water: (ThreadId, RunId),
    scenes: Vec<SceneState>,
    config: Config,
    replay: Option<Selected>,
}

#[derive(Default)]
struct Optional {
    claude_on: bool,
    models: Option<PathBuf>,
    stuck: Vec<(VoicePart, Folder)>,
}

struct Lifecycle {
    // Before publication, retain the old passive unwind order: Job, Brain, store, then pipes.
    jobs: Option<Arc<Job>>,
    brain: Option<Brain>,
    opened: Option<OpenedState>,
    exe: PathBuf,
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
    let opened = open_state(mode, &lifecycle.exe)?;
    let mut lifecycle = lifecycle.start(
        opened,
        accounts,
        #[cfg(test)]
        |_, _| Ok(()),
    )?;
    let identity = lifecycle.publish_capability()?;
    tracing::info!(target: STARTUP_LOG_TARGET, incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let brain = lifecycle.brain.as_mut().expect("started brain");
    let ended = serve(&mut lifecycle.pipes, &identity, brain).await;
    let quit = lifecycle.settle(&ended).await;
    match (ended, quit) {
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
            exe,
            pipes,
            session,
            published: false,
        })
    }

    fn start(
        mut self,
        opened: OpenedState,
        accounts: impl FnOnce(OAuthTuning, TokenTuning) -> Result<(Accounts, VaultHeld), BrainError>,
        #[cfg(test)] mut checkpoint: impl FnMut(Checkpoint, &mut Self) -> Result<(), BrainError>,
    ) -> Result<Self, BrainError> {
        self.opened = Some(opened);
        #[cfg(test)]
        checkpoint(Checkpoint::Open, &mut self)?;
        let mut optional = Optional::default();
        let (account, vault_held) = start_accounts(
            self.opened.as_mut().expect("opened state"),
            &mut optional,
            accounts,
        )?;
        self.brain = Some(build_brain(
            self.opened.take().expect("opened state"),
            account,
        )?);
        #[cfg(test)]
        checkpoint(Checkpoint::Brain, &mut self)?;
        let brain = self.brain.as_mut().expect("built brain");
        self.jobs = Some(start_helpers(brain, &mut optional)?);
        #[cfg(test)]
        checkpoint(Checkpoint::Helpers, &mut self)?;
        start_optional(
            self.brain.as_mut().expect("built brain"),
            &self.exe,
            self.jobs.as_ref().expect("started helpers"),
            vault_held,
            optional,
        );
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

    async fn settle(
        &mut self,
        ended: &Result<Option<tokio::time::Instant>, ipc::IpcError>,
    ) -> Option<Quit> {
        let brain = self.brain.take().expect("serving brain");
        let quit = if let Ok(Some(deadline)) = ended {
            Some(brain.quit(*deadline).await)
        } else {
            brain.close().await;
            None
        };
        // Ending the job waits on the processes, so it runs off the async thread.
        let jobs = self.jobs.take().expect("serving helpers");
        match tokio::task::spawn_blocking(move || jobs.shut_down()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: STARTUP_LOG_TARGET, %error, "couldn't end the helpers; closing the job ends them");
            }
            Err(error) => {
                tracing::warn!(target: STARTUP_LOG_TARGET, %error, "ending the helpers panicked");
            }
        }
        if std::mem::take(&mut self.published)
            && let Err(error) = credential::delete(self.session)
        {
            tracing::warn!(target: STARTUP_LOG_TARGET, %error, "couldn't delete the instance capability");
        }
        quit
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Checkpoint {
    Open,
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
        // Before any shell is served, so no run starts ahead of the recovery scan. Blocking is fine:
        // nothing else runs on the runtime yet.
        let mut store = Store::open(path)?;
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
    optional: &mut Optional,
    accounts: impl FnOnce(OAuthTuning, TokenTuning) -> Result<(Accounts, VaultHeld), BrainError>,
) -> Result<(Accounts, VaultHeld), BrainError> {
    #[cfg(windows)]
    let claude_on = opened.config.claude_code_enabled();
    #[cfg(windows)]
    {
        tracing::info!(target: STARTUP_LOG_TARGET, enabled = claude_on, "Claude Code setting");
        optional.claude_on = claude_on;
    }
    let _ = optional;
    // Before the capability is stored, so its write never overlaps the vault's first reads.
    let (account, vault_held) =
        accounts(opened.config.oauth_tuning(), opened.config.token_tuning())?;
    opened
        .config
        .count_in_use(&account.view(), opened.high_water.1);
    Ok((account, vault_held))
}

fn build_brain(opened: OpenedState, account: Accounts) -> Result<Brain, BrainError> {
    let OpenedState {
        store,
        main_thread,
        high_water,
        scenes,
        config,
        replay,
    } = opened;
    Brain::new(
        store,
        main_thread,
        high_water,
        scenes,
        match replay {
            Some(replay) => replay,
            None => engine(config.model_tuning())?,
        },
        config,
        account,
    )
    .map_err(BrainError::StoreThread)
}

fn start_helpers(brain: &mut Brain, optional: &mut Optional) -> Result<Arc<Job>, BrainError> {
    #[cfg(windows)]
    let models = platform::data_dir().map(|data| data.join(kumochi_protocol::voice::MODELS_DIR));
    #[cfg(windows)]
    {
        let stuck = models
            .as_deref()
            .map(|models| brain.finish_removals(models))
            .unwrap_or_default();
        optional.models = models;
        optional.stuck = stuck;
    }
    let _ = (brain, optional);
    Ok(Arc::new(Job::new().map_err(BrainError::Helpers)?))
}

fn start_optional(
    brain: &mut Brain,
    exe: &Path,
    jobs: &Arc<Job>,
    vault_held: VaultHeld,
    optional: Optional,
) {
    let Optional {
        claude_on,
        models,
        stuck,
    } = optional;
    #[cfg(windows)]
    {
        if let Some(models) = models {
            super::start_voice(brain, models, &stuck, exe, jobs);
        }
        let lane = super::claude_lane(vault_held, jobs, exe);
        brain.attach_claude(match lane {
            Some(lane) if claude_on => {
                crate::runtime::claude::Claude::start(lane, crate::runtime::claude::SIGN_IN)
            }
            None if claude_on => crate::runtime::claude::Claude::default(),
            lane => crate::runtime::claude::Claude::turned_off(lane),
        });
    }
    #[cfg(not(windows))]
    drop((vault_held, claude_on, models, stuck));
    start_screen(brain, exe.to_owned(), Ok(Arc::clone(jobs)));
}
