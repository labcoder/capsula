//! Voice model files under the admitted Support anchor: private folders, exclusive partials,
//! sealed finals and a removal that checks the whole tree before it deletes anything.
//!
//! The same sample as the rest of this module, under one cooperating writer: descriptors hold
//! objects, not names. Every call walks again from `/`, looks at each entry without following it
//! before opening it, and refuses what it sees change. Nothing is re-secured. A same-user rename
//! between two calls isn't detected.
//!
//! shortcut: an entry that already exists is admitted on its present metadata, not proven private
//! since it was made; upgrade with first-use provisioning if earlier storage must be proven.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::Path;

use super::private::{LIMITS, list, look};
use super::protected::{ObjectKind, inspect_private};
use super::shell_files::{Directories, admit_support};
use crate::Untrusted;
use crate::platform::unix::{
    AcquisitionError, DIRECTORY_FLAGS, components, named, open_at, same_object, stat,
};

const READ_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
const WRITE_FLAGS: libc::c_int =
    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
/// A partial file's mode until it's sealed.
const PARTIAL: libc::mode_t = 0o600;

/// The mode a published file is sealed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seal {
    /// 0400, for model files.
    ReadOnly,
    /// 0500, for staged native libraries.
    ReadExecute,
}

impl Seal {
    const fn mode(self) -> libc::mode_t {
        match self {
            Self::ReadOnly => 0o400,
            Self::ReadExecute => 0o500,
        }
    }
}

/// Whether a walk makes the folders it doesn't find.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Folders {
    Existing,
    Create,
}

/// Checks, opening nothing, that `path` names something below the absolute `support` anchor
/// through plain names only.
///
/// # Errors
/// `Unsafe` for a relative path, a dot, dot-dot or NUL component, a leading `//`, the anchor
/// itself, or anything outside it.
pub fn validate_private_path(support: &Path, path: &Path) -> Result<(), Untrusted> {
    below(support, path).map(drop)
}

/// Holds the chain from `/` to the folder `path`, creating nothing: the Support anchor as
/// [`crate::admit_support`] admits it, then every folder below it private (this user's, mode
/// 0700, no ACL at all) on the anchor's volume, each looked at before it's opened.
///
/// # Errors
/// `Missing` when a folder isn't there; otherwise refuses a link, a placeholder, anything but a
/// private folder, another volume and every failed or incomplete inspection.
pub fn hold_private_chain(support: &Path, path: &Path) -> Result<Vec<File>, Untrusted> {
    Ok(walk(support, below(support, path)?, Folders::Existing)?.into_files())
}

/// [`hold_private_chain`], making each missing folder at mode 0700 and flushing it and the
/// folder it's in. An existing folder must already pass.
///
/// # Errors
/// As [`hold_private_chain`], and a failed creation or flush; a folder this call made that
/// isn't private refuses and stays.
pub fn create_private_dir(support: &Path, path: &Path) -> Result<Vec<File>, Untrusted> {
    Ok(walk(support, below(support, path)?, Folders::Create)?.into_files())
}

/// Creates the file `path` exclusively at mode 0600 in its held private folder, locks it and
/// flushes the folder. It never opens an existing file.
///
/// # Errors
/// `Io` when anything is already at the name or the lock is held; otherwise refuses like
/// [`hold_private_chain`] and on a file that isn't private once made.
pub fn create_private_file(support: &Path, path: &Path) -> Result<File, Untrusted> {
    let (held, name) = parent_of(support, path)?;
    let folder = held.last()?;
    let file = open_at(
        folder.as_raw_fd(),
        &name,
        WRITE_FLAGS | libc::O_CREAT | libc::O_EXCL,
        PARTIAL,
    )
    .map_err(untrusted)?;
    let checked = inspect_private(&file, held.user(), ObjectKind::Exact(PARTIAL))?;
    named(folder, &name, &checked)?;
    lock(&file)?;
    sync(folder)?;
    held.check().map_err(untrusted)?;
    Ok(file)
}

/// Opens the existing partial file `path` for writing without truncating it, and locks it.
///
/// # Errors
/// `Missing` when nothing is there; `Io` when the lock is held; otherwise refuses anything but
/// this user's regular file at mode 0600 with one name and no ACL, before opening it.
pub fn open_private_write(support: &Path, path: &Path) -> Result<File, Untrusted> {
    let (held, name) = parent_of(support, path)?;
    let file = existing(&held, &name, WRITE_FLAGS, PARTIAL)?;
    lock(&file)?;
    Ok(file)
}

/// Opens the sealed file `path` read-only: the checked read-only open.
///
/// # Errors
/// `Missing` when nothing is there; otherwise refuses anything but this user's regular file at
/// exactly `seal`'s mode with one name and no ACL, before opening it.
pub fn open_sealed(support: &Path, path: &Path, seal: Seal) -> Result<File, Untrusted> {
    let (held, name) = parent_of(support, path)?;
    existing(&held, &name, READ_FLAGS, seal.mode())
}

/// Checks that `path` still names the held `file`.
///
/// # Errors
/// `Unsafe` when it names something else; otherwise as [`hold_private_chain`] for its folder.
pub fn check_named(support: &Path, file: &File, path: &Path) -> Result<(), Untrusted> {
    let (held, name) = parent_of(support, path)?;
    named(held.last()?, &name, &stat(file.as_raw_fd())?)
}

/// Keeps an already sealed file: checks that `path` still names it, then flushes its folder and
/// the file, completing the barrier an earlier publish may not have finished.
///
/// # Errors
/// As [`check_named`], and `Io` when a flush fails.
pub fn keep_sealed(support: &Path, file: &File, path: &Path) -> Result<(), Untrusted> {
    let (held, name) = parent_of(support, path)?;
    let folder = held.last()?;
    named(folder, &name, &stat(file.as_raw_fd())?)?;
    sync(folder)?;
    file.sync_all().map_err(|_| Untrusted::Io)
}

/// Seals the partial `file`, named `from`, at `seal`'s mode, renames it to its sibling `to`
/// without replacing anything there, then flushes the folder and the file. `parent` must be the
/// held folder they're in. A rename that fails puts the partial back at mode 0600.
///
/// # Errors
/// `Unsafe` for names that aren't siblings or a `from` that no longer names `file`; `Reparse` when
/// `parent` isn't their folder; `Io` when something is already at `to`, and when a flush fails, which
/// leaves the file published but its durability unproved.
pub fn seal_and_publish(
    support: &Path,
    file: File,
    parent: &File,
    from: &Path,
    to: &Path,
    seal: Seal,
) -> Result<(), Untrusted> {
    if from.parent() != to.parent() {
        return Err(Untrusted::Unsafe);
    }
    let source = below(support, from)?.pop().ok_or(Untrusted::Unsafe)?;
    let (held, target) = parent_of(support, to)?;
    let folder = held.last()?;
    if !same_object(&stat(folder.as_raw_fd())?, &stat(parent.as_raw_fd())?) {
        return Err(Untrusted::Reparse);
    }
    let identity = inspect_private(&file, held.user(), ObjectKind::Exact(PARTIAL))?;
    named(folder, &source, &identity)?;
    chmod(&file, seal.mode())?;
    let renamed = inspect_private(&file, held.user(), ObjectKind::Exact(seal.mode()))
        .and_then(|_| rename(folder, &source, &target));
    if let Err(error) = renamed {
        // A partial left sealed would refuse to resume.
        chmod(&file, PARTIAL)?;
        return Err(error);
    }
    sync(folder)?;
    file.sync_all().map_err(|_| Untrusted::Io)?;
    named(folder, &target, &stat(file.as_raw_fd())?)?;
    drop(file);
    Ok(())
}

/// Deletes the entry `path` when it still names the held `file`, then flushes the folder.
/// `parent` must be the held folder it's in.
///
/// # Errors
/// `Reparse` when `parent` isn't its folder; `Unsafe` when the name gives something else, which
/// deletes nothing; `Io` when the unlink or the flush fails.
pub fn discard_entry(
    support: &Path,
    file: File,
    parent: &File,
    path: &Path,
) -> Result<(), Untrusted> {
    let (held, name) = parent_of(support, path)?;
    let folder = held.last()?;
    if !same_object(&stat(folder.as_raw_fd())?, &stat(parent.as_raw_fd())?) {
        return Err(Untrusted::Reparse);
    }
    if !same_object(&look(folder, &name)?, &stat(file.as_raw_fd())?) {
        return Err(Untrusted::Unsafe);
    }
    unlink(folder, &name, 0)?;
    drop(file);
    sync(folder)
}

/// Deletes the folder `path` and everything in it in two passes, then flushes the folder it was
/// in. The first looks at the whole tree, opening only folders: every folder must be private, every
/// file this user's at mode 0600, 0500 or 0400, all on one volume, within 32 levels and 65,536
/// entries. The second deletes leaves first, each once its name still gives what the first pass
/// saw. A folder already gone flushes its folder and counts as deleted.
///
/// # Errors
/// Refuses a link, any other object, a placeholder, a foreign owner, another mode or volume and a
/// folder with an ACL before deleting anything. `Io` for anything that vanishes or changes after
/// the first look, a failed listing, a bound, or a failed unlink or flush.
pub fn remove_private_tree(support: &Path, path: &Path) -> Result<(), Untrusted> {
    let (held, name) = parent_of(support, path)?;
    let folder = held.last()?;
    let seen = match look(folder, &name) {
        Err(Untrusted::Missing) => return sync(folder),
        seen => seen?,
    };
    let mut tree = Tree {
        user: held.user(),
        device: stat(folder.as_raw_fd())?.st_dev,
        nodes: Vec::new(),
        listed: 0,
    };
    tree.gather(folder, None, name, &seen, 0)?;
    held.check().map_err(untrusted)?;
    for node in tree.nodes.iter().rev() {
        let parent = match node.parent {
            None => folder,
            Some(index) => tree.nodes[index].folder.as_ref().ok_or(Untrusted::Io)?,
        };
        let now = look(parent, &node.name).map_err(vanished)?;
        if !same_object(&now, &node.identity)
            || now.st_mode & libc::S_IFMT != node.identity.st_mode & libc::S_IFMT
        {
            return Err(Untrusted::Io);
        }
        let flags = if node.folder.is_some() {
            libc::AT_REMOVEDIR
        } else {
            0
        };
        unlink(parent, &node.name, flags)?;
    }
    sync(folder)
}

/// One entry the removal's first pass saw, its folder held when it is one.
struct Node {
    /// The index of the folder it's in; `None` for the removed folder itself.
    parent: Option<usize>,
    name: CString,
    identity: libc::stat,
    folder: Option<File>,
}

/// The removal's first pass, in the order it saw entries: each folder before what's in it.
struct Tree {
    user: libc::uid_t,
    device: libc::dev_t,
    nodes: Vec<Node>,
    listed: usize,
}

impl Tree {
    fn gather(
        &mut self,
        parent: &File,
        at: Option<usize>,
        name: CString,
        seen: &libc::stat,
        depth: usize,
    ) -> Result<(), Untrusted> {
        if seen.st_dev != self.device || seen.st_uid != self.user {
            return Err(Untrusted::Unsafe);
        }
        let index = self.nodes.len();
        match seen.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                if !matches!(seen.st_mode & 0o7777, 0o400 | 0o500 | 0o600) {
                    return Err(Untrusted::Unsafe);
                }
                self.nodes.push(Node {
                    parent: at,
                    name,
                    identity: *seen,
                    folder: None,
                });
                Ok(())
            }
            libc::S_IFDIR => {
                if depth > LIMITS.depth {
                    return Err(Untrusted::Io);
                }
                let folder = folder_at(parent, &name, seen, self.user)?;
                self.nodes.push(Node {
                    parent: at,
                    name,
                    identity: *seen,
                    folder: None,
                });
                for child in list(&folder, LIMITS, &mut self.listed)? {
                    let found = look(&folder, &child).map_err(vanished)?;
                    self.gather(&folder, Some(index), child, &found, depth + 1)?;
                }
                self.nodes[index].folder = Some(folder);
                Ok(())
            }
            libc::S_IFLNK => Err(Untrusted::Reparse),
            _ => Err(Untrusted::Unsafe),
        }
    }
}

/// `path`'s components below `support`, each a plain name. Lexical: it opens nothing.
fn below(support: &Path, path: &Path) -> Result<Vec<CString>, Untrusted> {
    if !support.is_absolute() || !path.is_absolute() {
        return Err(Untrusted::Unsafe);
    }
    let anchor = components(support)?;
    let mut names = components(path)?;
    if names.len() <= anchor.len() || names[..anchor.len()] != anchor[..] {
        return Err(Untrusted::Unsafe);
    }
    Ok(names.split_off(anchor.len()))
}

/// The held chain to `path`'s folder, and `path`'s own name in it.
fn parent_of(support: &Path, path: &Path) -> Result<(Directories, CString), Untrusted> {
    let mut names = below(support, path)?;
    let name = names.pop().ok_or(Untrusted::Unsafe)?;
    Ok((walk(support, names, Folders::Existing)?, name))
}

/// The anchor's held chain, then each of `names` in turn as a private folder.
fn walk(support: &Path, names: Vec<CString>, folders: Folders) -> Result<Directories, Untrusted> {
    let mut held = admit_support(support).map_err(untrusted)?.into_held();
    for name in names {
        let parent = held.last()?;
        let made = folders == Folders::Create && make(parent, &name)?;
        let folder = folder_at(parent, &name, &look(parent, &name)?, held.user())?;
        held.push_private(folder, name).map_err(untrusted)?;
        if made {
            sync(held.before_last()?)?;
            sync(held.last()?)?;
        }
    }
    held.check().map_err(untrusted)?;
    Ok(held)
}

/// Makes the folder `name` at mode 0700, and says whether this call made it.
fn make(parent: &File, name: &CStr) -> Result<bool, Untrusted> {
    // SAFETY: parent is held and name one validated component; an existing entry is kept.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EEXIST) => Ok(false),
        _ => Err(Untrusted::Io),
    }
}

/// The folder `name` in `parent`, which the look found as `seen`, opened and checked private.
fn folder_at(
    parent: &File,
    name: &CStr,
    seen: &libc::stat,
    user: libc::uid_t,
) -> Result<File, Untrusted> {
    kind(seen, libc::S_IFDIR)?;
    let folder = open_at(parent.as_raw_fd(), name, DIRECTORY_FLAGS, 0)
        .map_err(untrusted)
        .map_err(vanished)?;
    let checked = inspect_private(&folder, user, ObjectKind::Directory)?;
    if !same_object(seen, &checked) {
        return Err(Untrusted::Unsafe);
    }
    Ok(folder)
}

/// The existing file `name` in the last held folder, looked at first: a regular file with one
/// name, this user's, at exactly `mode`. Then opened without following it, compared with the look
/// and checked again through the descriptor, ACL included.
fn existing(
    held: &Directories,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, Untrusted> {
    let folder = held.last()?;
    let seen = look(folder, name)?;
    kind(&seen, libc::S_IFREG)?;
    if seen.st_nlink != 1 || seen.st_uid != held.user() || seen.st_mode & 0o7777 != mode {
        return Err(Untrusted::Unsafe);
    }
    let file = open_at(folder.as_raw_fd(), name, flags, 0)
        .map_err(untrusted)
        .map_err(vanished)?;
    let checked = inspect_private(&file, held.user(), ObjectKind::Exact(mode))?;
    if !same_object(&seen, &checked) {
        return Err(Untrusted::Unsafe);
    }
    named(folder, name, &checked)?;
    held.check().map_err(untrusted)?;
    Ok(file)
}

fn kind(seen: &libc::stat, expected: libc::mode_t) -> Result<(), Untrusted> {
    match seen.st_mode & libc::S_IFMT {
        found if found == expected => Ok(()),
        libc::S_IFLNK => Err(Untrusted::Reparse),
        _ => Err(Untrusted::Unsafe),
    }
}

fn lock(file: &File) -> Result<(), Untrusted> {
    file.try_lock().map_err(|_| Untrusted::Io)
}

fn chmod(file: &File, mode: libc::mode_t) -> Result<(), Untrusted> {
    // SAFETY: file is held and not consumed; mode is one of this module's fixed modes.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

fn rename(folder: &File, from: &CStr, to: &CStr) -> Result<(), Untrusted> {
    let fd = folder.as_raw_fd();
    // SAFETY: folder is held and both names are validated components; neither is followed and an
    // entry at `to` is never replaced.
    if unsafe { libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

fn unlink(folder: &File, name: &CStr, flags: libc::c_int) -> Result<(), Untrusted> {
    // SAFETY: folder is held and name a validated component just compared; it isn't followed.
    if unsafe { libc::unlinkat(folder.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

/// `fsync` of a held folder, which records its entries.
fn sync(folder: &File) -> Result<(), Untrusted> {
    // SAFETY: folder is a held descriptor this call doesn't consume.
    if unsafe { libc::fsync(folder.as_raw_fd()) } != 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

fn untrusted(error: AcquisitionError) -> Untrusted {
    match error {
        AcquisitionError::Admission(error) => error,
        AcquisitionError::Open(io::ErrorKind::NotFound) => Untrusted::Missing,
        AcquisitionError::Open(_) => Untrusted::Io,
    }
}

/// Something the look found that's gone by the open.
fn vanished(error: Untrusted) -> Untrusted {
    match error {
        Untrusted::Missing => Untrusted::Io,
        error => error,
    }
}

#[cfg(test)]
mod tests;
