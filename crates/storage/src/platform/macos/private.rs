//! Owner-only folders and private trees, checked through held descriptors.
//!
//! A sample under serialized, cooperating writers: fds hold objects, not names, so nothing here
//! is a frozen pathname or an atomic snapshot. Nothing is re-secured; an unsafe object refuses.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::Path;

use super::protected::{
    ObjectKind, components, hold_from, inspect_private, open_directory, same_inspection, stat,
};
use super::shell_files::{AcquisitionError, Entries, Next, named, writable_volume};
use crate::Untrusted;

/// How far a private walk goes before it refuses.
#[derive(Clone, Copy)]
struct Limits {
    /// Folders below the root.
    depth: usize,
    /// Entries seen across the whole walk.
    entries: usize,
}

/// Opens a file for its metadata only. Nothing reads through it: it stays an `OwnedFd`, which has
/// no read methods. Non-blocking, so a FIFO swapped in after the look can't stall the open.
const FILE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

const LIMITS: Limits = Limits {
    depth: 32,
    entries: 65_536,
};

/// Creates `path` when it's missing as a folder only this user may use (mode 0700, no ACL), then
/// opens and checks it. An existing folder is used only if it passes the same checks; one that
/// doesn't is refused, never re-secured. A folder this call made that a umask or an inherited ACL
/// left unprivate is refused too, and stays. Its parent is held from `/` and must sit on a local,
/// writable APFS volume that enforces ownership.
///
/// # Errors
/// Refuses a relative or root path, a privileged or mixed identity, an untrusted parent chain,
/// a link, anything but a private folder, and every failed or incomplete inspection.
pub fn owner_only_dir(path: &Path) -> Result<File, Untrusted> {
    let (parent, name, user) = held_parent(path)?;
    owner_only_at(&parent, &name, user)
}

/// Checks `path` and everything in it, each entry looked at without following it: every folder
/// must be private like [`owner_only_dir`]'s, and every file owned by this user, mode 0600, with one
/// link and no ACL at all. A file is opened read-only for its metadata and closed; it is never
/// read. Nothing else may be there, and nothing is re-secured. Its parent is held from `/`.
///
/// # Errors
/// `Missing` when `path` is; otherwise refuses like [`owner_only_dir`], and on a file that isn't
/// private, a link, any other object, a listing failure, or more than 32 levels or 65,536 entries.
pub fn private_tree(path: &Path) -> Result<(), Untrusted> {
    let (parent, name, user) = held_parent(path)?;
    private_at(&parent, &name, user, LIMITS)
}

fn held_parent(path: &Path) -> Result<(File, CString, libc::uid_t), Untrusted> {
    if !path.is_absolute() {
        return Err(Untrusted::Unsafe);
    }
    let mut names = components(path)?;
    let name = names.pop().ok_or(Untrusted::Unsafe)?;
    let user = user()?;
    let root = open_directory(libc::AT_FDCWD, c"/")?;
    let parent = hold_from(root, &names, user)?.pop().ok_or(Untrusted::Io)?;
    writable_volume(&parent)?;
    Ok((parent, name, user))
}

/// This process's user, when it isn't root and its real and effective identities match.
fn user() -> Result<libc::uid_t, Untrusted> {
    // SAFETY: getuid has no arguments or ownership effects.
    let user = unsafe { libc::getuid() };
    // SAFETY: geteuid only returns the effective identity.
    let effective = unsafe { libc::geteuid() };
    // SAFETY: getgid only returns the real group identity.
    let group = unsafe { libc::getgid() };
    // SAFETY: getegid only returns the effective group identity.
    let effective_group = unsafe { libc::getegid() };
    if user == 0 || user != effective || group != effective_group {
        return Err(Untrusted::Unsafe);
    }
    Ok(user)
}

fn owner_only_at(parent: &File, name: &CStr, user: libc::uid_t) -> Result<File, Untrusted> {
    // SAFETY: parent is held and name is one validated component; nothing existing is changed.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0
        && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
    {
        return Err(Untrusted::Io);
    }
    private_folder(parent, name, user).map(|(folder, _)| folder)
}

/// `name` in `parent`, looked at without following it, opened, and checked as a private folder
/// on its parent's device that the name still gives. `Missing` only when the first look finds
/// nothing.
fn private_folder(
    parent: &File,
    name: &CStr,
    user: libc::uid_t,
) -> Result<(File, libc::stat), Untrusted> {
    folder_at(parent, name, &look(parent, name)?, user)
}

/// [`private_folder`] after its look, which found `seen`.
fn folder_at(
    parent: &File,
    name: &CStr,
    seen: &libc::stat,
    user: libc::uid_t,
) -> Result<(File, libc::stat), Untrusted> {
    match seen.st_mode & libc::S_IFMT {
        libc::S_IFDIR => {}
        libc::S_IFLNK => return Err(Untrusted::Reparse),
        _ => return Err(Untrusted::Unsafe),
    }
    let folder = open_directory(parent.as_raw_fd(), name).map_err(|error| match error {
        Untrusted::Missing => Untrusted::Io,
        error => error,
    })?;
    let checked = inspect_private(&folder, user, ObjectKind::Directory)?;
    same_entry(parent, seen, &checked)?;
    named(parent, name, &checked)?;
    Ok((folder, checked))
}

/// `name` in `parent`, a regular file at the look, opened read-only without following it and
/// checked as a private file on its parent's device that the name still gives. Never read.
fn private_file(
    parent: &File,
    name: &CStr,
    seen: &libc::stat,
    user: libc::uid_t,
) -> Result<(), Untrusted> {
    // SAFETY: parent is held and name is one listed component; there's no creation flag.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), FILE_FLAGS) };
    if fd < 0 {
        return Err(match io::Error::last_os_error().raw_os_error() {
            Some(libc::ELOOP) => Untrusted::Reparse,
            _ => Untrusted::Io,
        });
    }
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    let file = unsafe { OwnedFd::from_raw_fd(fd) };
    let checked = inspect_private(&file, user, ObjectKind::Regular)?;
    same_entry(parent, seen, &checked)?;
    named(parent, name, &checked)
}

/// The checked object is the one the look found, on its parent's device.
fn same_entry(parent: &File, seen: &libc::stat, checked: &libc::stat) -> Result<(), Untrusted> {
    if checked.st_dev != stat(parent.as_raw_fd())?.st_dev
        || checked.st_dev != seen.st_dev
        || checked.st_ino != seen.st_ino
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn look(parent: &File, name: &CStr) -> Result<libc::stat, Untrusted> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: parent is held, name is terminated and metadata is writable stat storage.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Untrusted::Missing,
            _ => Untrusted::Io,
        });
    }
    // SAFETY: successful fstatat initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

fn private_at(
    parent: &File,
    name: &CStr,
    user: libc::uid_t,
    limits: Limits,
) -> Result<(), Untrusted> {
    walk(parent, name, user, limits, 0, &mut 0)
}

fn walk(
    parent: &File,
    name: &CStr,
    user: libc::uid_t,
    limits: Limits,
    depth: usize,
    seen: &mut usize,
) -> Result<(), Untrusted> {
    if depth > limits.depth {
        return Err(Untrusted::Io);
    }
    let found = look(parent, name)?;
    if found.st_mode & libc::S_IFMT == libc::S_IFREG {
        return private_file(parent, name, &found, user);
    }
    let (folder, checked) = folder_at(parent, name, &found, user)?;
    for child in list(&folder, limits, seen)? {
        match walk(&folder, &child, user, limits, depth + 1, seen) {
            Ok(()) | Err(Untrusted::Missing) => {}
            Err(error) => return Err(error),
        }
    }
    // A folder renamed or replaced during the walk refuses rather than passing.
    let now = stat(folder.as_raw_fd())?;
    if !same_inspection(&checked, &now) {
        return Err(Untrusted::Unsafe);
    }
    named(parent, name, &now)
}

/// Every name in `folder` but the dots, counted into `seen`. A failed or partial listing refuses.
fn list(folder: &File, limits: Limits, seen: &mut usize) -> Result<Vec<CString>, Untrusted> {
    let mut entries = Entries::open(folder).map_err(|error| match error {
        AcquisitionError::Admission(error) => error,
        AcquisitionError::Open(_) => Untrusted::Io,
    })?;
    let mut names = Vec::new();
    loop {
        let name = match entries.next_name() {
            Next::Name(name) => name,
            Next::End => {
                #[cfg(test)]
                tests::listed(&mut names);
                return Ok(names);
            }
            Next::Malformed | Next::Failed => return Err(Untrusted::Io),
        };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        *seen += 1;
        if *seen > limits.entries {
            return Err(Untrusted::Io);
        }
        names.push(name);
    }
}

#[cfg(test)]
mod tests;
