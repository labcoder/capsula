mod folders;
mod instance;
mod models;
mod private;
mod protected;
mod settings;
mod shell_files;

pub use crate::platform::unix::AcquisitionError;
pub use folders::{ApplicationSupportError, application_support_dir};
pub use instance::{
    INSTANCE_LEN, InstanceItem, ItemError, ItemQuery, Op, Role, instance_edition, status,
};
pub use models::{
    Seal, check_named, create_private_dir, create_private_file, discard_entry, hold_private_chain,
    keep_sealed, open_private_write, open_sealed, remove_private_tree, seal_and_publish,
    validate_private_path,
};
pub use private::{owner_only_dir, private_tree};
pub use protected::{check_product_folder, hold_chain};
pub use settings::{SettingsWriteError, read_settings, replace_settings};
pub use shell_files::{
    AdmittedLock, AdmittedLog, HeldFolder, Product, ProductFolder, Retention, ShellStorage,
    Support, admit_support, open_shell_storage,
};
