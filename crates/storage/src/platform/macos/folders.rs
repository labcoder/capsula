use std::ffi::{CStr, OsStr};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::ptr;

use libc::sysdir_search_path_directory_t::SYSDIR_DIRECTORY_APPLICATION_SUPPORT;
use libc::sysdir_search_path_domain_mask_t::SYSDIR_DOMAIN_MASK_USER;

const PATH_BYTES: usize = {
    assert!(
        libc::PATH_MAX == 1024,
        "sysdir requires a writable PATH_MAX buffer"
    );
    1024
};

/// Why macOS did not supply an absolute user-domain Application Support candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationSupportError {
    /// The OS could not supply a unique candidate and any account home it needs.
    #[error("macOS Application Support folder is unavailable")]
    Unavailable,
    /// The OS returned a malformed path.
    #[error("macOS returned an invalid Application Support path")]
    InvalidPath,
}

/// Gets the current user's Application Support candidate without opening or creating it.
///
/// The result does not establish that the directory exists or is safe to write to.
///
/// # Errors
/// Refuses if the OS cannot supply a unique, absolute candidate.
pub fn application_support_dir() -> Result<PathBuf, ApplicationSupportError> {
    // shortcut: Not an App Sandbox container locator; use a container-aware API for sandboxed builds.
    // SAFETY: These SDK constants select only the user-domain Application Support search.
    let state = unsafe {
        libc::sysdir_start_search_path_enumeration(
            SYSDIR_DIRECTORY_APPLICATION_SUPPORT,
            SYSDIR_DOMAIN_MASK_USER,
        )
    };
    if state == 0 {
        return Err(ApplicationSupportError::Unavailable);
    }
    let mut path = [u8::MAX; PATH_BYTES];
    // SAFETY: state is live and the API requires a writable PATH_MAX-byte buffer.
    let next =
        unsafe { libc::sysdir_get_next_search_path_enumeration(state, path.as_mut_ptr().cast()) };
    if next == 0 {
        return Err(ApplicationSupportError::Unavailable);
    }
    let mut extra = [u8::MAX; PATH_BYTES];
    // SAFETY: next is live and extra provides another writable PATH_MAX-byte buffer.
    if unsafe { libc::sysdir_get_next_search_path_enumeration(next, extra.as_mut_ptr().cast()) }
        != 0
    {
        return Err(ApplicationSupportError::Unavailable);
    }
    resolve_path(&path, account_home)
}

fn account_home() -> Result<PathBuf, ApplicationSupportError> {
    // SAFETY: geteuid has no arguments or caller preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut record = MaybeUninit::<libc::passwd>::uninit();
    let mut bytes = [u8::MAX; 16 * PATH_BYTES];
    let mut result = ptr::null_mut();
    // SAFETY: All output objects are writable and the supplied size is the buffer's capacity.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &raw mut result,
        )
    };
    if code != 0 || result != record.as_mut_ptr() {
        return Err(ApplicationSupportError::Unavailable);
    }
    // SAFETY: Success with result pointing at record means the API initialized it.
    let home = unsafe { record.assume_init() }.pw_dir;
    let path = home
        .addr()
        .checked_sub(bytes.as_ptr().addr())
        .and_then(|offset| bytes.get(offset..))
        .and_then(path_from_buffer)
        .ok_or(ApplicationSupportError::InvalidPath)?;
    Ok(path.to_owned())
}

fn path_from_buffer(bytes: &[u8]) -> Option<&Path> {
    let bytes = CStr::from_bytes_until_nul(bytes).ok()?.to_bytes();
    Some(Path::new(OsStr::from_bytes(bytes)))
}

fn resolve_path(
    bytes: &[u8],
    home: impl FnOnce() -> Result<PathBuf, ApplicationSupportError>,
) -> Result<PathBuf, ApplicationSupportError> {
    use ApplicationSupportError::InvalidPath;

    let path = path_from_buffer(bytes).ok_or(InvalidPath)?;
    if path.is_absolute() {
        return absolute_candidate(path)
            .then(|| path.to_owned())
            .ok_or(InvalidPath);
    }
    let suffix = path.strip_prefix("~").map_err(|_| InvalidPath)?;
    if suffix.as_os_str().is_empty()
        || !suffix
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(InvalidPath);
    }
    let root = home()?;
    if !absolute_candidate(&root) {
        return Err(InvalidPath);
    }
    Ok(root.join(suffix))
}

fn absolute_candidate(path: &Path) -> bool {
    path.is_absolute()
        && path.parent().is_some()
        && path
            .components()
            .skip(1)
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::{ApplicationSupportError, PATH_BYTES, resolve_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn absolute_paths_need_no_account_lookup() {
        assert_eq!(
            resolve_path(
                "/fixture/Zoë/Library/Application Support\0".as_bytes(),
                || panic!("no lookup")
            )
            .as_deref(),
            Ok(Path::new("/fixture/Zoë/Library/Application Support")),
        );
    }

    #[test]
    fn user_relative_paths_use_only_the_account_home() {
        let path = b"~/Library/Application Support\0";
        assert_eq!(
            resolve_path(path, || Ok(PathBuf::from("/fixture/Zoë"))).as_deref(),
            Ok(Path::new("/fixture/Zoë/Library/Application Support")),
        );
        assert_eq!(
            resolve_path(path, || Err(ApplicationSupportError::Unavailable)),
            Err(ApplicationSupportError::Unavailable),
        );
        assert_eq!(
            resolve_path(path, || Ok(PathBuf::from("relative"))),
            Err(ApplicationSupportError::InvalidPath),
        );
    }

    #[test]
    fn rejects_missing_unterminated_and_non_user_paths() {
        for bytes in [
            b"\0".as_slice(),
            &[b'a'; PATH_BYTES],
            b"Library/Application Support\0",
            b"~other/Library\0",
            b"~/\0",
            b"~/../Library\0",
            b"/\0",
            b"/fixture/../Library\0",
        ] {
            assert_eq!(
                resolve_path(bytes, || panic!("invalid path needs no lookup")),
                Err(ApplicationSupportError::InvalidPath),
            );
        }
        let mut full = [b'a'; PATH_BYTES];
        full[0] = b'/';
        full[PATH_BYTES - 1] = 0;
        let expected = format!("/{}", "a".repeat(PATH_BYTES - 2));
        assert_eq!(
            resolve_path(&full, || panic!("no lookup")).as_deref(),
            Ok(Path::new(&expected)),
        );
    }
}
