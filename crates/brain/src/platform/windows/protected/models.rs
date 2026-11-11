use std::fs::File;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_READ, FileRenameInfo,
    GetDriveTypeW, READ_CONTROL, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE,
};

use super::{
    Kind, Untrusted, check_marker, delete_on_close, hold_chain, hold_file, open, owner_only_dir,
};

pub(crate) fn validate_model_path(path: &Path) -> Result<(), Untrusted> {
    validate_path(path, |root| {
        // SAFETY: `root` is a live NUL-terminated drive-root string.
        unsafe { GetDriveTypeW(root.as_ptr()) }
    })
}

fn validate_path(path: &Path, drive_kind: impl FnOnce(&[u16; 4]) -> u32) -> Result<(), Untrusted> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(Untrusted::Unsafe);
    };
    let Prefix::Disk(drive) = prefix.kind() else {
        return Err(Untrusted::Unsafe);
    };
    if components.next() != Some(Component::RootDir)
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Untrusted::Unsafe);
    }
    let normalized: PathBuf = path.components().collect();
    if normalized.as_os_str() != path.as_os_str() {
        return Err(Untrusted::Unsafe);
    }
    let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
    let kind = drive_kind(&root);
    if !matches!(
        kind,
        DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK | DRIVE_CDROM
    ) {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

pub(crate) fn create_model_dir(path: &Path) -> Result<Vec<File>, Untrusted> {
    validate_model_path(path)?;
    let mut missing = Vec::new();
    let mut parent = path;
    let mut held = loop {
        match hold_chain(parent) {
            Ok(held) => break held,
            Err(Untrusted::Missing) => {
                missing.push(parent);
                parent = parent.parent().ok_or(Untrusted::Unsafe)?;
            }
            Err(error) => return Err(error),
        }
    };
    for path in missing.into_iter().rev() {
        held.push(owner_only_dir(path)?);
    }
    Ok(held)
}

pub(crate) fn model_file(path: &Path) -> Result<File, Untrusted> {
    let file = hold_file(path)?;
    if !file.metadata().map_err(|_| Untrusted::Io)?.is_file() {
        return Err(Untrusted::Unsafe);
    }
    Ok(file)
}

pub(crate) fn open_model_write(path: &Path) -> Result<File, Untrusted> {
    checked_write(open(path, Kind::File, FILE_GENERIC_WRITE | DELETE)?, path)
}

pub(crate) fn create_model_write(path: &Path) -> Result<File, Untrusted> {
    let file = File::options()
        .write(true)
        .access_mode(
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | READ_CONTROL | FILE_READ_ATTRIBUTES,
        )
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .create_new(true)
        .open(path)
        .map_err(|_| Untrusted::Io)?;
    checked_write(file, path)
}

fn checked_write(file: File, path: &Path) -> Result<File, Untrusted> {
    check_marker(&file, path)?;
    file.try_lock().map_err(|_| Untrusted::Io)?;
    Ok(file)
}

pub(crate) fn discard_model(file: File) -> Result<(), Untrusted> {
    delete_on_close(&file)?;
    drop(file);
    Ok(())
}

pub(crate) fn publish_model(file: File, parent: &File, target: &Path) -> Result<(), Untrusted> {
    validate_model_path(target)?;
    if !super::same_final_path(parent, target.parent().ok_or(Untrusted::Unsafe)?)? {
        return Err(Untrusted::Reparse);
    }
    let name = target.file_name().ok_or(Untrusted::Unsafe)?;
    if !matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) || Path::new(name).components().count() != 1
        || name
            .encode_wide()
            .any(|unit| unit == 0 || unit == u16::from(b':'))
    {
        return Err(Untrusted::Unsafe);
    }
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = (offset + (name.len() + 1) * size_of::<u16>()).max(size_of::<FILE_RENAME_INFO>());
    let mut buffer = vec![0_u64; size.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let header = FILE_RENAME_INFO {
        FileNameLength: u32::try_from(name.len() * size_of::<u16>()).map_err(|_| Untrusted::Io)?,
        ..FILE_RENAME_INFO::default()
    };
    // SAFETY: the buffer is large enough and aligned for the header.
    unsafe { info.write(header) };
    let destination = buffer
        .as_mut_ptr()
        .cast::<u8>()
        .wrapping_add(offset)
        .cast::<u16>();
    // SAFETY: the buffer has room for the name and its already-zero terminator.
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), destination, name.len()) };
    // SAFETY: the live buffer holds the sized rename record; both handles remain open.
    let renamed = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            buffer.as_ptr().cast(),
            u32::try_from(size).map_err(|_| Untrusted::Io)?,
        )
    };
    if renamed == 0 {
        return Err(Untrusted::Io);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;
    use std::io::Write as _;

    #[test]
    fn model_paths_refuse_unc_before_querying_a_drive_and_refuse_network_mappings() {
        assert_eq!(
            validate_path(Path::new(r"\\127.0.0.1\never-contact\models"), |_| panic!(
                "UNC must be lexical"
            )),
            Err(Untrusted::Unsafe)
        );
        assert_eq!(
            validate_path(Path::new(r"Z:\models"), |root| {
                assert_eq!(
                    root,
                    &[u16::from(b'Z'), u16::from(b':'), u16::from(b'\\'), 0]
                );
                windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE
            }),
            Err(Untrusted::Unsafe)
        );
    }
    #[test]
    fn model_paths_publish_the_held_partial_without_replacing_an_unexpected_arrival() {
        let scratch = Scratch::trusted("model-publication");
        let held = create_model_dir(scratch.dir()).unwrap();
        let parent = held.last().unwrap();
        let source = scratch.dir().join("fixture.part");
        let target = scratch.dir().join("fixture.bin");
        let mut file = create_model_write(&source).unwrap();
        file.write_all(b"verified").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            std::fs::rename(&source, source.with_extension("moved"))
                .map_err(|error| error.raw_os_error()),
            Err(Some(32))
        );
        let other = scratch.dir().join("other");
        std::fs::create_dir(&other).unwrap();
        assert_eq!(
            publish_model(file, parent, &other.join("fixture.bin")),
            Err(Untrusted::Reparse)
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"verified");
        assert_eq!(
            std::fs::symlink_metadata(other.join("fixture.bin"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        let file = open_model_write(&source).unwrap();
        std::fs::write(&target, b"arrival").unwrap();
        assert_eq!(publish_model(file, parent, &target), Err(Untrusted::Io));
        assert_eq!(std::fs::read(&target).unwrap(), b"arrival");
        assert_eq!(std::fs::read(&source).unwrap(), b"verified");
        std::fs::remove_file(&target).unwrap();
        let file = open_model_write(&source).unwrap();
        assert_eq!(
            publish_model(file, parent, &target),
            Ok(()),
            "publication OS code: {:?}",
            std::io::Error::last_os_error().raw_os_error(),
        );
        assert_eq!(std::fs::read(target).unwrap(), b"verified");
        assert_eq!(
            std::fs::symlink_metadata(source).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
