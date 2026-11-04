//! Win32 implementations of the brain's platform functions.

mod credential;
mod folders;
mod fs;
mod job;
mod pipe;
mod process;
mod root_store;
mod spawn;

pub(crate) use credential::WindowsCredentialStore;
pub use folders::{config_dir, data_dir, session_lock_path};
pub use fs::{free_space, links_unsupported, replace_file};
pub use job::{ChildPipes, Helper, Job};
pub use pipe::{Instance, PipeServer, client_image, create_owner_only, session_id};
#[cfg(test)]
pub(crate) use pipe::{PipeClient, test_pair};
pub use process::launch_detached;
pub use root_store::trust_roots;
pub use spawn::{Launch, Spawned, Stdio, spawn};
