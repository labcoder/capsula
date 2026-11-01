//! The operating system behind small, safe functions. Windows only for now.

use std::ffi::OsString;
use std::path::PathBuf;

use capsula_protocol::constants;

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the brain's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub use windows::{
    Helper, Instance, Job, PipeServer, client_image, create_owner_only, launch_detached, session_id,
};

/// The per-user folder for local data (logs, the store): `%LOCALAPPDATA%\\<product>`.
///
/// None unless `%LOCALAPPDATA%` is an absolute path, so a bad value never puts data under the
/// working directory.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    data_dir_under(std::env::var_os("LOCALAPPDATA"))
}

fn data_dir_under(base: Option<OsString>) -> Option<PathBuf> {
    let base = PathBuf::from(base?);
    base.is_absolute().then(|| base.join(constants::DATA_DIR))
}

#[cfg(test)]
mod tests {
    use super::data_dir_under;

    #[test]
    fn the_data_folder_needs_an_absolute_base() {
        for bad in ["", ".", "AppData", r"\Users\me", "C:AppData"] {
            assert_eq!(data_dir_under(Some(bad.into())), None, "{bad:?}");
        }
        assert_eq!(data_dir_under(None), None);
        let good = data_dir_under(Some(r"C:\Users\me\AppData\Local".into())).expect("absolute");
        assert!(good.is_absolute());
        assert!(good.ends_with(capsula_protocol::constants::DATA_DIR));
    }
}
