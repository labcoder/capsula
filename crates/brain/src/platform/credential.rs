//! Exact-name credential operations, without vault commit policy.

use std::path::PathBuf;

use kumochi_protocol::Secret;
#[cfg(not(windows))]
use kumochi_protocol::message::Capability;
use zeroize::Zeroizing;

/// This platform's store; code outside `platform` names only this one.
#[cfg(windows)]
pub(crate) use super::windows::WindowsCredentialStore as NativeCredentialStore;
#[cfg(windows)]
pub use super::windows::{delete_instance, store_instance};

/// Credential Manager refused an instance-capability operation.
#[cfg(windows)]
#[derive(Debug, thiserror::Error)]
#[error("Credential Manager failed")]
pub struct CredentialError(#[from] keyring::Error);

/// The instance capability could not be stored or deleted.
#[cfg(not(windows))]
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// This build has no native credential store.
    #[error("native credential storage is unsupported on this platform")]
    Unsupported,
}

impl CredentialError {
    #[cfg(windows)]
    pub(crate) fn raw_os_error(&self) -> Option<u32> {
        std::error::Error::source(&self.0)?
            .downcast_ref::<keyring::windows::Error>()
            .map(|error| error.0)
    }

    #[cfg(not(windows))]
    pub(crate) fn raw_os_error(&self) -> Option<u32> {
        match self {
            Self::Unsupported => None,
        }
    }
}

#[cfg(not(windows))]
pub(crate) struct NativeCredentialStore;

#[cfg(not(windows))]
impl CredentialStore for NativeCredentialStore {
    fn get(&self, _: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        Err(CredentialStoreError::Unsupported)
    }

    fn set(&self, _: &str, _: &SecretBytes) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::Unsupported)
    }

    fn delete(&self, _: &str) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::Unsupported)
    }
}

pub(crate) fn native_session_lock_path() -> Result<PathBuf, CredentialStoreError> {
    #[cfg(windows)]
    {
        super::session_lock_path().ok_or(CredentialStoreError::NoDataDir)
    }
    #[cfg(not(windows))]
    {
        Err(CredentialStoreError::Unsupported)
    }
}

/// Replaces the instance capability for the given session.
///
/// # Errors
/// Returns Unsupported because this build has no native credential store.
#[cfg(not(windows))]
pub fn store_instance(_: u32, _: &Capability) -> Result<(), CredentialError> {
    Err(CredentialError::Unsupported)
}

/// Deletes the instance capability for the given session.
///
/// # Errors
/// Returns Unsupported because this build has no native credential store.
#[cfg(not(windows))]
pub fn delete_instance(_: u32) -> Result<(), CredentialError> {
    Err(CredentialError::Unsupported)
}

#[cfg(all(test, windows))]
pub(crate) mod fixture;

/// Secret bytes whose owned allocation is wiped on drop and whose debug output is redacted.
pub type SecretBytes = Secret<Zeroizing<Vec<u8>>>;

/// The storage design's chunk size, below Windows' 2,560-byte generic credential limit.
pub use kumochi_protocol::constants::CREDENTIAL_CHUNK_SIZE as CHUNK_SIZE;

/// A credential operation failed; no target name or secret is carried in an error.
#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    /// This build has no native credential store.
    #[error("native credential storage is unsupported on this platform")]
    Unsupported,
    /// The native account data folder could not be resolved.
    #[error("account data directory is unavailable")]
    NoDataDir,
    /// A target is empty, contains a null, or exceeds the platform limit.
    #[error("invalid credential target")]
    InvalidTarget,
    /// A single blob exceeds the platform limit.
    #[error("credential blob is too large")]
    BlobTooLarge,
    /// The OS returned malformed credential metadata.
    #[error("invalid credential blob")]
    InvalidBlob,
    /// The OS refused a read with this error code.
    #[error("credential read failed ({0})")]
    Read(i32),
    /// The OS refused a write with this error code.
    #[error("credential write failed ({0})")]
    Write(i32),
    /// The OS refused a delete with this error code.
    #[error("credential delete failed ({0})")]
    Delete(i32),
    /// The store contradicted the account lane's own write or delete of this name, even
    /// after retries.
    #[error("credential store did not settle")]
    Unsettled,
}

/// Synchronous storage by exact target name, with no enumeration or implicit name mapping.
///
/// Callers run blocking vault work off the core loop and hold the vault's session lock until
/// the entire operation finishes, including when its async waiter is cancelled.
pub trait CredentialStore: Send + Sync {
    /// Reads one bounded blob. Only a missing credential returns `None`.
    ///
    /// # Errors
    /// Returns an error for invalid targets, malformed blobs, or a refused OS read.
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError>;

    /// Writes one bounded blob, replacing only this exact target.
    ///
    /// # Errors
    /// Returns an error for invalid targets, oversized blobs, or a refused OS write.
    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError>;

    /// Deletes this exact target. Deleting a missing credential succeeds.
    ///
    /// # Errors
    /// Returns an error for invalid targets or any OS failure other than not-found.
    fn delete(&self, target: &str) -> Result<(), CredentialStoreError>;
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn native_credentials_are_unsupported() {
        let name = "test-only-credential";
        let secret = Secret::new(Zeroizing::new(vec![1]));
        let capability = Capability::new([1; kumochi_protocol::message::CAPABILITY_LEN]);
        assert!(matches!(
            native_session_lock_path(),
            Err(CredentialStoreError::Unsupported)
        ));
        assert!(matches!(
            NativeCredentialStore.get(name),
            Err(CredentialStoreError::Unsupported)
        ));
        assert!(matches!(
            NativeCredentialStore.set(name, &secret),
            Err(CredentialStoreError::Unsupported)
        ));
        assert!(matches!(
            NativeCredentialStore.delete(name),
            Err(CredentialStoreError::Unsupported)
        ));
        assert!(matches!(
            crate::credential::store(0, &capability),
            Err(CredentialError::Unsupported)
        ));
        assert!(matches!(
            crate::credential::delete(0),
            Err(CredentialError::Unsupported)
        ));
    }
}
