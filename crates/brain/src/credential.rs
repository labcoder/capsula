//! The instance capability in Windows Credential Manager, the only place it's stored.

use keyring::Entry;
use kumochi_protocol::constants::{INSTANCE_ACCOUNT, INSTANCE_SERVICE};
use kumochi_protocol::message::Capability;

/// Credential Manager refused an operation.
#[derive(Debug, thiserror::Error)]
#[error("Credential Manager failed")]
pub struct CredentialError(#[from] keyring::Error);

/// Stores the capability for Windows session `session`, replacing the previous brain's there.
///
/// # Errors
/// Fails when Credential Manager refuses the write.
pub fn store(session: u32, capability: &Capability) -> Result<(), CredentialError> {
    Ok(entry(session)?.set_secret(capability.expose())?)
}

/// Deletes the capability. Deleting one that isn't there succeeds.
///
/// # Errors
/// Fails when Credential Manager refuses the delete.
pub fn delete(session: u32) -> Result<(), CredentialError> {
    match entry(session)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// One entry per session, because each session runs its own brain.
fn entry(session: u32) -> keyring::Result<Entry> {
    Entry::new(INSTANCE_SERVICE, &format!("{INSTANCE_ACCOUNT}-{session}"))
}
