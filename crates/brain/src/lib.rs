//! Capsula's brain: runs, tools, connections, the store and the helpers' Job Object.
//!
//! The library holds everything; `main.rs` only starts it, so integration tests can reach the
//! platform code.

pub mod credential;
pub mod ipc;
pub mod logging;
pub mod platform;
mod tap;

use std::io;

use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::SHELL_EXE;
use capsula_protocol::message::{CAPABILITY_LEN, Capability, Incarnation};

use crate::ipc::{Ending, Identity, Pipes};

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
}

/// Runs the brain until the shell quits or turns out to be from another build.
///
/// # Errors
/// Fails when the pipes can't be created (another brain holds them), the capability can't be
/// made or stored, or a pipe fails outright.
pub async fn run() -> Result<(), BrainError> {
    let session = platform::session_id()?;
    // The pipes come first: a second brain fails here, before it touches the capability.
    let mut pipes = Pipes::create(session)?;
    let mut bytes = [0; CAPABILITY_LEN];
    getrandom::fill(&mut bytes)?;
    let identity = Identity {
        capability: Capability::new(bytes),
        incarnation: Incarnation(getrandom::u64()?),
        shell_image: std::env::current_exe()?.with_file_name(SHELL_EXE),
    };
    credential::store(session, &identity.capability)?;
    tracing::info!(incarnation = ?identity.incarnation, build = BUILD_ID, "listening");
    let ending = pipes.serve(&identity, tap::Tap::from_env()).await;
    if let Err(error) = credential::delete(session) {
        tracing::warn!(%error, "couldn't delete the instance capability");
    }
    match ending? {
        Ending::Quit => tracing::info!("quit"),
        Ending::BuildMismatch => tracing::info!("a shell from another build connected; exiting"),
    }
    Ok(())
}
