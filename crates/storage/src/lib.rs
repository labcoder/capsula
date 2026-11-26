//! Protected filesystem operations: Windows' application data roots and folder chain check, and
//! dormant macOS shell-file primitives.

#![cfg(any(target_os = "macos", windows))]

mod platform;
#[cfg(all(test, windows))]
#[path = "../../../tests/support/scratch.rs"]
#[expect(
    dead_code,
    clippy::print_stderr,
    reason = "the shared fixture serves several crates and reports cleanup failures during unwind"
)]
mod scratch;

pub use platform::Untrusted;
#[cfg(target_os = "macos")]
pub use platform::macos::{
    AcquisitionError, AdmittedLock, AdmittedLog, ApplicationSupportError, GuardError, HeldFolder,
    Product, ProductFolder, Retention, SetupGuard, ShellStorage, Support, admit_support,
    application_support_dir, hold_chain, open_shell_storage,
};
#[cfg(windows)]
pub use platform::windows::{
    AppDataError, Child, GuardError, SQLITE_FILES, SetupGuard, Today, acquire_setup_guard,
    admit_leaf, check_leaf, hold_chain, hold_product_folder, known_folder, local_app_data,
    new_owner_only_dir, owner_only_dir, product_folder, profile_root, roaming_app_data, user_sid,
};
#[cfg(all(windows, any(test, feature = "test-support")))]
pub use platform::windows::{DaclEntry, dacl_entries, dacl_sddl, set_dacl, set_junction};

/// The checks [`hold_chain`] is built from, for the brain's own protected operations.
#[cfg(windows)]
pub mod checked {
    pub use crate::platform::windows::{
        Aces, Kind, Security, Trusted, attributes, check, final_path_matches, open, open_checked,
        same_final_path,
    };
}
