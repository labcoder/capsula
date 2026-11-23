//! Exact-name credential operations, without vault commit policy.

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

/// This platform's store; code outside `platform` names only this one.
#[cfg(windows)]
pub(crate) use super::windows::WindowsCredentialStore as NativeCredentialStore;

#[cfg(all(test, windows))]
pub(crate) mod fixture;

/// Secret bytes whose owned allocation is wiped on drop and whose debug output is redacted.
pub type SecretBytes = Secret<Zeroizing<Vec<u8>>>;

/// The storage design's chunk size, below Windows' 2,560-byte generic credential limit.
pub use kumochi_protocol::constants::CREDENTIAL_CHUNK_SIZE as CHUNK_SIZE;

/// A credential operation failed; no target name or secret is carried in an error.
#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
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
