//! The brain's protected operations on a held chain: marker files, private trees, deletion and
//! model storage, built from kumochi-storage's chain check, which documents the threat model.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) use kumochi_storage::checked::final_path_matches;
use kumochi_storage::checked::{
    Aces, Kind, Security, Trusted, attributes, check, open, open_checked, same_final_path,
};
use kumochi_storage::known_folder;
#[cfg(test)]
use kumochi_storage::user_sid;
#[cfg(test)]
pub(crate) use kumochi_storage::{DaclEntry, dacl_entries, dacl_sddl, set_dacl};
pub(crate) use kumochi_storage::{hold_chain, owner_only_dir};
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FULL_DIR_INFO, FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_READ_EA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileDispositionInfo, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL,
    SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::UI::Shell::FOLDERID_Profile;

use crate::platform::{FolderId, Untrusted};

mod models;
pub(crate) use models::{
    check_model_final, create_model_dir, create_model_write, discard_model, hold_model_creation,
    model_file, model_final, open_model_write, publish_model, remove_model_tree,
    validate_model_path,
};

/// The rights that read what's in a file or folder.
const READS: u32 = FILE_READ_DATA | FILE_READ_EA;

/// The local application data folder as the shell knows it, for path-sensitive test scratch.
/// Product code takes its folders from kumochi-storage's resolver.
#[cfg(test)]
pub(crate) fn local_app_data() -> io::Result<PathBuf> {
    known_folder(windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData)
}

/// The user's profile folder as the shell knows it, not as `USERPROFILE` says.
pub(crate) fn profile_dir() -> io::Result<PathBuf> {
    known_folder(FOLDERID_Profile)
}

/// Opens the file at `path` for reading, refusing writers and deleters while it's open, and
/// checks it like a held folder, and also against anyone untrusted appending. Its parent must be
/// held.
pub(crate) fn hold_file(path: &Path) -> Result<File, Untrusted> {
    open_checked(path, Kind::File, &Trusted::new()?)
}

/// Makes the empty marker file at `path` and flushes it, or takes the one an earlier deletion
/// left there. Nothing at `path` is truncated or followed: an existing one must be a plain file
/// with no other name, trusted like a held file. Its parent must be held.
pub(crate) fn mark(path: &Path) -> Result<(), Untrusted> {
    let options = |create: bool| {
        let mut options = File::options();
        options
            .write(create)
            .create_new(create)
            .access_mode(FILE_GENERIC_WRITE | READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options
    };
    let marker = match options(true).open(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => options(false).open(path),
        opened => opened,
    }
    .map_err(|_| Untrusted::Io)?;
    check_marker(&marker, path)?;
    marker.sync_all().map_err(|_| Untrusted::Io)
}

/// The marker at `path` open for deleting, or `None` when nothing is there. Something there
/// that isn't a marker [`mark`] would take is refused, so no deletion follows it. Its parent
/// must be held.
pub(crate) fn marker(path: &Path) -> Result<Option<File>, Untrusted> {
    let marker = match File::options()
        .access_mode(DELETE | READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened.map_err(|_| Untrusted::Io)?,
    };
    check_marker(&marker, path)?;
    Ok(Some(marker))
}

/// Deletes the marker [`marker`] opened, through its handle.
pub(crate) fn unmark(marker: File) -> Result<(), Untrusted> {
    delete_on_close(&marker)?;
    drop(marker);
    Ok(())
}

/// A marker must be a plain file with no other name, trusted like a held file.
fn check_marker(marker: &File, path: &Path) -> Result<(), Untrusted> {
    check(marker, path, Kind::File, &Trusted::new()?)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `info` is writable and the handle is open for the call.
    if unsafe { GetFileInformationByHandle(marker.as_raw_handle(), &raw mut info) } == 0 {
        return Err(Untrusted::Io);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks != 1 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

/// Checks `path` and everything in it, each entry opened without following it: it must pass the
/// held-folder or held-file checks, no one but this user and SYSTEM may read it, and no one
/// untrusted may read a folder's new entries or change their security through an inheritable
/// entry. Nothing is re-secured. Its parent must be held.
pub(crate) fn private_tree(path: &Path) -> Result<(), Untrusted> {
    private_entry(path, &Trusted::readers()?, &Trusted::new()?)
}

fn private_entry(path: &Path, readers: &Trusted, trusted: &Trusted) -> Result<(), Untrusted> {
    // Attributes and security only: no read-data bit, and a file held without sharing still opens.
    let entry = open(path, Kind::Folder, 0)?;
    #[cfg(test)]
    opened(path, &entry);
    let (kind, counted) = if attributes(&entry)? & FILE_ATTRIBUTE_DIRECTORY != 0 {
        (Kind::Folder, Aces::Inheritable)
    } else {
        (Kind::File, Aces::Effective)
    };
    check(&entry, path, kind, trusted)?;
    let security = Security::read(&entry)?;
    if security.others_may(WRITE_DAC | WRITE_OWNER, trusted, counted)?
        || security.others_may(READS, readers, counted)?
    {
        return Err(Untrusted::Unsafe);
    }
    if kind == Kind::Folder {
        let listing = listing(&entry, path, FILE_SHARE_READ | FILE_SHARE_WRITE)?;
        #[cfg(test)]
        opened(path, &listing);
        for name in names(&listing)? {
            match private_entry(&path.join(name), readers, trusted) {
                Ok(()) | Err(Untrusted::Missing) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// Deletes `path` and everything in it, a missing one included. Each entry is opened without
/// following a reparse point, must pass the held-folder or held-file checks through that handle,
/// and is deleted through it. A folder stays open, so no one can rename it, while what's in it is
/// listed through a second handle shown to be the same folder, and deleted. A reparse point or a folder others could change stops the deletion. Its
/// parent must be held.
pub(crate) fn remove_tree(path: &Path) -> Result<(), Untrusted> {
    match remove_entry(path, &Trusted::new()?) {
        Err(Untrusted::Missing) => Ok(()),
        removed => removed,
    }
}

fn remove_entry(path: &Path, trusted: &Trusted) -> Result<(), Untrusted> {
    let entry = open(path, Kind::Folder, DELETE)?;
    #[cfg(test)]
    opened(path, &entry);
    let attributes = attributes(&entry)?;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Untrusted::Reparse);
    }
    let kind = if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Folder
    } else {
        Kind::File
    };
    check(&entry, path, kind, trusted)?;
    if kind == Kind::Folder {
        #[cfg(test)]
        paused(path);
        // It shares delete with `entry` alone, which shares it with no one.
        let listing = listing(
            &entry,
            path,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )?;
        #[cfg(test)]
        opened(path, &listing);
        for name in names(&listing)? {
            match remove_entry(&path.join(name), trusted) {
                Ok(()) | Err(Untrusted::Missing) => {}
                Err(error) => return Err(error),
            }
        }
    }
    delete_on_close(&entry)
}

/// Deletes what `entry` is open on, with DELETE, once its last handle closes.
fn delete_on_close(entry: &File) -> Result<(), Untrusted> {
    let delete = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).map_err(|_| Untrusted::Io)?;
    // SAFETY: `delete` is readable for `size` bytes and the handle is open with DELETE.
    let set = unsafe {
        SetFileInformationByHandle(
            entry.as_raw_handle(),
            FileDispositionInfo,
            (&raw const delete).cast(),
            size,
        )
    };
    if set == 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

/// The folder at `path`, which `entry` holds, opened again to list it. The trailing separator
/// opens only a folder, and it must be the object `entry` holds, so nothing put at the name since
/// is listed.
fn listing(entry: &File, path: &Path, share: u32) -> Result<File, Untrusted> {
    let mut folder = path.as_os_str().to_owned();
    folder.push(r"\");
    let listing = File::options()
        .access_mode(FILE_LIST_DIRECTORY)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder)
        .map_err(|_| Untrusted::Io)?;
    if file_id(&listing)? != file_id(entry)? {
        return Err(Untrusted::Reparse);
    }
    Ok(listing)
}

/// The folder at `dir` itself, not a reparse point, by its volume and file ID. It opens with every
/// share mode, so it never stands in another opener's way.
pub(crate) fn folder_id(dir: &Path) -> Option<FolderId> {
    let folder = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .ok()?;
    let metadata = folder.metadata().ok()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return None;
    }
    held_folder_id(&folder)
}

/// The identity of what `held` is open on, by its volume and file ID, to compare with [`folder_id`].
pub(crate) fn held_folder_id(held: &File) -> Option<FolderId> {
    let (volume, file) = file_id(held).ok()?;
    Some(FolderId {
        volume,
        file: u128::from_ne_bytes(file),
    })
}

/// The volume and file ID of what `opened` is open on.
fn file_id(opened: &File) -> Result<(u64, [u8; 16]), Untrusted> {
    let mut info = FILE_ID_INFO::default();
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).map_err(|_| Untrusted::Io)?;
    // SAFETY: `info` is writable for `size` bytes and the handle is open for the call.
    let read = unsafe {
        GetFileInformationByHandleEx(
            opened.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            size,
        )
    };
    if read == 0 {
        return Err(Untrusted::Io);
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// The names in the folder open as `folder`, read through its handle.
fn names(folder: &File) -> Result<Vec<OsString>, Untrusted> {
    let mut buffer = vec![0_u64; 8 * 1024];
    let size = u32::try_from(buffer.len() * size_of::<u64>()).map_err(|_| Untrusted::Io)?;
    let mut names = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: `buffer` is writable for `size` bytes and aligned for the records, and the
        // handle is open for the call.
        let read = unsafe {
            GetFileInformationByHandleEx(
                folder.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                size,
            )
        };
        if read == 0 {
            return match io::Error::last_os_error().raw_os_error() {
                Some(code) if code == ERROR_NO_MORE_FILES.cast_signed() => Ok(names),
                _ => Err(Untrusted::Io),
            };
        }
        class = FileFullDirectoryInfo;
        let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
        let mut at = 0;
        loop {
            let field = |offset: usize| {
                bytes
                    .get(at + offset..at + offset + 4)
                    .and_then(|field| <[u8; 4]>::try_from(field).ok())
                    .and_then(|field| usize::try_from(u32::from_ne_bytes(field)).ok())
                    .ok_or(Untrusted::Io)
            };
            let next = field(0)?;
            let len = field(std::mem::offset_of!(FILE_FULL_DIR_INFO, FileNameLength))?;
            let start = at + std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
            let units = bytes
                .get(start..start + len)
                .ok_or(Untrusted::Io)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_ne_bytes(*unit))
                .collect::<Vec<u16>>();
            let name = OsString::from_wide(&units);
            if name != "." && name != ".." {
                names.push(name);
            }
            if next == 0 {
                break;
            }
            at += next;
        }
    }
}

/// A hook the deletion walk runs once, when it has checked the folder at the path.
#[cfg(test)]
type Pause = (PathBuf, Box<dyn FnOnce() + Send>);

#[cfg(test)]
static PAUSES: std::sync::Mutex<Vec<Pause>> = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, when the deletion walk has checked the folder at `path` and before it lists
/// what's in it.
#[cfg(test)]
pub(crate) fn pause_after_check(path: PathBuf, hook: Box<dyn FnOnce() + Send>) {
    PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((path, hook));
}

#[cfg(test)]
pub(crate) fn paused(path: &Path) {
    let hook = {
        let mut pauses = PAUSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = pauses.iter().position(|(paused, _)| paused == path);
        at.map(|at| pauses.swap_remove(at).1)
    };
    if let Some(hook) = hook {
        hook();
    }
}

/// Each handle the walks opened, with the access it was granted.
#[cfg(test)]
static OPENED: std::sync::Mutex<Vec<(PathBuf, u32)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn opened(path: &Path, handle: &File) {
    use windows_sys::Wdk::Foundation::{NtQueryObject, ObjectBasicInformation};
    use windows_sys::Win32::System::WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION;
    let mut info = PUBLIC_OBJECT_BASIC_INFORMATION::default();
    let size = u32::try_from(size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>()).expect("a small struct");
    // SAFETY: `info` is writable for `size` bytes and the handle is open for the call.
    let status = unsafe {
        NtQueryObject(
            handle.as_raw_handle(),
            ObjectBasicInformation,
            (&raw mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    assert!(status >= 0, "the handle's access reads");
    OPENED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((path.to_owned(), info.GrantedAccess));
}

/// The access each handle the walks opened on `path` was granted, in order.
#[cfg(test)]
fn granted(path: &Path) -> Vec<u32> {
    OPENED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(opened, _)| opened == path)
        .map(|&(_, access)| access)
        .collect()
}

/// This user's SID, for tests to expect.
#[cfg(test)]
pub(crate) fn this_user() -> String {
    user_sid().expect("this user's SID")
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::PathBuf;

    use super::{
        DELETE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, Kind, READ_CONTROL,
        Untrusted, granted,
    };
    use crate::scratch::Scratch;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;

    /// What reading an entry's attributes and security needs. Opening by name always adds
    /// SYNCHRONIZE and read-attributes.
    const LOOK: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    /// What listing a folder needs.
    const LIST: u32 = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

    /// An owner-only tree under a trusted root; other accounts may change the temp folder.
    fn scratch(name: &str) -> (Scratch, PathBuf) {
        let scratch = Scratch::under(&super::local_app_data().expect("local app data"), name);
        let root = scratch.join("owner-only");
        drop(super::owner_only_dir(&root).expect("an owner-only folder"));
        fs::write(root.join("file"), b"contents").expect("a file");
        fs::create_dir(root.join("folder")).expect("a folder");
        (scratch, root)
    }

    #[test]
    fn the_check_reads_no_file_data_and_lists_only_a_folder() {
        let (_scratch, root) = scratch("check");
        super::private_tree(&root).expect("a private tree");
        assert_eq!(granted(&root), [LOOK, LIST]);
        assert_eq!(granted(&root.join("file")), [LOOK]);
        assert_eq!(granted(&root.join("folder")), [LOOK, LIST]);
    }

    #[test]
    fn the_delete_reads_no_file_data_and_lists_only_a_folder() {
        let (_scratch, root) = scratch("delete");
        super::remove_tree(&root).expect("deleted");
        assert!(!root.exists(), "the tree is gone");
        assert_eq!(granted(&root), [LOOK | DELETE, LIST]);
        assert_eq!(granted(&root.join("file")), [LOOK | DELETE]);
        assert_eq!(granted(&root.join("folder")), [LOOK | DELETE, LIST]);
    }

    #[test]
    fn a_folder_swapped_after_its_check_is_never_listed() {
        let (_scratch, root) = scratch("swap");
        let folder = root.join("folder");
        let entry = super::open(&folder, Kind::Folder, 0).expect("the folder opens");
        fs::rename(&folder, root.join("moved")).expect("moved while open for its attributes");
        fs::create_dir(&folder).expect("another folder at its name");
        let listed = |entry| super::listing(entry, &folder, FILE_SHARE_READ).err();
        assert_eq!(listed(&entry), Some(Untrusted::Reparse));
        fs::remove_dir(&folder).expect("that folder removed");
        fs::write(&folder, b"contents").expect("a file at its name");
        assert_eq!(
            listed(&entry),
            Some(Untrusted::Io),
            "a file never opens to be listed"
        );
    }

    #[test]
    fn a_file_held_without_sharing_is_still_checked() {
        let (_scratch, root) = scratch("held");
        let _held = File::options()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(root.join("file"))
            .expect("held");
        super::private_tree(&root).expect("a private tree");
    }

    #[test]
    fn the_default_local_app_data_chain_passes_the_existing_boundary() {
        let (_scratch, root) = scratch("local-app-data-chain");
        let roaming_profile = root.join("Profile");
        let local = roaming_profile.join("AppData").join("Local");
        fs::create_dir_all(&local).expect("ordinary profile chain");
        super::models::validate_model_path(&local).expect("ordinary local profile drive");

        let moved = root.join("Moved");
        let held = super::hold_chain(&local).expect("ordinary local application data");
        assert!(
            fs::rename(&roaming_profile, &moved).is_err(),
            "a held ancestor cannot move"
        );
        assert!(
            local.is_dir(),
            "a refused move preserves the original chain"
        );
        assert!(!moved.exists());
        drop(held);
        fs::rename(&roaming_profile, &moved).expect("releasing the chain allows the move");
        assert!(moved.join("AppData").join("Local").is_dir());
    }
}
