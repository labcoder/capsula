//! Closed vault errors, without record contents or serialization diagnostics.

use std::io;

use crate::platform::SessionLockError;
use crate::platform::credential::CredentialStoreError;

/// An account record could not be read, committed or removed.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Another session holds the vault lock.
    #[error("account session is busy")]
    Busy,
    /// The local data directory is unavailable.
    #[error("local account data directory is unavailable")]
    NoLocalAppData,
    /// Opening the session lock failed.
    #[error("account session lock failed")]
    Lock(#[source] io::Error),
    /// The operating system refused a credential operation.
    #[error("account credential operation failed")]
    Store(#[from] CredentialStoreError),
    /// A field, record or entry count exceeds its bound.
    #[error("account record exceeds its bounds")]
    Bounds,
    /// Stored data is malformed, incomplete or from an unknown version.
    #[error("account record is unreadable")]
    Unreadable,
    /// This operation is incompatible with the persisted control state.
    #[error("account record operation is fenced")]
    Fenced,
    /// A by-name cleanup check found a surviving manifest or chunk.
    #[error("account credential names remain")]
    NotGone,
    /// The host identifier could not be generated.
    #[error("account host randomness is unavailable")]
    Random(#[from] getrandom::Error),
}

impl From<SessionLockError> for VaultError {
    fn from(error: SessionLockError) -> Self {
        match error {
            SessionLockError::Busy => Self::Busy,
            SessionLockError::Io(error) => Self::Lock(error),
        }
    }
}
