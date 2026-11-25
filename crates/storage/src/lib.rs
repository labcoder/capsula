//! Protected filesystem operations: Windows' folder chain check, and dormant macOS shell-file
//! primitives.

#![cfg(any(target_os = "macos", windows))]

mod platform;

pub use platform::Untrusted;
#[cfg(target_os = "macos")]
pub use platform::macos::{
    AcquisitionError, AdmittedLock, AdmittedLog, ApplicationSupportError, Retention, ShellStorage,
    application_support_dir, hold_chain, open_shell_storage,
};
#[cfg(all(windows, any(test, feature = "test-support")))]
pub use platform::windows::{DaclEntry, dacl_entries, dacl_sddl, set_dacl};
#[cfg(windows)]
pub use platform::windows::{hold_chain, known_folder, owner_only_dir, user_sid};

/// The checks [`hold_chain`] is built from, for the brain's own protected operations.
#[cfg(windows)]
pub mod checked {
    pub use crate::platform::windows::{
        Aces, Kind, Security, Trusted, attributes, check, final_path_matches, open, open_checked,
        same_final_path,
    };
}
