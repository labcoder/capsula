//! Reads the brain's instance capability from Windows Credential Manager.

use kumochi_protocol::message::Capability;

/// The capability couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// Credential Manager failed, or holds no capability yet.
    #[cfg(windows)]
    #[error("Credential Manager failed")]
    Store(#[from] keyring::Error),
    /// The stored value isn't a capability.
    #[error("the stored capability has the wrong length")]
    WrongLength,
    /// No native credential store is available to this build.
    #[cfg(not(windows))]
    #[error("native credential storage is unsupported on this platform")]
    Unsupported,
}

/// The capability the brain of Windows session `session` stored.
///
/// # Errors
/// Fails when there is none or it's malformed.
pub fn read(session: u32) -> Result<Capability, CredentialError> {
    crate::platform::credential::read_instance(session)
}
