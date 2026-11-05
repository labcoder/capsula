//! Kumochi's brain: runs, tools, connections, the store and the helpers' Job Object.
//!
//! The library holds everything; `main.rs` only starts it, so integration tests can reach the
//! platform code.

pub mod bridge;
mod config;
pub mod connections;
mod context;
pub mod coordinator;
pub mod dispatch;
pub mod credential;
pub mod drawing;
pub mod engines;
pub mod ipc;
mod jobs;
pub mod logging;
pub mod platform;
mod relaunch;
mod runtime;
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
    /// The helpers' Job Object or a helper couldn't be created.
    #[error("can't start the helpers")]
    Helpers(#[source] io::Error),
    /// The account's credential thread couldn't be started.
    #[error("can't start the credential thread")]
    Accounts(#[source] io::Error),
    /// The model connection's TLS setup failed.
    #[error("can't set up the model connection")]
    Model(#[source] connections::http_client::HttpError),
}

/// Ends the runtime [`run`] ran on without waiting for blocking work. Quit and close already
/// waited for a settings write up to their deadline; one still stuck on a slow disk ends with the
/// process, and `config.toml` stays whole because it's replaced, never written in place.
pub fn shut_down(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_timeout(Duration::ZERO);
}

/// Runs the brain until the shell quits or turns out to be from another build.
///
/// # Errors
/// Fails when the pipes can't be created (another brain holds them), the capability can't be
/// made or stored, the helpers can't start, or a pipe fails outright.
pub async fn run() -> Result<(), BrainError> {
    let session = platform::session_id()?;
    // The pipes come first: a second brain fails here, before it touches anything else.
    let mut pipes = Pipes::create(session)?;
    let exe = std::env::current_exe()?;
    // Before any shell is served, so no run starts ahead of the recovery scan. Blocking is fine:
    // nothing else runs on the runtime yet.
    let mut store = store::Store::open(&store::default_path()?)?;
    let recovered = store.recover()?;
    tracing::info!(
        crashed_runs = recovered.crashed_runs,
        finished_deletions = recovered.finished_deletions,
        purge_pending = recovered.purge_pending,
        failed_deletions = recovered.failed_deletions,
        "store recovered"
    );
    // Recovery created it if it was missing, so its id is below the high water.
    let main_thread = store.main_thread()?;
    let high_water = store.high_water()?;
    let scenes = store.scenes()?;
    let config = config::Config::load(platform::config_dir(), main_thread);
    // Before the capability is stored, so its write never overlaps the vault's first reads.
    let account = accounts().map_err(BrainError::Accounts)?;
    let mut brain = Brain::new(
        store,
        main_thread,
        high_water,
        scenes,
        engine()?,
        config,
        account,
    )
    .map_err(BrainError::StoreThread)?;
    let jobs = std::sync::Arc::new(Job::new().map_err(BrainError::Helpers)?);
    let screen = screen::ScreenSupervisor::start(exe.clone(), std::sync::Arc::clone(&jobs))
        .map_err(BrainError::Helpers)?;
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
    drop(screen);
    // Ending the job waits on the processes, so it runs off the async thread.
    match tokio::task::spawn_blocking(move || jobs.terminate()).await {
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

/// The account system on the real credential store, browser, clock and network.
fn accounts() -> io::Result<runtime::account::Accounts> {
    use std::sync::Arc;

    use connections::account::Worker;
    use connections::clock::SystemClock;
    use connections::http_client::{self, SharedHttp};
    use connections::oauth::SystemBrowser;

    let worker = Worker::system(Arc::new(SystemClock));
    let net = runtime::account::Net {
        http: Arc::new(|| http_client::build().map(SharedHttp::new)),
        launcher: Arc::new(SystemBrowser),
        clock: Arc::new(SystemClock),
    };
    runtime::account::Accounts::spawn(worker, net)
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
    let mut child = match platform::launch_detached(&shell) {
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
