//! Exclusive account-session ownership through the standard library's file lock.
//!
//! On Unix the existing-lock open refuses a final link at the open itself. A device node at the
//! path is still opened, without becoming a controlling terminal, before its type check refuses it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
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
    _file: File,
    /// The credential authority beside the locked file, fixed when the lock was taken.
    authority: PathBuf,
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
        let authority = authority_beside(path)?;
        let file = open_leaf(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        Self::try_lock(file, authority)
    }

    /// The credential authority file beside this lock, by the pathname the lock was taken at. A
    /// writable authority opens only here, so nothing resolves the data folder a second time;
    /// this binds the pathname, not the folder's identity if it is replaced.
    pub(crate) fn authority_path(&self) -> &Path {
        &self.authority
    }

    fn try_lock(file: File, authority: PathBuf) -> Result<Self, SessionLockError> {
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => SessionLockError::Busy,
            TryLockError::Error(error) => SessionLockError::Io(error),
        })?;
        Ok(Self {
            _file: file,
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

/// Opens the leaf itself, never a link's target, so the checks and the lock land on what this
/// open found: a final link is refused as `InvalidData`. Nonblocking, so a FIFO is refused as a
/// non-file instead of waiting for a writer.
#[cfg(not(windows))]
fn open_leaf(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;

    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::LOOP) => Err(io::ErrorKind::InvalidData.into()),
        Err(errno) => Err(errno.into()),
    }
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
        for refusal in [
            SessionLock::try_acquire(&folder.join(SESSION_LOCK)),
            SessionLock::try_acquire_existing(&folder.join(SESSION_LOCK)),
        ] {
            let Err(SessionLockError::Io(error)) = refusal else {
                panic!("a relative lock path must be refused");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn an_existing_fifo_is_refused_without_waiting() {
        let scratch = Scratch::new("session-lock-existing-fifo");
        let path = scratch.db().with_file_name(SESSION_LOCK);
        fs::create_dir_all(scratch.dir()).expect("scratch folder");
        rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::RUSR).expect("own fifo");
        match SessionLock::try_acquire_existing(&path) {
            Err(SessionLockError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("expected a non-file refusal, not {other:?}"),
        }
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
