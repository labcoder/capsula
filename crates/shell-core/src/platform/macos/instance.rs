//! The shell's per-user lock, opened through admitted Application Support storage, and its one
//! read of the instance capability.

use std::fs::TryLockError;
use std::path::PathBuf;

use kumochi_protocol::constants;
use kumochi_storage::{AdmittedLock, INSTANCE_LEN, InstanceItem, ItemError, Role, ShellStorage};

use crate::instance::SessionLockError;

/// Reads the brain's published instance capability into `into`, admitted as the shell.
///
/// # Errors
/// Why the item can't be admitted or read; `into` is filled only on success.
pub(crate) fn read_instance(into: &mut [u8; INSTANCE_LEN]) -> Result<(), ItemError> {
    InstanceItem::admit(Role::Shell)?.read(into)
}

/// Takes `<SHELL_LOCK>.lock` in the product folder without waiting, or returns `None` when
/// another shell of this user and identity holds it. The storage stays with the lock.
pub(crate) fn try_acquire_instance_lock()
-> Result<Option<(AdmittedLock, ShellStorage)>, SessionLockError> {
    try_acquire_under(super::support_dir().ok())
}

fn try_acquire_under(
    support: Option<PathBuf>,
) -> Result<Option<(AdmittedLock, ShellStorage)>, SessionLockError> {
    let support = support.ok_or(SessionLockError::DataDirectory)?;
    let storage = kumochi_storage::open_shell_storage(&support, constants::DATA_DIR)
        .map_err(SessionLockError::Admission)?;
    let lock = storage
        .open_instance_lock(&format!("{}.lock", constants::SHELL_LOCK))
        .map_err(SessionLockError::Admission)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some((lock, storage))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(SessionLockError::Lock(error)),
    }
}

#[cfg(test)]
mod tests {
    use kumochi_storage::{AcquisitionError, Untrusted};

    use super::*;

    #[test]
    fn a_missing_support_folder_is_refused_before_any_file() {
        assert!(matches!(
            try_acquire_under(None),
            Err(SessionLockError::DataDirectory)
        ));
    }

    #[test]
    fn a_relative_support_candidate_is_refused_before_any_file() {
        assert!(matches!(
            try_acquire_under(Some(PathBuf::from("Library/Application Support"))),
            Err(SessionLockError::Admission(AcquisitionError::Admission(
                Untrusted::Unsafe
            )))
        ));
    }
}
