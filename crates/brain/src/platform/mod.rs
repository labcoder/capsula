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
#[expect(unsafe_code, reason = "calls into the pinned sherpa-onnx C API")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "PR 4b's speech worker calls it")
)]
pub(crate) mod sherpa;

#[cfg(windows)]
pub use windows::{
    ChildPipes, Helper, Instance, Job, PipeServer, client_image, config_dir, create_owner_only,
    data_dir, free_space, launch_detached, links_unsupported, replace_file, server_image,
    session_id, session_lock_path, trust_roots,
};
#[cfg(windows)]
pub use windows::{Launch, Spawned, Stdio, spawn};
#[cfg(windows)]
pub use windows::{managed_presence, system_env};

/// Whether one managed-policy source exists, by existence only. Only a proven-missing source is
/// `Absent`; access denied and every other unresolved result are `Indeterminate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Proven missing.
    Absent,
    /// It exists.
    Present,
    /// Couldn't be ruled out; counts as present.
    Indeterminate,
}

/// Claude Code's endpoint-managed policy sources (macOS counterpart: /Library/Application
/// Support/ClaudeCode, unverified).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedPresence {
    /// `managed-settings.json`, `managed-settings.d` and `managed-mcp.json`.
    pub files: [Presence; 3],
    /// The machine's `Settings` policy value.
    pub machine: Presence,
    /// The user's `Settings` policy value.
    pub user: Presence,
}

impl ManagedPresence {
    /// Every source.
    pub fn all(&self) -> [Presence; 5] {
        let [settings, folder, mcp] = self.files;
        [settings, folder, mcp, self.machine, self.user]
    }
}
#[cfg(all(test, windows))]
pub(crate) use windows::{PipeClient, test_pair};
