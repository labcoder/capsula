//! Every name that changes if the product is renamed, in one place.
//!
//! Debug builds use separate names for everything a running install also uses (pipes, mutexes,
//! credentials, folders), so a dev build never talks to or overwrites an installed Kumochi.

/// The name people see.
pub const DISPLAY_NAME: &str = "Kumochi";

/// The fixed product identity sent on GitHub requests.
pub const HTTP_USER_AGENT: &str = "Kumochi";

/// The registered Kumochi Dev application's public client ID; release registration is pending.
pub const GITHUB_CLIENT_ID: Option<&str> = if cfg!(debug_assertions) {
    Some("Iv23liKFSP0RTvDt9XGM")
} else {
    None
};

/// The assistant's name until the user picks their own.
pub const DEFAULT_ASSISTANT_NAME: &str = "Kumo";

/// The shell's executable, `kumochi.exe`. Cargo's `[[bin]]` names must match.
pub const SHELL_EXE: &str = "kumochi.exe";

/// The brain's executable, `kumochid.exe`.
pub const BRAIN_EXE: &str = "kumochid.exe";

/// The folder under `%LOCALAPPDATA%` (logs, store, models) and `%APPDATA%` (config).
pub const DATA_DIR: &str = ProductIdentity::ACTIVE.data_dir;

/// Stored namespaces, selectable independently of the caller's build profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProductIdentity {
    /// The data and configuration folder.
    pub data_dir: &'static str,
    /// The account credential service.
    pub credential_service: &'static str,
    /// The instance capability service.
    pub instance_service: &'static str,
    /// The per-session kernel object prefix.
    pub object_prefix: &'static str,
    /// The shell's single-instance lock prefix.
    pub shell_lock: &'static str,
}

impl ProductIdentity {
    /// Installed product namespaces, even when selected by debug-built tooling.
    pub const RELEASE: Self = Self {
        data_dir: "Kumochi",
        credential_service: "Kumochi",
        instance_service: "Kumochi instance",
        object_prefix: "kumochi",
        shell_lock: "kumochi-shell",
    };

    /// Isolated development namespaces.
    pub const DEV: Self = Self {
        data_dir: "Kumochi Dev",
        credential_service: "Kumochi Dev",
        instance_service: "Kumochi Dev instance",
        object_prefix: "kumochi-dev",
        shell_lock: "kumochi-dev-shell",
    };

    /// The gate tools' own namespaces (OSC-680), so a gate run never touches either real identity.
    /// Only the `gate-identity` feature selects them, and only a gate tool's command line enables
    /// it: never a manifest, a default or another crate.
    pub const GATE: Self = Self {
        data_dir: "Kumochi Gate",
        credential_service: "Kumochi Gate",
        instance_service: "Kumochi Gate instance",
        object_prefix: "kumochi-gate",
        shell_lock: "kumochi-gate-shell",
    };

    /// Names used by the running app, unchanged from the profile-dependent aliases.
    pub const ACTIVE: Self = if cfg!(feature = "gate-identity") {
        Self::GATE
    } else if cfg!(debug_assertions) {
        Self::DEV
    } else {
        Self::RELEASE
    };
}

/// The brain's SQLite store, in the [`DATA_DIR`] folder under `%LOCALAPPDATA%`.
pub const STORE_FILE: &str = "kumochi.db";

/// The brain's credential authority, beside [`STORE_FILE`]; the gate tools' cleanup reads it too.
pub const AUTHORITY_FILE: &str = "credentials.db";

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
pub const OBJECT_PREFIX: &str = ProductIdentity::ACTIVE.object_prefix;

/// Credential Manager service for the user's credentials (accounts and keys, from slice 3).
pub const CREDENTIAL_SERVICE: &str = ProductIdentity::ACTIVE.credential_service;

/// Credential Manager service for the rotating instance capability.
pub const INSTANCE_SERVICE: &str = ProductIdentity::ACTIVE.instance_service;

/// The shell's single-instance lock file, `<SHELL_LOCK>-<session id>.lock` in the [`DATA_DIR`]
/// folder under `%LOCALAPPDATA%`: one shell per Windows session.
pub const SHELL_LOCK: &str = ProductIdentity::ACTIVE.shell_lock;

/// The Credential Manager account under [`INSTANCE_SERVICE`] holding the capability; the
/// session id is appended, since each session runs its own brain.
pub const INSTANCE_ACCOUNT: &str = "capability";

/// The vault's maximum number of chunks in one record slot.
pub const ACCOUNT_MAX_CHUNKS: usize = 64;

/// The vault's chunked records, account first, revocations second.
pub const ACCOUNT_CHUNKED_RECORDS: [&str; 2] = ["chatgpt", "revoke"];

/// An exact raw-store target; unlike keyring entries, the vault uses slash joining.
#[must_use]
pub fn account_target(service: &str, entry: &str) -> String {
    format!("{service}/{entry}")
}

/// Every account/key target, including both vault slots and the shell's key staging record.
pub fn account_targets(service: &str) -> impl Iterator<Item = String> + '_ {
    ["host", "signout"]
        .into_iter()
        .map(move |entry| account_target(service, entry))
        .chain(ACCOUNT_CHUNKED_RECORDS.into_iter().flat_map(move |record| {
            std::iter::once(account_target(service, record)).chain((0..2).flat_map(move |slot| {
                (0..ACCOUNT_MAX_CHUNKS)
                    .map(move |index| account_target(service, &format!("{record}/{slot}/{index}")))
            }))
        }))
        .chain([
            anthropic_key_target(service),
            anthropic_staging_target(service),
        ])
}

/// The keyring account for a Windows session's capability.
#[must_use]
pub fn instance_entry(session: u32) -> String {
    format!("{INSTANCE_ACCOUNT}-{session}")
}

/// Parses only canonical account names emitted by [`instance_entry`], not stored targets.
#[must_use]
pub fn parse_instance_entry(account: &str) -> Option<u32> {
    let digits = account.strip_prefix(INSTANCE_ACCOUNT)?.strip_prefix('-')?;
    if digits.is_empty()
        || digits.len() > 10
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    digits.parse().ok()
}

/// The Windows Credential Manager target used by keyring for an instance capability.
#[must_use]
pub fn instance_target(service: &str, session: u32) -> String {
    // keyring's Windows backend joins the account and service with a dot, not a slash.
    format!("{}.{service}", instance_entry(session))
}

/// The `dwExtraInfo` the approved input-session harness puts on every input it injects, so the
/// shell's gate observer and the harness's own fixture tell it from the user's input.
pub const INPUT_SESSION_MARKER: usize = 0x4350_5331;

/// The shell's single focus-grant input at `OpenInput`.
pub const FOCUS_INPUT_MARKER: usize = 0x4350_5346;

/// `kumochi --gate-observer` only: posted to the dock's hit proxy (`proxy_hwnd` in the observer's
/// startup record), it asks the shell for one dock repaint with no input. `WM_APP + 0x10`.
pub const OBSERVER_REPAINT_MESSAGE: u32 = 0x8010;

/// Kumochi's own Claude Code folder, in the [`DATA_DIR`] folder under `%LOCALAPPDATA%`: its
/// `config` folder is `CLAUDE_CONFIG_DIR`, where Claude Code keeps its sign-in and Kumochi never
/// looks, beside the profile, application data, temporary and working folders it's given.
pub const CLAUDE_CODE_DIR: &str = "claude-code";
/// The lock a brain holds, beside [`CLAUDE_CODE_DIR`], for as long as any `claude` it started
/// may still be running, so no other brain starts one against the same folder.
pub const CLAUDE_CODE_LOCK: &str = "claude-code.lock";
/// The marker a Disconnect writes, beside [`CLAUDE_CODE_DIR`], before it signs out; while it
/// stands, the folder is deleted before any `claude` starts.
pub const CLAUDE_CODE_DELETING: &str = "claude-code.deleting";
/// The prefix of the empty marker, beside [`CLAUDE_CODE_DIR`], that names a Claude Code version
/// whose audit failed, as in `claude-code.quarantine-2.1.306`. Disconnect never deletes it; only
/// finding a different version does.
pub const CLAUDE_CODE_QUARANTINE: &str = "claude-code.quarantine-";
/// The folder for each Claude Code launch's owned folders, in the [`DATA_DIR`] folder under
/// `%LOCALAPPDATA%`.
pub const ENGINE_DIR: &str = "engine";
/// The MCP server name Claude Code sees, so its tools are `mcp__kumochi__*`.
pub const MCP_SERVER_NAME: &str = "kumochi";
/// The Credential Manager account under [`CREDENTIAL_SERVICE`] holding the Anthropic API key.
pub const ANTHROPIC_KEY_ACCOUNT: &str = "anthropic-key";
/// The account the shell stages a pasted key in, for the brain to check and move.
pub const ANTHROPIC_STAGING_ACCOUNT: &str = "anthropic-key-staged";

/// The exact active-key target shared by the vault and purge inventory.
#[must_use]
pub fn anthropic_key_target(service: &str) -> String {
    format!("{ANTHROPIC_KEY_ACCOUNT}.{service}")
}

/// The exact staging target shared by the shell, vault and purge inventory.
#[must_use]
pub fn anthropic_staging_target(service: &str) -> String {
    format!("{ANTHROPIC_STAGING_ACCOUNT}.{service}")
}

/// The path segment that sets write-once credential generations apart from every legacy name.
pub const CREDENTIAL_GENERATION_SEGMENT: &str = "gen";

/// The exact name of chunk `index` of one write-once credential generation.
#[must_use]
pub fn generation_target(service: &str, kind: &str, generation: &str, index: usize) -> String {
    format!("{service}/{CREDENTIAL_GENERATION_SEGMENT}/{kind}/{generation}/{index}")
}

/// The credential chunk size, below Windows' generic credential blob limit.
pub const CREDENTIAL_CHUNK_SIZE: usize = 2000;

/// One write-once credential's exact bounded layout, shared by both processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialLayout(usize);

impl CredentialLayout {
    /// Refuses records above the existing 48-KiB authority bound.
    pub fn new(bytes: usize) -> Option<Self> {
        (bytes <= 49152).then_some(Self(bytes))
    }
    /// The exact total byte length.
    pub const fn bytes(self) -> usize {
        self.0
    }
    /// The number of names, including one empty chunk for an empty record.
    pub fn chunks(self) -> usize {
        self.0.div_ceil(CREDENTIAL_CHUNK_SIZE).max(1)
    }
    /// The exact size of a chunk at an index below `chunks()`.
    pub fn chunk_len(self, index: usize) -> usize {
        if index + 1 < self.chunks() {
            CREDENTIAL_CHUNK_SIZE
        } else {
            self.0 - CREDENTIAL_CHUNK_SIZE * (self.chunks() - 1)
        }
    }
}

/// Canonical lowercase identity spelling for every generation's target names.
pub fn generation_hex(id: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    id.iter().fold(String::with_capacity(32), |mut hex, byte| {
        write!(hex, "{byte:02x}").expect("writing into a String cannot fail");
        hex
    })
}
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
/// The debug allowance for Anthropic catalog requests in one process.
pub const LIVE_KEY_CATALOG_BUDGET_ENV: &str = "KUMOCHI_LIVE_KEY_CATALOG_BUDGET";
/// The debug allowance for Anthropic Messages requests in one process.
pub const LIVE_KEY_MESSAGES_BUDGET_ENV: &str = "KUMOCHI_LIVE_KEY_MESSAGES_BUDGET";

/// Enables content-free `ChatGPT` attempt stamps in a copied Dev brain.
pub const LIVE_CHATGPT_TEXT_ENV: &str = "KUMOCHI_LIVE_CHATGPT_TEXT";
/// The folder that holds numbered copies for Dev live checks.
pub const LIVE_RUNS_DIR: &str = "live";
/// The executable folder within one copied Dev live check.
pub const LIVE_RUN_BIN_DIR: &str = "bin";
/// The content-free record within one copied Dev live check.
pub const LIVE_CHATGPT_TEXT_STAMPS_FILE: &str = "chatgpt-text.stamps";

/// Dev builds only: the most Claude Code questions the brain may seed. A value that isn't a
/// count allows none.
pub const CLAUDE_QUESTIONS_ENV: &str = "KUMOCHI_CLAUDE_QUESTIONS";
/// Dev builds only: when set, to any value, every Claude Code launch, title jobs included, passes
/// `--max-turns 1` to request one native turn, with `--permission-mode dontAsk` and no Kumochi
/// tool, for the desk's picture check. It doesn't bound provider requests. Release builds ignore it.
pub const CLAUDE_ONE_TURN_ENV: &str = "KUMOCHI_CLAUDE_ONE_TURN";

/// Dev builds only: presence denies issue filing and recovery before either can start a job.
#[cfg(debug_assertions)]
pub const LIVE_NO_FILING_ENV: &str = "KUMOCHI_LIVE_NO_FILING";

/// Dev builds only: the brain's one-shot catalog provenance capture, which
/// `cargo xtask live catalog-provenance` runs. A brain built without it exits 10.
pub const CATALOG_PROVENANCE_ARG: &str = "--catalog-provenance";

/// Dev builds only: the bounded image check, which `cargo xtask live image` runs. A brain built
/// without it exits 10.
pub const IMAGE_CHECK_ARG: &str = "--image-check";

/// Dev builds only: the image check's two-request diagnostic, which
/// `cargo xtask live image --diagnostic` runs. A brain built without it exits 10.
pub const IMAGE_DIAGNOSTIC_ARG: &str = "--image-diagnostic";

/// Dev builds only: the screen marks live check, which `cargo xtask live marks` runs. A brain
/// built without it exits 10.
pub const MARKS_CHECK_ARG: &str = "--marks-check";

/// The capture's one output file, in its `target\live\<time>` run folder.
pub const CATALOG_PROVENANCE_FILE: &str = "catalog-provenance.json";

/// Dev builds only: the uncovered backlog, in estimated tokens, at which a thread compacts after
/// a run, lowered for the live check. A value that isn't a count keeps the default.
pub const COMPACT_AT_ENV: &str = "KUMOCHI_COMPACT_AT";

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
mod tests {
    use super::{
        BRAIN_LOG, CREDENTIAL_GENERATION_SEGMENT, CREDENTIAL_SERVICE, SCREEN_LOG, SHELL_LOG,
        SPEECH_LOG, account_targets, generation_target,
    };

    #[test]
    fn github_registration_matches_the_build_profile() {
        #[cfg(debug_assertions)]
        assert_eq!(super::GITHUB_CLIENT_ID, Some("Iv23liKFSP0RTvDt9XGM"));
        #[cfg(not(debug_assertions))]
        assert_eq!(super::GITHUB_CLIENT_ID, None);
    }

    #[test]
    fn generation_names_never_overlap_a_legacy_name() {
        let prefix = format!("{CREDENTIAL_SERVICE}/{CREDENTIAL_GENERATION_SEGMENT}/");
        let legacy: Vec<String> = account_targets(CREDENTIAL_SERVICE).collect();
        assert_eq!(legacy.len(), 262, "every legacy name is counted");
        for name in &legacy {
            assert!(
                !name.starts_with(&prefix),
                "a legacy name is in the generation space"
            );
        }
        let name = generation_target(CREDENTIAL_SERVICE, "chatgpt", &"0".repeat(32), 24);
        assert!(
            name.starts_with(&prefix),
            "generation names use the segment"
        );
        assert!(
            !legacy.contains(&name),
            "a generation name is a legacy name"
        );
    }

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
