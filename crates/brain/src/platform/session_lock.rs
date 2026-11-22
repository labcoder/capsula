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
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => SessionLockError::Busy,
            TryLockError::Error(error) => SessionLockError::Io(error),
        })?;
        Ok(Self { _file: file })
    }
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
}
