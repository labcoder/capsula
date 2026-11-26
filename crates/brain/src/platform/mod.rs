//! Small, safe operating-system adapters. The native runtime is still Windows only.

pub mod credential;

/// A greeting read by its own authenticated pair. Only macOS has such a pair.
#[cfg(not(target_os = "macos"))]
pub(crate) enum BoundHello {}

#[cfg(not(target_os = "macos"))]
impl BoundHello {
    pub(crate) fn into_greeting(self) -> kumochi_protocol::message::Hello {
        match self {}
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::pair::BoundHello;

/// The inherited pair's descriptors, owned. Only macOS starts a brain on an inherited pair.
#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub enum InheritedFds {}

#[cfg(target_os = "macos")]
pub use macos::entry::InheritedFds;

/// Readies a parent-bound process before admission; only macOS starts one.
#[cfg(not(target_os = "macos"))]
pub(crate) fn harden() -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(target_os = "macos")]
pub(crate) use macos::entry::harden;

/// The instance item's session lock and item; only macOS has them.
#[cfg(not(target_os = "macos"))]
pub(crate) enum InstanceLock {}

#[cfg(not(target_os = "macos"))]
#[expect(dead_code, reason = "parent-bound startup is not wired yet")]
pub(crate) enum InstanceItem {}

#[cfg(not(target_os = "macos"))]
impl crate::startup::instance::InstanceStore for InstanceItem {
    type Error = std::convert::Infallible;

    fn add(&self, _: &[u8; 32]) -> Result<crate::startup::instance::Added, Self::Error> {
        match *self {}
    }

    fn inspect(&self) -> Result<(), Self::Error> {
        match *self {}
    }

    fn update(&self, _: &[u8; 32]) -> Result<(), Self::Error> {
        match *self {}
    }

    fn read(&self, _: &mut [u8; 32]) -> Result<(), Self::Error> {
        match *self {}
    }

    fn delete(&self) -> Result<(), Self::Error> {
        match *self {}
    }
}

#[cfg(not(target_os = "macos"))]
#[expect(dead_code, reason = "parent-bound startup is not wired yet")]
pub(crate) fn instance_custody() -> Result<InstanceLock, crate::startup::custody::CustodyError> {
    Err(crate::startup::custody::CustodyError::NotAdmitted)
}

#[cfg(all(test, not(target_os = "macos")))]
pub(crate) fn instance_custody_at(
    _: &std::path::Path,
) -> Result<InstanceLock, crate::startup::custody::CustodyError> {
    Err(crate::startup::custody::CustodyError::NotAdmitted)
}

/// A fresh Support folder for the instance item's native fixtures, and its cleanup; only macOS
/// has them.
#[cfg(all(test, not(target_os = "macos")))]
pub(crate) fn instance_fixture(_: &str) -> std::path::PathBuf {
    panic!("only macOS has the instance item's native fixtures")
}

#[cfg(all(test, not(target_os = "macos")))]
pub(crate) fn instance_fixture_clean(_: &std::path::Path, _: &[&str]) {
    panic!("only macOS has the instance item's native fixtures")
}

#[cfg(target_os = "macos")]
pub(crate) type InstanceLock = kumochi_storage::AdmittedLock;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::instance::lock_at as instance_custody_at;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::instance::tests::{
    clean as instance_fixture_clean, support as instance_fixture,
};
#[cfg(target_os = "macos")]
#[expect(unused_imports, reason = "parent-bound startup is not wired yet")]
pub(crate) use macos::instance::{KeychainItem as InstanceItem, instance_custody};

#[cfg(target_os = "macos")]
pub(crate) use macos::capture::{display_at, screen_capturer};
#[cfg(target_os = "macos")]
pub(crate) use macos::clock::{qpc_at, qpc_left};

#[cfg(all(windows, feature = "test-support"))]
pub use windows::{WindowProbe, WindowSample, WindowSession};

mod connected_shell;
#[cfg(not(windows))]
mod fs;
#[cfg(target_os = "macos")]
mod macos;
mod model_files;
pub(crate) mod native_workers;
mod pipe;
mod process;
mod protected;
mod runtime_admission;
mod session_lock;
#[cfg(any(not(target_os = "macos"), test))]
mod settings;
pub(crate) mod shell_process;
pub(crate) mod speech_wire;
mod tls_control;
mod trust_roots;
#[cfg(unix)]
mod unix;

pub(crate) use model_files::ModelFiles;
pub(crate) use shell_process::ShellProcess;

#[cfg(all(target_os = "macos", not(test)))]
pub(crate) use macos::settings::{read_settings, write_settings};
#[cfg(any(not(target_os = "macos"), test))]
pub(crate) use settings::{read_settings, write_settings};

/// Why the settings file couldn't be read: kinds only, never a path or the file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsRead {
    /// The folder or the file isn't one only this user controls.
    Untrusted,
    /// Reading failed, or the text isn't UTF-8.
    Read(std::io::ErrorKind),
}

/// How a settings replacement that didn't finish ended.
#[derive(Debug)]
pub(crate) enum SettingsWrite {
    /// The old file is untouched.
    Unchanged(std::io::Error),
    /// The new file may be the one named, but that it's durable, or still named, isn't known.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            dead_code,
            reason = "only the macOS adapter can leave a save unconfirmed"
        )
    )]
    Unconfirmed,
}

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
pub(super) use connected_shell::register_shell;
#[cfg(not(target_os = "macos"))]
pub(crate) const DATA_ROOT_ERROR: &str = "the local application data folder is unavailable";
#[cfg(target_os = "macos")]
pub(crate) const DATA_ROOT_ERROR: &str = "macOS Application Support folder is unavailable";
#[cfg(not(any(windows, target_os = "macos")))]
pub use fs::{config_dir, data_dir};
#[cfg(not(windows))]
pub use fs::{free_space, links_unsupported, replace_file};
#[cfg(unix)]
pub use unix::product_folder;
#[cfg(target_os = "macos")]
#[path = "macos/folders.rs"]
mod folders;
#[cfg(target_os = "macos")]
pub use folders::{config_dir, data_dir};
pub use pipe::Instance;
pub(crate) use pipe::{BridgeConnectError, ClientRefused};
#[cfg(not(windows))]
pub(crate) use pipe::{PipeClient, bridge_peer, client_image, open_bridge, shell_connection};
#[cfg(not(windows))]
pub use pipe::{PipeServer, create_owner_only, session_id};
#[cfg(all(target_os = "macos", debug_assertions, feature = "test-support"))]
pub use process::group_parent_fixture;
pub(crate) use process::spawn_if;
pub use process::{
    ChildPipes, Containment, ContainmentTier, Helper, Job, Launch, Spawned, Stdio, containment,
    launch_detached, spawn, system_env,
};
pub(crate) use protected::{Untrusted, is_unsupported};

#[cfg(not(windows))]
pub(crate) use protected::{
    check_model_final, create_model_dir, create_model_write, discard_model, hold_chain,
    hold_model_creation, mark, marker, model_file, model_final, open_model_write, owner_only_dir,
    private_tree, publish_model, remove_model_tree, remove_tree, unmark, validate_model_path,
};
pub use runtime_admission::{Mode, supports};
pub(crate) use runtime_admission::{require_mode, serve_speech};
pub use session_lock::{SessionLock, SessionLockError};
pub(crate) use tls_control::{
    ConnectCut, connect_owned_scope, connect_scope, connect_timeout_at, connect_with_cut,
};
#[cfg(test)]
pub(crate) use trust_roots::client_tls_from_roots;
#[cfg(not(any(windows, target_os = "macos")))]
pub use trust_roots::trust_roots;
pub use trust_roots::{NativeStream, TrustRoots, TrustRootsError};
pub(crate) use trust_roots::{
    NativeTlsError, TrustDiagnostics, connect_native_tls, native_client_tls, native_http_builder,
};
#[cfg(windows)]
pub(crate) use windows::DumpExclusion;
#[cfg(windows)]
pub(crate) use windows::{PipeClient, bridge_peer, open_bridge, shell_connection};

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
#[cfg(all(test, windows))]
pub(crate) use windows::local_app_data;
#[cfg(windows)]
pub use windows::managed_presence;
#[cfg(windows)]
pub(crate) use windows::ocr::open as open_ocr;
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
pub(crate) use windows::qpc_now;
#[cfg(all(windows, debug_assertions))]
pub use windows::raise_access_violation;
#[cfg(all(test, windows))]
pub(crate) use windows::{
    DaclEntry, dacl_entries, dacl_sddl, pause_after_check, paused, set_dacl, this_user,
};
#[cfg(windows)]
pub use windows::{
    PipeServer, client_image, client_process, config_dir, create_owner_only, data_dir,
    detached_command, free_space, has_exited, links_unsupported, process_image, product_folder,
    quiet_failures, replace_file, server_image, session_id, session_lock_path, trust_roots,
};
#[cfg(all(test, windows))]
pub(crate) use windows::{SpawnEdge, observe_next_spawn};
#[cfg(windows)]
pub(crate) use windows::{
    WINDOW_CAPTURE, display_at, qpc_age, qpc_left, screen_capturer, screen_elements,
    window_unchanged,
};
#[cfg(windows)]
pub(crate) use windows::{
    check_model_final, create_model_dir, create_model_write, discard_model, hold_chain,
    hold_model_creation, mark, marker, model_file, model_final, open_model_write, owner_only_dir,
    private_tree, profile_dir, publish_model, remove_model_tree, remove_tree, unmark,
    validate_model_path,
};

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

/// The Windows deadline stamp for a budget that starts now, keeping the existing QPC encoding.
#[cfg(windows)]
pub(crate) fn qpc_at(budget: std::time::Duration) -> Option<u64> {
    windows::qpc_at(std::time::Instant::now().checked_add(budget)?)
}

/// No system-wide deadline clock is implemented here.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn qpc_at(_budget: std::time::Duration) -> Option<u64> {
    None
}

/// How long until a deadline stamp from `qpc_at`, so never known here.
#[cfg(not(any(windows, target_os = "macos")))]
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

/// The monitor that covers a captured display's bounds now. Only Windows and macOS capture a
/// display, so there's none.
#[cfg(not(any(windows, target_os = "macos")))]
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

/// Without a native probe, no managed-policy source can be ruled out.
#[cfg(not(windows))]
#[must_use]
pub fn managed_presence() -> ManagedPresence {
    ManagedPresence {
        files: [Presence::Indeterminate; 3],
        machine: Presence::Indeterminate,
        user: Presence::Indeterminate,
    }
}

#[cfg(all(test, windows))]
pub(crate) use windows::test_pair;

#[cfg(test)]
#[test]
fn missing_data_root_diagnostics_name_the_platform_source() {
    let expected = if cfg!(target_os = "macos") {
        "macOS Application Support folder is unavailable"
    } else {
        "the local application data folder is unavailable"
    };
    assert_eq!(
        crate::store::StoreError::NoLocalAppData.to_string(),
        expected
    );
    assert_eq!(
        crate::logging::LoggingError::NoLocalAppData.to_string(),
        expected
    );
}
