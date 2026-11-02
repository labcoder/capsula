//! Win32 implementations of the brain's platform functions.

mod job;
mod pipe;
mod process;

pub use job::{Helper, Job};
pub use pipe::{Instance, PipeServer, client_image, create_owner_only, session_id};
#[cfg(test)]
pub(crate) use pipe::{PipeClient, test_pair};
pub use process::launch_detached;
