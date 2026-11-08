//! Protected filesystem operations, with admission-only support on macOS.

mod platform;

#[cfg(not(windows))]
pub use platform::Untrusted;
#[cfg(target_os = "macos")]
pub use platform::macos::hold_chain;

#[cfg(all(windows, any(test, feature = "test-support")))]
pub use platform::windows::{
    DaclEntry, dacl_entries, dacl_sddl, final_path_matches, pause_after_check, set_dacl, this_user,
};
#[cfg(windows)]
pub use platform::windows::{
    Untrusted, hold_chain, hold_file, mark, marker, owner_only_dir, private_tree, remove_tree,
    unmark, user_sid,
};
