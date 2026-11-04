//! Win32 implementations of the brain's platform functions.

mod credential;
mod folders;
mod fs;
mod job;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "PR 4b's speech input owner calls it")
)]
mod library;
mod pipe;
mod process;
mod root_store;

pub(crate) use credential::WindowsCredentialStore;
pub use folders::{config_dir, data_dir, session_lock_path};
pub use fs::{free_space, links_unsupported, replace_file};
pub use job::{Helper, Job};
pub(super) use library::{
    Library, RawExport, missing_dependency, modules, open_locked, system_dir,
};
pub use pipe::{Instance, PipeServer, client_image, create_owner_only, session_id};
#[cfg(test)]
pub(crate) use pipe::{PipeClient, test_pair};
pub use process::launch_detached;
pub use root_store::trust_roots;
