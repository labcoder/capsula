//! Read-only candidates for Darwin's user-domain Application Support folder.

use std::ffi::{CStr, OsStr, c_char, c_int, c_long, c_uint};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::ptr;

use kumochi_protocol::constants;

const APPLICATION_SUPPORT: c_uint = 14;
const USER_DOMAIN: c_uint = 1;
const PATH_MAX: usize = 1024;

#[repr(C)]
struct Passwd {
    name: *mut c_char,
    password: *mut c_char,
    uid: c_uint,
    gid: c_uint,
    change: c_long,
    class: *mut c_char,
    gecos: *mut c_char,
    home: *mut c_char,
    shell: *mut c_char,
    expire: c_long,
}

// SAFETY: These match the macOS SDK declarations in sysdir.h, unistd.h and pwd.h;
// Passwd matches Darwin's struct passwd on both supported 64-bit architectures.
unsafe extern "C" {
    fn sysdir_start_search_path_enumeration(directory: c_uint, domain: c_uint) -> c_uint;
    fn sysdir_get_next_search_path_enumeration(state: c_uint, path: *mut c_char) -> c_uint;
    fn geteuid() -> c_uint;
    fn getpwuid_r(
        uid: c_uint,
        pwd: *mut Passwd,
        buffer: *mut c_char,
        size: usize,
        result: *mut *mut Passwd,
    ) -> c_int;
}

/// A per-user data path candidate. It need not exist and isn't checked for writability.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    // SAFETY: Both arguments are the standalone SDK constants for this user-domain query.
    let start = unsafe { sysdir_start_search_path_enumeration(APPLICATION_SUPPORT, USER_DOMAIN) };
    if start == 0 {
        return None;
    }
    let mut path = [u8::MAX; PATH_MAX];
    // SAFETY: The state is live and sysdir requires a writable PATH_MAX-byte buffer.
    let next = unsafe { sysdir_get_next_search_path_enumeration(start, path.as_mut_ptr().cast()) };
    if next == 0 {
        return None;
    }
    let mut extra = [u8::MAX; PATH_MAX];
    // SAFETY: The previous call returned a live state, and extra has PATH_MAX writable bytes.
    if unsafe { sysdir_get_next_search_path_enumeration(next, extra.as_mut_ptr().cast()) } != 0 {
        return None;
    }
    product_path(&user_home()?, Some(&path), constants::DATA_DIR)
}

/// Configuration and data share the same per-user Application Support candidate.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    data_dir()
}

fn user_home() -> Option<PathBuf> {
    // SAFETY: geteuid takes no arguments and has no caller-side preconditions.
    let uid = unsafe { geteuid() };
    let mut record = MaybeUninit::<Passwd>::uninit();
    let mut buffer = [u8::MAX; 16 * PATH_MAX];
    let mut result = ptr::null_mut();
    // SAFETY: All outputs are writable for the documented sizes; the backing buffer lives
    // until after the home field has been copied.
    let status = unsafe {
        getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut result,
        )
    };
    if status != 0 || result != record.as_mut_ptr() {
        return None;
    }
    // SAFETY: A zero status and result pointing to record mean getpwuid_r initialized it.
    let home = unsafe { record.assume_init() }.home;
    let offset = home.addr().checked_sub(buffer.as_ptr().addr())?;
    path_from_buffer(buffer.get(offset..)?).map(Path::to_path_buf)
}

fn path_from_buffer(bytes: &[u8]) -> Option<&Path> {
    let bytes = CStr::from_bytes_until_nul(bytes).ok()?.to_bytes();
    Some(Path::new(OsStr::from_bytes(bytes)))
}

fn product_path(home: &Path, candidate: Option<&[u8]>, product: &str) -> Option<PathBuf> {
    // Only sysdir's user-domain form qualifies; absolute paths and URLs don't specify this home.
    let suffix = path_from_buffer(candidate?)?.strip_prefix("~").ok()?;
    if !home.is_absolute()
        || suffix.as_os_str().is_empty()
        || !suffix
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(home.join(suffix).join(product))
}

#[cfg(test)]
mod tests {
    use super::{PATH_MAX, config_dir, data_dir, path_from_buffer, product_path, user_home};
    use kumochi_protocol::{constants, constants::ProductIdentity, message::ThreadId};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    #[test]
    fn rejects_missing_or_invalid_native_paths() {
        let mut full = [b'a'; PATH_MAX];
        assert_eq!(path_from_buffer(&full), None);
        full[PATH_MAX - 1] = 0;
        assert_eq!(
            path_from_buffer(&full).unwrap().as_os_str().as_bytes(),
            &full[..PATH_MAX - 1]
        );
        for candidate in [
            None,
            Some(b"\0".as_slice()),
            Some(b"~/Library".as_slice()),
            Some(b"Library/Application Support\0".as_slice()),
            Some(b"/Library/Application Support\0".as_slice()),
            Some(b"https://example.invalid/Library\0".as_slice()),
            Some(b"~/\0".as_slice()),
            Some(b"~/../Library\0".as_slice()),
        ] {
            assert_eq!(
                product_path(Path::new("/fixture"), candidate, constants::DATA_DIR),
                None
            );
        }
        for home in ["", "relative"] {
            assert_eq!(
                product_path(Path::new(home), Some(b"~/Library\0"), constants::DATA_DIR),
                None
            );
        }
    }

    #[test]
    fn joins_the_exact_path_for_each_product_identity() {
        let home = Path::new("/fixture/Zoë Test");
        for (identity, expected) in [
            (
                ProductIdentity::RELEASE,
                "/fixture/Zoë Test/Library/Application Support/résumé/Kumochi",
            ),
            (
                ProductIdentity::DEV,
                "/fixture/Zoë Test/Library/Application Support/résumé/Kumochi Dev",
            ),
            (
                ProductIdentity::GATE,
                "/fixture/Zoë Test/Library/Application Support/résumé/Kumochi Gate",
            ),
        ] {
            assert_eq!(
                product_path(
                    home,
                    Some("~/Library/Application Support/résumé\0".as_bytes()),
                    identity.data_dir
                )
                .as_deref(),
                Some(Path::new(expected))
            );
        }
    }

    #[test]
    fn discovers_the_native_candidates_without_opening_them() {
        let home = user_home().expect("the test user has an OS account home");
        let expected = home
            .join("Library/Application Support")
            .join(constants::DATA_DIR);
        assert!(
            expected.is_absolute(),
            "expected an absolute user-domain candidate"
        );
        assert!(
            data_dir().as_ref() == Some(&expected),
            "data candidate differs"
        );
        assert!(
            config_dir().as_ref() == Some(&expected),
            "config candidate differs"
        );
        let store = crate::store::default_path().expect("expected a store candidate");
        assert!(
            store == expected.join("kumochi.db"),
            "store candidate differs"
        );
    }

    #[test]
    fn config_reads_only_the_fixture_file_and_lookup_leaves_the_tree_unchanged() {
        let home = crate::scratch::dir("mac-folders");
        let expected = home
            .join("Library/Application Support")
            .join(constants::DATA_DIR);
        let file = expected.join("config.toml");
        let contents = "assistant_name = \"Nimbus\"\n";
        std::fs::create_dir_all(&expected).unwrap();
        std::fs::write(&file, contents).unwrap();
        let actual = product_path(
            &home,
            Some(b"~/Library/Application Support\0"),
            constants::DATA_DIR,
        );
        assert_eq!(actual.as_ref(), Some(&expected));
        let config = crate::config::Config::load(actual, ThreadId(1));
        assert_eq!(config.view().assistant_name.as_str(), "Nimbus");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), contents);
        for (parent, child) in [
            (home.path().to_owned(), "Library"),
            (home.join("Library"), "Application Support"),
            (
                home.join("Library/Application Support"),
                constants::DATA_DIR,
            ),
            (expected, "config.toml"),
        ] {
            let names: Vec<_> = parent
                .read_dir()
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [child]);
        }
    }
}
