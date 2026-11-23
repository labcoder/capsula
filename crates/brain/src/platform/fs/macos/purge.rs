use std::ffi::CString;
use std::fs::File;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use super::settings::{admitted_root, io_fault};
use super::{
    Fault, RenameMode, Stage, available_bytes, call, check_name, custody, inspect, matches_name,
    named_stat, open_at, private_mode, rename, sync_directory, unlink, volume,
};

pub(crate) struct PurgeDirectory {
    path: PathBuf,
    directory: File,
    device: libc::dev_t,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PurgeIdentity(libc::dev_t, libc::ino_t);

pub(crate) type PurgeFileId = [u8; 24];

pub(crate) struct PurgeInode {
    file: File,
    identity: PurgeIdentity,
    file_id: PurgeFileId,
}

impl PurgeInode {
    pub(crate) fn identity(&self) -> PurgeIdentity {
        self.identity
    }

    pub(crate) fn file_id(&self) -> PurgeFileId {
        self.file_id
    }

    pub(crate) fn read_exact(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.file.read_exact(bytes)?;
        if self.file.read(&mut [0])? != 0 {
            return Err(ErrorKind::InvalidData.into());
        }
        Ok(())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub(crate) fn is_empty(&self) -> io::Result<bool> {
        Ok(self.file.metadata()?.len() == 0)
    }
}

impl PurgeDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Option<Self>> {
        let Ok(root) = admitted_root() else {
            return Ok(None);
        };
        let Some(directory) = root.existing_directory(path).map_err(io_fault)? else {
            return Ok(None);
        };
        let device = inspect(&directory, Stage::Parent).map_err(io_fault)?.st_dev;
        Ok(Some(Self {
            path: path.to_owned(),
            directory,
            device,
        }))
    }

    pub(crate) fn free_space(&self) -> io::Result<u64> {
        available_bytes(&volume(&self.directory, Stage::Capacity).map_err(io_fault)?)
            .map_err(io_fault)
    }

    fn name(&self, path: &Path, stage: Stage) -> Result<CString, Fault> {
        let name = path
            .strip_prefix(&self.path)
            .map_err(|_| Fault::new(stage, ErrorKind::PermissionDenied))?;
        let name = CString::new(name.as_os_str().as_bytes())
            .map_err(|_| Fault::new(stage, ErrorKind::InvalidInput))?;
        check_name(&name, stage)?;
        Ok(name)
    }

    fn sample(&self, file: &File, stage: Stage) -> Result<libc::stat, Fault> {
        let sample = inspect(file, stage)?;
        if sample.st_dev != self.device
            || !private_mode(&sample, custody::user(), libc::S_IFREG | 0o600)
            || !(1..=2).contains(&sample.st_nlink)
        {
            return Err(Fault::new(stage, ErrorKind::PermissionDenied));
        }
        Ok(sample)
    }

    pub(crate) fn absent(&self, path: &Path) -> io::Result<bool> {
        let name = self.name(path, Stage::Destination).map_err(io_fault)?;
        match named_stat(&self.directory, &name, Stage::Destination) {
            Ok(_) => Ok(false),
            Err(error) if error.errno == Some(libc::ENOENT) => Ok(true),
            Err(error) => Err(io_fault(error)),
        }
    }

    pub(crate) fn hold(&self, path: &Path) -> io::Result<PurgeInode> {
        let stage = Stage::Source;
        let name = self.name(path, stage).map_err(io_fault)?;
        let file = open_at(&self.directory, &name, libc::O_RDWR, stage).map_err(io_fault)?;
        self.bind(file, &name, stage)
    }

    pub(crate) fn verify(&self, path: &Path, inode: &PurgeInode, links: u16) -> io::Result<()> {
        let stage = Stage::PublishedIdentity;
        let name = self.name(path, stage).map_err(io_fault)?;
        let sample = self.sample(&inode.file, stage).map_err(io_fault)?;
        if sample.st_nlink != links || PurgeIdentity(sample.st_dev, sample.st_ino) != inode.identity
        {
            return Err(io_fault(Fault::new(stage, ErrorKind::InvalidData)));
        }
        matches_name(&self.directory, &name, &sample, stage).map_err(io_fault)
    }

    pub(crate) fn stage_intent(&self, path: &Path, contents: &[u8]) -> io::Result<PurgeInode> {
        let mut inode = self.create(path)?;
        inode.file.write_all(contents)?;
        inode.sync()?;
        self.verify(path, &inode, 1)?;
        Ok(inode)
    }

    pub(crate) fn create(&self, path: &Path) -> io::Result<PurgeInode> {
        let stage = Stage::Create;
        let name = self.name(path, stage).map_err(io_fault)?;
        let file = open_at(
            &self.directory,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            stage,
        )
        .map_err(io_fault)?;
        self.bind(file, &name, stage)
    }

    fn bind(&self, file: File, name: &std::ffi::CStr, stage: Stage) -> io::Result<PurgeInode> {
        let sample = self.sample(&file, stage).map_err(io_fault)?;
        matches_name(&self.directory, name, &sample, stage).map_err(io_fault)?;
        let mut file_id = [0; 24];
        file_id[..8].copy_from_slice(&sample.st_ino.to_le_bytes());
        file_id[8..16].copy_from_slice(&sample.st_birthtime.to_le_bytes());
        file_id[16..].copy_from_slice(&sample.st_birthtime_nsec.to_le_bytes());
        Ok(PurgeInode {
            file,
            identity: PurgeIdentity(sample.st_dev, sample.st_ino),
            file_id,
        })
    }

    pub(crate) fn link(&self, path: &Path, alias: &Path) -> io::Result<()> {
        let from = self.name(path, Stage::Source).map_err(io_fault)?;
        let to = self.name(alias, Stage::Destination).map_err(io_fault)?;
        let fd = self.directory.as_raw_fd();
        // SAFETY: Both checked sibling names and the held directory fd remain live for linkat.
        call(unsafe { libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0) })
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path, mode: RenameMode) -> io::Result<()> {
        let from = self.name(from, Stage::Source).map_err(io_fault)?;
        let to = self.name(to, Stage::Destination).map_err(io_fault)?;
        rename(&self.directory, &from, &to, mode)
    }

    pub(crate) fn durable(&self, inode: &PurgeInode) -> io::Result<()> {
        sync_directory(&self.directory)?;
        // SAFETY: The live checked file owns the fd and F_FULLFSYNC takes no extra argument.
        call(unsafe { libc::fcntl(inode.file.as_raw_fd(), libc::F_FULLFSYNC) })
    }

    pub(crate) fn remove(&self, path: &Path, inode: &PurgeInode, links: u16) -> io::Result<()> {
        self.verify(path, inode, links)?;
        let stage = Stage::Cleanup;
        let name = self.name(path, stage).map_err(io_fault)?;
        let sample = self.sample(&inode.file, stage).map_err(io_fault)?;
        unlink(&self.directory, &name, &sample, 0).map_err(io_fault)
    }
}
