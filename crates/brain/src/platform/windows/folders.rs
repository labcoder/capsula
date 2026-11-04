//! Per-user folders, honoring the environment used by sandboxed runs.

use std::ffi::OsString;
use std::path::PathBuf;

use kumochi_protocol::constants;

/// The per-user folder for local data (logs, the store): `%LOCALAPPDATA%\\<product>`.
///
/// None unless `%LOCALAPPDATA%` is an absolute path, so a bad value never puts data under the
/// working directory.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    data_dir_under(std::env::var_os("LOCALAPPDATA"))
}

/// The empty account-session lock file under the local data folder, separate for dev builds.
///
/// None unless `%LOCALAPPDATA%` is an absolute path, by the same rule as [`data_dir`].
#[must_use]
pub fn session_lock_path() -> Option<PathBuf> {
    Some(data_dir()?.join(constants::SESSION_LOCK))
}

/// The per-user folder for settings, which roam: `%APPDATA%\\<product>`, by the same rule.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    data_dir_under(std::env::var_os("APPDATA"))
}

fn data_dir_under(base: Option<OsString>) -> Option<PathBuf> {
    let base = PathBuf::from(base?);
    base.is_absolute().then(|| base.join(constants::DATA_DIR))
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;

    use kumochi_protocol::constants::DATA_DIR;

    use super::data_dir_under;

    #[test]
    fn the_data_folder_needs_an_absolute_base() {
        for bad in ["", ".", "AppData", r"\Users\me", "C:AppData"] {
            assert_eq!(data_dir_under(Some(bad.into())), None, "{bad:?}");
        }
        assert_eq!(data_dir_under(None), None);
        assert_eq!(
            data_dir_under(Some(r"C:\Users\me\AppData\Local".into())),
            Some(PathBuf::from(r"C:\Users\me\AppData\Local").join(DATA_DIR))
        );
    }
}
