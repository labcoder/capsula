//! macOS candidates for shell-owned platform resources.

mod instance;

pub(crate) use instance::try_acquire_instance_lock;
pub use kumochi_storage::application_support_dir as support_dir;
