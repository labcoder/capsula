//! One shell per Windows session, through the standard library's file lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// Held for the shell's whole life; closing the file releases the lock.
#[derive(Debug)]
#[must_use = "dropping the guard lets another shell start"]
pub struct InstanceLock {
    _file: File,
}

/// Why the shell couldn't acquire its current session's lock.
#[derive(Debug, thiserror::Error)]
pub enum SessionLockError {
    /// The platform could not discover its application data folder.
    #[error("can't find the data folder for the single-instance lock")]
    DataDirectory,
    /// The platform could not identify this session.
    #[error("can't read the session for the single-instance lock")]
    Session(#[source] io::Error),
    /// The lock target could not be opened or locked.
    #[error("can't take the single-instance lock")]
    Lock(#[source] io::Error),
    /// The platform has no admitted current-session lock target.
    #[error("native single-instance admission is unsupported on this platform")]
    Unsupported,
}

impl InstanceLock {
    /// Acquires the shell's lock for this session, or returns `None` when already held.
    ///
    /// # Errors
    /// Fails when the platform cannot supply a target or the lock cannot be taken.
    pub fn try_acquire_current_session() -> Result<Option<Self>, SessionLockError> {
        let path = crate::platform::instance_lock_path()?;
        Self::try_acquire(&path).map_err(SessionLockError::Lock)
    }

    /// Opens or creates the empty lock file at `path` and locks it without waiting, or returns
    /// `None` when another shell holds it.
    ///
    /// # Errors
    /// Fails when the folder or file can't be created, or the lock fails for a reason other than
    /// another holder.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::InstanceLock;
    use crate::scratch;

    #[cfg(not(windows))]
    #[test]
    fn unsupported_current_session_has_no_lock_target() {
        let error = InstanceLock::try_acquire_current_session()
            .expect_err("an unsupported session must not acquire a lock");
        assert!(matches!(error, super::SessionLockError::Unsupported));
    }

    #[test]
    fn a_second_shell_is_refused_until_the_first_drops_its_lock() {
        let dir = scratch::dir("instance-refused");
        let path = dir.join("shell-1.lock");
        let first = InstanceLock::try_acquire(&path)
            .expect("the lock opens")
            .expect("the first shell holds it");
        assert!(
            InstanceLock::try_acquire(&path)
                .expect("the lock opens")
                .is_none(),
            "a second shell is refused"
        );
        drop(first);
        let _second = InstanceLock::try_acquire(&path)
            .expect("the lock opens")
            .expect("dropping the guard releases the lock");
    }

    #[test]
    fn a_lock_that_cannot_open_is_an_error_not_another_shell() {
        let dir = scratch::dir("instance-error");
        fs::create_dir_all(dir.join("shell-1.lock")).expect("a directory where the file goes");
        assert!(InstanceLock::try_acquire(&dir.join("shell-1.lock")).is_err());
    }
}
