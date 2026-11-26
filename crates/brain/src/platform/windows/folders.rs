//! Per-user folders, from the resolver the shell shares: today's route in an ordinary build, and
//! in Gate and debug builds the override pair, admitted before the first write, or the Known
//! Folders.

use std::io;
use std::path::{Path, PathBuf};

use kumochi_protocol::constants;
use kumochi_storage::{Today, local_app_data, roaming_app_data};

/// The per-user folder for local data (logs, the store): this product's folder in the local
/// application data root. None when the resolver refuses, so a bad value never puts data under
/// the working directory.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    Some(
        local_app_data(Today::Environment)
            .ok()?
            .join(constants::DATA_DIR),
    )
}

/// The empty account-session lock file under the local data folder, separate for dev builds.
///
/// None when the resolver refuses, by the same rule as [`data_dir`].
#[must_use]
pub fn session_lock_path() -> Option<PathBuf> {
    Some(data_dir()?.join(constants::SESSION_LOCK))
}

/// The per-user folder for settings, which roam: this product's folder in the roaming
/// application data root, by the same rule.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    Some(roaming_app_data().ok()?.join(constants::DATA_DIR))
}

/// Creates the product folder at `path` owner-only when it's missing, and checks an existing one
/// without changing it.
///
/// # Errors
/// Fails when it can't be created, or an existing one isn't a trusted plain folder.
pub fn product_folder(path: &Path) -> io::Result<()> {
    kumochi_storage::product_folder(path)
        .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))
}
