use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::ptr::NonNull;

use crate::platform::Untrusted;
use crate::platform::unix::{
    components, open_directory, safe_directory, safe_regular, same_inspection, stat,
};

// SDK sys/fcntl.h's filesec_property_t; libc does not declare the filesec APIs.
const FILESEC_OWNER: libc::c_int = 1;
const FILESEC_MODE: libc::c_int = 4;
const FILESEC_ACL: libc::c_int = 5;

const PRESENT_ACL_ATTRIBUTES: libc::attrgroup_t = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_OWNERID
    | libc::ATTR_CMN_GRPID
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_EXTENDED_SECURITY
    | libc::ATTR_CMN_FILEID;

#[repr(C, align(4))]
pub(super) struct AttributeBuffer(pub(super) [u8; 4096]);

unsafe extern "C" {
    fn filesec_init() -> *mut libc::c_void;
    fn filesec_free(filesec: *mut libc::c_void);
    fn filesec_get_property(
        filesec: *mut libc::c_void,
        property: libc::c_int,
        output: *mut libc::c_void,
    ) -> libc::c_int;
    fn filesec_query_property(
        filesec: *mut libc::c_void,
        property: libc::c_int,
        present: *mut libc::c_int,
    ) -> libc::c_int;
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(
        fd: libc::c_int,
        metadata: *mut libc::stat,
        filesec: *mut libc::c_void,
    ) -> libc::c_int;
}

/// Opens and inspects an absolute directory chain, retaining each object from root to leaf.
///
/// Fds do not freeze names. This admission alone cannot authorize native loading or launch,
/// and does not establish real application anchors or Windows integrity-label equivalence.
/// Inspection samples held objects; it does not establish an atomic ACL/BSD transaction or
/// detect every intervening change. Arbitrary or ownership-disabled volumes are not certified.
///
/// # Errors
/// Refuses non-absolute, dot, parent or NUL components, links, device crossings, unsafe
/// ownership/modes, ACL grants or unsupported ACL data, and every incomplete or failed
/// inspection. Only completed absence or a fully supported returned empty/DENY-only ACL passes.
pub fn hold_chain(path: &Path) -> Result<Vec<File>, Untrusted> {
    if !path.is_absolute() {
        return Err(Untrusted::Unsafe);
    }
    let names = components(path)?;
    // SAFETY: getuid has no arguments or ownership effects.
    let user = unsafe { libc::getuid() };
    let root = open_directory(libc::AT_FDCWD, c"/")?;
    hold_from(root, &names, user)
}

pub(super) fn hold_from(
    root: File,
    names: &[CString],
    user: libc::uid_t,
) -> Result<Vec<File>, Untrusted> {
    let device = inspect(&root, user)?.st_dev;
    let mut held = vec![root];
    for name in names {
        let parent = held.last().ok_or(Untrusted::Io)?;
        let child = open_directory(parent.as_raw_fd(), name)?;
        let metadata = inspect(&child, user)?;
        // shortcut: refuse device crossings; real APFS/firmlink anchor admission needs its own gate.
        if metadata.st_dev != device {
            return Err(Untrusted::Unsafe);
        }
        held.push(child);
    }
    Ok(held)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObjectKind {
    Directory,
    Regular,
}

impl ObjectKind {
    fn attribute_type(self) -> u32 {
        match self {
            Self::Directory => 2,
            Self::Regular => 1,
        }
    }

    fn from_mode(mode: libc::mode_t) -> Result<Self, Untrusted> {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => Ok(Self::Directory),
            libc::S_IFREG => Ok(Self::Regular),
            _ => Err(Untrusted::Io),
        }
    }
}

/// Inspects a held object that must be private: a folder owned by this user at mode exactly 0700,
/// or a file that passes [`safe_regular`], and no ACL at all, so a DENY-only or empty present ACL
/// refuses too. Only metadata is read.
pub(super) fn inspect_private(
    file: &impl AsRawFd,
    user: libc::uid_t,
    kind: ObjectKind,
) -> Result<libc::stat, Untrusted> {
    let before = stat(file.as_raw_fd())?;
    match kind {
        ObjectKind::Directory => {
            if before.st_mode & libc::S_IFMT != libc::S_IFDIR
                || before.st_uid != user
                || before.st_mode & 0o7777 != 0o700
            {
                return Err(Untrusted::Unsafe);
            }
        }
        ObjectKind::Regular => safe_regular(&before, user)?,
    }
    Filesec::new()?.inspect_with(file.as_raw_fd(), &before, Acl::Absent)?;
    let after = stat(file.as_raw_fd())?;
    if !same_inspection(&before, &after)
        || (kind == ObjectKind::Regular && before.st_nlink != after.st_nlink)
    {
        return Err(Untrusted::Io);
    }
    Ok(after)
}

/// Checks only this held folder, not its name or ancestors, against the existing ownership,
/// mode and ACL policy. The caller opens it without following the final component.
///
/// # Errors
/// Refuses unsafe or incomplete descriptor inspection.
pub fn check_product_folder(folder: &File) -> Result<(), Untrusted> {
    // SAFETY: geteuid only returns the effective identity.
    let user = unsafe { libc::geteuid() };
    inspect(folder, user).map(drop)
}

pub(super) fn inspect(file: &File, user: libc::uid_t) -> Result<libc::stat, Untrusted> {
    inspect_object(file, user, ObjectKind::Directory)
}

pub(super) fn inspect_object(
    file: &File,
    user: libc::uid_t,
    kind: ObjectKind,
) -> Result<libc::stat, Untrusted> {
    let before = stat(file.as_raw_fd())?;
    match kind {
        ObjectKind::Directory => safe_directory(&before, user)?,
        ObjectKind::Regular => safe_regular(&before, user)?,
    }
    let security = Filesec::new()?;
    security.inspect(file.as_raw_fd(), &before)?;
    let after = stat(file.as_raw_fd())?;
    if !same_inspection(&before, &after)
        || (kind == ObjectKind::Regular && before.st_nlink != after.st_nlink)
    {
        return Err(Untrusted::Io);
    }
    Ok(after)
}

/// What a present ACL may hold and still pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Acl {
    /// Only supported DENY entries.
    DenyOnly,
    /// Nothing: any present ACL refuses.
    Absent,
}

struct Filesec(NonNull<libc::c_void>);

impl Filesec {
    fn new() -> Result<Self, Untrusted> {
        // SAFETY: filesec_init has no arguments and returns a newly owned filesec or null.
        NonNull::new(unsafe { filesec_init() })
            .map(Self)
            .ok_or(Untrusted::Io)
    }

    fn inspect(&self, fd: RawFd, expected: &libc::stat) -> Result<(), Untrusted> {
        self.inspect_with(fd, expected, Acl::DenyOnly)
    }

    fn inspect_with(&self, fd: RawFd, expected: &libc::stat, acl: Acl) -> Result<(), Untrusted> {
        // SAFETY: all-zero libc stat is valid initialized storage, not an inspection result.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is borrowed; metadata is writable libc stat and self owns a fresh filesec.
        if unsafe { fstatx_np(fd, &raw mut metadata, self.0.as_ptr()) } != 0 {
            return Err(Untrusted::Io);
        }
        if !same_inspection(expected, &metadata)
            || (expected.st_mode & libc::S_IFMT == libc::S_IFREG
                && expected.st_nlink != metadata.st_nlink)
        {
            return Err(Untrusted::Io);
        }
        let mut owner = libc::uid_t::MAX;
        // SAFETY: FILESEC_OWNER writes a uid_t; the filesec and output are live.
        if unsafe { filesec_get_property(self.0.as_ptr(), FILESEC_OWNER, (&raw mut owner).cast()) }
            != 0
            || owner != expected.st_uid
        {
            return Err(Untrusted::Io);
        }
        let mut mode = libc::mode_t::MAX;
        // SAFETY: FILESEC_MODE writes a mode_t; the filesec and output are live.
        if unsafe { filesec_get_property(self.0.as_ptr(), FILESEC_MODE, (&raw mut mode).cast()) }
            != 0
            || mode != expected.st_mode
        {
            return Err(Untrusted::Io);
        }
        let mut present = -1;
        // SAFETY: FILESEC_ACL queries presence into a live int without allocating an ACL.
        let result =
            unsafe { filesec_query_property(self.0.as_ptr(), FILESEC_ACL, &raw mut present) };
        match acl_absent(result, present) {
            Ok(()) if acl == Acl::Absent => inspect_absent_acl(fd, expected),
            Ok(()) => Ok(()),
            Err(Untrusted::Unsafe) if acl == Acl::DenyOnly => inspect_present_acl(fd, expected),
            Err(error) => Err(error),
        }
    }
}

/// Filesec reports a present ACL with no entries as absent, so absence also needs the attribute
/// query to leave the ACL out: filesec rules out entries, the query rules out an empty ACL.
fn inspect_absent_acl(fd: RawFd, expected: &libc::stat) -> Result<(), Untrusted> {
    let (result, buffer) = query_present_acl(fd);
    classify_absent_acl(result, &buffer.0, expected)
}

pub(super) fn classify_absent_acl(
    result: libc::c_int,
    bytes: &[u8],
    expected: &libc::stat,
) -> Result<(), Untrusted> {
    if result != 0 {
        return Err(Untrusted::Io);
    }
    let length = usize::try_from(u32::from_ne_bytes(attribute_bytes(bytes, 0)?))
        .map_err(|_| Untrusted::Io)?;
    if !(52..=4096).contains(&length) {
        return Err(Untrusted::Io);
    }
    let returned = u32::from_ne_bytes(attribute_bytes(bytes, 4)?);
    if returned & libc::ATTR_CMN_EXTENDED_SECURITY != 0 {
        return Err(Untrusted::Unsafe);
    }
    if returned != PRESENT_ACL_ATTRIBUTES & !libc::ATTR_CMN_EXTENDED_SECURITY
        || [8, 12, 16, 20]
            .into_iter()
            .any(|offset| attribute_bytes(bytes, offset) != Ok([0; 4]))
        || i32::from_ne_bytes(attribute_bytes(bytes, 24)?) != expected.st_dev
        || u32::from_ne_bytes(attribute_bytes(bytes, 28)?)
            != ObjectKind::from_mode(expected.st_mode)?.attribute_type()
        || u32::from_ne_bytes(attribute_bytes(bytes, 32)?) != expected.st_uid
        || u32::from_ne_bytes(attribute_bytes(bytes, 36)?) != expected.st_gid
        || u32::from_ne_bytes(attribute_bytes(bytes, 40)?) & 0o7777
            != u32::from(expected.st_mode & 0o7777)
        || u64::from_ne_bytes(attribute_bytes(bytes, 44)?) != expected.st_ino
    {
        return Err(Untrusted::Io);
    }
    Ok(())
}

fn inspect_present_acl(fd: RawFd, expected: &libc::stat) -> Result<(), Untrusted> {
    let (result, buffer) = query_present_acl(fd);
    classify_present_acl(result, &buffer.0, expected)
}

pub(super) fn query_present_acl(fd: RawFd) -> (libc::c_int, AttributeBuffer) {
    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: PRESENT_ACL_ATTRIBUTES,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = AttributeBuffer([0; 4096]);
    // SAFETY: fd is borrowed; the request and aligned bounded output buffer remain live.
    let result = unsafe {
        libc::fgetattrlist(
            fd,
            (&raw mut attributes).cast(),
            buffer.0.as_mut_ptr().cast(),
            buffer.0.len(),
            libc::FSOPT_REPORT_FULLSIZE,
        )
    };
    (result, buffer)
}

fn attribute_bytes<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], Untrusted> {
    let end = offset.checked_add(N).ok_or(Untrusted::Io)?;
    bytes
        .get(offset..end)
        .and_then(|value| value.try_into().ok())
        .ok_or(Untrusted::Io)
}

fn classify_present_acl(
    result: libc::c_int,
    bytes: &[u8],
    expected: &libc::stat,
) -> Result<(), Untrusted> {
    if result != 0 {
        return Err(Untrusted::Io);
    }
    let length = usize::try_from(u32::from_ne_bytes(attribute_bytes(bytes, 0)?))
        .map_err(|_| Untrusted::Io)?;
    if !(60..=4096).contains(&length) {
        return Err(Untrusted::Io);
    }
    let bytes = bytes.get(..length).ok_or(Untrusted::Io)?;
    if u32::from_ne_bytes(attribute_bytes(bytes, 4)?) != PRESENT_ACL_ATTRIBUTES
        || [8, 12, 16, 20]
            .into_iter()
            .any(|offset| attribute_bytes(bytes, offset) != Ok([0; 4]))
    {
        return Err(Untrusted::Io);
    }
    if i32::from_ne_bytes(attribute_bytes(bytes, 24)?) != expected.st_dev
        || u32::from_ne_bytes(attribute_bytes(bytes, 28)?)
            != ObjectKind::from_mode(expected.st_mode)?.attribute_type()
        || u32::from_ne_bytes(attribute_bytes(bytes, 32)?) != expected.st_uid
        || u32::from_ne_bytes(attribute_bytes(bytes, 36)?) != expected.st_gid
        || u32::from_ne_bytes(attribute_bytes(bytes, 40)?) & 0o7777
            != u32::from(expected.st_mode & 0o7777)
        || u64::from_ne_bytes(attribute_bytes(bytes, 52)?) != expected.st_ino
    {
        return Err(Untrusted::Io);
    }
    let offset = i32::from_ne_bytes(attribute_bytes(bytes, 44)?);
    let start = 44_i64
        .checked_add(i64::from(offset))
        .and_then(|start| usize::try_from(start).ok())
        .ok_or(Untrusted::Io)?;
    let payload_length = usize::try_from(u32::from_ne_bytes(attribute_bytes(bytes, 48)?))
        .map_err(|_| Untrusted::Io)?;
    let end = start.checked_add(payload_length).ok_or(Untrusted::Io)?;
    if start < 60 || !start.is_multiple_of(4) || end != length {
        return Err(Untrusted::Io);
    }
    let payload = bytes.get(start..end).ok_or(Untrusted::Io)?;
    let count = u32::from_ne_bytes(attribute_bytes(payload, 36)?);
    if count > 128 {
        return Err(Untrusted::Io);
    }
    let count = usize::try_from(count).map_err(|_| Untrusted::Io)?;
    let required = count
        .checked_mul(24)
        .and_then(|entries| entries.checked_add(44))
        .ok_or(Untrusted::Io)?;
    if payload.len() != required {
        return Err(Untrusted::Io);
    }
    if u32::from_ne_bytes(attribute_bytes(payload, 40)?) & !(1 << 17) != 0 {
        return Err(Untrusted::Unsafe);
    }
    for index in 0..count {
        let entry = 44 + index * 24;
        let flags = u32::from_ne_bytes(attribute_bytes(payload, entry + 16)?);
        let rights = u32::from_ne_bytes(attribute_bytes(payload, entry + 20)?);
        if flags & 0xf != 2 || flags & !0x1ff != 0 || rights & !0x0010_3ffe != 0 {
            return Err(Untrusted::Unsafe);
        }
    }
    Ok(())
}

fn acl_absent(result: libc::c_int, present: libc::c_int) -> Result<(), Untrusted> {
    if result != 0 || present < 0 {
        return Err(Untrusted::Io);
    }
    if present != 0 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

impl Drop for Filesec {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner; filesec_free is void and is called exactly once.
        unsafe { filesec_free(self.0.as_ptr()) };
    }
}

#[cfg(test)]
pub(super) mod tests;
