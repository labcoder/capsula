//! Writes each brain-issued staging name once with non-roaming persistence.

use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
};
use windows::core::PWSTR;
use zeroize::Zeroizing;

use crate::credential::CredentialError;
use crate::platform::credential::{
    KeyStageWriter, PastedAnthropicKey, StageError, StagePermit, write_stage,
};
use keyring::Entry;
use kumochi_protocol::Secret;
use kumochi_protocol::constants::{INSTANCE_SERVICE, instance_entry};
use kumochi_protocol::message::{CAPABILITY_LEN, Capability, KeyStageId};

pub(crate) fn read_instance(session: u32) -> Result<Capability, CredentialError> {
    let account = instance_entry(session);
    let bytes = Entry::new(INSTANCE_SERVICE, &account)?.get_secret()?;
    let bytes: [u8; CAPABILITY_LEN] = bytes.try_into().map_err(|_| CredentialError::WrongLength)?;
    Ok(Secret::new(bytes))
}

#[derive(Debug)]
pub(crate) struct WindowsKeyStageWriter;

impl KeyStageWriter for WindowsKeyStageWriter {
    fn write(
        &self,
        permit: StagePermit,
        key: PastedAnthropicKey,
    ) -> Result<KeyStageId, StageError> {
        write_stage(permit, key, write)
    }
}

fn write(target: &str, bytes: &[u8]) -> Result<(), StageError> {
    write_with(target, bytes, |credential, _blob| {
        // SAFETY: the initialized credential and live target/zeroizing blob outlive this call.
        unsafe { CredWriteW(credential, 0) }.map_err(|_| StageError::Store)
    })
}

fn write_with(
    target: &str,
    bytes: &[u8],
    call: impl FnOnce(&CREDENTIALW, &Zeroizing<Vec<u8>>) -> Result<(), StageError>,
) -> Result<(), StageError> {
    let mut target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let mut blob = Zeroizing::new(bytes.to_vec());
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(blob.len()).map_err(|_| StageError::InvalidPlan)?,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..CREDENTIALW::default()
    };
    call(&credential, &blob)
}

#[cfg(test)]
pub(crate) fn assert_offline_native_header() {
    let mut calls = 0;
    let result = write_with(
        "synthetic-target",
        b"synthetic-fixture",
        |credential, blob| {
            calls += 1;
            assert_eq!(credential.Type, CRED_TYPE_GENERIC);
            assert_eq!(credential.Persist, CRED_PERSIST_LOCAL_MACHINE);
            assert_eq!(credential.CredentialBlobSize, 17);
            assert_eq!(credential.CredentialBlob.cast_const(), blob.as_ptr());
            assert_eq!(blob.as_slice(), b"synthetic-fixture");
            Ok(())
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(calls, 1);
}

#[cfg(test)]
mod tests {
    use std::{ptr, slice, time::SystemTime};

    use kumochi_protocol::constants::CREDENTIAL_SERVICE;
    use windows::Win32::Security::Credentials::{CredDeleteW, CredFree, CredReadW};
    use windows::core::PCWSTR;
    use zeroize::Zeroize;

    use super::*;

    #[test]
    #[ignore = "writes the real credential store; only run in an authorized isolated gate"]
    fn native_roundtrip_is_generic_and_local_machine() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = format!(
            "{CREDENTIAL_SERVICE}/shell-credential-test-{nonce}-{}",
            std::process::id()
        );
        let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        assert_eq!(write(&target, b"synthetic-fixture"), Ok(()));
        let mut raw = ptr::null_mut();
        // SAFETY: target is terminated and live; raw is a writable out pointer.
        unsafe { CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None, &raw mut raw) }.unwrap();
        // SAFETY: a successful CredReadW returned this allocation, which is not freed yet.
        let credential = unsafe { &mut *raw };
        // SAFETY: the successful read owns a live blob of CredentialBlobSize bytes.
        let blob = unsafe {
            slice::from_raw_parts_mut(
                credential.CredentialBlob,
                usize::try_from(credential.CredentialBlobSize).unwrap(),
            )
        };
        let actual = Zeroizing::new(blob.to_vec());
        blob.zeroize();
        let kind = credential.Type;
        let persistence = credential.Persist;
        // SAFETY: the successful read allocation is freed once after its last use.
        unsafe { CredFree(raw.cast()) };
        // SAFETY: target is terminated and live; flags are reserved zero.
        unsafe { CredDeleteW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None) }.unwrap();
        assert_eq!(actual.as_slice(), b"synthetic-fixture");
        assert_eq!(kind, CRED_TYPE_GENERIC);
        assert_eq!(persistence, CRED_PERSIST_LOCAL_MACHINE);
    }
}
