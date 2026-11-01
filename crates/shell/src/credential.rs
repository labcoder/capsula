//! Reads the brain's instance capability from Windows Credential Manager.

use capsula_protocol::Secret;
use capsula_protocol::constants::{INSTANCE_ACCOUNT, INSTANCE_SERVICE};
use capsula_protocol::message::{CAPABILITY_LEN, Capability};
use keyring::Entry;

/// The capability couldn't be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CredentialError {
    /// Credential Manager failed, or holds no capability yet.
    #[error("Credential Manager failed")]
    Store(#[from] keyring::Error),
    /// The stored value isn't a capability.
    #[error("the stored capability has the wrong length")]
    WrongLength,
}

/// The capability the brain of Windows session `session` stored.
///
/// # Errors
/// Fails when there is none or it's malformed.
pub(crate) fn read(session: u32) -> Result<Capability, CredentialError> {
    let account = format!("{INSTANCE_ACCOUNT}-{session}");
    let bytes = Entry::new(INSTANCE_SERVICE, &account)?.get_secret()?;
    let bytes: [u8; CAPABILITY_LEN] = bytes.try_into().map_err(|_| CredentialError::WrongLength)?;
    Ok(Secret::new(bytes))
}
