//! Every name that changes if the product is renamed, in one place.
//!
//! Debug builds use separate names for everything a running install also uses (pipes, mutexes,
//! credentials, folders), so a dev build never talks to or overwrites an installed Capsula.

/// The name people see.
pub const DISPLAY_NAME: &str = "Capsula";

/// The shell's executable, `capsula.exe`. Cargo's `[[bin]]` names must match.
pub const SHELL_EXE: &str = "capsula.exe";

/// The brain's executable, `capsulad.exe`.
pub const BRAIN_EXE: &str = "capsulad.exe";

/// The folder under `%LOCALAPPDATA%` (logs, store, models) and `%APPDATA%` (config).
pub const DATA_DIR: &str = if cfg!(debug_assertions) {
    "Capsula Dev"
} else {
    "Capsula"
};

/// Log file name prefixes; the appender adds the date.
pub const BRAIN_LOG: &str = "capsulad";
/// See [`BRAIN_LOG`].
pub const SHELL_LOG: &str = "capsula";

/// Prefix of every per-session kernel object name: pipes and mutexes.
pub const OBJECT_PREFIX: &str = if cfg!(debug_assertions) {
    "capsula-dev"
} else {
    "capsula"
};

/// Credential Manager service for the user's credentials (accounts and keys, from slice 3).
pub const CREDENTIAL_SERVICE: &str = if cfg!(debug_assertions) {
    "Capsula Dev"
} else {
    "Capsula"
};

/// Credential Manager service for the rotating instance capability.
pub const INSTANCE_SERVICE: &str = if cfg!(debug_assertions) {
    "Capsula Dev instance"
} else {
    "Capsula instance"
};

/// The shell's single-instance mutex, per session.
pub const SHELL_MUTEX: &str = if cfg!(debug_assertions) {
    r"Local\capsula-dev-shell"
} else {
    r"Local\capsula-shell"
};

/// The Credential Manager account under [`INSTANCE_SERVICE`] holding the capability.
pub const INSTANCE_ACCOUNT: &str = "capability";

/// Dev builds only: set this to a file path and the brain records every pipe frame there, for
/// `cargo xtask pipe-dump`. Capabilities are zeroed in the recording.
pub const PIPE_TAP_ENV: &str = "CAPSULA_PIPE_TAP";
