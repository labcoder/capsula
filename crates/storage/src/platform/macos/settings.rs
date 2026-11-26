//! The settings file in this edition's product folder, read and replaced through the held
//! Support chain, the only names it touches being `CONFIG_FILE` and its reserved `.tmp` sibling.
//!
//! A sample under one cooperating writer, the brain's session lock: descriptors hold objects, not
//! names, so this refuses what it sees change but is no defence against a hostile owning user.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::Path;

use kumochi_protocol::constants::CONFIG_FILE;

use super::private::DATALESS;
use super::protected::{ObjectKind, inspect_private};
use super::shell_files::{
    AcquisitionError, DIRECTORY_FLAGS, Directories, admit_support, basename, entry_stat, named,
    open_at,
};
use crate::Untrusted;

const READ_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
const STAGE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// How a settings replacement that didn't finish ended. It carries no path or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsWriteError {
    /// Nothing was published: the settings file is the one that was there.
    #[error("the settings weren't written")]
    Unchanged(#[from] AcquisitionError),
    /// The new file may be the one named, but that it's durable, or still named, isn't known.
    #[error("whether the settings were written isn't known")]
    Unconfirmed,
}

impl From<Untrusted> for SettingsWriteError {
    fn from(error: Untrusted) -> Self {
        Self::Unchanged(error.into())
    }
}

/// Where a test makes a replacement fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    AnchorSync,
    RenameBefore,
    RenameAfter,
    FolderSync,
    Recheck,
}

#[cfg(test)]
thread_local! {
    /// The one failure this thread's next replacement meets.
    pub(super) static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
}

/// Reads `CONFIG_FILE` in `product` under the existing Support anchor `support`, creating nothing.
/// `None` only when the product folder or the file is missing under the admitted anchor.
///
/// # Errors
/// Refuses a missing or unsafe anchor, a product folder that isn't private like
/// [`crate::owner_only_dir`]'s, a file that isn't this user's with mode 0600, one link and no
/// ACL, a link, anything that changed while it was read, and a failed read.
pub fn read_settings(support: &Path, product: &str) -> Result<Option<Vec<u8>>, AcquisitionError> {
    let Some(held) = product_chain(support, product, false)? else {
        return Ok(None);
    };
    let (name, _) = leaves()?;
    let folder = held.last()?;
    let Some((file, checked)) = private_file(folder, &name, held.user())? else {
        held.check()?;
        return Ok(None);
    };
    let mut text = Vec::new();
    (&file)
        .read_to_end(&mut text)
        .map_err(|error| AcquisitionError::Open(error.kind()))?;
    named(folder, &name, &checked)?;
    held.check()?;
    Ok(Some(text))
}

/// Replaces `CONFIG_FILE` in `product` under the existing Support anchor `support` with `text`,
/// creating the product folder at mode 0700 when it's missing. The text goes to the reserved
/// sibling first, created exclusively at 0600 after any crash leftover there is unlinked unread;
/// it's flushed, renamed over the file, and the folder and file are flushed again. The anchor is
/// flushed before the rename, so a first save's folder entry is on disk too.
///
/// # Errors
/// `Unchanged` when nothing was published; `Unconfirmed` when the new file may be named but a
/// barrier, a rename error or the final recheck leaves its durability or identity unproved.
pub fn replace_settings(
    support: &Path,
    product: &str,
    text: &[u8],
) -> Result<(), SettingsWriteError> {
    let held = product_chain(support, product, true)?.ok_or(Untrusted::Io)?;
    let (name, temp) = leaves()?;
    let folder = held.last()?;
    let user = held.user();
    sync(held.before_last()?, Fault::AnchorSync).map_err(AcquisitionError::Open)?;
    let old = private_file(folder, &name, user)?.map(|(_, checked)| checked);
    // SAFETY: folder is held and temp is the fixed reserved leaf; unlinking never follows it.
    if unsafe { libc::unlinkat(folder.as_raw_fd(), temp.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(AcquisitionError::Open(error.kind()).into());
        }
    }
    let staged = open_at(folder.as_raw_fd(), &temp, STAGE_FLAGS, 0o600)?;
    let identity = inspect_private(&staged, user, ObjectKind::Regular)?;
    (&staged)
        .write_all(text)
        .and_then(|()| staged.sync_all())
        .map_err(|error| AcquisitionError::Open(error.kind()))?;
    named(folder, &temp, &identity)?;
    unchanged(folder, &name, old.as_ref())?;
    held.check()?;
    if let Err(error) = rename(folder, &temp, &name) {
        let published = entry_stat(folder, &name)
            .is_ok_and(|now| now.st_dev == identity.st_dev && now.st_ino == identity.st_ino);
        if !published {
            return if unchanged(folder, &name, old.as_ref()).is_ok()
                && named(folder, &temp, &identity).is_ok()
            {
                Err(AcquisitionError::Open(error.kind()).into())
            } else {
                Err(SettingsWriteError::Unconfirmed)
            };
        }
    }
    if sync(folder, Fault::FolderSync).is_err() || staged.sync_all().is_err() {
        return Err(SettingsWriteError::Unconfirmed);
    }
    if injected(Fault::Recheck) || named(folder, &name, &identity).is_err() || held.check().is_err()
    {
        return Err(SettingsWriteError::Unconfirmed);
    }
    Ok(())
}

/// The admitted anchor's chain with the private product folder last; `None` only when `create`
/// is false and opening that exact name in the held anchor found nothing.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a read creates nothing, a replacement makes a missing product folder"
)]
fn product_chain(
    support: &Path,
    product: &str,
    create: bool,
) -> Result<Option<Directories>, AcquisitionError> {
    let product = basename(product)?;
    let mut held = admit_support(support)?.into_held();
    held.check()?;
    let anchor = held.last()?;
    // SAFETY: anchor is held and product is a validated basename; nothing existing is changed.
    if create && unsafe { libc::mkdirat(anchor.as_raw_fd(), product.as_ptr(), 0o700) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(AcquisitionError::Open(error.kind()));
        }
    }
    let folder = match open_at(anchor.as_raw_fd(), &product, DIRECTORY_FLAGS, 0) {
        Ok(folder) => folder,
        Err(AcquisitionError::Open(io::ErrorKind::NotFound)) if !create => {
            held.check()?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    inspect_private(&folder, held.user(), ObjectKind::Directory)?;
    held.push_private(folder, product)?;
    held.check()?;
    Ok(Some(held))
}

fn leaves() -> Result<(CString, CString), Untrusted> {
    Ok((
        basename(CONFIG_FILE)?,
        basename(&format!("{CONFIG_FILE}.tmp"))?,
    ))
}

/// `name` in `folder` as a private regular file, opened read-only without following it and
/// checked; `None` only when the first look finds nothing. A second name refuses before the open.
fn private_file(
    folder: &File,
    name: &CStr,
    user: libc::uid_t,
) -> Result<Option<(File, libc::stat)>, AcquisitionError> {
    let seen = match entry_stat(folder, name) {
        Ok(seen) => seen,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Untrusted::Io.into()),
    };
    match seen.st_mode & libc::S_IFMT {
        libc::S_IFREG => {}
        libc::S_IFLNK => return Err(Untrusted::Reparse.into()),
        _ => return Err(Untrusted::Unsafe.into()),
    }
    if seen.st_nlink != 1 || seen.st_flags & DATALESS != 0 {
        return Err(Untrusted::Unsafe.into());
    }
    let file = open_at(folder.as_raw_fd(), name, READ_FLAGS, 0)?;
    let checked = inspect_private(&file, user, ObjectKind::Regular)?;
    if checked.st_dev != seen.st_dev || checked.st_ino != seen.st_ino {
        return Err(Untrusted::Unsafe.into());
    }
    Ok(Some((file, checked)))
}

/// `name` still gives `old`, or is still missing when there was none.
fn unchanged(folder: &File, name: &CStr, old: Option<&libc::stat>) -> Result<(), Untrusted> {
    match old {
        Some(old) => named(folder, name, old),
        None => match entry_stat(folder, name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => Err(Untrusted::Unsafe),
        },
    }
}

fn rename(folder: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    if injected(Fault::RenameBefore) {
        return Err(io::Error::from_raw_os_error(libc::EBUSY));
    }
    let fd = folder.as_raw_fd();
    // SAFETY: folder is held and both names are the fixed leaves; neither is followed.
    if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if injected(Fault::RenameAfter) {
        return Err(io::Error::from_raw_os_error(libc::EBUSY));
    }
    Ok(())
}

/// `fsync` of a held folder, which records its entries.
fn sync(folder: &File, fault: Fault) -> Result<(), io::ErrorKind> {
    if injected(fault) {
        return Err(io::ErrorKind::Other);
    }
    // SAFETY: folder is a held descriptor this call doesn't consume.
    if unsafe { libc::fsync(folder.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error().kind());
    }
    Ok(())
}

#[cfg(test)]
fn injected(fault: Fault) -> bool {
    FAULT.get() == Some(fault) && FAULT.replace(None).is_some()
}

#[cfg(not(test))]
const fn injected(_: Fault) -> bool {
    false
}

#[cfg(test)]
mod tests;
