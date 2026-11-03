//! Win32 implementations of the brain's platform functions.

mod credential;
mod fs;
mod job;
mod pipe;
mod process;
mod root_store;

pub(crate) use credential::WindowsCredentialStore;
pub use fs::{free_space, links_unsupported, replace_file};
pub use job::{Helper, Job};
pub use pipe::{Instance, PipeServer, client_image, create_owner_only, session_id};
#[cfg(test)]
pub(crate) use pipe::{PipeClient, test_pair};
pub use process::launch_detached;
pub use root_store::trust_roots;
