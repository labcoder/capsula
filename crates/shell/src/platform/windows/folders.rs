//! Per-user data folder, honoring the environment used by sandboxed runs.

use std::ffi::OsString;
use std::path::PathBuf;

use kumochi_protocol::constants;

/// The local data folder, only when `%LOCALAPPDATA%` is an absolute path.
pub(crate) fn data_dir() -> Option<PathBuf> {
    data_dir_under(std::env::var_os("LOCALAPPDATA"))
}

fn data_dir_under(base: Option<OsString>) -> Option<PathBuf> {
    let base = PathBuf::from(base?);
    base.is_absolute().then(|| base.join(constants::DATA_DIR))
}

#[cfg(test)]
mod tests {
    use super::data_dir_under;
    use std::path::PathBuf;

    use kumochi_protocol::constants::DATA_DIR;

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
