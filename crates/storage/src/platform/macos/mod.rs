mod protected;
mod shell_files;

pub use protected::hold_chain;
pub use shell_files::{
    AcquisitionError, AdmittedLock, AdmittedLog, Retention, ShellStorage, open_shell_storage,
};
