use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Write as _};
use std::os::fd::{FromRawFd as _, RawFd};
use std::os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{
    Destination, Fault, Publication, Scratch, Stage, call, check_name, custody, inspect,
    make_directory, matches_name, open_at, private_mode, rename, sync_directory, volume,
};

static RUN: OnceLock<Result<SettingsRoot, Fault>> = OnceLock::new();

// SAFETY: Mach-O calls each pointer in this section with the C ABI before libtest dispatch.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")]
static INIT: extern "C" fn() = initialize;

extern "C" fn initialize() {
    let _admission = RUN.get_or_init(SettingsRoot::admit);
}

fn stop_startup() -> ! {
    let _ = writeln!(
        io::stderr(),
        "macOS scratch descriptor handoff could not be sealed"
    );
    std::process::exit(77);
}

pub(crate) fn write_scratch_settings(path: &Path, text: &[u8]) -> io::Result<()> {
    write_scratch_settings_with(path, text, File::sync_all)
}

pub(super) fn write_scratch_settings_with(
    path: &Path,
    text: &[u8],
    sync_file: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    let process = std::process::id();
    let refused = Fault::new(Stage::Root, ErrorKind::Unsupported);
    let admission = RUN.get().ok_or_else(|| io_fault(refused))?;
    let root = admission.as_ref().map_err(|error| io_fault(*error))?;
    if root.pid != process {
        return Err(io_fault(refused));
    }
    root.write(path, text, sync_file).map_err(io_fault)
}

fn io_fault(error: Fault) -> io::Error {
    error.errno.map_or_else(
        || io::Error::new(error.kind, error),
        io::Error::from_raw_os_error,
    )
}

struct SettingsRoot {
    path: PathBuf,
    directory: File,
    pid: u32,
    uid: libc::uid_t,
    device: libc::dev_t,
}

impl SettingsRoot {
    fn admit() -> Result<Self, Fault> {
        let refused = Fault::new(Stage::Root, ErrorKind::Unsupported);
        let advertised = std::env::var_os("MAC_SCRATCH_TEST_FD").ok_or(refused)?;
        let Some(inherited) = advertised
            .to_str()
            .and_then(|fd| fd.parse::<RawFd>().ok())
            .filter(|fd| *fd > libc::STDERR_FILENO)
        else {
            stop_startup();
        };

        // Seal the passed fd before any admission check can return control to libtest.
        // SAFETY: fcntl validates the integer fd; no Rust ownership is taken by setting this flag.
        if let Err(error) = call(unsafe { libc::fcntl(inherited, libc::F_SETFD, libc::FD_CLOEXEC) })
        {
            if error.raw_os_error() == Some(libc::EBADF) {
                return Err(refused);
            }
            stop_startup();
        }

        let pid = std::process::id();
        let owner = std::env::var("MAC_SCRATCH_TEST_PID")
            .ok()
            .and_then(|id| id.parse::<u32>().ok());
        if owner != Some(pid) {
            return Err(refused);
        }
        let path = std::env::var_os("MAC_SCRATCH_TEST_PARENT")
            .map(PathBuf::from)
            .filter(|path| *path == std::env::temp_dir())
            .ok_or(refused)?;
        absolute_names(&path)?;
        // The trusted runner creates and retains the inherited directory; PID guards accidental inheritance.
        // SAFETY: fcntl checks the integer descriptor and returns a separately owned fd on success.
        let duplicate = unsafe { libc::fcntl(inherited, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::EBADF) {
                refused
            } else {
                Fault::io(Stage::Root, &error)
            });
        }
        // SAFETY: Successful fcntl transferred a new descriptor, owned once by this File.
        let directory = unsafe { File::from_raw_fd(duplicate) };
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| Fault::io(Stage::Root, &error))?;
        let st = inspect(&directory, Stage::Root)?;
        let sample = inspect(&named, Stage::Root)?;
        if st.st_dev != sample.st_dev || st.st_ino != sample.st_ino {
            return Err(Fault::new(Stage::Root, ErrorKind::InvalidData));
        }
        let uid = custody::user();
        if uid == 0 || !private_mode(&st, uid, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Root, ErrorKind::PermissionDenied));
        }
        volume(&directory, Stage::Root)?;
        Ok(Self {
            path,
            directory,
            pid,
            uid,
            device: st.st_dev,
        })
    }

    fn child(&self, parent: &File, name: &CStr) -> Result<File, Fault> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let directory = match open_at(parent, name, flags, Stage::Parent) {
            Ok(directory) => directory,
            Err(error) if error.errno == Some(libc::ENOENT) => {
                // shortcut: New parent entries aren't synced; add barriers before promising crash durability.
                make_directory(parent, name, Stage::Parent)?;
                open_at(parent, name, flags, Stage::Parent)?
            }
            Err(error) => return Err(error),
        };
        let sample = inspect(&directory, Stage::Parent)?;
        if sample.st_dev != self.device || !private_mode(&sample, self.uid, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Parent, ErrorKind::PermissionDenied));
        }
        matches_name(parent, name, &sample, Stage::Parent)?;
        Ok(directory)
    }

    fn write(
        &self,
        path: &Path,
        text: &[u8],
        sync_file: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<(), Fault> {
        absolute_names(path)?;
        let relative = path
            .strip_prefix(&self.path)
            .map_err(|_| Fault::new(Stage::Parent, ErrorKind::PermissionDenied))?;
        let parts = names(relative.as_os_str().as_bytes())?;
        let Some((target, parents)) = parts.split_last() else {
            return Err(Fault::new(Stage::Parent, ErrorKind::InvalidInput));
        };
        let Some((last, ancestors)) = parents.split_last() else {
            return Err(Fault::new(Stage::Parent, ErrorKind::PermissionDenied));
        };
        let mut parent = self
            .directory
            .try_clone()
            .map_err(|error| Fault::io(Stage::Parent, &error))?;
        for name in ancestors {
            parent = self.child(&parent, name)?;
        }
        let directory = self.child(&parent, last)?;
        let mut scratch = Scratch {
            parent,
            name: last.clone(),
            directory,
            owner: self.uid,
            stopped: false,
        };
        let temp = CString::new([target.to_bytes(), b".tmp"].concat())
            .map_err(|_| Fault::new(Stage::Create, ErrorKind::InvalidInput))?;
        let mut file = scratch.stage(&temp)?;
        if let Err(error) = file.write_all(text) {
            scratch.remove(&temp, &file)?;
            return Err(Fault::io(Stage::Create, &error));
        }
        match scratch.replace_with(
            &temp,
            (target, Destination::MayBeAbsent),
            &file,
            rename,
            sync_directory,
            sync_file,
        ) {
            Publication::Completed => Ok(()),
            Publication::NotPublished(error) => {
                scratch.remove(&temp, &file)?;
                Err(error)
            }
            Publication::Indeterminate(error) => {
                let _ = writeln!(
                    io::stderr(),
                    "macOS settings scratch publication indeterminate: {error}"
                );
                std::process::exit(78);
            }
        }
    }
}

fn absolute_names(path: &Path) -> Result<(), Fault> {
    let relative = path
        .as_os_str()
        .as_bytes()
        .strip_prefix(b"/")
        .ok_or_else(|| Fault::new(Stage::Parent, ErrorKind::InvalidInput))?;
    names(relative)?;
    Ok(())
}

fn names(bytes: &[u8]) -> Result<Vec<CString>, Fault> {
    bytes
        .split(|&separator| separator == b'/')
        .map(|bytes| {
            let part = CString::new(bytes)
                .map_err(|_| Fault::new(Stage::Parent, ErrorKind::InvalidInput))?;
            check_name(&part, Stage::Parent)?;
            Ok(part)
        })
        .collect()
}
