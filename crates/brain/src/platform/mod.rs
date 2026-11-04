//! The operating system behind small, safe functions. Windows only for now.

pub mod credential;

mod session_lock;
mod trust_roots;

pub use session_lock::{SessionLock, SessionLockError};
pub use trust_roots::{TrustRoots, TrustRootsError};

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the brain's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub use windows::{
    ChildPipes, Helper, Instance, Job, PipeServer, client_image, config_dir, create_owner_only,
    data_dir, free_space, launch_detached, links_unsupported, replace_file, session_id,
    session_lock_path, trust_roots,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{PipeClient, test_pair};
