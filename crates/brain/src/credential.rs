//! The instance capability in Windows Credential Manager, the only place it's stored.

use capsula_protocol::constants::{INSTANCE_ACCOUNT, INSTANCE_SERVICE};
use capsula_protocol::message::Capability;
use keyring::Entry;

/// Credential Manager refused an operation.
#[derive(Debug, thiserror::Error)]
#[error("Credential Manager failed")]
pub struct CredentialError(#[from] keyring::Error);

/// Stores the capability, replacing the previous brain's.
///
/// # Errors
/// Fails when Credential Manager refuses the write.
pub fn store(capability: &Capability) -> Result<(), CredentialError> {
    Ok(entry()?.set_secret(capability.expose())?)
}

/// Deletes the capability. Deleting one that isn't there succeeds.
///
/// # Errors
/// Fails when Credential Manager refuses the delete.
pub fn delete() -> Result<(), CredentialError> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn entry() -> keyring::Result<Entry> {
    Entry::new(INSTANCE_SERVICE, INSTANCE_ACCOUNT)
}
