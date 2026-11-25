//! Holding a chain of folders so none of its names can be rebound while it's open, and the
//! owner-only folders created under a held parent.
//!
//! The threat is code that isn't the signed-in user at medium integrity: another account, or a
//! lower-integrity process. A component is trusted when it's no reparse point, it resolves to the
//! path that named it, its owner is this user, SYSTEM, Administrators or `TrustedInstaller`, no one
//! else may change it (its name, data, attributes or security), and its integrity label isn't
//! below medium. Medium-integrity code running as this user can already change the product, so it
//! isn't defended against here.
#![expect(
    clippy::missing_errors_doc,
    reason = "every error is an Untrusted, which names the check that refused"
)]

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE,
    HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_OBJECT_TYPE,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING,
    GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, INHERIT_ONLY_ACE,
    LABEL_SECURITY_INFORMATION, MapGenericMask, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, FileAttributeTagInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    READ_CONTROL, SYNCHRONIZE, VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_ACE_TYPE, SECURITY_MANDATORY_MEDIUM_RID,
    SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};

use crate::platform::Untrusted;

mod anchor;
pub use anchor::{Child, hold_product_folder, profile_root};
mod guard;
pub use guard::{GuardError, SetupGuard, acquire_setup_guard};

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

/// The folder Windows reports for the Known Folder `folder`, never an environment variable.
pub fn known_folder(folder: windows_sys::core::GUID) -> io::Result<PathBuf> {
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
/// each through its own handle while the ones above it stay open. The handles have no data access,
/// so they take no part in share checks and don't stop a rename or removal. The checks keep the
/// chain in place: each refuses a component that someone untrusted may rename, remove or change,
/// or whose children they may delete. This user's own processes are outside that threat model.
pub fn hold_chain(path: &Path) -> Result<Vec<File>, Untrusted> {
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
pub fn owner_only_dir(path: &Path) -> Result<File, Untrusted> {
    owner_only(path, Existing::Use)
}

/// Creates `path` as a new owner-only folder like [`owner_only_dir`], then opens and checks it.
/// Anything already at `path` is refused, so a run never reuses a folder it didn't make. Its
/// parent must be held.
pub fn new_owner_only_dir(path: &Path) -> Result<File, Untrusted> {
    owner_only(path, Existing::Refuse)
}

/// What [`owner_only`] does with a folder already at its path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Existing {
    Use,
    Refuse,
}

fn owner_only(path: &Path, existing: Existing) -> Result<File, Untrusted> {
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
        && (existing == Existing::Refuse
            || io::Error::last_os_error().raw_os_error()
                != Some(ERROR_ALREADY_EXISTS.cast_signed()))
    {
        return Err(Untrusted::Io);
    }
    open_checked(path, Kind::Folder, &Trusted::new()?)
}

/// The SID of the user this process runs as, in string form (`S-1-5-21-…`).
///
/// # Panics
/// Never: Windows' `u32` lengths fit in `usize`.
pub fn user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: plain call returning a pseudo-handle that needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `token` receives a handle owned below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a fresh handle owned only here.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0u32;
    // SAFETY: the sizing call writes only `len`; its expected failure is ignored.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut len,
        )
    };
    // u64s, so the TOKEN_USER at the start is aligned.
    let mut buffer = vec![0u64; usize::try_from(len).expect("u32 fits in usize").div_ceil(8)];
    // SAFETY: `buffer` holds at least `len` writable bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of the aligned buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` points into `buffer`, alive here; on success `text` is LocalAlloc'd.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SID strings are under 200 characters; the bound only stops a runaway scan.
    let len = (0..256)
        .take_while(|&i| {
            // SAFETY: `text` is NUL-terminated and `i` never passes the NUL.
            let unit = unsafe { text.add(i) };
            // SAFETY: as above.
            unsafe { *unit != 0 }
        })
        .count();
    // SAFETY: the `len` units before the NUL are initialized.
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: `text` came from ConvertSidToStringSidW and is no longer referenced.
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// Which ACEs a DACL check counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aces {
    /// Those that apply to the object itself.
    Effective,
    /// Those too that only what's made in it inherits.
    Inheritable,
}

/// What a checked path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A folder, held with no data access.
    Folder,
    /// A file, held for reading.
    File,
}

/// Checks the file a writer is about to open at `path`, without following it: nothing there is
/// fine, and something there must be a plain file with no other name and no reparse point. Its
/// parent must be held.
pub fn check_leaf(path: &Path) -> Result<(), Untrusted> {
    let leaf = match File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        opened => opened.map_err(|_| Untrusted::Io)?,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `info` is writable and the handle is open for the call.
    if unsafe { GetFileInformationByHandle(leaf.as_raw_handle(), &raw mut info) } == 0 {
        return Err(Untrusted::Io);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Untrusted::Reparse);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks != 1 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

/// Opens `path` without following a reparse point and checks it through that handle.
pub fn open_checked(path: &Path, kind: Kind, trusted: &Trusted) -> Result<File, Untrusted> {
    let opened = open(path, kind, 0)?;
    check(&opened, path, kind, trusted)?;
    Ok(opened)
}

/// Opens `path` without following a reparse point, with `extra` rights beside those a check needs.
pub fn open(path: &Path, kind: Kind, extra: u32) -> Result<File, Untrusted> {
    open_native(path, kind, extra).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Untrusted::Missing,
        _ => Untrusted::Io,
    })
}

/// Opens `path` like `open`, keeping the native error.
fn open_native(path: &Path, kind: Kind, extra: u32) -> io::Result<File> {
    let (access, share) = match kind {
        Kind::Folder => (0, FILE_SHARE_READ | FILE_SHARE_WRITE),
        Kind::File => (FILE_READ_DATA | SYNCHRONIZE, FILE_SHARE_READ),
    };
    File::options()
        .access_mode(access | READ_CONTROL | FILE_READ_ATTRIBUTES | extra)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Checks what `opened` holds: no reparse point, the object `path` names, and trusted security.
pub fn check(opened: &File, path: &Path, kind: Kind, trusted: &Trusted) -> Result<(), Untrusted> {
    check_not_reparse(opened)?;
    if !same_final_path(opened, path)? {
        return Err(Untrusted::Reparse);
    }
    trusted_security(&Security::read(opened)?, kind, trusted)
}

/// Whether `security` has a trusted owner, no label below medium, and no ACE that lets someone
/// untrusted change the object.
fn trusted_security(security: &Security, kind: Kind, trusted: &Trusted) -> Result<(), Untrusted> {
    if !trusted.contains(security.owner)
        || security.open_from_below(SYSTEM_MANDATORY_LABEL_NO_WRITE_UP)?
    {
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

/// The attributes of what `opened` is open on.
pub fn attributes(opened: &File) -> Result<u32, Untrusted> {
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
pub fn same_final_path(opened: &File, path: &Path) -> Result<bool, Untrusted> {
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
pub fn final_path_matches(resolved: &str, lexical: &str) -> bool {
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
///
/// # Panics
/// When the folder or its security can't be read.
#[cfg(any(test, feature = "test-support"))]
pub fn dacl_sddl(path: &Path) -> String {
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

/// One entry of a DACL, read without SDDL's account aliases.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, PartialEq, Eq)]
pub struct DaclEntry {
    /// An access-allowed entry; any other kind reads as false.
    pub allow: bool,
    /// Its flags, less `INHERITED_ACE`.
    pub flags: u32,
    /// Whether `INHERITED_ACE` is set: where it came from, not what it grants.
    pub inherited: bool,
    /// Its access mask.
    pub rights: u32,
    /// Its SID as `S-1-...`.
    pub sid: String,
}

/// The entries of the DACL on the folder or file at `path`, in order.
///
/// # Panics
/// When the entry or its security can't be read.
#[cfg(any(test, feature = "test-support"))]
pub fn dacl_entries(path: &Path) -> Vec<DaclEntry> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::INHERITED_ACE;
    let entry = open(path, Kind::Folder, 0).expect("the entry opens");
    let security = Security::read(&entry).expect("its security reads");
    assert!(!security.dacl.is_null(), "a DACL, not a null one");
    let aces = aces(security.dacl).expect("its entries read");
    aces.into_iter()
        .map(|(header, ace)| {
            let (rights, sid) = mask_and_sid(ace);
            let mut text: *mut u16 = std::ptr::null_mut();
            // SAFETY: `sid` lies inside the live descriptor and `text` is a writable local.
            let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut text) };
            assert_ne!(converted, 0, "the SID converts");
            let mut len = 0;
            loop {
                // SAFETY: the string is NUL-terminated and `len` hasn't passed its NUL.
                let unit = unsafe { text.add(len) };
                // SAFETY: as above, `unit` points inside the string.
                if unsafe { *unit } == 0 {
                    break;
                }
                len += 1;
            }
            // SAFETY: `text` holds `len` units before its NUL.
            let units = unsafe { std::slice::from_raw_parts(text, len) };
            let sid = String::from_utf16_lossy(units);
            // SAFETY: allocated by the conversion and freed once.
            unsafe { LocalFree(text.cast()) };
            let flags = u32::from(header.AceFlags);
            DaclEntry {
                allow: u32::from(header.AceType) == ACCESS_ALLOWED_ACE_TYPE,
                flags: flags & !INHERITED_ACE,
                inherited: flags & INHERITED_ACE != 0,
                rights,
                sid,
            }
        })
        .collect()
}

/// Makes the empty folder `dir` a junction to `target`, for tests to plant a link.
///
/// # Errors
/// When the folder can't be opened or the reparse point can't be set.
#[cfg(any(test, feature = "test-support"))]
pub fn set_junction(dir: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

    let folder = File::options()
        .access_mode(FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let substitute: Vec<u16> = format!(r"\??\{}", target.display())
        .encode_utf16()
        .collect();
    let name = u16::try_from(substitute.len() * 2).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for field in [8 + name + 4, 0, 0, name, name + 2, 0] {
        buffer.extend(field.to_le_bytes());
    }
    for unit in substitute {
        buffer.extend(unit.to_le_bytes());
    }
    buffer.extend([0; 4]);
    let size = u32::try_from(buffer.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut returned = 0;
    // SAFETY: `buffer` is a complete mount-point REPARSE_DATA_BUFFER of `size` bytes, the handle
    // is open, and no output buffer or overlapped I/O is used.
    let set = unsafe {
        DeviceIoControl(
            folder.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            size,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Replaces the folder at `path`'s DACL with the protected one in `sddl`, which gets this user's
/// SID, for tests to set up.
///
/// # Panics
/// When the SDDL doesn't parse or the DACL can't be set.
#[cfg(any(test, feature = "test-support"))]
pub fn set_dacl(path: &Path, sddl: impl FnOnce(&str) -> String) {
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

/// This user's SID and the system SIDs.
#[derive(Debug)]
pub struct Trusted(Vec<Sid>);

impl Trusted {
    /// This user, SYSTEM, Administrators and `TrustedInstaller`.
    pub fn new() -> Result<Self, Untrusted> {
        let user = user_sid().map_err(|_| Untrusted::Io)?;
        let mut sids = vec![Sid::parse(&user)?];
        for system in SYSTEM_SIDS {
            sids.push(Sid::parse(system)?);
        }
        Ok(Self(sids))
    }

    /// This user and SYSTEM: the only ones who may read an owner-only folder.
    pub fn readers() -> Result<Self, Untrusted> {
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
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
pub struct Security {
    owner: PSID,
    dacl: *const ACL,
    sacl: *const ACL,
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        expect(dead_code, reason = "it owns the buffer the other fields point into")
    )]
    descriptor: Descriptor,
}

impl Security {
    /// The owner, DACL and label of what `opened` is open on, read through the handle.
    pub fn read(opened: &File) -> Result<Self, Untrusted> {
        Self::read_object(opened.as_raw_handle(), SE_FILE_OBJECT)
    }

    /// The security of the `kind` of object open at `handle`.
    fn read_object(handle: HANDLE, kind: SE_OBJECT_TYPE) -> Result<Self, Untrusted> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the handle has READ_CONTROL; on success every pointer lands inside
        // `descriptor`, which is LocalAlloc'd and owned by the result.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                kind,
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
        let read =
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut defaulted) };
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
    pub fn others_may(
        &self,
        rights: u32,
        trusted: &Trusted,
        counted: Aces,
    ) -> Result<bool, Untrusted> {
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

    /// Whether lower-integrity code gets past this object's label: a label that applies to it is
    /// below medium or lacks one of `fence`'s policies. With no such label, Windows applies medium
    /// and no-write-up only.
    fn open_from_below(&self, fence: u32) -> Result<bool, Untrusted> {
        let unlabelled = fence & !SYSTEM_MANDATORY_LABEL_NO_WRITE_UP != 0;
        if self.sacl.is_null() {
            return Ok(unlabelled);
        }
        let mut labelled = false;
        for (header, ace) in aces(self.sacl)? {
            if u32::from(header.AceType) != SYSTEM_MANDATORY_LABEL_ACE_TYPE
                || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
            {
                continue;
            }
            labelled = true;
            let (mask, sid) = mask_and_sid(ace);
            if mask & fence != fence {
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
        Ok(!labelled && unlabelled)
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
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

    use super::{
        Kind, Security, Trusted, Untrusted, hold_chain, known_folder, new_owner_only_dir,
        owner_only_dir, trusted_security, user_sid,
    };
    use crate::scratch::Scratch;

    #[test]
    fn an_exclusive_create_refuses_a_folder_already_there() {
        let local = known_folder(FOLDERID_LocalAppData).expect("local app data");
        let scratch = Scratch::under(&local, "exclusive-create");
        let _held = hold_chain(&scratch).expect("the scratch chain");
        let run = scratch.join("run");
        drop(new_owner_only_dir(&run).expect("a new folder"));
        assert_eq!(new_owner_only_dir(&run).err(), Some(Untrusted::Io));
        drop(owner_only_dir(&run).expect("the open-or-create variant takes it"));
        assert_eq!(
            std::fs::read_dir(&run).expect("the folder lists").count(),
            0,
            "nothing was made inside it"
        );
    }

    #[test]
    fn only_an_owner_in_the_trusted_set_is_trusted() {
        let user = user_sid().expect("this user's SID");
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
