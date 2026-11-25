//! Exclusive account-session ownership through the standard library's file lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// Another session owns the lock, or the lock file could not be opened or locked.
#[derive(Debug, thiserror::Error)]
pub enum SessionLockError {
    /// A different file handle already holds the lock.
    #[error("account session is busy")]
    Busy,
    /// Creating, opening or locking the file failed for a reason other than contention.
    #[error("account session lock failed")]
    Io(#[from] io::Error),
}

/// Owns the account session until dropped, when closing the file releases its lock.
///
/// Keep this guard for the brain's whole session, including until all blocking vault work ends.
#[derive(Debug)]
#[must_use = "dropping the guard releases account-session ownership"]
pub struct SessionLock {
    _file: File,
}

impl SessionLock {
    /// Opens or creates the lock file and tries to acquire exclusive ownership without waiting.
    ///
    /// The parent folder is created if needed. The file is never written or truncated.
    ///
    /// # Errors
    /// Returns [`SessionLockError::Busy`] only for contention; all other failures retain their
    /// I/O error. Production callers use [`super::session_lock_path`] for the path.
    pub fn try_acquire(path: &Path) -> Result<Self, SessionLockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Self::try_lock(file)
    }

    /// Locks an existing empty regular file on the handle whose type and size were checked,
    /// creating nothing. Only contention is Busy. The caller still needs a trusted, stable parent
    /// for profile admission.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the existing-profile reader that calls it isn't landed yet"
        )
    )]
    pub(crate) fn try_acquire_existing(path: &Path) -> Result<Self, SessionLockError> {
        let file = open_leaf(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        Self::try_lock(file)
    }

    fn try_lock(file: File) -> Result<Self, SessionLockError> {
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => SessionLockError::Busy,
            TryLockError::Error(error) => SessionLockError::Io(error),
        })?;
        Ok(Self { _file: file })
    }
}

/// Opens the leaf itself, never a reparse target; backup semantics let a folder open, so it's
/// refused as a non-file. Read and write sharing lets a stock opener reach the lock and see
/// contention, and without delete sharing the name can't be renamed or deleted while it's open.
#[cfg(windows)]
fn open_leaf(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_leaf(path: &Path) -> io::Result<File> {
    // shortcut: the check and the open aren't atomic, so a link swapped in between is followed;
    // open with O_NOFOLLOW before a Unix profile reader is wired into startup.
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    File::open(path)
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::constants::SESSION_LOCK;

    use crate::test_support::Scratch;

    use super::*;

    #[test]
    fn session_is_busy_until_the_guard_drops_and_the_file_stays_empty() {
        let scratch = Scratch::new("session-lock");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        let first = SessionLock::try_acquire(&path).expect("first session owns the lock");
        assert!(
            matches!(SessionLock::try_acquire(&path), Err(SessionLockError::Busy)),
            "another handle cannot own the session"
        );
        assert_eq!(fs::metadata(&path).expect("lock metadata").len(), 0);
        drop(first);
        let second = SessionLock::try_acquire(&path).expect("drop releases ownership");
        assert_eq!(fs::metadata(&path).expect("lock metadata").len(), 0);
        drop(second);
        assert!(
            fs::read(&path).expect("lock contents").is_empty(),
            "the lock holds no data"
        );
    }

    #[test]
    fn an_io_failure_is_not_busy() {
        let scratch = Scratch::new("session-lock-error");
        let directory = scratch.db().with_file_name("directory");
        fs::create_dir_all(&directory).expect("scratch directory");
        let Err(SessionLockError::Io(error)) = SessionLock::try_acquire(&directory) else {
            panic!("an invalid lock file is an I/O error, not contention");
        };
        #[cfg(windows)]
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        #[cfg(not(windows))]
        assert_eq!(error.kind(), std::io::ErrorKind::IsADirectory);
    }

    #[test]
    fn an_existing_lock_and_a_stock_opener_contend_in_either_order() {
        let scratch = Scratch::new("session-lock-existing");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        drop(SessionLock::try_acquire(&path).expect("the stock opener creates the lock"));
        let existing = SessionLock::try_acquire_existing(&path).expect("the existing lock locks");
        assert!(matches!(
            SessionLock::try_acquire(&path),
            Err(SessionLockError::Busy)
        ));
        drop(existing);
        let stock = SessionLock::try_acquire(&path).expect("drop releases ownership");
        assert!(matches!(
            SessionLock::try_acquire_existing(&path),
            Err(SessionLockError::Busy)
        ));
        drop(stock);
        drop(SessionLock::try_acquire_existing(&path).expect("drop releases ownership"));
        assert!(fs::read(&path).expect("lock contents").is_empty());
    }

    #[test]
    fn an_existing_lock_refuses_a_link_even_to_a_busy_lock() {
        let scratch = Scratch::new("session-lock-existing-link");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        let other = path.with_file_name("other.lock");
        let _owner = SessionLock::try_acquire(&other).expect("the stock opener creates the target");
        #[cfg(windows)]
        crate::platform::test_symlink_file(&other, &path)
            .expect("own lock symlink: needs the symbolic-link privilege or Developer Mode");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, &path).expect("own lock symlink");
        match SessionLock::try_acquire_existing(&path) {
            Err(SessionLockError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("expected a link refusal, not {other:?}"),
        }
        assert_eq!(fs::read_link(&path).expect("the link stays"), other);
    }

    #[cfg(windows)]
    fn refusal(result: Result<SessionLock, SessionLockError>) -> io::Error {
        match result {
            Err(SessionLockError::Io(error)) => error,
            other => panic!("expected an I/O refusal, not {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_held_existing_lock_keeps_its_name() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let scratch = Scratch::new("session-lock-held-name");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        let moved = path.with_file_name("moved.lock");
        drop(SessionLock::try_acquire(&path).expect("the stock opener creates the lock"));
        let held = SessionLock::try_acquire_existing(&path).expect("the existing lock locks");
        for error in [
            fs::rename(&path, &moved).expect_err("a held lock can't be renamed"),
            fs::remove_file(&path).expect_err("a held lock can't be deleted"),
        ] {
            assert_eq!(
                error.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION.cast_signed())
            );
        }
        assert!(path.is_file() && !moved.exists());
        drop(held);
        fs::rename(&path, &moved).expect("the name is free once the guard drops");
    }

    #[cfg(windows)]
    #[test]
    fn share_and_acl_refusals_are_not_busy() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let scratch = Scratch::new("session-lock-refused");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        drop(SessionLock::try_acquire(&path).expect("the stock opener creates the lock"));
        let exclusive = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("a holder that shares nothing");
        assert_eq!(
            refusal(SessionLock::try_acquire_existing(&path)).raw_os_error(),
            Some(ERROR_SHARING_VIOLATION.cast_signed())
        );
        drop(exclusive);
        let _stock = SessionLock::try_acquire(&path).expect("a stock holder");
        crate::platform::set_dacl(&path, |user| {
            format!("D:P(D;;0x1;;;{user})(A;;FA;;;{user})")
        });
        assert_eq!(
            refusal(SessionLock::try_acquire_existing(&path)).kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
