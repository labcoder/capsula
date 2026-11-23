use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Write as _};
use std::os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{
    Destination, Fault, Publication, Scratch, Stage, check_name, custody, inspect, make_directory,
    matches_name, open_at, private_mode, rename, sync_directory, volume,
};

static RUN: OnceLock<Result<SettingsRoot, Fault>> = OnceLock::new();

pub(crate) fn write_scratch_settings(path: &Path, text: &[u8]) -> io::Result<()> {
    let root = RUN.get_or_init(|| {
        let path = std::env::var_os("MAC_SCRATCH_TEST_PARENT").map(PathBuf::from);
        let owner = std::env::var("MAC_SCRATCH_TEST_PID")
            .ok()
            .and_then(|id| id.parse::<u32>().ok());
        match path {
            Some(path) if owner == Some(std::process::id()) && path == std::env::temp_dir() => {
                SettingsRoot::open(path)
            }
            _ => Err(Fault::new(Stage::Root, ErrorKind::Unsupported)),
        }
    });
    root.as_ref()
        .map_err(|error| io_fault(*error))?
        .write(path, text)
        .map_err(io_fault)
}

fn io_fault(error: Fault) -> io::Error {
    error.errno.map_or_else(
        || io::Error::new(error.kind, error),
        io::Error::from_raw_os_error,
    )
}

pub(super) struct SettingsRoot {
    path: PathBuf,
    directory: File,
    uid: libc::uid_t,
    device: libc::dev_t,
}

impl SettingsRoot {
    pub(super) fn open(path: PathBuf) -> Result<Self, Fault> {
        absolute_names(&path)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| Fault::io(Stage::Root, &error))?;
        let st = inspect(&directory, Stage::Root)?;
        let uid = custody::user();
        if uid == 0 || !private_mode(&st, uid, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Root, ErrorKind::PermissionDenied));
        }
        volume(&directory, Stage::Root)?;
        Ok(Self {
            path,
            directory,
            uid,
            device: st.st_dev,
        })
    }

    fn child(&self, parent: &File, name: &CStr) -> Result<File, Fault> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let directory = match open_at(parent, name, flags, Stage::Parent) {
            Ok(directory) => directory,
            Err(error) if error.errno == Some(libc::ENOENT) => {
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

    pub(super) fn write(&self, path: &Path, text: &[u8]) -> Result<(), Fault> {
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
            target,
            Destination::MayBeAbsent,
            rename,
            sync_directory,
            File::sync_all,
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
