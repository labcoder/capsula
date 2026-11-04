//! One shell per Windows session, through the standard library's file lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// Held for the shell's whole life; closing the file releases the lock.
#[derive(Debug)]
#[must_use = "dropping the guard lets another shell start"]
pub(crate) struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Opens or creates the empty lock file at `path` and locks it without waiting, or returns
    /// `None` when another shell holds it.
    ///
    /// # Errors
    /// Fails when the folder or file can't be created, or the lock fails for a reason other than
    /// another holder.
    pub(crate) fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
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
    use std::path::PathBuf;

    use super::InstanceLock;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kumochi-instance-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_second_shell_is_refused_until_the_first_drops_its_lock() {
        let dir = scratch("refused");
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
        let second = InstanceLock::try_acquire(&path).expect("the lock opens");
        assert!(second.is_some(), "dropping the guard releases the lock");
        drop(second);
        assert_eq!(fs::metadata(&path).expect("the lock file").len(), 0);
        fs::remove_dir_all(dir).expect("scratch removed");
    }

    #[test]
    fn a_lock_that_cannot_open_is_an_error_not_another_shell() {
        let dir = scratch("error");
        fs::create_dir_all(dir.join("shell-1.lock")).expect("a directory where the file goes");
        assert!(InstanceLock::try_acquire(&dir.join("shell-1.lock")).is_err());
        fs::remove_dir_all(dir).expect("scratch removed");
    }
}
