//! The operating system behind small, safe functions. Windows only for now.

pub mod credential;

#[cfg(all(windows, feature = "test-support"))]
pub use windows::{WindowProbe, WindowSample, WindowSession};

mod connected_shell;
mod model_files;
mod session_lock;
mod shell_process;
mod trust_roots;

pub(crate) use model_files::ModelFiles;
pub(crate) use shell_process::ShellProcess;

#[cfg(all(test, windows))]
pub(crate) use std::os::windows::fs::{
    symlink_dir as test_symlink_dir, symlink_file as test_symlink_file,
};

#[cfg(test)]
pub(crate) fn test_is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) use connected_shell::connected_shell_pid;
#[cfg(windows)]
pub(super) use connected_shell::register_shell;
pub use session_lock::{SessionLock, SessionLockError};
pub(crate) use trust_roots::native_trust_supported;
#[cfg(not(windows))]
pub use trust_roots::trust_roots;
pub use trust_roots::{TrustRoots, TrustRootsError};
#[cfg(windows)]
pub(crate) use windows::DumpExclusion;
#[cfg(windows)]
pub(crate) use windows::shell_connection;

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the brain's only unsafe code lives here"
)]
mod windows;

/// Standard output as an unbuffered file: a duplicate of its handle, so what's written there
/// passes through no buffer the standard library keeps and never zeroes.
#[cfg(windows)]
pub(crate) fn raw_stdout() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle as _;
    Ok(std::io::stdout().as_handle().try_clone_to_owned()?.into())
}

/// Standard output as an unbuffered file: a duplicate of its descriptor, so what's written there
/// passes through no buffer the standard library keeps and never zeroes.
#[cfg(unix)]
pub(crate) fn raw_stdout() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd as _;
    Ok(std::io::stdout().as_fd().try_clone_to_owned()?.into())
}

static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes every process start that can inherit handles. A handle made inheritable for one
/// child leaks into any other child created while it is, so std `Command` and dependencies that
/// spawn (the browser launch) take this guard too.
pub fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
pub(crate) fn spawn_lock_until(
    remaining: &dyn Fn() -> Option<std::time::Duration>,
) -> std::io::Result<std::sync::MutexGuard<'static, ()>> {
    loop {
        let guard = match SPAWN.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        let left = remaining().ok_or(std::io::ErrorKind::Interrupted)?;
        if let Some(guard) = guard {
            return Ok(guard);
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(5)));
    }
}

#[cfg(windows)]
#[expect(unsafe_code, reason = "calls into the pinned sherpa-onnx C API")]
pub(crate) mod sherpa;

#[cfg(all(windows, debug_assertions))]
pub(crate) use windows::call_throwing_export;
#[cfg(all(test, windows))]
pub(crate) use windows::fake_claude::FAKE_CLAUDE;
#[cfg(windows)]
pub(crate) use windows::ocr::open as open_ocr;
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
pub(crate) use windows::qpc_now;
#[cfg(all(windows, debug_assertions))]
pub use windows::raise_access_violation;
#[cfg(windows)]
pub(crate) use windows::spawn_if;
#[cfg(windows)]
pub use windows::{
    ChildPipes, Helper, Instance, Job, PipeServer, client_image, client_process, config_dir,
    create_owner_only, data_dir, detached_command, free_space, has_exited, launch_detached,
    links_unsupported, process_image, quiet_failures, replace_file, server_image, session_id,
    session_lock_path, trust_roots,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{
    DaclEntry, dacl_entries, dacl_sddl, pause_after_check, paused, set_dacl, this_user,
};
#[cfg(windows)]
pub use windows::{Launch, Spawned, Stdio, spawn};
#[cfg(all(test, windows))]
pub(crate) use windows::{SpawnEdge, observe_next_spawn};
#[cfg(windows)]
pub(crate) use windows::{
    Untrusted, check_model_final, create_model_dir, create_model_write, discard_model, hold_chain,
    hold_model_creation, local_app_data, mark, marker, model_file, model_final, open_model_write,
    owner_only_dir, private_tree, profile_dir, publish_model, remove_model_tree, remove_tree,
    unmark, validate_model_path,
};
#[cfg(windows)]
pub(crate) use windows::{
    WINDOW_CAPTURE, display_at, qpc_age, qpc_at, qpc_left, screen_capturer, screen_elements,
    window_unchanged,
};
#[cfg(windows)]
pub use windows::{managed_presence, system_env};

/// Whether this platform captures one window alone. No platform but Windows does yet.
// shortcut: macOS will capture with ScreenCaptureKit; true once that adapter lands.
#[cfg(not(windows))]
pub(crate) const WINDOW_CAPTURE: bool = false;

/// How long ago a shell's input stamp was taken. Only Windows stamps input with a clock the brain
/// reads, so elsewhere no age is known.
// shortcut: off Windows a screen hold always goes voice only; read macOS's clock here once its
// shell sends screen holds.
#[cfg(not(windows))]
pub(crate) fn qpc_age(_t_qpc: u64) -> Option<std::time::Duration> {
    None
}

/// A capture deadline's stamp for the screen worker, on the clock `qpc_age` reads, so none here.
// shortcut: off Windows the worker refuses every ticket; stamp with macOS's clock once it captures.
#[cfg(not(windows))]
pub(crate) fn qpc_at(_until: std::time::Instant) -> Option<u64> {
    None
}

/// How long until a deadline stamp from `qpc_at`, so never known here.
#[cfg(not(windows))]
pub(crate) fn qpc_left(_stamp: u64) -> Option<std::time::Duration> {
    None
}

/// Whether a captured window is still the one its capture resolved. No platform but Windows
/// captures windows yet, so none is ever unchanged.
// shortcut: macOS will check the window here.
#[cfg(not(windows))]
pub(crate) fn window_unchanged(_target: &crate::screen::Target) -> bool {
    false
}

/// The monitor that covers a captured display's bounds now. No platform but Windows captures a
/// display yet, so there's none.
// shortcut: macOS will read its display here.
#[cfg(not(windows))]
pub(crate) fn display_at(
    _bounds: kumochi_protocol::geometry::PhysicalRect,
) -> Option<(kumochi_protocol::geometry::PhysicalRect, u32)> {
    None
}

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
