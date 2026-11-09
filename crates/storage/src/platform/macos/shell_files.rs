use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{File, TryLockError};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, RawFd};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::protected::{self, ObjectKind};
use crate::Untrusted;

const DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const SCAN_LIMIT: usize = 256;

/// The acquisition stage that failed, without paths or foreign error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AcquisitionError {
    /// Ordinary creation/open failure, not a completed security inspection.
    #[error("private object creation or open failed")]
    Open(io::ErrorKind),
    /// Privacy, type, volume, ACL or identity was unsafe or could not be established.
    #[error("private object admission failed: {0}")]
    Admission(#[from] Untrusted),
}

/// Outcomes from at most 256 consumed directory entries, including unrelated entries and dots.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// All entries consumed, not just matching logs.
    pub scanned: usize,
    /// Noncandidates and unsafe, replaced, missing or unsuccessful deletion candidates.
    pub skipped: usize,
    /// Successfully removed directory entries; not a secure-erasure claim.
    pub removed: usize,
    /// The pass hit its entry budget or an enumeration error, so EOF was not established.
    pub incomplete: bool,
}

impl Retention {
    fn can_read(&mut self) -> bool {
        if self.scanned == SCAN_LIMIT {
            self.incomplete = true;
            return false;
        }
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Volume {
    device: libc::dev_t,
    fsid: [i32; 2],
}

struct Directory {
    file: File,
    name: CString,
    identity: libc::stat,
    volume: Volume,
}

struct Directories {
    held: Vec<Directory>,
    private_start: usize,
    user: libc::uid_t,
}

/// Held anchor/product/log directories. The candidate path must come from OS folder discovery.
///
/// Only the read-only anchor walk permits independently inspected local APFS device transitions.
/// Descendants stay on the Support volume. This is not paired-boot-volume proof or native admission.
pub struct ShellStorage(Arc<Directories>);

/// An opaque private lock descriptor retaining its admitted ancestors until it closes.
#[must_use = "dropping the owner releases its descriptor and any instance lock"]
pub struct AdmittedLock {
    file: File,
    _directories: Arc<Directories>,
}

/// An opaque append-only log descriptor retaining its admitted ancestors until it closes.
///
/// Shared-reference writes work through `Arc` without exposing a `File` or native descriptor.
/// They are synchronous; admission samples do not continuously monitor later security changes.
#[must_use = "retain the admitted log for as long as writes are needed"]
pub struct AdmittedLog {
    file: File,
    directories: Arc<Directories>,
    identity: libc::stat,
    prefix: String,
}

impl fmt::Debug for ShellStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellStorage").finish_non_exhaustive()
    }
}

impl fmt::Debug for AdmittedLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedLock").finish_non_exhaustive()
    }
}

impl fmt::Debug for AdmittedLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedLog").finish_non_exhaustive()
    }
}

impl AdmittedLock {
    /// Takes the standard nonblocking exclusive lock; closing this owner releases it.
    ///
    /// # Errors
    /// Returns `WouldBlock` for another holder, or the standard lock error.
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        self.file.try_lock()
    }
}

impl Write for &AdmittedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&self.file).write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.file).flush()
    }
}

/// Admits an existing absolute Support anchor, creating only the product and `logs` directories.
///
/// Existing unsafe objects are refused without repair. The owning UID and OS are trusted;
/// descriptors retain objects, not names, and bracketed inspection is not an atomic transaction.
///
/// # Errors
/// Refuses invalid names, privileged identity, unsafe or incomplete inspection, unsupported volumes,
/// crossings below Support, and creation/open failure. No cleanup or retry is performed.
pub fn open_shell_storage(support: &Path, product: &str) -> Result<ShellStorage, AcquisitionError> {
    let product = basename(product)?;
    if !support.is_absolute() {
        return Err(Untrusted::Unsafe.into());
    }
    let names = protected::components(support)?;
    // SAFETY: identity queries have no arguments or ownership effects.
    let user = unsafe { libc::getuid() };
    // SAFETY: geteuid only returns the effective identity.
    let effective = unsafe { libc::geteuid() };
    // SAFETY: getgid only returns the real group identity.
    let group = unsafe { libc::getgid() };
    // SAFETY: getegid only returns the effective group identity.
    let effective_group = unsafe { libc::getegid() };
    if user == 0 || user != effective || group != effective_group {
        return Err(Untrusted::Unsafe.into());
    }
    let root = open_at(libc::AT_FDCWD, c"/", DIRECTORY_FLAGS, 0)?;
    let mut held = vec![directory(root, c"/".to_owned(), user)?];
    for name in names {
        let parent = &held.last().ok_or(Untrusted::Io)?.file;
        let child = directory(
            open_at(parent.as_raw_fd(), &name, DIRECTORY_FLAGS, 0)?,
            name,
            user,
        )?;
        named(parent, &child.name, &child.identity)?;
        held.push(child);
    }
    let support = held.last().ok_or(Untrusted::Io)?;
    if support.identity.st_uid != user {
        return Err(Untrusted::Unsafe.into());
    }
    volume(&support.file, true)?;
    let private_start = held.len();
    let mut directories = Directories {
        held,
        private_start,
        user,
    };
    directories.check()?;
    for name in [product, c"logs".to_owned()] {
        directories.check()?;
        let parent = &directories.held.last().ok_or(Untrusted::Io)?.file;
        // SAFETY: parent is retained; name is a validated basename; no existing object is repaired.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(AcquisitionError::Open(error.kind()));
            }
        }
        let child = directory(
            open_at(parent.as_raw_fd(), &name, DIRECTORY_FLAGS, 0)?,
            name,
            user,
        )?;
        private_directory(&child.identity, user)?;
        if child.volume != directories.held[private_start - 1].volume {
            return Err(Untrusted::Unsafe.into());
        }
        named(parent, &child.name, &child.identity)?;
        directories.held.push(child);
    }
    directories.check()?;
    Ok(ShellStorage(Arc::new(directories)))
}

impl ShellStorage {
    /// Opens/creates one private lock basename in the product directory, never truncating.
    ///
    /// # Errors
    /// Returns a stage-tagged creation/open or privacy/inspection refusal.
    pub fn open_instance_lock(&self, name: &str) -> Result<AdmittedLock, AcquisitionError> {
        let name = basename(name)?;
        if !name.to_bytes().ends_with(b".lock") {
            return Err(Untrusted::Unsafe.into());
        }
        let file = self.open_private(self.0.private_start, &name, libc::O_RDWR, true)?;
        Ok(AdmittedLock {
            file,
            _directories: Arc::clone(&self.0),
        })
    }

    /// Opens/creates `<prefix>.YYYY-MM-DD.log` privately for append, never truncating.
    ///
    /// # Errors
    /// Returns a stage-tagged creation/open or privacy/inspection refusal.
    pub fn append_log(&self, name: &str) -> Result<AdmittedLog, AcquisitionError> {
        let (prefix, _) = name
            .strip_suffix(".log")
            .and_then(|s| s.rsplit_once('.'))
            .ok_or(Untrusted::Unsafe)?;
        log_date(prefix, name.as_bytes()).ok_or(Untrusted::Unsafe)?;
        let prefix = prefix.to_owned();
        let name = basename(name)?;
        let file = self.open_private(
            self.0.private_start + 1,
            &name,
            libc::O_WRONLY | libc::O_APPEND,
            true,
        )?;
        let identity = protected::stat(file.as_raw_fd())?;
        Ok(AdmittedLog {
            file,
            directories: Arc::clone(&self.0),
            identity,
            prefix,
        })
    }

    fn open_private(
        &self,
        parent: usize,
        name: &CStr,
        access: libc::c_int,
        create: bool,
    ) -> Result<File, AcquisitionError> {
        self.0.check()?;
        let folder = &self.0.held[parent].file;
        let flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY;
        let file = if create {
            match open_at(
                folder.as_raw_fd(),
                name,
                flags | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => file,
                Err(AcquisitionError::Open(io::ErrorKind::AlreadyExists)) => {
                    precheck(folder, name, self.0.user)?;
                    open_at(folder.as_raw_fd(), name, flags, 0)?
                }
                Err(error) => return Err(error),
            }
        } else {
            precheck(folder, name, self.0.user)?;
            open_at(folder.as_raw_fd(), name, flags, 0)?
        };
        let identity = protected::inspect_object(&file, self.0.user, ObjectKind::Regular)?;
        if volume(&file, true)? != self.0.held[self.0.private_start - 1].volume {
            return Err(Untrusted::Unsafe.into());
        }
        named(folder, name, &identity)?;
        self.0.check()?;
        Ok(file)
    }

    /// Makes one finite best-effort pass, protecting this owner's active log by identity.
    ///
    /// Candidate failures are skips, not shell failures. Concurrent namespace changes and an
    /// incomplete scan preclude a seven-file guarantee. Deletion is name-based, not atomic by inode.
    ///
    /// # Errors
    /// Refuses a different owner's current log, invalid arguments/parent admission or stream open.
    pub fn retain_logs(
        &self,
        prefix: &str,
        keep: usize,
        current: &AdmittedLog,
    ) -> Result<Retention, AcquisitionError> {
        if !valid_prefix(prefix)
            || prefix != current.prefix
            || keep == 0
            || keep > SCAN_LIMIT
            || !Arc::ptr_eq(&self.0, &current.directories)
        {
            return Err(Untrusted::Unsafe.into());
        }
        self.0.check()?;
        let mut result = Retention::default();
        let logs = &self.0.held[self.0.private_start + 1].file;
        let entries = Entries::open(logs)?;
        let mut candidates = Vec::new();
        while result.can_read() {
            // SAFETY: __error returns this thread's live errno storage.
            let errno = unsafe { libc::__error() };
            // SAFETY: errno is writable thread-local storage.
            unsafe { errno.write(0) };
            // SAFETY: entries uniquely owns a live directory stream.
            let entry = unsafe { libc::readdir(entries.0.as_ptr()) };
            if entry.is_null() {
                // SAFETY: errno is the live storage used by the preceding readdir call.
                result.incomplete = unsafe { errno.read() } != 0;
                break;
            }
            result.scanned += 1;
            // SAFETY: readdir's entry remains live until the next call on this stream.
            let entry = unsafe { &*entry };
            let length = usize::from(entry.d_namlen);
            if length >= entry.d_name.len() || entry.d_name[length] != 0 {
                result.skipped += 1;
                continue;
            }
            let bytes: Vec<u8> = entry.d_name[..length]
                .iter()
                .map(|b| u8::from_ne_bytes(b.to_ne_bytes()))
                .collect();
            let Some(date) = log_date(prefix, &bytes) else {
                result.skipped += 1;
                continue;
            };
            let Ok(name) = CString::new(bytes) else {
                result.skipped += 1;
                continue;
            };
            let Ok(file) =
                self.open_private(self.0.private_start + 1, &name, libc::O_RDONLY, false)
            else {
                result.skipped += 1;
                continue;
            };
            let Ok(identity) = protected::stat(file.as_raw_fd()) else {
                result.skipped += 1;
                continue;
            };
            if same_object(&identity, &current.identity) {
                result.skipped += 1;
                continue;
            }
            let created = file.metadata().and_then(|m| m.created()).unwrap_or(date);
            candidates.push((created, name, identity));
        }
        candidates.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
        });
        for (_, name, identity) in candidates.into_iter().skip(keep - 1) {
            self.0.check()?;
            let Ok(file) =
                self.open_private(self.0.private_start + 1, &name, libc::O_RDONLY, false)
            else {
                result.skipped += 1;
                continue;
            };
            let Ok(now) = protected::stat(file.as_raw_fd()) else {
                result.skipped += 1;
                continue;
            };
            if !same_object(&identity, &now) || named(logs, &name, &now).is_err() {
                result.skipped += 1;
                continue;
            }
            // SAFETY: logs is retained/admitted and name is one checked matching basename.
            if unsafe { libc::unlinkat(logs.as_raw_fd(), name.as_ptr(), 0) } == 0 {
                result.removed += 1;
            } else {
                result.skipped += 1;
            }
        }
        Ok(result)
    }
}

impl Directories {
    fn check(&self) -> Result<(), AcquisitionError> {
        for (index, node) in self.held.iter().enumerate() {
            let now = protected::inspect(&node.file, self.user)?;
            if !protected::same_inspection(&node.identity, &now)
                || volume(&node.file, index + 1 >= self.private_start)? != node.volume
            {
                return Err(Untrusted::Unsafe.into());
            }
            if index >= self.private_start {
                private_directory(&now, self.user)?;
            }
            if index > 0 {
                named(&self.held[index - 1].file, &node.name, &now)?;
            }
        }
        Ok(())
    }
}

fn private_directory(metadata: &libc::stat, user: libc::uid_t) -> Result<(), Untrusted> {
    if metadata.st_uid != user || metadata.st_mode & 0o7777 != 0o700 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn directory(file: File, name: CString, user: libc::uid_t) -> Result<Directory, AcquisitionError> {
    let identity = protected::inspect(&file, user)?;
    let volume = volume(&file, false)?;
    Ok(Directory {
        file,
        name,
        identity,
        volume,
    })
}

fn volume(file: &File, writable: bool) -> Result<Volume, Untrusted> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: file is held and metadata is writable statfs storage.
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(Untrusted::Io);
    }
    // SAFETY: successful fstatfs initialized metadata.
    let metadata = unsafe { metadata.assume_init() };
    filesystem_policy(&metadata, writable)?;
    // SAFETY: Darwin fsid_t is a repr(C) pair of i32 values, with no padding (sys/_types/_fsid_t.h).
    let fsid = unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(metadata.f_fsid) };
    Ok(Volume {
        device: protected::stat(file.as_raw_fd())?.st_dev,
        fsid,
    })
}

fn filesystem_policy(metadata: &libc::statfs, writable: bool) -> Result<(), Untrusted> {
    let flags = i64::from(metadata.f_flags);
    if metadata.f_fstypename[..5] != [97, 112, 102, 115, 0]
        || flags & i64::from(libc::MNT_LOCAL) == 0
        || flags & i64::from(libc::MNT_IGNORE_OWNERSHIP) != 0
        || (writable && flags & i64::from(libc::MNT_RDONLY) != 0)
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn basename(name: &str) -> Result<CString, Untrusted> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(Untrusted::Unsafe);
    }
    CString::new(name).map_err(|_| Untrusted::Unsafe)
}

fn open_at(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, AcquisitionError> {
    // SAFETY: name is terminated, parent is retained or AT_FDCWD for root; mode is promoted for C.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP) => Untrusted::Reparse.into(),
            Some(libc::ENOTDIR | libc::EISDIR) => Untrusted::Unsafe.into(),
            _ => AcquisitionError::Open(error.kind()),
        });
    }
    // SAFETY: successful openat transferred this new uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn entry_stat(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: retained parent, terminated basename and writable stat storage; no link following.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

fn precheck(parent: &File, name: &CStr, user: libc::uid_t) -> Result<(), AcquisitionError> {
    let metadata = match entry_stat(parent, name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AcquisitionError::Open(io::ErrorKind::NotFound));
        }
        Err(_) => return Err(Untrusted::Io.into()),
    };
    protected::safe_regular(&metadata, user)?;
    Ok(())
}

fn named(parent: &File, name: &CStr, expected: &libc::stat) -> Result<(), Untrusted> {
    let actual = entry_stat(parent, name).map_err(|_| Untrusted::Io)?;
    if !protected::same_inspection(&actual, expected) || actual.st_nlink != expected.st_nlink {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn same_object(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn log_date(prefix: &str, name: &[u8]) -> Option<SystemTime> {
    if !valid_prefix(prefix) {
        return None;
    }
    let date = name
        .strip_prefix(prefix.as_bytes())?
        .strip_prefix(b".")?
        .strip_suffix(b".log")?;
    if date.len() != 10 || date[4] != b'-' || date[7] != b'-' {
        return None;
    }
    let number = |bytes: &[u8]| {
        bytes.iter().try_fold(0_u32, |n, b| {
            b.is_ascii_digit().then(|| n * 10 + u32::from(b - b'0'))
        })
    };
    let year = number(&date[..4])?;
    let month = number(&date[5..7])?;
    let day = number(&date[8..])?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let months = [
        31_u32,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > months[usize::try_from(month - 1).ok()?]
    {
        return None;
    }
    let previous = i64::from(year) - 1;
    let days = (i64::from(year) - 1970) * 365 + previous / 4 - previous / 100 + previous / 400
        - 477
        + i64::from(
            months[..usize::try_from(month - 1).ok()?]
                .iter()
                .sum::<u32>(),
        )
        + i64::from(day - 1);
    let seconds = days * 86400;
    if seconds < 0 {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).ok()?))
    }
}

struct Entries(NonNull<libc::DIR>);

impl Entries {
    fn open(logs: &File) -> Result<Self, AcquisitionError> {
        let file = open_at(logs.as_raw_fd(), c".", DIRECTORY_FLAGS, 0)?;
        if !same_object(
            &protected::stat(logs.as_raw_fd())?,
            &protected::stat(file.as_raw_fd())?,
        ) {
            return Err(Untrusted::Unsafe.into());
        }
        let fd = file.into_raw_fd();
        // SAFETY: fd is a uniquely transferred directory descriptor.
        let stream = unsafe { libc::fdopendir(fd) };
        if let Some(stream) = NonNull::new(stream) {
            return Ok(Self(stream));
        }
        let error = io::Error::last_os_error();
        // SAFETY: failed fdopendir did not consume fd; this is its only close.
        unsafe { libc::close(fd) };
        Err(AcquisitionError::Open(error.kind()))
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: this owner uniquely owns the stream and its transferred descriptor.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests;
