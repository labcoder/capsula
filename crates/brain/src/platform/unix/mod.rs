//! Unix product-folder admission.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// Creates the product folder owner-only when missing. Checks the final folder on an open
/// descriptor, without following a link at its name or changing an existing folder.
///
/// # Errors
/// Refuses a missing final name, a non-directory or link, an untrusted owner or write
/// permission, and failed creation or inspection.
pub fn product_folder(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or(ErrorKind::InvalidInput)?;
    let leaf = parent.join(path.file_name().ok_or(ErrorKind::InvalidInput)?);
    fs::create_dir_all(parent)?;
    if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&leaf)
        && error.kind() != ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    let fd = match rustix::fs::open(
        &leaf,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Err(Errno::LOOP | Errno::NOTDIR) => return Err(ErrorKind::PermissionDenied.into()),
        opened => opened?,
    };
    check_folder(&File::from(fd))
}

#[cfg(target_os = "macos")]
fn check_folder(folder: &File) -> io::Result<()> {
    use kumochi_storage::Untrusted;
    kumochi_storage::check_product_folder(folder).map_err(|error| {
        match error {
            Untrusted::Unsafe | Untrusted::Reparse => ErrorKind::PermissionDenied,
            Untrusted::Missing => ErrorKind::NotFound,
            Untrusted::Io => ErrorKind::Other,
        }
        .into()
    })
}

#[cfg(not(target_os = "macos"))]
fn check_folder(folder: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = folder.metadata()?;
    if !trusted_mode(
        metadata.uid(),
        rustix::process::geteuid().as_raw(),
        metadata.mode(),
    ) {
        return Err(ErrorKind::PermissionDenied.into());
    }
    #[cfg(target_os = "linux")]
    match rustix::fs::fgetxattr(folder, c"system.posix_acl_default", &mut [0_u8; 0]) {
        Err(Errno::NODATA) => {}
        Ok(_) => return Err(ErrorKind::PermissionDenied.into()),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn trusted_mode(owner: u32, effective: u32, mode: u32) -> bool {
    (owner == effective || owner == 0) && mode & 0o022 == 0
}

#[cfg(test)]
mod tests;
