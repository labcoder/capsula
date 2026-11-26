//! Windows: the application data roots, holding a folder chain, and the checks it's built from.

mod admit;
mod folders;
mod protected;

pub use admit::{SQLITE_FILES, admit_leaf};
pub use folders::{AppDataError, Today, local_app_data, roaming_app_data};

pub use protected::{
    Aces, Kind, Security, Trusted, attributes, check, check_leaf, final_path_matches, hold_chain,
    known_folder, new_owner_only_dir, open, open_checked, owner_only_dir, product_folder,
    same_final_path, user_sid,
};
#[cfg(any(test, feature = "test-support"))]
pub use protected::{DaclEntry, dacl_entries, dacl_sddl, set_dacl, set_junction};
