//! Every name that changes if the product is renamed, in one place.
//!
//! Debug builds use separate names for everything a running install also uses (pipes, mutexes,
//! credentials, folders), so a dev build never talks to or overwrites an installed Kumochi.

/// The name people see.
pub const DISPLAY_NAME: &str = "Kumochi";

/// The assistant's name until the user picks their own.
pub const DEFAULT_ASSISTANT_NAME: &str = "Kumo";

/// The shell's executable, `kumochi.exe`. Cargo's `[[bin]]` names must match.
pub const SHELL_EXE: &str = "kumochi.exe";

/// The brain's executable, `kumochid.exe`.
pub const BRAIN_EXE: &str = "kumochid.exe";

/// The folder under `%LOCALAPPDATA%` (logs, store, models) and `%APPDATA%` (config).
pub const DATA_DIR: &str = if cfg!(debug_assertions) {
    "Kumochi Dev"
} else {
    "Kumochi"
};

/// The brain's SQLite store, in the [`DATA_DIR`] folder under `%LOCALAPPDATA%`.
pub const STORE_FILE: &str = "kumochi.db";

/// Log file name prefixes; the appender adds the date. Neither may start with the other, because
/// the appender prunes old files by prefix and would count the other process's logs.
pub const BRAIN_LOG: &str = "kumochi-brain";
/// See [`BRAIN_LOG`].
pub const SHELL_LOG: &str = "kumochi-shell";

/// Prefix of every per-session kernel object name: pipes and mutexes.
pub const OBJECT_PREFIX: &str = if cfg!(debug_assertions) {
    "kumochi-dev"
} else {
    "kumochi"
};

/// Credential Manager service for the user's credentials (accounts and keys, from slice 3).
pub const CREDENTIAL_SERVICE: &str = if cfg!(debug_assertions) {
    "Kumochi Dev"
} else {
    "Kumochi"
};

/// Credential Manager service for the rotating instance capability.
pub const INSTANCE_SERVICE: &str = if cfg!(debug_assertions) {
    "Kumochi Dev instance"
} else {
    "Kumochi instance"
};

/// The shell's single-instance mutex, per session.
pub const SHELL_MUTEX: &str = if cfg!(debug_assertions) {
    r"Local\kumochi-dev-shell"
} else {
    r"Local\kumochi-shell"
};

/// The Credential Manager account under [`INSTANCE_SERVICE`] holding the capability; the
/// session id is appended, since each session runs its own brain.
pub const INSTANCE_ACCOUNT: &str = "capability";

/// The `dwExtraInfo` the approved input-session harness puts on every input it injects, so the
/// shell's gate observer and the harness's own fixture tell it from the user's input.
pub const INPUT_SESSION_MARKER: usize = 0x4350_5331;

/// The shell's single focus-grant input at `OpenInput`.
pub const FOCUS_INPUT_MARKER: usize = 0x4350_5346;

/// `kumochi --gate-observer` only: posted to the dock's hit proxy (`proxy_hwnd` in the observer's
/// startup record), it asks the shell for one dock repaint with no input. `WM_APP + 0x10`.
pub const OBSERVER_REPAINT_MESSAGE: u32 = 0x8010;

/// Dev builds only: set this to a file path and the brain records every pipe frame there, for
/// `cargo xtask pipe-dump`. Capabilities are zeroed in the recording.
pub const PIPE_TAP_ENV: &str = "KUMOCHI_PIPE_TAP";

/// Dev builds only: set this to a fixture script and the brain's fixture engine plays it instead
/// of the built-in one.
pub const FIXTURE_ENV: &str = "KUMOCHI_FIXTURE";

#[cfg(test)]
mod tests {
    use super::{BRAIN_LOG, SHELL_LOG};

    #[test]
    fn log_prefixes_dont_overlap() {
        assert!(
            !BRAIN_LOG.starts_with(SHELL_LOG),
            "{BRAIN_LOG} starts with {SHELL_LOG}"
        );
        assert!(
            !SHELL_LOG.starts_with(BRAIN_LOG),
            "{SHELL_LOG} starts with {BRAIN_LOG}"
        );
    }
}
