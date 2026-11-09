mod protected;
mod sid;

#[cfg(any(test, feature = "test-support"))]
pub use protected::{
    DaclEntry, dacl_entries, dacl_sddl, final_path_matches, pause_after_check, set_dacl, this_user,
};
pub use protected::{
    Untrusted, hold_chain, hold_file, mark, marker, owner_only_dir, private_tree, remove_tree,
    unmark,
};
pub use sid::user_sid;
