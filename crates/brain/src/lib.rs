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
use std::time::{Duration, Instant};

use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::SHELL_EXE;
use capsula_protocol::message::{CAPABILITY_LEN, Capability, Incarnation};

use crate::ipc::{Identity, Pipes, Session};
use crate::platform::Job;
use crate::relaunch::Crashes;

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
    if let Err(error) = jobs.terminate() {
        tracing::warn!(%error, "couldn't end the helpers; closing the job ends them");
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
    loop {
        match pipes.next_session(identity, &mut tap).await? {
            Session::Quit => {
                tracing::info!("quit");
                return Ok(());
            }
            Session::BuildMismatch => {
                tracing::info!("a shell from another build connected; exiting");
                return Ok(());
            }
            Session::ShellLost => {
                let Some(delay) = crashes.record(Instant::now()) else {
                    tracing::error!("the shell keeps crashing; not relaunching it");
                    continue;
                };
                tracing::warn!(?delay, "the shell died; relaunching it");
                tokio::spawn(relaunch(identity.shell_image.clone(), delay));
            }
        }
    }
}

async fn relaunch(shell: PathBuf, delay: Duration) {
    tokio::time::sleep(delay).await;
    match platform::launch_detached(&shell) {
        Ok(pid) => tracing::info!(pid, "relaunched the shell"),
        Err(error) => tracing::error!(%error, "can't relaunch the shell"),
    }
}
