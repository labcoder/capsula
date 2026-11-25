//! Protected filesystem operations and dormant macOS shell-file primitives.

#![cfg(target_os = "macos")]

mod platform;

pub use platform::Untrusted;
pub use platform::macos::{
    AcquisitionError, AdmittedLock, AdmittedLog, Retention, ShellStorage, hold_chain,
    open_shell_storage,
};
