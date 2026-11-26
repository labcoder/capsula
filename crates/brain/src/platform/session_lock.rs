//! Exclusive account-session ownership through the standard library's file lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, io};

use kumochi_protocol::constants::AUTHORITY_FILE;

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
#[must_use = "dropping the guard releases account-session ownership"]
pub struct SessionLock {
    _guard: Guard,
    /// The credential authority beside the locked file, fixed when the lock was taken.
    authority: PathBuf,
}

/// What holds the lock: this guard's own file, or session custody's shared lock.
#[expect(
    dead_code,
    reason = "each guard is held only for its drop, which releases the lock"
)]
enum Guard {
    File(File),
    Lent(Arc<super::InstanceLock>),
}

impl fmt::Debug for SessionLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionLock")
    }
}

impl SessionLock {
    /// Opens or creates the lock file and tries to acquire exclusive ownership without waiting.
    ///
    /// The parent folder is created if needed. The file is never written or truncated.
    ///
    /// # Errors
    /// Returns [`SessionLockError::Busy`] only for contention; a relative path is refused as
    /// `InvalidInput` before anything is created, and all other failures retain their I/O error.
    pub fn try_acquire(path: &Path) -> Result<Self, SessionLockError> {
        let authority = authority_beside(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Self::try_lock(file, authority)
    }

    /// The credential authority file beside this lock, by the pathname the lock was taken at. A
    /// writable authority opens only here, so nothing resolves the data folder a second time;
    /// this binds the pathname, not the folder's identity if it is replaced.
    pub(crate) fn authority_path(&self) -> &Path {
        &self.authority
    }

    /// The folder the authority sits in: the store's and the settings' folder too.
    pub(crate) fn data_dir(&self) -> &Path {
        self.authority
            .parent()
            .expect("an absolute authority file has a folder")
    }

    /// Custody's held lock, shared, with the absolute product folder its acquisition walked.
    /// Only custody calls this, so the folder is never resolved a second time.
    pub(crate) fn lent(lock: Arc<super::InstanceLock>, product: &Path) -> Self {
        Self {
            _guard: Guard::Lent(lock),
            authority: product.join(AUTHORITY_FILE),
        }
    }

    fn try_lock(file: File, authority: PathBuf) -> Result<Self, SessionLockError> {
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => SessionLockError::Busy,
            TryLockError::Error(error) => SessionLockError::Io(error),
        })?;
        Ok(Self {
            _guard: Guard::File(file),
            authority,
        })
    }
}

/// The authority beside an absolute lock path; a relative one would follow the working folder.
fn authority_beside(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(path.with_file_name(AUTHORITY_FILE))
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::constants::SESSION_LOCK;

    use crate::test_support::Scratch;

    use super::*;

    #[test]
    fn a_relative_lock_path_is_refused_before_anything_is_created() {
        // The scratch parent sits in the working folder, so a regressed guard's folder is cleaned up.
        let cwd = std::env::current_dir().expect("working folder");
        let scratch = Scratch::under(&cwd, "relative-lock");
        let folder = scratch
            .dir()
            .strip_prefix(&cwd)
            .expect("scratch under the working folder");
        assert!(folder.is_relative());
        let Err(SessionLockError::Io(error)) = SessionLock::try_acquire(&folder.join(SESSION_LOCK))
        else {
            panic!("a relative lock path must be refused");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!scratch.dir().exists(), "nothing was created");
    }

    #[test]
    fn the_authority_is_the_one_beside_the_lock() {
        let scratch = Scratch::new("session-lock-authority");
        let lock = SessionLock::try_acquire(&scratch.dir().join(SESSION_LOCK)).expect("lock");
        assert_eq!(lock.authority_path(), scratch.dir().join("credentials.db"));
    }

    #[test]
    fn a_session_lock_prints_no_path() {
        const MARKER: &str = "kumochi-lock-path-marker";
        let scratch = Scratch::new(MARKER);
        let lock = SessionLock::try_acquire(&scratch.dir().join(SESSION_LOCK)).expect("lock");
        let printed = format!("{lock:?}");
        assert_eq!(printed, "SessionLock");
        assert!(
            !printed
                .as_bytes()
                .windows(MARKER.len())
                .any(|window| window == MARKER.as_bytes())
        );
    }

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
