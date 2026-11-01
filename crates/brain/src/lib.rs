//! Capsula's brain: runs, tools, connections, the store and the helpers' Job Object.
//!
//! The library holds everything; `main.rs` only starts it, so integration tests can reach the
//! platform code.

pub mod credential;
pub mod ipc;
pub mod logging;
pub mod platform;
mod relaunch;
mod tap;

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::SHELL_EXE;
use capsula_protocol::message::{CAPABILITY_LEN, Capability, Incarnation};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::ipc::{Identity, Pipes, Session};
use crate::platform::Job;
use crate::relaunch::Crashes;

/// How long a relaunched shell the brain can't watch gets to connect [guess].
const UNWATCHED_GRACE: Duration = Duration::from_secs(60);

/// The argument that starts `capsulad` as the screen worker.
pub const SCREEN_WORKER: &str = "--screen";

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
    /// The helpers' Job Object or a helper couldn't be created.
    #[error("can't start the helpers")]
    Helpers(#[source] io::Error),
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
    let jobs = Job::new().map_err(BrainError::Helpers)?;
    // shortcut: a fake screen worker that only waits; slice 6 gives it work and respawns it.
    let _screen = Job::spawn(&exe, SCREEN_WORKER, &[&jobs]).map_err(BrainError::Helpers)?;
    let mut bytes = [0; CAPABILITY_LEN];
    getrandom::fill(&mut bytes)?;
    let identity = Identity {
        capability: Capability::new(bytes),
        incarnation: Incarnation(getrandom::u64()?),
        shell_image: exe.with_file_name(SHELL_EXE),
    };
    credential::store(session, &identity.capability)?;
    tracing::info!(incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let ended = serve(&mut pipes, &identity).await;
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
    ended.map_err(BrainError::from)
}

/// Serves shells until one quits or is from another build, relaunching a shell that dies.
async fn serve(pipes: &mut Pipes, identity: &Identity) -> Result<(), ipc::IpcError> {
    let mut tap = tap::Tap::from_env();
    let mut crashes = Crashes::default();
    let mut launched = None;
    // Dropped when serving ends, which cancels a relaunch still waiting out its backoff.
    let mut relaunches = JoinSet::new();
    loop {
        let session = pipes
            .next_session(identity, &mut tap, &mut launched)
            .await?;
        // A shell connected or the relaunched one died; either way nothing waits on it now.
        launched = None;
        match session {
            Session::Quit => {
                tracing::info!("quit");
                return Ok(());
            }
            Session::BuildMismatch => {
                tracing::info!("a shell from another build connected; exiting");
                return Ok(());
            }
            Session::NoShell => {
                tracing::info!("no shell connected; exiting");
                return Ok(());
            }
            Session::ShellLost => {
                let Some(delay) = crashes.record(Instant::now()) else {
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
