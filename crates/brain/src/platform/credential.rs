//! Exact-name credential operations and bounded chunks, without vault commit policy.

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
pub const CHUNK_SIZE: usize = 2_000;
/// The storage design's maximum chunks in one record slot.
pub use kumochi_protocol::constants::ACCOUNT_MAX_CHUNKS as MAX_CHUNKS;

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
    /// A chunk count or secret size exceeds the bounded record size.
    #[error("invalid credential chunk count")]
    InvalidChunkCount,
    /// A named chunk is absent; partial records never load.
    #[error("credential chunk is missing")]
    MissingChunk,
    /// A chunk is oversized, empty, or short before the final chunk.
    #[error("invalid credential chunk length")]
    InvalidChunkLength,
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

/// Writes `<base>/0` through `<base>/<count-1>`, returning the chunk count.
///
/// This is not atomic and does not remove leftovers. The vault chooses an inactive slot,
/// cleans it, and commits a manifest separately. An empty secret uses one empty chunk.
///
/// # Errors
/// Rejects values over 64 chunks before writing; otherwise reports the first failed write.
pub fn set_chunks(
    store: &dyn CredentialStore,
    base: &str,
    secret: &SecretBytes,
) -> Result<usize, CredentialStoreError> {
    let bytes = secret.expose();
    let count = bytes.len().div_ceil(CHUNK_SIZE).max(1);
    validate_count(count)?;
    if bytes.is_empty() {
        store.set(&format!("{base}/0"), secret)?;
    } else {
        for (index, bytes) in bytes.chunks(CHUNK_SIZE).enumerate() {
            let chunk = Secret::new(Zeroizing::new(bytes.to_vec()));
            store.set(&format!("{base}/{index}"), &chunk)?;
        }
    }
    Ok(count)
}

/// Reads exactly `count` chunks, never scanning for an end marker or falling back to old data.
///
/// # Errors
/// Rejects invalid counts, missing/malformed chunks, and any failed OS read.
pub fn get_chunks(
    store: &dyn CredentialStore,
    base: &str,
    count: usize,
) -> Result<SecretBytes, CredentialStoreError> {
    validate_count(count)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(count * CHUNK_SIZE));
    for index in 0..count {
        let chunk = store
            .get(&format!("{base}/{index}"))?
            .ok_or(CredentialStoreError::MissingChunk)?;
        let chunk = chunk.expose();
        if chunk.len() > CHUNK_SIZE
            || (index + 1 < count && chunk.len() != CHUNK_SIZE)
            || (count > 1 && chunk.is_empty())
        {
            return Err(CredentialStoreError::InvalidChunkLength);
        }
        bytes.extend_from_slice(chunk);
    }
    Ok(Secret::new(bytes))
}

/// Deletes exactly `count` chunk names, trying every name even if an earlier delete fails.
///
/// # Errors
/// Rejects invalid counts; otherwise returns the first delete failure, if any.
pub fn delete_chunks(
    store: &dyn CredentialStore,
    base: &str,
    count: usize,
) -> Result<(), CredentialStoreError> {
    validate_count(count)?;
    let mut result = Ok(());
    for index in 0..count {
        if let Err(error) = store.delete(&format!("{base}/{index}"))
            && result.is_ok()
        {
            result = Err(error);
        }
    }
    result
}

fn validate_count(count: usize) -> Result<(), CredentialStoreError> {
    if !(1..=MAX_CHUNKS).contains(&count) {
        return Err(CredentialStoreError::InvalidChunkCount);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct MemoryStore(Mutex<BTreeMap<String, SecretBytes>>);

    impl CredentialStore for MemoryStore {
        fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
            Ok(self.0.lock().expect("test lock").get(target).cloned())
        }

        fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
            self.0
                .lock()
                .expect("test lock")
                .insert(target.to_owned(), secret.clone());
            Ok(())
        }

        fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
            if target == "refused/0" {
                return Err(CredentialStoreError::Delete(5));
            }
            self.0.lock().expect("test lock").remove(target);
            Ok(())
        }
    }

    fn bytes(len: usize) -> SecretBytes {
        Secret::new(Zeroizing::new(vec![42; len]))
    }

    #[test]
    fn chunks_round_trip_at_boundaries_and_delete_only_the_named_count() {
        let store = MemoryStore::default();
        for len in [0, 1, CHUNK_SIZE, CHUNK_SIZE + 1, CHUNK_SIZE * MAX_CHUNKS] {
            let expected = bytes(len);
            let count = set_chunks(&store, "slot", &expected).expect("write chunks");
            let actual = get_chunks(&store, "slot", count).expect("read chunks");
            assert!(
                actual.expose() == expected.expose(),
                "round trip of {len} bytes"
            );
            delete_chunks(&store, "slot", count).expect("delete chunks");
            assert!(
                store.0.lock().expect("test lock").is_empty(),
                "all chunks removed"
            );
        }
        store.set("slot/2", &bytes(1)).expect("neighbor");
        delete_chunks(&store, "slot", 2).expect("missing deletes succeed");
        assert!(
            store.get("slot/2").expect("neighbor read").is_some(),
            "neighbor survives"
        );
    }

    #[test]
    fn chunk_bounds_and_partial_records_fail_closed() {
        let store = MemoryStore::default();
        for count in [0, MAX_CHUNKS + 1, usize::MAX] {
            assert!(matches!(
                get_chunks(&store, "slot", count),
                Err(CredentialStoreError::InvalidChunkCount)
            ));
            assert!(matches!(
                delete_chunks(&store, "slot", count),
                Err(CredentialStoreError::InvalidChunkCount)
            ));
        }
        assert!(matches!(
            set_chunks(&store, "slot", &bytes(CHUNK_SIZE * MAX_CHUNKS + 1)),
            Err(CredentialStoreError::InvalidChunkCount)
        ));
        assert!(
            store.0.lock().expect("test lock").is_empty(),
            "oversize writes nothing"
        );
        store
            .set("slot/0", &bytes(CHUNK_SIZE))
            .expect("first chunk");
        assert!(matches!(
            get_chunks(&store, "slot", 2),
            Err(CredentialStoreError::MissingChunk)
        ));
        store.set("slot/1", &bytes(0)).expect("empty final chunk");
        assert!(matches!(
            get_chunks(&store, "slot", 2),
            Err(CredentialStoreError::InvalidChunkLength)
        ));
        store
            .set("slot/0", &bytes(1))
            .expect("short nonfinal chunk");
        assert!(matches!(
            get_chunks(&store, "slot", 2),
            Err(CredentialStoreError::InvalidChunkLength)
        ));
        store
            .set("slot/0", &bytes(CHUNK_SIZE + 1))
            .expect("oversized chunk");
        assert!(matches!(
            get_chunks(&store, "slot", 1),
            Err(CredentialStoreError::InvalidChunkLength)
        ));
    }

    #[test]
    fn failed_delete_still_removes_later_chunks() {
        let store = MemoryStore::default();
        store.set("refused/1", &bytes(1)).expect("second chunk");
        assert!(matches!(
            delete_chunks(&store, "refused", 2),
            Err(CredentialStoreError::Delete(5))
        ));
        assert!(
            store.get("refused/1").expect("second chunk read").is_none(),
            "later delete runs"
        );
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new(Zeroizing::new(b"test-only-secret-sentinel".to_vec()));
        assert_eq!(format!("{secret:?}"), "***");
    }
}
