//! Existing per-user namespace and append/create semantics, with finite exact-name retention.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use time::{Date, OffsetDateTime};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
};

use crate::platform::logging::{self, Error, Log, Retention, Store};

type Identity = (u32, u32, u32);

pub(crate) struct Backend {
    directory: PathBuf,
    prefix: String,
}
pub(crate) struct FileLog {
    file: File,
    identity: Identity,
}

impl Backend {
    pub(crate) fn new(prefix: &str) -> Result<Self, Error> {
        logging::filename(prefix, OffsetDateTime::now_utc().date())?;
        let directory = super::data_dir()
            .ok_or(Error::Open(io::ErrorKind::InvalidInput))?
            .join("logs");
        fs::create_dir_all(&directory).map_err(ordinary)?;
        Ok(Self {
            directory,
            prefix: prefix.to_owned(),
        })
    }
}

impl Store for Backend {
    type Log = FileLog;

    fn open(&mut self, day: Date) -> Result<FileLog, Error> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(self.directory.join(logging::filename(&self.prefix, day)?))
            .map_err(ordinary)?;
        Ok(FileLog {
            identity: identity(&file).map_err(ordinary)?,
            file,
        })
    }

    fn retain(&mut self, current: &FileLog) -> Result<Retention, Error> {
        let mut result = Retention::default();
        let mut candidates = Vec::new();
        let entries = fs::read_dir(&self.directory).map_err(ordinary)?;
        let mut scanned = 0;
        for entry in entries.take(logging::SCAN_LIMIT) {
            scanned += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    result.incomplete = true;
                    result.skipped += 1;
                    continue;
                }
            };
            let candidate = (|| {
                let name = entry.file_name();
                let day = logging::file_date(&self.prefix, name.to_str()?)?;
                let (file, id) = candidate(&entry.path()).ok()?;
                if id == current.identity {
                    return None;
                }
                let created = file.metadata().and_then(|m| m.created()).ok().or_else(|| {
                    let seconds = day
                        .with_time(time::Time::MIDNIGHT)
                        .assume_utc()
                        .unix_timestamp();
                    let duration = Duration::from_secs(seconds.unsigned_abs());
                    if seconds < 0 {
                        UNIX_EPOCH.checked_sub(duration)
                    } else {
                        UNIX_EPOCH.checked_add(duration)
                    }
                })?;
                Some((created, name, id))
            })();
            if let Some(candidate) = candidate {
                candidates.push(candidate);
            } else {
                result.skipped += 1;
            }
        }
        result.incomplete |= scanned == logging::SCAN_LIMIT;
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, name, expected) in candidates.into_iter().skip(logging::KEEP_FILES - 1) {
            let path = self.directory.join(name);
            if candidate(&path).is_ok_and(|(_, id)| id == expected && id != current.identity)
                && fs::remove_file(&path).is_ok()
            {
                continue;
            }
            result.skipped += 1;
        }
        Ok(result)
    }
}

impl Log for FileLog {
    fn append(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.file).write_all(bytes)
    }
    fn flush(&self) -> io::Result<()> {
        (&self.file).flush()
    }
}

fn candidate(path: &Path) -> io::Result<(File, Identity)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let id = identity(&file)?;
    Ok((file, id))
}

fn identity(file: &File) -> io::Result<Identity> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file retains its handle and info is writable for the call.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
        .map_err(|_| io::Error::last_os_error())?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

fn ordinary(error: io::Error) -> Error {
    Error::Open(error.kind())
}
