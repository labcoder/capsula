//! Windows: holding a folder chain and the checks it's built from.

mod protected;

pub use protected::{
    Aces, Kind, Security, Trusted, attributes, check, final_path_matches, hold_chain, known_folder,
    open, open_checked, owner_only_dir, same_final_path, user_sid,
};
#[cfg(any(test, feature = "test-support"))]
pub use protected::{DaclEntry, dacl_entries, dacl_sddl, set_dacl};
