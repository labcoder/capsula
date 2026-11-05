//! Win32 implementations of the brain's platform functions.

mod credential;
mod folders;
mod fs;
mod job;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "PR 4b's speech worker calls it")
)]
mod library;
mod pipe;
mod policy;
mod process;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "PR 4b's speech worker stages its natives")
)]
mod protected;
mod root_store;
mod spawn;

pub(crate) use credential::WindowsCredentialStore;
pub use folders::{config_dir, data_dir, session_lock_path};
pub use fs::{free_space, links_unsupported, replace_file};
pub use job::{ChildPipes, Helper, Job};
pub(super) use library::{
    Library, RawExport, missing_dependency, modules, open_locked, system_dir,
};
pub use pipe::{Instance, PipeServer, client_image, create_owner_only, server_image, session_id};
#[cfg(test)]
pub(crate) use pipe::{PipeClient, test_pair};
pub use policy::{managed_presence, system_env};
pub use process::launch_detached;
pub(super) use protected::{Untrusted, hold_chain, hold_file, local_app_data, owner_only_dir};
#[cfg(test)]
pub(super) use protected::{dacl_sddl, final_path_matches};
pub use root_store::trust_roots;
pub use spawn::{Launch, Spawned, Stdio, spawn};
