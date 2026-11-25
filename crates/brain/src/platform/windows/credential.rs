//! Non-roaming generic credentials, accessed only by exact target name.

use std::fmt;
use std::io;
use std::ptr::{self, NonNull};
use std::slice;

use keyring::Entry;
use kumochi_protocol::Secret;
use kumochi_protocol::constants::{INSTANCE_SERVICE, instance_entry};
use kumochi_protocol::message::Capability;
use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
use windows_sys::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_MAX_GENERIC_TARGET_NAME_LENGTH, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use zeroize::{Zeroize, Zeroizing};

use crate::platform::credential::{
    CredentialError, CredentialStore, CredentialStoreError, SecretBytes,
};

/// Stores the capability for Windows session `session`, replacing the previous brain's there.
///
/// # Errors
/// Fails when Credential Manager refuses the write.
pub fn store_instance(session: u32, capability: &Capability) -> Result<(), CredentialError> {
    Ok(entry(session)?.set_secret(capability.expose())?)
}

/// Deletes the capability. Deleting one that isn't there succeeds.
///
/// # Errors
/// Fails when Credential Manager refuses the delete.
pub fn delete_instance(session: u32) -> Result<(), CredentialError> {
    match entry(session)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// One entry per session, because each session runs its own brain.
fn entry(session: u32) -> keyring::Result<Entry> {
    Entry::new(INSTANCE_SERVICE, &instance_entry(session))
}

/// Windows Credential Manager with local-machine persistence, never profile roaming.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsCredentialStore;

impl CredentialStore for WindowsCredentialStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        let Some(allocation) = read(target)? else {
            return Ok(None);
        };
        allocation.secret().map(Some)
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        let mut target = target_wide(target)?;
        let len = u32::try_from(secret.expose().len())
            .ok()
            .filter(|len| *len <= CRED_MAX_CREDENTIAL_BLOB_SIZE)
            .ok_or(CredentialStoreError::BlobTooLarge)?;
        let mut blob = Zeroizing::new(secret.expose().to_vec());
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: len,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..CREDENTIALW::default()
        };
        // SAFETY: the initialized credential and bounded, live target/blob buffers outlive the call.
        if unsafe { CredWriteW(&raw const credential, 0) } == 0 {
            let code = io::Error::last_os_error()
                .raw_os_error()
                .expect("a Win32 last error has a raw OS code");
            return Err(CredentialStoreError::Write(code));
        }
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        let target = target_wide(target)?;
        // SAFETY: target is a validated, live, null-terminated UTF-16 buffer; flags are reserved zero.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let code = io::Error::last_os_error()
                .raw_os_error()
                .expect("a Win32 last error has a raw OS code");
            if u32::try_from(code) != Ok(ERROR_NOT_FOUND) {
                return Err(CredentialStoreError::Delete(code));
            }
        }
        Ok(())
    }
}

fn target_wide(target: &str) -> Result<Vec<u16>, CredentialStoreError> {
    let max = usize::try_from(CRED_MAX_GENERIC_TARGET_NAME_LENGTH)
        .expect("the Windows target limit fits usize");
    if target.is_empty() || target.contains('\0') || target.encode_utf16().count() > max {
        return Err(CredentialStoreError::InvalidTarget);
    }
    Ok(target.encode_utf16().chain(Some(0)).collect())
}

fn read(target: &str) -> Result<Option<ReadCredential>, CredentialStoreError> {
    let target = target_wide(target)?;
    let mut credential = ptr::null_mut();
    // SAFETY: target is live and null-terminated; credential is a writable out pointer.
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut credential) } == 0 {
        let code = io::Error::last_os_error()
            .raw_os_error()
            .expect("a Win32 last error has a raw OS code");
        return if u32::try_from(code) == Ok(ERROR_NOT_FOUND) {
            Ok(None)
        } else {
            Err(CredentialStoreError::Read(code))
        };
    }
    let credential = NonNull::new(credential).ok_or(CredentialStoreError::InvalidBlob)?;
    Ok(Some(ReadCredential(credential)))
}

fn blob_len(credential: &CREDENTIALW) -> Result<usize, CredentialStoreError> {
    let len = credential.CredentialBlobSize;
    if len > CRED_MAX_CREDENTIAL_BLOB_SIZE || (len > 0 && credential.CredentialBlob.is_null()) {
        return Err(CredentialStoreError::InvalidBlob);
    }
    Ok(usize::try_from(len).expect("the bounded Windows blob length fits usize"))
}

struct ReadCredential(NonNull<CREDENTIALW>);

impl ReadCredential {
    fn credential(&self) -> &CREDENTIALW {
        // SAFETY: this non-null pointer is a successful CredReadW allocation owned until Drop.
        unsafe { self.0.as_ref() }
    }

    fn secret(&self) -> Result<SecretBytes, CredentialStoreError> {
        let credential = self.credential();
        let len = blob_len(credential)?;
        if len == 0 {
            return Ok(Secret::new(Zeroizing::new(Vec::new())));
        }
        // SAFETY: CredReadW owns this live blob; its non-null pointer and bounded length were checked.
        let bytes = unsafe { slice::from_raw_parts(credential.CredentialBlob, len) };
        Ok(Secret::new(Zeroizing::new(bytes.to_vec())))
    }
}

impl fmt::Debug for ReadCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReadCredential(***)")
    }
}

impl Drop for ReadCredential {
    fn drop(&mut self) {
        let credential = self.credential();
        if let Ok(len) = blob_len(credential)
            && len > 0
        {
            // SAFETY: the checked blob is exclusively owned by this allocation, before CredFree.
            let blob = unsafe { slice::from_raw_parts_mut(credential.CredentialBlob, len) };
            blob.zeroize();
        }
        // SAFETY: this successful CredReadW allocation is freed exactly once, on every exit.
        unsafe { CredFree(self.0.as_ptr().cast()) };
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use kumochi_protocol::constants::CREDENTIAL_SERVICE;

    use crate::platform::credential::conformance;
    use crate::platform::credential::fixture::Fixture;

    use super::*;

    fn isolated(test: impl FnOnce(&str)) {
        let nonce = getrandom::u64().expect("random test service");
        let service = format!("{CREDENTIAL_SERVICE}-credential-test-{nonce:016x}");
        let _fixture = Fixture::new(vec![service.clone()]);
        test(&service);
    }

    #[test]
    fn persistence_round_trip_is_local_machine_and_debug_is_redacted() {
        isolated(|target| {
            let secret = Secret::new(Zeroizing::new(b"test-only-secret-sentinel".to_vec()));
            WindowsCredentialStore
                .set(target, &secret)
                .expect("write credential");
            let allocation = read(target).expect("CredReadW").expect("credential exists");
            assert_eq!(allocation.credential().Persist, CRED_PERSIST_LOCAL_MACHINE);
            assert_eq!(allocation.credential().Type, CRED_TYPE_GENERIC);
            assert_eq!(format!("{allocation:?}"), "ReadCredential(***)");
            drop(allocation);
            let actual = WindowsCredentialStore
                .get(target)
                .expect("read credential")
                .expect("credential exists");
            assert!(
                actual.expose() == secret.expose(),
                "persisted secret matches"
            );
        });
    }

    #[test]
    fn the_store_meets_the_shared_contract() {
        let nonce = getrandom::u64().expect("random test service");
        let target = format!("{CREDENTIAL_SERVICE}-credential-test-{nonce:016x}");
        let other = format!("{target}-other");
        let _fixture = Fixture::new(vec![target.clone(), other.clone()]);
        conformance::refuses_invalid_input(&WindowsCredentialStore, &target);
        conformance::round_trip(&WindowsCredentialStore, &target, &other);
    }

    #[test]
    fn a_write_on_one_thread_reads_back_on_another() {
        isolated(|target| {
            let planted = b"test-only-cross-thread-sentinel";
            let secret = Secret::new(Zeroizing::new(planted.to_vec()));
            WindowsCredentialStore
                .set(target, &secret)
                .expect("write on the test thread");
            thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let actual = WindowsCredentialStore
                            .get(target)
                            .expect("read on another thread")
                            .expect("credential exists");
                        assert!(
                            actual.expose().as_slice() == planted,
                            "the planted bytes read back exactly"
                        );
                    })
                    .join()
                    .expect("reader thread");
            });
        });
    }

    #[test]
    fn a_real_credential_fixture_cleans_up_while_unwinding() {
        let nonce = getrandom::u64().expect("random test service");
        let target = format!("{CREDENTIAL_SERVICE}-credential-test-{nonce:016x}");
        let outcome: Result<(), _> = std::panic::catch_unwind(|| {
            let _fixture = Fixture::new(vec![target.clone()]);
            let secret = Secret::new(Zeroizing::new(vec![42]));
            WindowsCredentialStore
                .set(&target, &secret)
                .expect("plant owned fixture data");
            panic!("intentional fixture unwind");
        });
        assert_eq!(
            outcome
                .expect_err("intentional unwind")
                .downcast_ref::<&str>(),
            Some(&"intentional fixture unwind")
        );
        let _fixture = Fixture::new(vec![target.clone()]);
        assert!(
            WindowsCredentialStore
                .get(&target)
                .expect("read exact owned name")
                .is_none(),
            "Drop cleaned up during unwinding"
        );
    }

    #[test]
    fn absence_and_missing_deletion_are_idempotent() {
        isolated(|target| {
            let store = WindowsCredentialStore;
            assert!(
                store.get(target).expect("missing read").is_none(),
                "not found is absence"
            );
            store.delete(target).expect("missing delete succeeds");
            let empty = Secret::new(Zeroizing::new(Vec::new()));
            store.set(target, &empty).expect("empty blob write");
            let actual = store
                .get(target)
                .expect("empty blob read")
                .expect("empty blob exists");
            assert!(actual.expose().is_empty(), "empty blob survives");
            store.delete(target).expect("delete existing");
            store.delete(target).expect("delete again");
        });
    }

    #[test]
    fn invalid_targets_and_blob_lengths_are_rejected_before_ffi() {
        let store = WindowsCredentialStore;
        let secret = Secret::new(Zeroizing::new(Vec::new()));
        let long = "x".repeat(32_768);
        for target in ["", "test\0suffix", &long] {
            assert!(matches!(
                store.get(target),
                Err(CredentialStoreError::InvalidTarget)
            ));
            assert!(matches!(
                store.set(target, &secret),
                Err(CredentialStoreError::InvalidTarget)
            ));
            assert!(matches!(
                store.delete(target),
                Err(CredentialStoreError::InvalidTarget)
            ));
        }
        isolated(|target| {
            let limit = usize::try_from(CRED_MAX_CREDENTIAL_BLOB_SIZE).expect("blob limit");
            let secret = Secret::new(Zeroizing::new(vec![42; limit]));
            store.set(target, &secret).expect("exact maximum blob");
            let actual = store
                .get(target)
                .expect("maximum read")
                .expect("maximum exists");
            assert!(actual.expose() == secret.expose(), "maximum blob matches");
            let oversized = Secret::new(Zeroizing::new(vec![42; limit + 1]));
            assert!(matches!(
                store.set(target, &oversized),
                Err(CredentialStoreError::BlobTooLarge)
            ));
        });
        for len in [1, CRED_MAX_CREDENTIAL_BLOB_SIZE + 1, u32::MAX] {
            let credential = CREDENTIALW {
                CredentialBlobSize: len,
                ..CREDENTIALW::default()
            };
            assert!(
                matches!(
                    blob_len(&credential),
                    Err(CredentialStoreError::InvalidBlob)
                ),
                "invalid pointer/length rejected"
            );
        }
    }
}
