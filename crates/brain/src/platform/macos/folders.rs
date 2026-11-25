use std::path::PathBuf;

use kumochi_protocol::constants;
use kumochi_storage::application_support_dir;

/// The per-user data candidate beneath the OS's Application Support folder.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    Some(application_support_dir().ok()?.join(constants::DATA_DIR))
}

/// Configuration and data share the same per-user Application Support candidate.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    data_dir()
}
