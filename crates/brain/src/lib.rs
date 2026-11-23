//! Kumochi's brain: runs, tools, connections, the store and the helpers' Job Object.
//!
//! The library holds everything; `main.rs` only starts it, so integration tests can reach the
//! platform code.

pub mod bridge;
mod config;
pub mod connections;
mod context;
pub mod coordinator;
pub mod credential;
pub mod dispatch;
pub mod drawing;
pub mod engines;
pub mod ipc;
pub mod issues;
mod jobs;
pub mod logging;
pub mod platform;
mod recall;
mod relaunch;
mod runtime;
#[cfg(any(test, all(windows, debug_assertions, feature = "test-support")))]
#[cfg_attr(
    all(windows, debug_assertions, feature = "test-support"),
    expect(
        clippy::print_stderr,
        reason = "the shared fixture reports cleanup failures during unwind"
    )
)]
#[path = "../../../tests/support/scratch.rs"]
mod scratch;
pub mod screen;
#[cfg(windows)]
pub mod speech;
pub mod store;
mod supervise;
mod tap;
#[cfg(test)]
mod test_support;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "PR 5 wires speech input; PR 7 wires spoken replies; PR 8 wires voice installation"
    )
)]
mod voice;

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::SHELL_EXE;
use kumochi_protocol::message::{CAPABILITY_LEN, Capability, Incarnation};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{ModelClient, Tls};
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
use crate::connections::model::text_observation::Observation;
#[cfg(debug_assertions)]
use crate::engines::fixture::Fixture;
use crate::engines::responses::{AccessHandle, Responses};
use crate::ipc::{Identity, Pipes, Session};
use crate::platform::Job;
use crate::runtime::{Brain, Selected};

/// How long a relaunched shell the brain can't watch gets to connect: past the shell's own
/// 10 s connect deadline, and short enough that three such losses plus the backoff still fit
/// in the one-minute crash window [guess].
const UNWATCHED_GRACE: Duration = Duration::from_secs(15);

/// The argument that starts `kumochid` as the screen worker.
pub const SCREEN_WORKER: &str = "--screen";
/// The argument that starts `kumochid` as the speech worker.
pub const SPEECH_WORKER: &str = "--speech";

/// The argument that starts `kumochid` as Claude Code's MCP bridge, before the launch value.
pub const MCP_BRIDGE: &str = "--mcp-bridge";

/// Why the brain stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    /// This build has no native runtime adapter.
    #[error("native brain startup is unsupported")]
    NativeRuntime(#[source] io::Error),
    /// The session id or the executable's path couldn't be read.
    #[error("can't read this process's own details")]
    Process(#[from] io::Error),
    /// The pipes couldn't be created, usually because another brain holds them.
    #[error(transparent)]
    Ipc(#[from] ipc::IpcError),
    /// The OS random source failed.
    #[error("no randomness for the capability")]
    Random(#[from] getrandom::Error),
    /// Credential Manager refused the capability.
    #[error(transparent)]
    Credential(#[from] credential::CredentialError),
    /// The store couldn't be opened or recovered.
    #[error("can't open the store")]
    Store(#[from] store::StoreError),
    /// The fixture script a dev build was pointed at doesn't load or parse.
    #[error("can't load the fixture script")]
    Fixture(#[source] io::Error),
    /// The store's thread couldn't be started.
    #[error("can't start the store's thread")]
    StoreThread(#[source] io::Error),
    /// The helpers' Job Object couldn't be created.
    #[error("can't start the helpers")]
    Helpers(#[source] io::Error),
    /// The account's credential thread couldn't be started.
    #[error("can't start the credential thread")]
    Accounts(#[source] io::Error),
    /// The build has no native adapter for trust roots.
    #[error(transparent)]
    NativeTrust(#[from] platform::TrustRootsError),
    /// The model connection's TLS setup failed.
    #[error("can't set up the model connection")]
    Model(#[source] connections::http_client::HttpError),
}

/// Ends the runtime [`run`] ran on without waiting for blocking work. Quit and close already
/// waited for a settings write up to their deadline; one still stuck on a slow disk ends with the
/// process, and `config.toml` stays whole because it's replaced, never written in place.
pub fn shut_down(runtime: tokio::runtime::Runtime) {
    end_runtime(
        runtime,
        #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
        Observation::current().map(AsRef::as_ref),
    );
}

fn end_runtime(
    runtime: tokio::runtime::Runtime,
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))] observation: Option<
        &Observation,
    >,
) {
    runtime.shutdown_timeout(Duration::ZERO);
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    if let Some(observation) = observation {
        observation.finish();
    }
}

/// Runs the brain until the shell quits or turns out to be from another build.
///
/// # Errors
/// Fails when the pipes can't be created (another brain holds them), the capability can't be
/// made or stored, the helpers can't start, a pipe fails outright, or this build has no native
/// runtime adapter.
pub fn run() -> impl Future<Output = Result<(), BrainError>> {
    use kumochi_protocol::constants::{LIVE_CHATGPT_TEXT_ENV, ProductIdentity};

    let admission = platform::require_native_runtime()
        .map_err(BrainError::NativeRuntime)
        .and_then(|()| {
            let mode = std::env::var(LIVE_CHATGPT_TEXT_ENV).ok();
            draw_admission(mode.as_deref(), ProductIdentity::ACTIVE)
        });
    run_admitted(
        admission,
        #[cfg(test)]
        || {},
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartMode {
    Normal,
    Draw,
}

fn draw_admission(
    mode: Option<&str>,
    identity: kumochi_protocol::constants::ProductIdentity,
) -> Result<StartMode, BrainError> {
    use kumochi_protocol::constants::ProductIdentity;

    let draw = mode == Some("draw");
    let refusal = if draw
        && !cfg!(all(
            windows,
            any(debug_assertions, feature = "gate-draw-replay")
        )) {
        Some("drawing replay is not compiled in")
    } else if (draw
        || cfg!(all(
            windows,
            feature = "gate-draw-replay",
            not(debug_assertions)
        )))
        && identity != ProductIdentity::GATE
    {
        Some("drawing replay requires Gate identity")
    } else if !draw
        && cfg!(all(
            windows,
            feature = "gate-draw-replay",
            not(debug_assertions)
        ))
    {
        Some("release drawing replay requires draw mode")
    } else {
        None
    };
    if let Some(refusal) = refusal {
        return Err(BrainError::Fixture(io::Error::other(refusal)));
    }
    Ok(if draw {
        StartMode::Draw
    } else {
        StartMode::Normal
    })
}

async fn run_admitted(
    admission: Result<StartMode, BrainError>,
    #[cfg(test)] starting: impl FnOnce(),
) -> Result<(), BrainError> {
    let mode = admission?;
    #[cfg(test)]
    starting();
    let session = platform::session_id()?;
    // The pipes come first: a second brain fails here, before it touches anything else.
    let mut pipes = Pipes::create(session)?;
    let exe = std::env::current_exe()?;
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    Observation::prepare(&exe);
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    let replay = (mode == StartMode::Draw).then(draw_engine).transpose()?;
    #[cfg(not(all(windows, any(debug_assertions, feature = "gate-draw-replay"))))]
    let replay = {
        let _ = mode;
        None
    };
    // Before any shell is served, so no run starts ahead of the recovery scan. Blocking is fine:
    // nothing else runs on the runtime yet.
    let mut store = store::Store::open(&store::default_path()?)?;
    let recovered = store.recover()?;
    tracing::info!(
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
    let mut config = config::Config::load(platform::config_dir(), main_thread);
    #[cfg(all(windows, feature = "gate-draw-replay", not(debug_assertions)))]
    require_draw_model(&config)?;
    #[cfg(windows)]
    let claude_on = config.claude_code_enabled();
    #[cfg(windows)]
    tracing::info!(enabled = claude_on, "Claude Code setting");
    // Before the capability is stored, so its write never overlaps the vault's first reads.
    let (account, vault_held) = accounts()?;
    config.count_in_use(&account.view(), high_water.1);
    let mut brain = Brain::new(
        store,
        main_thread,
        high_water,
        scenes,
        match replay {
            Some(replay) => replay,
            None => engine()?,
        },
        config,
        account,
    )
    .map_err(BrainError::StoreThread)?;
    #[cfg(windows)]
    let models = platform::data_dir().map(|data| data.join(kumochi_protocol::voice::MODELS_DIR));
    #[cfg(windows)]
    let stuck = models
        .as_deref()
        .map(|models| brain.finish_removals(models))
        .unwrap_or_default();
    let jobs = std::sync::Arc::new(Job::new().map_err(BrainError::Helpers)?);
    #[cfg(windows)]
    {
        if let Some(models) = models {
            start_voice(&mut brain, models, &stuck, &exe, &jobs);
        }
        let lane = claude_lane(vault_held, &jobs, &exe);
        brain.attach_claude(match lane {
            Some(lane) if claude_on => {
                runtime::claude::Claude::start(lane, runtime::claude::SIGN_IN)
            }
            None if claude_on => runtime::claude::Claude::default(),
            lane => runtime::claude::Claude::turned_off(lane),
        });
    }
    #[cfg(not(windows))]
    drop(vault_held);
    start_screen(&mut brain, exe.clone(), Ok(std::sync::Arc::clone(&jobs)));
    let mut bytes = [0; CAPABILITY_LEN];
    getrandom::fill(&mut bytes)?;
    let identity = Identity {
        capability: Capability::new(bytes),
        incarnation: Incarnation(getrandom::u64()?),
        shell_image: exe.with_file_name(SHELL_EXE),
    };
    credential::store(session, &identity.capability)?;
    tracing::info!(incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let ended = serve(&mut pipes, &identity, &mut brain).await;
    match ended {
        Ok(Some(deadline)) => {
            brain.quit(deadline).await;
        }
        Ok(None) | Err(_) => brain.close().await,
    }
    // Ending the job waits on the processes, so it runs off the async thread.
    match tokio::task::spawn_blocking(move || jobs.shut_down()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "couldn't end the helpers; closing the job ends them");
        }
        Err(error) => tracing::warn!(%error, "ending the helpers panicked"),
    }
    if let Err(error) = credential::delete(session) {
        tracing::warn!(%error, "couldn't delete the instance capability");
    }
    ended.map(|_| ()).map_err(BrainError::from)
}

/// Starts speech input's worker and spoken replies' thread, then voice. A handle that can't start
/// leaves its part unavailable for the session.
#[cfg(windows)]
fn start_voice(
    brain: &mut Brain,
    models: PathBuf,
    stuck: &[(kumochi_protocol::voice::VoicePart, voice::installer::Folder)],
    exe: &std::path::Path,
    jobs: &std::sync::Arc<Job>,
) {
    use voice::Reports;
    use voice::stt::SttHandle;
    use voice::tts::TtsHandle;

    let runtime = tokio::runtime::Handle::current();
    let natives = exe
        .parent()
        .expect("the brain's path names a file in a folder")
        .to_path_buf();
    let (stt_reports, stt_heard) = Reports::new();
    let stt = speech::SpeechSupervisor::start(exe.to_path_buf(), std::sync::Arc::clone(jobs))
        .and_then(|speech| SttHandle::spawn(runtime.clone(), speech, natives, stt_reports))
        .inspect_err(|error| tracing::warn!(kind = ?error.kind(), "speech input can't start"))
        .ok();
    let (tts_reports, tts_heard) = Reports::new();
    let tts = TtsHandle::spawn(runtime, tts_reports)
        .inspect_err(|error| tracing::warn!(kind = ?error.kind(), "spoken replies can't start"))
        .ok();
    let fetcher = std::sync::Arc::new(voice::download::Online(models.clone()));
    brain.attach_voice(models, stuck, (stt, tts), [stt_heard, tts_heard], fetcher);
}

/// Gives screen holds the brain's one screen worker. One that can't start leaves screen holds
/// `Unsupported` for the session, the way a voice part that can't start is unavailable.
fn start_screen(brain: &mut Brain, exe: PathBuf, jobs: io::Result<std::sync::Arc<Job>>) {
    match jobs.and_then(|jobs| screen::HoldClient::start(exe, jobs)) {
        Ok(client) => brain.attach_screen(std::sync::Arc::new(client)),
        Err(error) => tracing::warn!(kind = ?error.kind(), "the screen worker can't start"),
    }
}

/// The account system on the real credential store, browser, clock and network, and the fact that
/// its worker holds the session lock.
fn accounts() -> Result<
    (
        runtime::account::Accounts,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ),
    BrainError,
> {
    use std::sync::Arc;

    use connections::account::Worker;
    use connections::clock::SystemClock;
    use connections::http_client::{self, SharedHttp};
    use connections::oauth::SystemBrowser;

    if !platform::native_trust_supported() {
        return Err(platform::TrustRootsError::Unsupported.into());
    }
    let worker = Worker::system(Arc::new(SystemClock));
    let net = runtime::account::Net {
        http: Arc::new(|| http_client::build().map(SharedHttp::new)),
        launcher: Arc::new(SystemBrowser),
        clock: Arc::new(SystemClock),
    };
    let held = worker.held();
    Ok((
        runtime::account::Accounts::spawn(worker, net).map_err(BrainError::Accounts)?,
        held,
    ))
}

/// Claude Code's lane on the installed `claude.exe`; none when it can't start.
#[cfg(windows)]
fn claude_lane(
    vault_held: std::sync::Arc<std::sync::atomic::AtomicBool>,
    jobs: &std::sync::Arc<Job>,
    brain_exe: &std::path::Path,
) -> Option<engines::claude_code::lane::Lane> {
    use engines::claude_code::install::{FindError, locate};
    use engines::claude_code::lane::{Host, Lane, Program};

    let base = match platform::local_app_data() {
        Ok(local) => local.join(kumochi_protocol::constants::DATA_DIR),
        Err(error) => {
            tracing::warn!(kind = ?error.kind(), "no local data folder for Claude Code");
            return None;
        }
    };
    if let Err(error) = std::fs::create_dir(&base)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        tracing::warn!(kind = ?error.kind(), "couldn't make the local data folder for Claude Code");
        return None;
    }
    let host = Host {
        find: Box::new(|| {
            let profile = platform::profile_dir().map_err(|_| FindError::NotFound)?;
            locate(&profile).map(|exe| Program {
                exe,
                prefix: Vec::new(),
            })
        }),
        base,
        vault_held,
        managed: Box::new(platform::managed_presence),
        brain_job: std::sync::Arc::clone(jobs),
        console: platform::Stdio::Console,
        terminate: Job::terminate,
        brain_exe: brain_exe.to_owned(),
        runtime: tokio::runtime::Handle::current(),
    };
    Lane::start(host)
        .inspect_err(
            |error| tracing::warn!(kind = ?error.kind(), "Claude Code's lane didn't start"),
        )
        .ok()
}

#[cfg(all(
    windows,
    any(
        all(test, debug_assertions),
        all(feature = "gate-draw-replay", not(debug_assertions))
    )
))]
fn require_draw_model(config: &config::Config) -> Result<(), BrainError> {
    if !config
        .default_choice()
        .display()
        .ok()
        .flatten()
        .is_some_and(|choice| {
            choice.connection == kumochi_protocol::message::ConnectionKind::ChatGpt
                && choice.model.as_str() == engines::responses::replay::MODEL
                && choice.effort == kumochi_protocol::message::Effort::Low
        })
    {
        return Err(BrainError::Fixture(io::Error::other(
            "release drawing replay needs its fixed model",
        )));
    }
    Ok(())
}

#[cfg(all(test, windows, debug_assertions))]
#[test]
fn draw_opt_in_refuses_a_different_model() {
    use kumochi_protocol::{constants::CONFIG_FILE, message::ThreadId};

    let scratch = crate::test_support::Scratch::new("draw-fixed-model");
    std::fs::create_dir(scratch.dir()).unwrap();
    for (model, effort, connection, accepted) in [
        ("draw-replay", "Low", "ChatGpt", true),
        ("other-model", "Low", "ChatGpt", false),
        ("draw-replay", "High", "ChatGpt", false),
        ("draw-replay", "Low", "AnthropicKey", false),
    ] {
        std::fs::write(
            scratch.dir().join(CONFIG_FILE),
            format!(r#"default_model = {{ model = "{model}", effort = "{effort}", connection = "{connection}" }}"#),
        ).unwrap();
        let config = config::Config::load(Some(scratch.dir().to_owned()), ThreadId(1));
        assert_eq!(require_draw_model(&config).is_ok(), accepted);
    }
}

#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
fn draw_engine() -> Result<Selected, BrainError> {
    let observation = Observation::drawing().ok_or_else(|| {
        BrainError::Fixture(io::Error::other(
            "drawing replay needs a fresh gate receipt",
        ))
    })?;
    let replay =
        engines::responses::replay::Replay::new(observation).map_err(BrainError::Fixture)?;
    Ok(Selected::DrawReplay(Box::new(replay)))
}

#[cfg(all(test, windows, debug_assertions))]
#[tokio::test]
async fn direct_dev_draw_refuses_before_store_or_vault_startup() {
    use kumochi_protocol::constants::ProductIdentity;

    let refused = draw_admission(Some("draw"), ProductIdentity::DEV);
    let Err(BrainError::Fixture(error)) = Box::pin(run_admitted(refused, || {
        panic!("refused draw entered store/vault startup");
    }))
    .await
    else {
        panic!("direct Dev draw did not refuse at startup");
    };
    assert_eq!(error.to_string(), "drawing replay requires Gate identity");
    assert!(matches!(
        draw_admission(Some("1"), ProductIdentity::DEV),
        Ok(StartMode::Normal)
    ));
}

#[cfg(all(test, not(windows)))]
#[tokio::test]
async fn native_library_entries_return_unsupported() {
    assert!(matches!(
        run().await,
        Err(BrainError::NativeRuntime(error)) if error.kind() == io::ErrorKind::Unsupported
    ));
    assert!(matches!(
        bridge::serve("not a launch id"),
        Err(bridge::BridgeError::NativeRuntime(error)) if error.kind() == io::ErrorKind::Unsupported
    ));
    assert!(matches!(
        screen::serve(),
        Err(kumochi_protocol::frame::FrameError::Io(error))
            if error.kind() == io::ErrorKind::Unsupported
    ));
}

/// The engine runs use: in a dev build that names a script in
/// [`kumochi_protocol::constants::FIXTURE_ENV`], that script answers every run; otherwise the
/// account's Responses engine answers, once the account is ready.
fn engine() -> Result<Selected, BrainError> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(kumochi_protocol::constants::FIXTURE_ENV) {
        let script = std::fs::read_to_string(path).map_err(BrainError::Fixture)?;
        let fixture = Fixture::parse(&script).map_err(|error| {
            BrainError::Fixture(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        return Ok(Selected::Fixture(Box::new(fixture)));
    }
    let (access, asks) = AccessHandle::new();
    let client = ModelClient::new(Tls::build().map_err(BrainError::Model)?);
    let engine = Responses::new(client, Budget::from_env(), access);
    Ok(Selected::Account {
        engine: Box::new(engine),
        asks,
    })
}

/// Serves shells until one quits or is from another build, relaunching a shell that dies. A quit
/// returns its shutdown deadline.
async fn serve(
    pipes: &mut Pipes,
    identity: &Identity,
    brain: &mut Brain,
) -> Result<Option<tokio::time::Instant>, ipc::IpcError> {
    let mut tap = tap::Tap::from_env();
    let mut launched = None;
    // Dropped when serving ends, which cancels a relaunch still waiting out its backoff.
    let mut relaunches = JoinSet::new();
    loop {
        let session = pipes
            .next_session(identity, brain, &mut tap, &mut launched)
            .await?;
        // A shell connected or the relaunched one died; either way nothing waits on it now.
        launched = None;
        match session {
            Session::Quit { deadline } => {
                tracing::info!("quit");
                return Ok(Some(deadline));
            }
            Session::BuildMismatch => {
                tracing::info!("a shell from another build connected; exiting");
                return Ok(None);
            }
            Session::NoShell => {
                tracing::info!("no shell connected; exiting");
                return Ok(None);
            }
            Session::ShellLost => {
                let plan = brain.crashes.plan(std::time::Instant::now());
                if !brain.clear_before_relaunch(&plan.clear).await {
                    tracing::error!("drawing clears were not confirmed; not relaunching the shell");
                    continue;
                }
                let Some(delay) = plan.delay else {
                    tracing::error!("the shell keeps crashing; not relaunching it");
                    continue;
                };
                tracing::warn!(?delay, "the shell died; relaunching it");
                let (exited, exit) = oneshot::channel();
                launched = Some(exit);
                relaunches.spawn(relaunch(identity.shell_image.clone(), delay, exited));
            }
        }
    }
}

/// Relaunches the shell after `delay`, then reports on `exited` when it ends, so a shell that
/// dies before connecting counts as a crash too. A relaunch that fails drops `exited`, which
/// counts the same way, so the backoff retries it and then gives up.
async fn relaunch(shell: PathBuf, delay: Duration, exited: oneshot::Sender<()>) {
    tokio::time::sleep(delay).await;
    let budgets = connections::model::budget::handed_on(|name| std::env::var_os(name).is_some());
    let mut child = match platform::launch_detached(&shell, budgets) {
        Ok(child) => child,
        Err(error) => {
            tracing::error!(%error, "can't relaunch the shell");
            return;
        }
    };
    tracing::info!(pid = child.id(), "relaunched the shell");
    // A plain thread, not the runtime's blocking pool, so a brain that's done never waits
    // for this shell to end.
    let (ended, end) = oneshot::channel();
    let watcher = thread::Builder::new()
        .name("shell-watch".to_owned())
        .spawn(move || match child.wait() {
            Ok(_) => ended.send(()).unwrap_or_default(),
            Err(error) => tracing::warn!(%error, "lost track of the relaunched shell"),
        });
    if let Err(error) = watcher {
        tracing::warn!(%error, "can't watch the relaunched shell");
    }
    if end.await.is_err() {
        // Unwatched, the shell may still be alive: give it time to connect before counting it
        // lost, which at worst starts a second shell that exits on the mutex.
        tokio::time::sleep(UNWATCHED_GRACE).await;
    }
    if exited.send(()).is_err() {
        tracing::debug!("the relaunched shell had connected");
    }
}
