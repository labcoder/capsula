//! Claude Code's launch policy, per reviewed version: what each entry point gets on its command
//! line and in its environment, and what refuses a launch before anything starts. Pure data and
//! functions: C2 and C5a start the processes, own the jobs and keep [`LaunchData`] alive.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use kumochi_protocol::constants::CLAUDE_ONE_TURN_ENV;
use kumochi_protocol::message::{ClaudeVersion, Effort, PolicyFailed, Unsupported};

use super::audit::AuditSchema;
use super::stream::MAX_NATIVE_TURNS;
use crate::platform::{ManagedPresence, Presence};

/// One reviewed Claude Code version: its launch policy and the facts its audit expects.
#[derive(Debug)]
pub(crate) struct Reviewed {
    pub(crate) version: ClaudeVersion,
    pub(crate) policy: LaunchPolicy,
    pub(crate) audit: AuditSchema,
    /// The aliases a run may name on this version, with the names the picker shows, in the
    /// order it lists them. Only these reach `--model`.
    pub(crate) models: &'static [ReviewedModel],
    /// The aliases in `models` this version's own probe showed taking a picture on its input.
    /// One alias's evidence says nothing about another, so an unprobed alias isn't here.
    pub(crate) images: &'static [&'static str],
    /// The alias a profile with only Claude Code starts on, at `Low`; one of `models`, named here
    /// so their order decides nothing.
    pub(crate) default_model: &'static str,
}

#[derive(Debug)]
pub(crate) struct ReviewedModel {
    pub(crate) alias: &'static str,
    pub(crate) name: &'static str,
    pub(crate) efforts: &'static [Effort],
}

const EFFORTS_2_1_306: &[Effort] = &[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// Adding a version is a reviewed PR that adds a row here, from that version's probe.
pub(crate) static REVIEWED_CLAUDE_CODE: &[Reviewed] = &[Reviewed {
    // P0, 2026-11-04: handoffs/CAPS-158/claude-code-2.1.306.md.
    version: ClaudeVersion {
        major: 2,
        minor: 1,
        patch: 306,
    },
    policy: LaunchPolicy {
        turn_cap: TurnCap::Verified,
        server_managed: ServerManaged::Disclosed,
    },
    audit: AuditSchema {
        agents: &[
            "claude",
            "Explore",
            "general-purpose",
            "Plan",
            "statusline-setup",
        ],
        plugins: &["cc-plugin-agents-md", "cc-plugin-plugin-authoring"],
        api_key_sources: &["none"],
    },
    // The owner's list (OSC-742). P0's `default` alias is left out: it names no fixed model.
    models: &[
        ReviewedModel {
            alias: "sonnet",
            name: "Sonnet",
            efforts: EFFORTS_2_1_306,
        },
        ReviewedModel {
            alias: "opus",
            name: "Opus",
            efforts: EFFORTS_2_1_306,
        },
        ReviewedModel {
            alias: "haiku",
            name: "Haiku",
            efforts: EFFORTS_2_1_306,
        },
        ReviewedModel {
            alias: "fable",
            name: "Fable",
            efforts: EFFORTS_2_1_306,
        },
    ],
    // No alias is probed for pictures yet.
    images: &[],
    // The owner's default (OSC-742).
    default_model: "haiku",
}];

/// The reviewed entry for `version`. Every other version is refused, with no override: D2a's
/// synthetic maintenance probe has its own path to an unknown version's facts.
///
/// # Errors
/// [`Unsupported::Unreviewed`] for a version with no row.
pub(crate) fn reviewed(version: ClaudeVersion) -> Result<&'static Reviewed, Unsupported> {
    REVIEWED_CLAUDE_CODE
        .iter()
        .find(|row| row.version == version)
        .ok_or(Unsupported::Unreviewed(version))
}

/// One version's switches whose effect is a fact about that version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaunchPolicy {
    pub(crate) turn_cap: TurnCap,
    pub(crate) server_managed: ServerManaged,
}

/// Whether `--max-turns` [`MAX_NATIVE_TURNS`] was shown to hold for the version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnCap {
    Verified,
    Unverified,
}

/// What to do about a Team or Enterprise organization's server-managed settings, which Kumochi
/// can't see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerManaged {
    /// Disclosed on the Connect screen; the launch goes ahead.
    Disclosed,
    /// The owner's veto: refused until a version documents a signal, which none does yet.
    RequireSignal,
}

/// Kumochi's folders under its Claude Code folder; C2 creates them and Disconnect removes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedFolders {
    pub(crate) config: PathBuf,
    pub(crate) profile: PathBuf,
    pub(crate) appdata: PathBuf,
    pub(crate) local_appdata: PathBuf,
    pub(crate) tmp: PathBuf,
    /// Every entry point's working folder.
    pub(crate) work: PathBuf,
}

/// What a launch is for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum EntryPoint<'a> {
    /// `--version`.
    Version,
    /// `auth status --json`.
    Status,
    /// `auth login`; C2 runs it with a console.
    Login,
    /// `auth logout`.
    Logout,
    /// One question.
    Question {
        /// The launch's `mcp.json`.
        mcp_config: &'a Path,
        /// C5b's frozen `mcp__kumochi__*` set for the launch.
        allowed_tools: &'a BTreeSet<String>,
        /// From C5b's snapshot.
        model: &'a str,
        /// From C5b's snapshot; `None` leaves Claude Code's default.
        effort: Option<&'a str>,
        /// [`native_turns`]: `--max-turns`; 1 also drops the allow list and adds `dontAsk`.
        max_turns: u8,
    },
}

/// Claude Code's `--max-turns` for every launch, title jobs included: [`MAX_NATIVE_TURNS`], or 1 in
/// a dev build whose environment sets [`CLAUDE_ONE_TURN_ENV`], to request one native turn for the
/// desk's picture check. It doesn't bound provider requests: one native turn can retry its request
/// (`fixtures/claude-code/retry.jsonl`). Release builds and unflagged dev builds keep the stock 8.
pub(crate) fn native_turns() -> u8 {
    if cfg!(debug_assertions) && std::env::var_os(CLAUDE_ONE_TURN_ENV).is_some() {
        1
    } else {
        MAX_NATIVE_TURNS
    }
}

/// A launch's arguments and whole environment, owned, so the caller can borrow them into
/// `platform::Launch` with the exe, `folders.work`, its stdio and its jobs.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LaunchData {
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
}

impl fmt::Debug for LaunchData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchData")
            .field("args", &self.args.len())
            .field("env", &self.env.len())
            .finish()
    }
}

/// The documented switches every entry point gets, with their values. Kumochi's server is the only
/// one Claude Code may offer, so a claude.ai sign-in's connectors are never fetched.
const SWITCHES: [(&str, &str); 9] = [
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
    // Above `MAX_LAUNCH`, which bounds every wait a call has, so no call times out while its
    // launch lives, and no idle check: a call waiting on the store sends no progress.
    ("MCP_TOOL_TIMEOUT", "300000"),
    ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0"),
];

/// The arguments and environment for `entry`. `system` is `platform::system_env()`: the only
/// values not owned by Kumochi, read from the OS and never from the brain's environment.
pub(crate) fn launch_data(
    folders: &OwnedFolders,
    system: &[(OsString, OsString)],
    entry: &EntryPoint<'_>,
) -> LaunchData {
    let words = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
    let args = match *entry {
        EntryPoint::Version => words(&["--version"]),
        EntryPoint::Status => words(&["auth", "status", "--json"]),
        EntryPoint::Login => words(&["auth", "login"]),
        EntryPoint::Logout => words(&["auth", "logout"]),
        EntryPoint::Question {
            mcp_config,
            allowed_tools,
            model,
            effort,
            max_turns,
        } => {
            // P0's order: `--allowedTools` takes every value up to the next switch.
            let mut args = words(&["-p", "--restricted", "--strict-mcp-config", "--mcp-config"]);
            args.push(mcp_config.into());
            args.extend(words(&[
                "--disable-slash-commands",
                "--tools",
                "",
                "--no-session-persistence",
            ]));
            // One turn can't carry a tool's result back, so the one-turn check passes no allow list
            // and asks Claude Code to deny prompts.
            if max_turns == 1 {
                args.extend(words(&["--permission-mode", "dontAsk"]));
            } else if !allowed_tools.is_empty() {
                args.push("--allowedTools".into());
                args.extend(allowed_tools.iter().map(OsString::from));
            }
            args.extend(words(&[
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--max-turns",
            ]));
            args.push(max_turns.to_string().into());
            args.extend(words(&[
                "--exclude-dynamic-system-prompt-sections",
                "--model",
                model,
            ]));
            if let Some(effort) = effort {
                args.extend(words(&["--effort", effort]));
            }
            args
        }
    };
    let owned = [
        ("CLAUDE_CONFIG_DIR", &folders.config),
        ("USERPROFILE", &folders.profile),
        ("HOME", &folders.profile),
        ("APPDATA", &folders.appdata),
        ("LOCALAPPDATA", &folders.local_appdata),
        ("TEMP", &folders.tmp),
        ("TMP", &folders.tmp),
        ("CLAUDE_CODE_TMPDIR", &folders.tmp),
    ];
    let env = owned
        .into_iter()
        .map(|(name, path)| (name.into(), path.into()))
        .chain(
            SWITCHES
                .into_iter()
                .map(|(name, value)| (name.into(), value.into())),
        )
        .chain(system.iter().cloned())
        .collect();
    LaunchData { args, env }
}

/// Refuses before anything starts.
///
/// # Errors
/// [`PolicyFailed::ManagedPolicy`] when a managed source is present or can't be ruled out, or
/// when the policy requires a server-managed signal (none exists yet); [`PolicyFailed::TurnCap`]
/// when the turn cap isn't verified.
pub(crate) fn may_launch(
    policy: LaunchPolicy,
    managed: ManagedPresence,
) -> Result<(), PolicyFailed> {
    if managed
        .all()
        .iter()
        .any(|presence| *presence != Presence::Absent)
        || policy.server_managed == ServerManaged::RequireSignal
    {
        return Err(PolicyFailed::ManagedPolicy);
    }
    if policy.turn_cap != TurnCap::Verified {
        return Err(PolicyFailed::TurnCap);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
