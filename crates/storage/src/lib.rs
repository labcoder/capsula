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
    AcquisitionError, AdmittedLock, AdmittedLog, ApplicationSupportError, HeldFolder, INSTANCE_LEN,
    InstanceItem, ItemError, ItemQuery, Op, Product, ProductFolder, Retention, Role, Seal,
    SettingsWriteError, ShellStorage, Support, admit_support, application_support_dir, check_named,
    check_product_folder, create_private_dir, create_private_file, discard_entry, hold_chain,
    hold_private_chain, instance_edition, keep_sealed, open_private_write, open_sealed,
    open_shell_storage, owner_only_dir, private_tree, read_settings, remove_private_tree,
    replace_settings, seal_and_publish, status, validate_private_path,
};
#[cfg(windows)]
pub use platform::windows::{
    AppDataError, SQLITE_FILES, Today, admit_leaf, check_leaf, hold_chain, known_folder,
    local_app_data, new_owner_only_dir, owner_only_dir, product_folder, roaming_app_data, user_sid,
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
