//! The shell's per-user lock, opened through admitted Application Support storage, and its one
//! read of the instance capability.

use std::fs::TryLockError;
use std::path::{Path, PathBuf};

use kumochi_protocol::constants;
use kumochi_storage::{
    AdmittedLock, INSTANCE_LEN, InstanceItem, ItemError, ProductFolder, Role, ShellStorage,
};

use crate::instance::{FirstTouchError, SessionLockError};

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

/// Takes `<SHELL_LOCK>.lock` in the existing product folder under `support` without waiting,
/// then makes `logs`; never creates the product folder. The storage stays with the lock.
pub(crate) fn try_acquire_existing_lock(
    support: &Path,
) -> Result<(AdmittedLock, ShellStorage), FirstTouchError> {
    let anchor = kumochi_storage::admit_support(support).map_err(|_| FirstTouchError::Refused)?;
    match anchor
        .product_folder(constants::DATA_DIR)
        .map_err(|_| FirstTouchError::Refused)?
    {
        ProductFolder::Missing(_) => Err(FirstTouchError::Missing),
        ProductFolder::Held(product) => product
            .try_lock_storage(&format!("{}.lock", constants::SHELL_LOCK))
            .map_err(|_| FirstTouchError::Refused)?
            .ok_or(FirstTouchError::Busy),
    }
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
    use std::os::unix::fs::DirBuilderExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kumochi_storage::{AcquisitionError, Untrusted};

    use super::*;

    /// A fresh private Support folder under the commissioned fixture parent.
    fn support(label: &str) -> PathBuf {
        let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
            .expect("an explicitly commissioned fixture parent");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let support = Path::new(&parent).join(format!(
            "first-touch-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&support)
            .expect("exclusive fixture anchor");
        support
    }

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    fn native_first_touch_creates_no_product_folder() {
        let support = support("missing");
        let touched = try_acquire_existing_lock(&support).map(drop);
        let made = std::fs::read_dir(&support)
            .expect("list the anchor")
            .count();
        std::fs::remove_dir(&support).expect("remove the fixture anchor");
        assert_eq!(touched, Err(FirstTouchError::Missing));
        assert_eq!(made, 0, "nothing was created");
    }

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    fn native_first_touch_is_busy_while_another_shell_holds_the_lock() {
        let support = support("busy");
        let product = support.join(constants::DATA_DIR);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&product)
            .expect("the brain's product folder");
        let held = try_acquire_existing_lock(&support).expect("the first shell holds the lock");
        let second = try_acquire_existing_lock(&support).map(drop);
        drop(held);
        let after = try_acquire_existing_lock(&support).map(drop);
        std::fs::remove_dir(product.join("logs")).expect("remove the fixture logs folder");
        std::fs::remove_file(product.join(format!("{}.lock", constants::SHELL_LOCK)))
            .expect("remove the fixture lock");
        std::fs::remove_dir(&product).expect("remove the fixture product folder");
        std::fs::remove_dir(&support).expect("remove the fixture anchor");
        assert_eq!(second, Err(FirstTouchError::Busy));
        assert_eq!(after, Ok(()), "dropping the lock releases it");
    }

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
