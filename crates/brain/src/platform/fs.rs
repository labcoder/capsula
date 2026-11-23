//! Ordinary file operations where Windows' platform adapter is absent.

use std::io;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;

/// No per-user runtime data root is configured on this platform.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    None
}

/// No per-user runtime config root is configured on this platform.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    None
}

/// Replaces a file at an explicit sibling path in a Linux scratch test.
///
/// # Errors
/// Fails for non-sibling paths, or when opening, syncing the source or renaming fails.
///
/// The test process exits unsuccessfully if directory sync fails after the rename.
#[cfg(all(test, target_os = "linux"))]
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let parent = to
        .parent()
        .filter(|parent| parent.is_absolute() && from.parent() == Some(*parent))
        .ok_or(io::ErrorKind::InvalidInput)?;
    let directory = std::fs::File::open(parent)?;
    std::fs::File::open(from)?.sync_all()?;
    std::fs::rename(from, to)?;
    if directory.sync_all().is_err() {
        use std::io::Write as _;

        std::io::stderr()
            .write_all(b"broken Linux test host: could not sync the directory after replacement\n")
            .ok();
        std::process::exit(1);
    }
    Ok(())
}

/// Replaces a file only when a storage adapter is available.
///
/// # Errors
/// Returns `Unsupported` without accessing either path.
#[cfg(not(all(test, target_os = "linux")))]
pub fn replace_file(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The bytes available to unprivileged writes on the filesystem containing a Linux test path.
///
/// # Errors
/// Fails when the path's filesystem can't be queried or the byte count overflows.
#[cfg(all(test, target_os = "linux"))]
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let stat = rustix::fs::statvfs(dir)?;
    stat.f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

/// Reports free space only when a storage adapter is available.
///
/// # Errors
/// Returns `Unsupported`; the caller must not assume there is room.
#[cfg(not(all(test, target_os = "linux")))]
pub fn free_space(_: &Path) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Only an explicit unsupported-operation error permits replacing without a hard link.
#[must_use]
pub fn links_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{free_space, links_unsupported, replace_file};
    use std::fs;
    use std::io;

    #[test]
    fn scratch_replacement_preserves_the_old_file_until_the_rename() {
        let scratch = crate::scratch::dir("replace-sibling");
        let from = scratch.join("next");
        let to = scratch.join("current");
        let unrelated = scratch.join("other").join("next");
        fs::create_dir(scratch.join("other")).expect("other directory");
        fs::write(&unrelated, b"unrelated").expect("other file");
        fs::write(&to, b"old").expect("old file");
        assert_eq!(
            replace_file(&unrelated, &to)
                .expect_err("not siblings")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            replace_file(&from, &to).expect_err("no source file").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read(&to).expect("old file"), b"old");

        fs::write(&from, b"new").expect("replacement");
        replace_file(&from, &to).expect("rename and sync");
        assert_eq!(fs::read(&to).expect("new file"), b"new");
        assert!(!from.exists(), "the source name was moved");
    }

    #[test]
    fn filesystem_errors_are_not_guessed_as_capacity_or_link_support() {
        let scratch = crate::scratch::dir("fs-errors");
        assert_eq!(
            free_space(&scratch.join("absent"))
                .expect_err("no volume at an absent path")
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(links_unsupported(&io::ErrorKind::Unsupported.into()));
        assert!(!links_unsupported(&io::ErrorKind::PermissionDenied.into()));
    }
}
