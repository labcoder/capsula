//! macOS candidates for shell-owned platform resources.

mod folders;
mod instance;

pub use folders::support_dir;
pub(crate) use instance::try_acquire_instance_lock;
