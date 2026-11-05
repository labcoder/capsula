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

/// The empty account-session lock file, in the [`DATA_DIR`] folder under `%LOCALAPPDATA%`.
pub const SESSION_LOCK: &str = "session.lock";

/// The user's settings, in the [`DATA_DIR`] folder under `%APPDATA%`.
pub const CONFIG_FILE: &str = "config.toml";

/// Log file name prefixes; the appender adds the date. Neither may start with the other, because
/// the appender prunes old files by prefix and would count the other process's logs.
pub const BRAIN_LOG: &str = "kumochi-brain";
/// See [`BRAIN_LOG`]; the screen worker's own file.
pub const SCREEN_LOG: &str = "kumochi-screen";
/// See [`BRAIN_LOG`]; the speech worker's own file.
pub const SPEECH_LOG: &str = "kumochi-speech";
/// See [`BRAIN_LOG`]; Claude Code's MCP bridge, in the launch's own folder.
pub const BRIDGE_LOG: &str = "kumochi-bridge";
/// See [`BRAIN_LOG`].
pub const SHELL_LOG: &str = "kumochi-shell";

/// The pipe Claude Code's MCP bridge connects back on, after [`OBJECT_PREFIX`], the session and a
/// dash, and before the launch's value.
pub const BRIDGE_PIPE: &str = "claude-bridge";

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

/// The shell's single-instance lock file, `<SHELL_LOCK>-<session id>.lock` in the [`DATA_DIR`]
/// folder under `%LOCALAPPDATA%`: one shell per Windows session.
pub const SHELL_LOCK: &str = if cfg!(debug_assertions) {
    "kumochi-dev-shell"
} else {
    "kumochi-shell"
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

/// Kumochi's own Claude Code configuration folder (`CLAUDE_CONFIG_DIR`), in the [`DATA_DIR`]
/// folder under `%LOCALAPPDATA%`. Claude Code keeps its sign-in there; Kumochi never opens it.
pub const CLAUDE_CODE_DIR: &str = "claude-code";
/// The folder for each Claude Code launch's owned folders, in the [`DATA_DIR`] folder under
/// `%LOCALAPPDATA%`.
pub const ENGINE_DIR: &str = "engine";
/// The MCP server name Claude Code sees, so its tools are `mcp__kumochi__*`.
pub const MCP_SERVER_NAME: &str = "kumochi";
/// The Credential Manager account under [`CREDENTIAL_SERVICE`] holding the Anthropic API key.
pub const ANTHROPIC_KEY_ACCOUNT: &str = "anthropic-key";
/// The account the shell stages a pasted key in, for the brain to check and move.
pub const ANTHROPIC_STAGING_ACCOUNT: &str = "anthropic-key-staged";
/// Where Claude Code's native installer puts `claude.exe`, under `%USERPROFILE%`, in the order they're tried.
pub const CLAUDE_CODE_PATHS: &[&str] = &[r".local\bin\claude.exe"];
/// Dev builds only: set this to a file path and the brain records every pipe frame there, for
/// `cargo xtask pipe-dump`. Capabilities are zeroed in the recording.
pub const PIPE_TAP_ENV: &str = "KUMOCHI_PIPE_TAP";

/// Dev builds only: set this to a fixture script and the brain's fixture engine plays it instead
/// of the built-in one.
pub const FIXTURE_ENV: &str = "KUMOCHI_FIXTURE";
/// Dev builds only: fail the screen worker's next request.
pub const SCREEN_FAULT_ENV: &str = "KUMOCHI_SCREEN_FAULT";

/// Dev builds only: the most model requests the brain may send, set by `cargo xtask live`.
/// A value that isn't a count allows none.
pub const LIVE_BUDGET_ENV: &str = "KUMOCHI_LIVE_BUDGET";

/// Dev builds only: the uncovered backlog, in estimated tokens, at which a thread compacts after
/// a run, lowered for the live check. A value that isn't a count keeps the default.
pub const COMPACT_AT_ENV: &str = "KUMOCHI_COMPACT_AT";

#[cfg(test)]
mod tests {
    use super::{BRAIN_LOG, SCREEN_LOG, SHELL_LOG, SPEECH_LOG};

    #[test]
    fn log_prefixes_dont_overlap() {
        let logs = [BRAIN_LOG, SCREEN_LOG, SHELL_LOG, SPEECH_LOG];
        for (index, one) in logs.iter().enumerate() {
            for other in &logs[index + 1..] {
                assert!(
                    !one.starts_with(other) && !other.starts_with(one),
                    "{one} and {other} overlap"
                );
            }
        }
    }
}
