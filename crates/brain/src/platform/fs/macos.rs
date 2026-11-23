use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd as _;

mod custody;
mod settings;
mod tests;

use custody::{
    check_name, inspect, make_directory, matches_name, named_stat, open_at, private_mode, unlink,
};
pub(crate) use settings::write_scratch_settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Parent,
    Root,
    Create,
    Source,
    Destination,
    SourceSync,
    Rename,
    PublishedIdentity,
    DirectorySync,
    PublishedSync,
    Capacity,
    Cleanup,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{stage:?}: {kind:?} (errno {errno:?})")]
struct Fault {
    stage: Stage,
    kind: ErrorKind,
    errno: Option<i32>,
}

impl Fault {
    fn new(stage: Stage, kind: ErrorKind) -> Self {
        Self {
            stage,
            kind,
            errno: None,
        }
    }

    fn io(stage: Stage, error: &io::Error) -> Self {
        Self {
            stage,
            kind: error.kind(),
            errno: error.raw_os_error(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Publication {
    Completed,
    NotPublished(Fault),
    Indeterminate(Fault),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Destination {
    Existing,
    MayBeAbsent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameMode {
    Replace,
    NoReplace,
}

struct Scratch {
    parent: File,
    name: CString,
    directory: File,
    owner: libc::uid_t,
    stopped: bool,
}

impl Scratch {
    fn create(parent: File, name: CString) -> Result<Self, Fault> {
        check_name(&name, Stage::Root)?;
        let owner = custody::user();
        let anchor = inspect(&parent, Stage::Parent)?;
        if owner == 0 || !private_mode(&anchor, owner, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Parent, ErrorKind::PermissionDenied));
        }
        volume(&parent, Stage::Parent)?;
        make_directory(&parent, &name, Stage::Root)?;
        let directory = open_at(
            &parent,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            Stage::Root,
        )?;
        let root = inspect(&directory, Stage::Root)?;
        if root.st_dev != anchor.st_dev || !private_mode(&root, owner, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Root, ErrorKind::PermissionDenied));
        }
        matches_name(&parent, &name, &root, Stage::Root)?;
        Ok(Self {
            parent,
            name,
            directory,
            owner,
            stopped: false,
        })
    }

    fn active(&self) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::new(Stage::Stopped, ErrorKind::Other));
        }
        Ok(())
    }

    fn stage(&self, name: &CStr) -> Result<File, Fault> {
        self.active()?;
        let file = open_at(
            &self.directory,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            Stage::Create,
        )?;
        let sample = inspect(&file, Stage::Create)?;
        self.check_regular(&sample, Stage::Create)?;
        matches_name(&self.directory, name, &sample, Stage::Create)?;
        Ok(file)
    }

    fn check_regular(&self, sample: &libc::stat, stage: Stage) -> Result<(), Fault> {
        if !private_mode(sample, self.owner, libc::S_IFREG | 0o600) || sample.st_nlink != 1 {
            return Err(Fault::new(stage, ErrorKind::PermissionDenied));
        }
        Ok(())
    }

    fn regular(
        &self,
        name: &CStr,
        flags: libc::c_int,
        stage: Stage,
    ) -> Result<(File, libc::stat), Fault> {
        let named = named_stat(&self.directory, name, stage)?;
        self.check_regular(&named, stage)?;
        let opened = open_at(&self.directory, name, flags, stage)?;
        let held = inspect(&opened, stage)?;
        self.check_regular(&held, stage)?;
        matches_name(&self.directory, name, &held, stage)?;
        if named.st_dev != held.st_dev || named.st_ino != held.st_ino {
            return Err(Fault::new(stage, ErrorKind::InvalidData));
        }
        Ok((opened, held))
    }

    fn prepare(
        &self,
        staged: &File,
        from: &CStr,
        to: &CStr,
        policy: Destination,
    ) -> Result<(libc::stat, RenameMode), Fault> {
        self.active()?;
        let source = inspect(staged, Stage::Source)?;
        self.check_regular(&source, Stage::Source)?;
        matches_name(&self.directory, from, &source, Stage::Source)?;
        let destination = match self.regular(to, libc::O_RDONLY, Stage::Destination) {
            Ok((_old, sample)) => Some(sample),
            Err(error)
                if policy == Destination::MayBeAbsent && error.errno == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        if destination
            .as_ref()
            .is_some_and(|dest| source.st_dev != dest.st_dev || source.st_ino == dest.st_ino)
        {
            return Err(Fault::new(Stage::Destination, ErrorKind::InvalidInput));
        }
        staged
            .sync_all()
            .map_err(|error| Fault::io(Stage::SourceSync, &error))?;
        matches_name(&self.directory, from, &source, Stage::Source)?;
        if let Some(destination) = destination {
            matches_name(&self.directory, to, &destination, Stage::Destination)?;
            Ok((source, RenameMode::Replace))
        } else {
            Ok((source, RenameMode::NoReplace))
        }
    }

    fn replace(&mut self, from: &CStr, to: &CStr) -> Publication {
        if let Err(error) = self.active() {
            return Publication::NotPublished(error);
        }
        let (staged, _) = match self.regular(from, libc::O_RDWR, Stage::Source) {
            Ok(staged) => staged,
            Err(error) => return Publication::NotPublished(error),
        };
        self.replace_with(
            from,
            (to, Destination::Existing),
            &staged,
            rename,
            sync_directory,
            File::sync_all,
        )
    }

    fn replace_with(
        &mut self,
        from: &CStr,
        (to, policy): (&CStr, Destination),
        staged: &File,
        rename: impl FnOnce(&File, &CStr, &CStr, RenameMode) -> io::Result<()>,
        sync_directory: impl FnOnce(&File) -> io::Result<()>,
        sync_file: impl FnOnce(&File) -> io::Result<()>,
    ) -> Publication {
        let (source, mode) = match self.prepare(staged, from, to, policy) {
            Ok(ready) => ready,
            Err(error) => return Publication::NotPublished(error),
        };
        let renamed = rename(&self.directory, from, to, mode);
        if mode == RenameMode::NoReplace
            && let Err(error) = &renamed
            && error.raw_os_error() == Some(libc::EEXIST)
        {
            return Publication::NotPublished(Fault::io(Stage::Rename, error));
        }
        let published = (|| {
            renamed.map_err(|error| Fault::io(Stage::Rename, &error))?;
            matches_name(&self.directory, to, &source, Stage::PublishedIdentity)?;
            sync_directory(&self.directory)
                .map_err(|error| Fault::io(Stage::DirectorySync, &error))?;
            sync_file(staged).map_err(|error| Fault::io(Stage::PublishedSync, &error))
        })();
        match published {
            Ok(()) => Publication::Completed,
            Err(error) => {
                self.stopped = true;
                Publication::Indeterminate(error)
            }
        }
    }

    fn free_space(&self) -> Result<u64, Fault> {
        self.active()?;
        available_bytes(&volume(&self.directory, Stage::Capacity)?)
    }

    fn remove(&self, name: &CStr, held: &File) -> Result<(), Fault> {
        self.active()?;
        let sample = inspect(held, Stage::Cleanup)?;
        self.check_regular(&sample, Stage::Cleanup)?;
        unlink(&self.directory, name, &sample, 0)
    }

    fn finish(self) -> Result<(), Fault> {
        self.active()?;
        let sample = inspect(&self.directory, Stage::Cleanup)?;
        if !private_mode(&sample, self.owner, libc::S_IFDIR | 0o700) {
            return Err(Fault::new(Stage::Cleanup, ErrorKind::PermissionDenied));
        }
        unlink(&self.parent, &self.name, &sample, libc::AT_REMOVEDIR)
    }
}

fn call(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename(directory: &File, from: &CStr, to: &CStr, mode: RenameMode) -> io::Result<()> {
    let fd = directory.as_raw_fd();
    if mode == RenameMode::NoReplace {
        // SAFETY: Both names are checked siblings and the parent fd stays live for the call.
        return call(unsafe {
            libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL)
        });
    }
    // SAFETY: Both checked names and the single held parent fd remain valid for the call.
    call(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) })
}

fn sync_directory(directory: &File) -> io::Result<()> {
    // SAFETY: The directory file owns this fd for the duration of fsync.
    call(unsafe { libc::fsync(directory.as_raw_fd()) })
}

fn volume(directory: &File, stage: Stage) -> Result<libc::statfs, Fault> {
    let mut out = MaybeUninit::uninit();
    // SAFETY: The held directory is live and out can hold a complete statfs.
    call(unsafe { libc::fstatfs(directory.as_raw_fd(), out.as_mut_ptr()) })
        .map_err(|error| Fault::io(stage, &error))?;
    // SAFETY: Successful fstatfs initialized the complete structure.
    let sample = unsafe { out.assume_init() };
    let flags = i64::from(sample.f_flags);
    if !sample
        .f_fstypename
        .starts_with(&b"apfs\0".map(u8::cast_signed))
        || flags & i64::from(libc::MNT_LOCAL) == 0
        || flags & i64::from(libc::MNT_RDONLY | libc::MNT_IGNORE_OWNERSHIP) != 0
    {
        return Err(Fault::new(stage, ErrorKind::Unsupported));
    }
    Ok(sample)
}

fn available_bytes(sample: &libc::statfs) -> Result<u64, Fault> {
    if sample.f_bsize == 0 || sample.f_bavail > sample.f_blocks {
        return Err(Fault::new(Stage::Capacity, ErrorKind::InvalidData));
    }
    sample
        .f_bavail
        .checked_mul(u64::from(sample.f_bsize))
        .ok_or_else(|| Fault::new(Stage::Capacity, ErrorKind::InvalidData))
}

fn links_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP))
}
