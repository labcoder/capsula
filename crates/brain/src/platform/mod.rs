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

/// Serializes every process start that can inherit handles. A handle made inheritable for one
/// child leaks into any other child created while it is, so std `Command` and dependencies that
/// spawn (the browser launch) take this guard too.
pub fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SPAWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
pub use windows::{
    ChildPipes, Helper, Instance, Job, PipeServer, client_image, config_dir, create_owner_only,
    data_dir, free_space, launch_detached, links_unsupported, replace_file, server_image,
    session_id, session_lock_path, trust_roots,
};
#[cfg(windows)]
pub use windows::{Launch, Spawned, Stdio, spawn};
#[cfg(all(test, windows))]
pub(crate) use windows::{PipeClient, test_pair};
