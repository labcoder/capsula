//! The per-user data folder, from the resolver the brain shares: today's route in an ordinary
//! build, and in Gate and debug builds the override pair, admitted before the first write, or
//! the Known Folder.

use std::path::PathBuf;

use kumochi_protocol::constants;
use kumochi_storage::{Today, local_app_data};

/// This product's folder in the local application data root, unless the resolver refuses.
pub fn data_dir() -> Option<PathBuf> {
    Some(
        local_app_data(Today::Environment)
            .ok()?
            .join(constants::DATA_DIR),
    )
}
