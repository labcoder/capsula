mod folders;
mod private;
mod protected;
mod settings;
mod shell_files;

pub use folders::{ApplicationSupportError, application_support_dir};
pub use private::{owner_only_dir, private_tree};
pub use protected::{check_product_folder, hold_chain};
pub use settings::{SettingsWriteError, read_settings, replace_settings};
pub use shell_files::{
    AcquisitionError, AdmittedLock, AdmittedLog, HeldFolder, Product, ProductFolder, Retention,
    ShellStorage, Support, admit_support, open_shell_storage,
};
