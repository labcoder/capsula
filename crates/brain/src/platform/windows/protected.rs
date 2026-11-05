//! Holding a chain of folders so none of its names can be rebound while it's open, the trusted
//! per-user root, and the owner-only folders natives are staged in.
//!
//! The threat is code that isn't the signed-in user at medium integrity: another account, or a
//! lower-integrity process. A component is trusted when it's no reparse point, it resolves to the
//! path that named it, its owner is this user, SYSTEM, Administrators or `TrustedInstaller`, no one
//! else may change it (its name, data, attributes or security), and its integrity label isn't
//! below medium. Medium-integrity code running as this user can already change the brain, so it
//! isn't defended against here.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_NO_MORE_FILES, ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE,
    GENERIC_READ, GENERIC_WRITE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING,
    GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, INHERIT_ONLY_ACE,
    LABEL_SECURITY_INFORMATION, MapGenericMask, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_DELETE_CHILD, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_READ_EA, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    FILE_WRITE_DATA, FILE_WRITE_EA, FileAttributeTagInfo, FileDispositionInfo,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, READ_CONTROL, SYNCHRONIZE,
    SetFileInformationByHandle, VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_ACE_TYPE, SECURITY_MANDATORY_MEDIUM_RID,
    SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

use super::pipe::user_sid;

/// Why a path can't be trusted. It carries no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Untrusted {
    #[error("a path component doesn't exist")]
    Missing,
    #[error("a path component is a reparse point or resolves elsewhere")]
    Reparse,
    #[error("someone other than this user or the system can change a path component")]
    Unsafe,
    #[error("a path component can't be opened or read")]
    Io,
}

/// SYSTEM, Administrators and `TrustedInstaller`, which may change anything here; this user joins
/// them at run time.
const SYSTEM_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// The rights that change a component's name, data, attributes (setting a reparse point needs
/// only write-data or write-attributes) or security, or remove a child.
const CHANGES: u32 =
    DELETE | WRITE_DAC | WRITE_OWNER | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | FILE_DELETE_CHILD;

/// The rights that read what's in a file or folder.
const READS: u32 = FILE_READ_DATA | FILE_READ_EA;

/// The local application data folder as the shell knows it, not as the environment says.
pub(crate) fn local_app_data() -> io::Result<PathBuf> {
    known_folder(FOLDERID_LocalAppData)
}

/// The user's profile folder as the shell knows it, not as `USERPROFILE` says.
pub(crate) fn profile_dir() -> io::Result<PathBuf> {
    known_folder(FOLDERID_Profile)
}

fn known_folder(folder: windows_sys::core::GUID) -> io::Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success `path` holds a CoTaskMemAlloc'd, NUL-terminated string freed below.
    let status = unsafe {
        SHGetKnownFolderPath(
            &raw const folder,
            KF_FLAG_DEFAULT.cast_unsigned(),
            std::ptr::null_mut(),
            &raw mut path,
        )
    };
    let read = if status < 0 || path.is_null() {
        Err(io::Error::from_raw_os_error(status))
    } else {
        let mut len = 0;
        loop {
            // SAFETY: the string is NUL-terminated and `len` hasn't passed its NUL.
            let unit = unsafe { path.add(len) };
            // SAFETY: as above, `unit` points inside the string.
            if unsafe { *unit } == 0 {
                break;
            }
            len += 1;
        }
        // SAFETY: `path` holds `len` readable units before its NUL.
        let units = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(OsString::from_wide(units)))
    };
    // SAFETY: `path` is null or the buffer the shell allocated, freed once.
    unsafe { CoTaskMemFree(path.cast()) };
    read
}

/// Opens every component of the absolute folder `path`, from the volume root down, and checks
/// each through its own handle while the ones above it stay open. None is opened with delete
/// sharing, so while they're held no component can be renamed or removed; the checks make sure
/// no one untrusted may change one in place.
pub(crate) fn hold_chain(path: &Path) -> Result<Vec<File>, Untrusted> {
    let trusted = Trusted::new()?;
    let mut held = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(disk) if matches!(disk.kind(), Prefix::Disk(_)) => {
                prefix.push(disk.as_os_str());
            }
            Component::RootDir if !prefix.as_os_str().is_empty() && held.is_empty() => {
                prefix.push(r"\");
                held.push(open_checked(&prefix, Kind::Folder, &trusted)?);
            }
            Component::Normal(name) if !held.is_empty() => {
                prefix.push(name);
                held.push(open_checked(&prefix, Kind::Folder, &trusted)?);
            }
            _ => return Err(Untrusted::Unsafe),
        }
    }
    if held.is_empty() {
        return Err(Untrusted::Unsafe);
    }
    Ok(held)
}

/// Creates `path` when it's missing as an owner-only folder whose files inherit that DACL, then
/// opens and checks it. An existing folder is used only if it passes the same checks: one others
/// could change is refused, never re-secured, because a new DACL doesn't take back rights already
/// granted to open handles. Its parent must be held.
pub(crate) fn owner_only_dir(path: &Path) -> Result<File, Untrusted> {
    let user = user_sid().map_err(|_| Untrusted::Io)?;
    let descriptor = Descriptor::new(&format!("D:P(A;OICI;FA;;;{user})"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(|_| Untrusted::Io)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and `attributes` points to a live descriptor.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) } == 0
        && io::Error::last_os_error().raw_os_error() != Some(ERROR_ALREADY_EXISTS.cast_signed())
    {
        return Err(Untrusted::Io);
    }
    open_checked(path, Kind::Folder, &Trusted::new()?)
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
    // A file someone holds without sharing still opens for its attributes and security alone.
    let entry = match open(path, Kind::Folder, FILE_LIST_DIRECTORY) {
        Err(Untrusted::Io) => open(path, Kind::Folder, 0)?,
        opened => opened?,
    };
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
        for name in names(&entry)? {
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
/// listed through its handle and deleted. A reparse point or a folder others could change stops the deletion. Its
/// parent must be held.
pub(crate) fn remove_tree(path: &Path) -> Result<(), Untrusted> {
    match remove_entry(path, &Trusted::new()?) {
        Err(Untrusted::Missing) => Ok(()),
        removed => removed,
    }
}

fn remove_entry(path: &Path, trusted: &Trusted) -> Result<(), Untrusted> {
    let entry = open(path, Kind::Folder, DELETE | FILE_LIST_DIRECTORY)?;
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
        for name in names(&entry)? {
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
fn paused(path: &Path) {
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

/// Which ACEs a DACL check counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Aces {
    /// Those that apply to the object itself.
    Effective,
    /// Those too that only what's made in it inherits.
    Inheritable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
}

fn open_checked(path: &Path, kind: Kind, trusted: &Trusted) -> Result<File, Untrusted> {
    let opened = open(path, kind, 0)?;
    check(&opened, path, kind, trusted)?;
    Ok(opened)
}

fn open(path: &Path, kind: Kind, extra: u32) -> Result<File, Untrusted> {
    let (access, share) = match kind {
        Kind::Folder => (0, FILE_SHARE_READ | FILE_SHARE_WRITE),
        Kind::File => (FILE_READ_DATA | SYNCHRONIZE, FILE_SHARE_READ),
    };
    File::options()
        .access_mode(access | READ_CONTROL | FILE_READ_ATTRIBUTES | extra)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Untrusted::Missing,
            _ => Untrusted::Io,
        })
}

fn check(opened: &File, path: &Path, kind: Kind, trusted: &Trusted) -> Result<(), Untrusted> {
    check_not_reparse(opened)?;
    if !same_final_path(opened, path)? {
        return Err(Untrusted::Reparse);
    }
    trusted_security(&Security::read(opened)?, kind, trusted)
}

/// Whether `security` has a trusted owner, no label below medium, and no ACE that lets someone
/// untrusted change the object.
fn trusted_security(security: &Security, kind: Kind, trusted: &Trusted) -> Result<(), Untrusted> {
    if !trusted.contains(security.owner) || security.writable_from_below()? {
        return Err(Untrusted::Unsafe);
    }
    let changes = match kind {
        Kind::Folder => CHANGES,
        Kind::File => CHANGES | FILE_APPEND_DATA | FILE_WRITE_EA,
    };
    if security.others_may(changes, trusted, Aces::Effective)? {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn check_not_reparse(opened: &File) -> Result<(), Untrusted> {
    if attributes(opened)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Untrusted::Reparse);
    }
    Ok(())
}

fn attributes(opened: &File) -> Result<u32, Untrusted> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    let size = u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).map_err(|_| Untrusted::Io)?;
    // SAFETY: `info` is writable for `size` bytes and the handle is open for the call.
    let read = unsafe {
        GetFileInformationByHandleEx(
            opened.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size,
        )
    };
    if read == 0 {
        return Err(Untrusted::Io);
    }
    Ok(info.FileAttributes)
}

/// Whether the object behind the handle is the one `path` names.
fn same_final_path(opened: &File, path: &Path) -> Result<bool, Untrusted> {
    let mut buffer = vec![0_u16; 32_768];
    let capacity = u32::try_from(buffer.len()).map_err(|_| Untrusted::Io)?;
    // SAFETY: `buffer` is writable for `capacity` units and the handle is open for the call.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            opened.as_raw_handle(),
            buffer.as_mut_ptr(),
            capacity,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if len == 0 || len >= capacity {
        return Err(Untrusted::Io);
    }
    buffer.truncate(usize::try_from(len).map_err(|_| Untrusted::Io)?);
    Ok(final_path_matches(
        &String::from_utf16_lossy(&buffer),
        &path.to_string_lossy(),
    ))
}

/// Whether a final path as Windows reports it (`\\?\C:\…`) names the same drive path as
/// `lexical`, ignoring case and a trailing separator. A UNC or volume-GUID result never does.
pub(crate) fn final_path_matches(resolved: &str, lexical: &str) -> bool {
    let Some(resolved) = resolved.strip_prefix(r"\\?\") else {
        return false;
    };
    if resolved.as_bytes().get(1) != Some(&b':') {
        return false;
    }
    let tidy = |path: &str| {
        let path = path.to_lowercase();
        match path.trim_end_matches('\\') {
            drive if drive.len() == 2 => format!(r"{drive}\"),
            trimmed => trimmed.to_owned(),
        }
    };
    tidy(resolved) == tidy(lexical)
}

/// The DACL of the folder at `path` as SDDL, for tests to read back.
#[cfg(test)]
pub(crate) fn dacl_sddl(path: &Path) -> String {
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    let folder = open(path, Kind::Folder, 0).expect("the folder opens");
    let security = Security::read(&folder).expect("its security reads");
    let mut text: *mut u16 = std::ptr::null_mut();
    let mut len = 0_u32;
    // SAFETY: the descriptor is live; on success `text` is LocalAlloc'd and freed below.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            security.descriptor.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            &raw mut len,
        )
    };
    assert_ne!(converted, 0, "the descriptor converts");
    // SAFETY: `text` holds `len` units, the last being its NUL.
    let units = unsafe { std::slice::from_raw_parts(text, usize::try_from(len).expect("u32")) };
    let sddl = String::from_utf16_lossy(units)
        .trim_end_matches('\0')
        .to_owned();
    // SAFETY: allocated by the conversion and freed once.
    unsafe { LocalFree(text.cast()) };
    sddl
}

/// Replaces the folder at `path`'s DACL with the protected one in `sddl`, which gets this user's
/// SID, for tests to set up.
#[cfg(test)]
pub(crate) fn set_dacl(path: &Path, sddl: impl FnOnce(&str) -> String) {
    use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let user = user_sid().expect("this user's SID");
    let descriptor = Descriptor::new(&sddl(&user)).expect("the SDDL parses");
    let folder = open(path, Kind::Folder, WRITE_DAC).expect("the folder opens");
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor is live and every output is a writable local.
    let read = unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    };
    assert!(read != 0 && present != 0, "the SDDL has a DACL");
    // SAFETY: the handle has WRITE_DAC and `dacl` lives inside `descriptor` for the call.
    let set = unsafe {
        SetSecurityInfo(
            folder.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    assert_eq!(set, ERROR_SUCCESS, "the DACL is set");
}

/// This user's SID, for tests to expect.
#[cfg(test)]
pub(crate) fn this_user() -> String {
    user_sid().expect("this user's SID")
}

/// This user's SID and the system SIDs.
struct Trusted(Vec<Sid>);

impl Trusted {
    fn new() -> Result<Self, Untrusted> {
        let user = user_sid().map_err(|_| Untrusted::Io)?;
        let mut sids = vec![Sid::parse(&user)?];
        for system in SYSTEM_SIDS {
            sids.push(Sid::parse(system)?);
        }
        Ok(Self(sids))
    }

    /// This user and SYSTEM: the only ones who may read an owner-only folder.
    fn readers() -> Result<Self, Untrusted> {
        let user = user_sid().map_err(|_| Untrusted::Io)?;
        Ok(Self(vec![Sid::parse(&user)?, Sid::parse(SYSTEM_SIDS[0])?]))
    }

    fn contains(&self, sid: PSID) -> bool {
        !sid.is_null()
            && self.0.iter().any(|known| {
                // SAFETY: both are valid SIDs: one parsed here, one inside a live descriptor.
                unsafe { EqualSid(known.0, sid) != 0 }
            })
    }
}

/// A SID parsed from text, freed on drop.
struct Sid(PSID);

impl Sid {
    fn parse(text: &str) -> Result<Self, Untrusted> {
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `text` is NUL-terminated; on success `sid` is LocalAlloc'd and owned by Self.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &raw mut sid) } == 0 {
            return Err(Untrusted::Io);
        }
        Ok(Self(sid))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSidToSidW and freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// A security descriptor allocated by `LocalAlloc`, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn new(sddl: &str) -> Result<Self, Untrusted> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is LocalAlloc'd, owned here.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(Untrusted::Io);
        }
        Ok(Self(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion or by GetSecurityInfo, and freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// An object's owner, DACL and label ACL, pointing into one descriptor read through its handle.
struct Security {
    owner: PSID,
    dacl: *const ACL,
    sacl: *const ACL,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "it owns the buffer the other fields point into")
    )]
    descriptor: Descriptor,
}

impl Security {
    fn read(opened: &File) -> Result<Self, Untrusted> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the handle has READ_CONTROL; on success every pointer lands inside
        // `descriptor`, which is LocalAlloc'd and owned by the result.
        let status = unsafe {
            GetSecurityInfo(
                opened.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                &raw mut sacl,
                &raw mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(Untrusted::Io);
        }
        Ok(Self {
            owner,
            dacl,
            sacl,
            descriptor: Descriptor(descriptor),
        })
    }

    /// The owner and DACL `sddl` describes, for tests to judge without a file.
    #[cfg(test)]
    fn parse(sddl: &str) -> Self {
        use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, GetSecurityDescriptorOwner};
        let descriptor = Descriptor::new(sddl).expect("the SDDL parses");
        let (mut owner, mut defaulted) = (std::ptr::null_mut(), 0);
        // SAFETY: the descriptor is live and both outputs are writable locals.
        let read = unsafe {
            GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut defaulted)
        };
        assert_ne!(read, 0, "the owner reads");
        let (mut present, mut dacl): (_, *mut ACL) = (0, std::ptr::null_mut());
        // SAFETY: the descriptor is live and every output is a writable local.
        let read = unsafe {
            GetSecurityDescriptorDacl(
                descriptor.0,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        };
        assert!(read != 0 && present != 0, "the DACL reads");
        Self {
            owner,
            dacl,
            sacl: std::ptr::null(),
            descriptor,
        }
    }

    /// Whether an ACE `counted` covers grants someone untrusted one of `rights`. A null DACL grants
    /// everyone everything, and an allow ACE of a kind not read here counts.
    fn others_may(&self, rights: u32, trusted: &Trusted, counted: Aces) -> Result<bool, Untrusted> {
        if self.dacl.is_null() {
            return Ok(true);
        }
        for (header, ace) in aces(self.dacl)? {
            let kind = u32::from(header.AceType);
            if kind == ACCESS_DENIED_ACE_TYPE || kind == ACCESS_DENIED_CALLBACK_ACE_TYPE {
                continue;
            }
            if kind != ACCESS_ALLOWED_ACE_TYPE && kind != ACCESS_ALLOWED_CALLBACK_ACE_TYPE {
                return Ok(true);
            }
            if counted == Aces::Effective && u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            let (mask, sid) = mask_and_sid(ace);
            if mapped(mask) & rights != 0 && !trusted.contains(sid) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether lower-integrity code may write to this object: a label that applies to it is below
    /// medium or lacks no-write-up. With no such label, Windows applies medium and no-write-up.
    fn writable_from_below(&self) -> Result<bool, Untrusted> {
        if self.sacl.is_null() {
            return Ok(false);
        }
        for (header, ace) in aces(self.sacl)? {
            if u32::from(header.AceType) != SYSTEM_MANDATORY_LABEL_ACE_TYPE
                || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
            {
                continue;
            }
            let (mask, sid) = mask_and_sid(ace);
            if mask & SYSTEM_MANDATORY_LABEL_NO_WRITE_UP == 0 {
                return Ok(true);
            }
            // SAFETY: `sid` is a valid SID inside the live descriptor.
            let count = unsafe { GetSidSubAuthorityCount(sid) };
            // SAFETY: it points to the SID's subauthority count.
            let count = unsafe { *count };
            let Some(last) = u32::from(count).checked_sub(1) else {
                return Ok(true);
            };
            // SAFETY: `last` indexes an existing subauthority of the valid SID.
            let rid = unsafe { GetSidSubAuthority(sid, last) };
            // SAFETY: it points to that subauthority.
            let rid = unsafe { *rid };
            if rid < SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Every ACE of a live ACL: its header and its address.
fn aces(acl: *const ACL) -> Result<Vec<(ACE_HEADER, *const u8)>, Untrusted> {
    // SAFETY: `acl` points to a valid ACL inside a live descriptor.
    let count = unsafe { (*acl).AceCount };
    let mut list = Vec::with_capacity(usize::from(count));
    for index in 0..u32::from(count) {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the ACL's count and `ace` is writable.
        if unsafe { GetAce(acl, index, &raw mut ace) } == 0 {
            return Err(Untrusted::Io);
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        list.push((header, ace.cast_const().cast::<u8>()));
    }
    Ok(list)
}

/// The mask and SID of an ACE laid out like `ACCESS_ALLOWED_ACE`, as the allow, callback-allow and
/// mandatory-label kinds are.
fn mask_and_sid(ace: *const u8) -> (u32, PSID) {
    // SAFETY: the ACE has this layout, so its mask follows the header.
    let mask = unsafe { ace.cast::<ACCESS_ALLOWED_ACE>().read_unaligned().Mask };
    let sid = ace
        .wrapping_add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
        .cast_mut()
        .cast();
    (mask, sid)
}

/// `mask` with its generic rights mapped to file rights.
fn mapped(mut mask: u32) -> u32 {
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    if mask & (GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL) != 0 {
        // SAFETY: both pointers are to live locals.
        unsafe { MapGenericMask(&raw mut mask, &raw const mapping) };
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::{Kind, Security, Trusted, Untrusted, this_user, trusted_security};

    #[test]
    fn only_an_owner_in_the_trusted_set_is_trusted() {
        let user = this_user();
        let judged = |owner: &str| {
            let security = Security::parse(&format!("O:{owner}D:P(A;OICI;FA;;;{user})"));
            trusted_security(&security, Kind::Folder, &Trusted::new().expect("the set"))
        };
        assert_eq!(judged(&user), Ok(()));
        assert_eq!(judged("SY"), Ok(()));
        assert_eq!(judged("BA"), Ok(()));
        assert_eq!(judged("S-1-5-21-1-2-3-1002"), Err(Untrusted::Unsafe));
    }
}
