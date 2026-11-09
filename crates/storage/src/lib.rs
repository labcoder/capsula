//! Protected filesystem operations and dormant macOS shell-file primitives.

mod platform;

#[cfg(not(windows))]
pub use platform::Untrusted;
#[cfg(target_os = "macos")]
pub use platform::macos::{
    AcquisitionError, AdmittedLock, AdmittedLog, Retention, ShellStorage, hold_chain,
    open_shell_storage,
};

#[cfg(all(windows, any(test, feature = "test-support")))]
pub use platform::windows::{
    DaclEntry, dacl_entries, dacl_sddl, final_path_matches, pause_after_check, set_dacl, this_user,
};
#[cfg(windows)]
pub use platform::windows::{
    Untrusted, hold_chain, hold_file, mark, marker, owner_only_dir, private_tree, remove_tree,
    unmark, user_sid,
};
