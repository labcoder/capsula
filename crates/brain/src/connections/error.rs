//! Closed vault errors, without record contents or serialization diagnostics, and what a typed
//! provider error asks the user to do.

use std::io;

use super::authority::AuthorityError;
use crate::platform::SessionLockError;
use crate::platform::credential::CredentialStoreError;

/// An account record could not be read, committed or removed.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Another session holds the vault lock.
    #[error("account session is busy")]
    Busy,
    /// The platform's account data directory is unavailable.
    #[error("account data directory is unavailable")]
    NoDataDir,
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
    /// The credential authority is unavailable or refused a transition.
    #[error("account authority is unavailable")]
    Authority,
}

impl From<AuthorityError> for VaultError {
    fn from(error: AuthorityError) -> Self {
        match error {
            AuthorityError::Store(error) => Self::Store(error),
            AuthorityError::Unsettled
            | AuthorityError::MissingChunk
            | AuthorityError::WrongChunkLength => Self::Store(CredentialStoreError::Unsettled),
            AuthorityError::Bounds => Self::Bounds,
            AuthorityError::Corrupt => Self::Unreadable,
            AuthorityError::Random(error) => Self::Random(error),
            AuthorityError::Unavailable(_)
            | AuthorityError::NewerSchema
            | AuthorityError::Conflict => Self::Authority,
        }
    }
}

impl VaultError {
    /// Stored data that's malformed, incomplete or oversized, as opposed to a refused operation,
    /// which a retry may get past.
    #[must_use]
    pub fn is_damage(&self) -> bool {
        matches!(
            self,
            Self::Unreadable | Self::Bounds | Self::Store(CredentialStoreError::InvalidBlob)
        )
    }
}

/// What a typed provider error asks for. Only the public ledger's codes map here (design §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The session ended: clear the tokens, keep the registration, and sign in again.
    SignInAgain,
    /// Keep the credentials and try again later.
    RetryLater,
    /// The client or grant is wrong, so retrying it unchanged won't help.
    FixRequest,
    /// This account can't use the plan.
    Unavailable,
}

impl From<SessionLockError> for VaultError {
    fn from(error: SessionLockError) -> Self {
        match error {
            SessionLockError::Busy => Self::Busy,
            SessionLockError::Io(error) => Self::Lock(error),
        }
    }
}
