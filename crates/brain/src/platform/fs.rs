//! Ordinary file operations where Windows' platform adapter is absent.

use std::io;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
use std::fs::{self, File};
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
use std::io::Write as _;

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

/// Replaces a file at an explicit sibling path in an isolated Unix scratch test.
///
/// # Errors
/// Fails for non-sibling paths, or when opening, syncing the source or renaming fails.
///
/// If publication is uncertain, exit status 78 tells the scratch runner to retain the root.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    replace_with(from, to, |from, to| fs::rename(from, to), sync_publication)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn replace_with(
    from: &Path,
    to: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync: impl FnOnce(&File, &File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = to
        .parent()
        .filter(|parent| parent.is_absolute() && from.parent() == Some(*parent))
        .ok_or(io::ErrorKind::InvalidInput)?;
    let directory = File::open(parent)?;
    let staged = File::open(from)?;
    staged.sync_all()?;
    match rename(from, to) {
        Ok(()) if sync(&directory, &staged).is_ok() => return Ok(()),
        Err(error) if error.raw_os_error() != Some(rustix::io::Errno::IO.raw_os_error()) => {
            return Err(error);
        }
        _ => {}
    }

    std::io::stderr()
        .write_all(b"scratch file publication is uncertain; stopping without cleanup\n")
        .ok();
    std::process::exit(crate::test_support::SCRATCH_PUBLICATION_UNCERTAIN);
}

#[cfg(all(test, target_os = "linux"))]
fn sync_publication(directory: &File, _: &File) -> io::Result<()> {
    directory.sync_all()
}

#[cfg(all(test, target_os = "macos"))]
fn sync_publication(directory: &File, staged: &File) -> io::Result<()> {
    rustix::fs::fsync(directory)?;
    staged.sync_all()
}

/// Replaces a file only when a storage adapter is available.
///
/// # Errors
/// Returns `Unsupported` without accessing either path.
#[cfg(not(all(test, any(target_os = "linux", target_os = "macos"))))]
pub fn replace_file(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The bytes available to the caller on the filesystem containing a Unix test path.
///
/// # Errors
/// Fails when the path's filesystem can't be queried or the byte count overflows.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub fn free_space(dir: &Path) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    let stat = rustix::fs::statvfs(dir)?;
    #[cfg(target_os = "linux")]
    let block_size = stat.f_frsize;
    #[cfg(target_os = "macos")]
    let stat = rustix::fs::statfs(dir)?;
    #[cfg(target_os = "macos")]
    let block_size = u64::from(stat.f_bsize);
    available_bytes(stat.f_bavail, block_size)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn available_bytes(blocks: u64, block_size: u64) -> io::Result<u64> {
    blocks
        .checked_mul(block_size)
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

/// Reports free space only when a storage adapter is available.
///
/// # Errors
/// Returns `Unsupported`; the caller must not assume there is room.
#[cfg(not(all(test, any(target_os = "linux", target_os = "macos"))))]
pub fn free_space(_: &Path) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Only an explicit unsupported-operation error permits replacing without a hard link.
#[must_use]
pub fn links_unsupported(error: &io::Error) -> bool {
    #[cfg(all(test, target_os = "macos"))]
    if let Some(code) = error.raw_os_error() {
        use rustix::io::Errno;
        return code == Errno::NOTSUP.raw_os_error() || code == Errno::OPNOTSUPP.raw_os_error();
    }
    error.kind() == io::ErrorKind::Unsupported
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::{available_bytes, free_space, links_unsupported, replace_file, replace_with};
    use crate::test_support::{SCRATCH_PUBLICATION_UNCERTAIN, propagate_scratch_publication};
    use std::env;
    use std::fs;
    use std::io::{self, Read as _, Write as _};
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

    #[test]
    fn scratch_replacement_preserves_the_old_file_until_the_rename() {
        let scratch = crate::scratch::dir("replace-sibling");
        let from = scratch.join("next");
        let to = scratch.join("current");
        let unrelated = scratch.join("other").join("next");
        fs::create_dir(scratch.join("other")).expect("other directory");
        fs::write(&unrelated, b"unrelated").expect("other file");
        fs::write(&to, b"old").expect("old file");
        let mut reader = fs::File::open(&to).expect("old reader");
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
        let error =
            replace_file(&from, &scratch.join("other")).expect_err("destination is a directory");
        assert_eq!(error.kind(), io::ErrorKind::IsADirectory);
        assert_eq!(fs::read(&unrelated).expect("unrelated file"), b"unrelated");
        assert_eq!(fs::read(&from).expect("staged file"), b"new");
        replace_file(&from, &to).expect("rename and sync");
        assert_eq!(fs::read(&to).expect("new file"), b"new");
        assert!(!from.exists(), "the source name was moved");
        let mut held = Vec::new();
        reader.read_to_end(&mut held).expect("read old inode");
        assert_eq!(held, b"old");
    }

    #[test]
    #[ignore = "invoked with a private scratch ticket by the publication test"]
    fn publication_child() {
        let root = env::current_dir().expect("scratch directory");
        let temp = env::temp_dir().canonicalize().expect("scratch parent");
        assert_eq!(root.parent(), Some(temp.as_path()));
        let mut ticket = [0; 17];
        io::stdin().read_exact(&mut ticket).expect("scratch ticket");
        let from = root.join("next");
        let metadata = fs::symlink_metadata(&from).expect("staging metadata");
        assert!(metadata.is_file() && metadata.len() == 16);
        assert!(
            fs::read(&from).expect("staging proof") == ticket[1..],
            "scratch ticket does not match"
        );
        let scenario = ticket[0];
        assert!(scenario <= 2, "unknown publication scenario");
        if scenario == 2 {
            ticket[0] = 0;
            propagate_scratch_publication(publication_child_output(&root, &ticket).status);
            panic!("nested uncertainty returned to its parent");
        }
        let result = replace_with(
            &from,
            &root.join("current"),
            |from, to| {
                fs::rename(from, to)?;
                if scenario == 1 {
                    return Err(rustix::io::Errno::IO.into());
                }
                Ok(())
            },
            |_, _| {
                assert_eq!(scenario, 0, "rename EIO cannot reach the barrier");
                Err(rustix::io::Errno::IO.into())
            },
        );
        panic!("uncertain publication returned: {result:?}");
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "this Unix test spawns an owned child under the shared spawn lock"
    )]
    fn publication_child_output(root: &Path, ticket: &[u8; 17]) -> Output {
        let mut child = {
            let _spawning = crate::platform::spawn_lock();
            Command::new(env::current_exe().expect("test binary"))
                .args([
                    "--ignored",
                    "--exact",
                    "platform::fs::tests::publication_child",
                ])
                .env_clear()
                .env("TMPDIR", env::temp_dir())
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn publication child")
        };
        let sent = child.stdin.take().expect("ticket pipe").write_all(ticket);
        let output = child.wait_with_output().expect("reap publication child");
        sent.expect("send scratch ticket");
        output
    }

    #[test]
    fn uncertain_publication_leaves_both_named_copies_without_returning() {
        for scenario in [0, 1, 2] {
            let scratch = crate::scratch::dir("replace-uncertain");
            let current = scratch.join("current");
            let old = scratch.join("old");
            let mut ticket = [scenario; 17];
            getrandom::fill(&mut ticket[1..]).expect("scratch ticket entropy");
            fs::write(&current, b"old").expect("old file");
            fs::hard_link(&current, &old).expect("old alias");
            fs::write(scratch.join("next"), &ticket[1..]).expect("staged file");
            let child = publication_child_output(scratch.path(), &ticket);
            assert_eq!(
                child.status.code(),
                Some(SCRATCH_PUBLICATION_UNCERTAIN),
                "scenario {scenario}: {child:?}"
            );
            assert_eq!(fs::read(current).expect("new canonical"), ticket[1..]);
            assert_eq!(fs::read(old).expect("old alias"), b"old");
        }
    }

    #[test]
    fn capacity_counts_wide_available_blocks_and_refuses_overflow() {
        assert_eq!(
            available_bytes(0x1_0000_0001, 4096).expect("wide capacity"),
            17_592_186_048_512
        );
        assert_eq!(
            available_bytes(u64::MAX, 2).expect_err("overflow").kind(),
            io::ErrorKind::InvalidData
        );
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

    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_two_darwin_unsupported_link_errnos_permit_fallback() {
        for code in [45, 102] {
            assert!(links_unsupported(&io::Error::from_raw_os_error(code)));
        }
        for code in [1, 5, 17, 18] {
            assert!(!links_unsupported(&io::Error::from_raw_os_error(code)));
        }
    }
}
