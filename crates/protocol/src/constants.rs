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

/// Log file name prefixes; the appender adds the date. Neither may start with the other, because
/// the appender prunes old files by prefix and would count the other process's logs.
pub const BRAIN_LOG: &str = "capsula-brain";
/// See [`BRAIN_LOG`].
pub const SHELL_LOG: &str = "capsula-shell";

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
