//! The operating system behind small, safe functions. Windows only for now.

pub mod credential;

#[cfg(all(windows, feature = "test-support"))]
pub use windows::{WindowProbe, WindowSample, WindowSession};

mod connected_shell;
mod session_lock;
mod trust_roots;

pub(crate) use connected_shell::connected_shell_pid;
#[cfg(windows)]
pub(super) use connected_shell::register_shell;
pub use session_lock::{SessionLock, SessionLockError};
pub use trust_roots::{TrustRoots, TrustRootsError};
#[cfg(windows)]
pub(crate) use windows::shell_connection;

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
pub(crate) mod sherpa;

#[cfg(all(windows, debug_assertions))]
pub(crate) use windows::call_throwing_export;
#[cfg(all(test, windows))]
pub(crate) use windows::fake_claude::FAKE_CLAUDE;
#[cfg(all(windows, debug_assertions))]
pub use windows::raise_access_violation;
#[cfg(windows)]
pub use windows::{
    ChildPipes, Helper, Instance, Job, PipeServer, client_image, client_process, config_dir,
    create_owner_only, data_dir, free_space, has_exited, launch_detached, links_unsupported,
    process_image, quiet_failures, replace_file, server_image, session_id, session_lock_path,
    trust_roots,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{
    DaclEntry, dacl_entries, dacl_sddl, pause_after_check, set_dacl, this_user,
};
#[cfg(windows)]
pub use windows::{Launch, Spawned, Stdio, spawn};
#[cfg(windows)]
pub(crate) use windows::{
    Untrusted, hold_chain, local_app_data, mark, marker, owner_only_dir, private_tree, profile_dir,
    remove_tree, unmark,
};
#[cfg(windows)]
pub use windows::{login_browser, managed_presence, system_env};
#[cfg(windows)]
pub(crate) use windows::{screen_capturer, screen_elements};

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
