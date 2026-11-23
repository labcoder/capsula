use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::*;

#[test]
fn claude_code_never_times_out_a_call_while_its_launch_lives() {
    let switch = |name| {
        SWITCHES
            .iter()
            .find(|(switch, _)| *switch == name)
            .map(|(_, value)| *value)
    };
    let timeout: u128 = switch("MCP_TOOL_TIMEOUT")
        .expect("pinned")
        .parse()
        .expect("milliseconds");
    assert!(timeout > super::super::supervisor::MAX_LAUNCH.as_millis());
    assert_eq!(switch("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"), Some("0"));
}

fn folders() -> OwnedFolders {
    let at = |name: &str| PathBuf::from(format!(r"C:\k\claude-code\{name}"));
    OwnedFolders {
        config: at("config"),
        profile: at("profile"),
        appdata: at("appdata"),
        local_appdata: at("local"),
        tmp: at("tmp"),
        work: at("work"),
    }
}

fn system() -> Vec<(OsString, OsString)> {
    [
        ("SystemRoot", r"C:\Windows"),
        ("SystemDrive", "C:"),
        ("PATH", r"C:\Windows\System32;C:\Windows"),
    ]
    .map(|(name, value)| (name.into(), value.into()))
    .to_vec()
}

fn strings(values: &[OsString]) -> Vec<&str> {
    values
        .iter()
        .map(|value| value.to_str().expect("ascii"))
        .collect()
}

const V2_1_306: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 306,
};

#[test]
fn only_a_reviewed_version_has_a_policy() {
    let row = reviewed(V2_1_306).expect("2.1.306 is reviewed");
    assert_eq!(row.version, V2_1_306);
    assert_eq!(
        row.policy,
        LaunchPolicy {
            turn_cap: TurnCap::Verified,
            server_managed: ServerManaged::Disclosed,
        }
    );
    for other in [
        ClaudeVersion {
            patch: 307,
            ..V2_1_306
        },
        ClaudeVersion {
            patch: 305,
            ..V2_1_306
        },
        ClaudeVersion {
            major: 3,
            ..V2_1_306
        },
    ] {
        assert_eq!(
            reviewed(other).map(|row| row.version),
            Err(Unsupported::Unreviewed(other))
        );
    }
}

#[test]
fn each_entry_point_gets_exactly_its_arguments() {
    let tools: BTreeSet<String> = ["mcp__kumochi__see", "mcp__kumochi__draw"]
        .map(String::from)
        .into();
    let mcp = PathBuf::from(r"C:\k\engine\0a\work\mcp.json");
    let question = |effort| EntryPoint::Question {
        mcp_config: &mcp,
        allowed_tools: &tools,
        model: "sonnet",
        effort,
        max_turns: MAX_NATIVE_TURNS,
    };
    let head = [
        "-p",
        "--restricted",
        "--strict-mcp-config",
        "--mcp-config",
        r"C:\k\engine\0a\work\mcp.json",
        "--disable-slash-commands",
        "--tools",
        "",
        "--no-session-persistence",
        "--allowedTools",
        "mcp__kumochi__draw",
        "mcp__kumochi__see",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--max-turns",
        "8",
        "--exclude-dynamic-system-prompt-sections",
        "--model",
        "sonnet",
    ];
    let args = |entry| launch_data(&folders(), &system(), &entry).args;
    assert_eq!(strings(&args(question(None))), head);
    assert_eq!(
        strings(&args(question(Some("high")))),
        [&head[..], &["--effort", "high"]].concat()
    );
    assert_eq!(strings(&args(EntryPoint::Version)), ["--version"]);
    assert_eq!(
        strings(&args(EntryPoint::Status)),
        ["auth", "status", "--json"]
    );
    assert_eq!(strings(&args(EntryPoint::Login)), ["auth", "login"]);
    assert_eq!(strings(&args(EntryPoint::Logout)), ["auth", "logout"]);

    let none = BTreeSet::new();
    let bare = launch_data(
        &folders(),
        &system(),
        &EntryPoint::Question {
            mcp_config: &mcp,
            allowed_tools: &none,
            model: "haiku",
            effort: None,
            max_turns: MAX_NATIVE_TURNS,
        },
    );
    let without_tools: Vec<&str> = head
        .iter()
        .copied()
        .filter(|word| {
            !["--allowedTools", "mcp__kumochi__draw", "mcp__kumochi__see"].contains(word)
        })
        .map(|word| if word == "sonnet" { "haiku" } else { word })
        .collect();
    assert_eq!(
        strings(&bare.args),
        without_tools,
        "no tools, no switch to swallow the next one, and the built-in tools still off"
    );
}

/// The desk's picture check: one native turn, no Kumochi tool even with tools frozen for the
/// launch, and `dontAsk`, so anything that would prompt is denied; every other word as a stock
/// question has. A stock question keeps 8 and no permission mode.
#[test]
fn a_one_turn_question_permits_no_tool() {
    let tools: BTreeSet<String> = ["mcp__kumochi__see", "mcp__kumochi__draw"]
        .map(String::from)
        .into();
    let mcp = PathBuf::from(r"C:\k\engine\0a\work\mcp.json");
    let args = |max_turns| {
        launch_data(
            &folders(),
            &system(),
            &EntryPoint::Question {
                mcp_config: &mcp,
                allowed_tools: &tools,
                model: "haiku",
                effort: Some("low"),
                max_turns,
            },
        )
        .args
    };
    assert_eq!(
        strings(&args(1)),
        [
            "-p",
            "--restricted",
            "--strict-mcp-config",
            "--mcp-config",
            r"C:\k\engine\0a\work\mcp.json",
            "--disable-slash-commands",
            "--tools",
            "",
            "--no-session-persistence",
            "--permission-mode",
            "dontAsk",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--max-turns",
            "1",
            "--exclude-dynamic-system-prompt-sections",
            "--model",
            "haiku",
            "--effort",
            "low",
        ]
    );
    let stock = args(MAX_NATIVE_TURNS);
    let stock = strings(&stock);
    assert_eq!(
        native_turns(),
        8,
        "a test brain's environment never sets the check"
    );
    assert_eq!(
        stock[9..12],
        ["--allowedTools", "mcp__kumochi__draw", "mcp__kumochi__see"]
    );
    assert_eq!(stock[18..20], ["--max-turns", "8"]);
    assert!(!stock.contains(&"--permission-mode"));
}

#[test]
fn every_entry_point_gets_the_same_exact_environment() {
    let expected: Vec<(&str, &str)> = vec![
        ("CLAUDE_CONFIG_DIR", r"C:\k\claude-code\config"),
        ("USERPROFILE", r"C:\k\claude-code\profile"),
        ("HOME", r"C:\k\claude-code\profile"),
        ("APPDATA", r"C:\k\claude-code\appdata"),
        ("LOCALAPPDATA", r"C:\k\claude-code\local"),
        ("TEMP", r"C:\k\claude-code\tmp"),
        ("TMP", r"C:\k\claude-code\tmp"),
        ("CLAUDE_CODE_TMPDIR", r"C:\k\claude-code\tmp"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
        ("MCP_TOOL_TIMEOUT", "300000"),
        ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0"),
        ("SystemRoot", r"C:\Windows"),
        ("SystemDrive", "C:"),
        ("PATH", r"C:\Windows\System32;C:\Windows"),
    ];
    let tools = BTreeSet::new();
    let mcp = PathBuf::from("mcp.json");
    for entry in [
        EntryPoint::Version,
        EntryPoint::Status,
        EntryPoint::Login,
        EntryPoint::Logout,
        EntryPoint::Question {
            mcp_config: &mcp,
            allowed_tools: &tools,
            model: "opus",
            effort: None,
            max_turns: MAX_NATIVE_TURNS,
        },
    ] {
        let env = launch_data(&folders(), &system(), &entry).env;
        let env: Vec<(&str, &str)> = env
            .iter()
            .map(|(name, value)| {
                (
                    name.to_str().expect("ascii"),
                    value.to_str().expect("ascii"),
                )
            })
            .collect();
        assert_eq!(env, expected, "{entry:?}");
    }
}

#[cfg(windows)]
#[test]
#[cfg(windows)]
fn the_system_values_come_from_the_os_and_no_parent_path_entry_reaches_the_child() {
    let system = crate::platform::system_env().expect("system folders");
    let names: Vec<&str> = system
        .iter()
        .map(|(name, _)| name.to_str().expect("ascii"))
        .collect();
    assert_eq!(names, ["SystemRoot", "SystemDrive", "PATH"]);
    let root = system[0].1.to_str().expect("ascii").to_owned();
    assert_eq!(
        system[1].1,
        OsString::from(&root[..2]),
        "the drive of SystemRoot"
    );
    let path = system[2].1.to_str().expect("ascii");
    let (first, second) = path.split_once(';').expect("two folders");
    assert_eq!(second, root, "then the Windows folder");
    assert_eq!(
        first.to_lowercase(),
        format!(r"{}\system32", root.to_lowercase()),
        "the system folder first"
    );

    // Cargo puts the test binary's own folder on the parent's PATH: the planted entry. It spells
    // the folder as the target dir was given, so folders compare as case-folded paths.
    let folded = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
    let planted = std::env::current_exe().expect("test binary");
    let planted = folded(planted.parent().expect("its folder"));
    let on_path = |entry: &Path| folded(entry) == planted;
    let respelled = planted.to_string_lossy().replace('\\', "/").to_uppercase();
    assert!(
        on_path(Path::new(&respelled)),
        "a forward-slash, upper-case spelling is the same folder"
    );
    let parent = std::env::var_os("PATH").expect("a parent PATH");
    assert!(
        std::env::split_paths(&parent).any(|entry| on_path(&entry)),
        "precondition: the parent PATH holds the test binary's folder"
    );
    let tools = BTreeSet::new();
    let mcp = PathBuf::from("mcp.json");
    let data = launch_data(
        &folders(),
        &system,
        &EntryPoint::Question {
            mcp_config: &mcp,
            allowed_tools: &tools,
            model: "opus",
            effort: None,
            max_turns: MAX_NATIVE_TURNS,
        },
    );
    let planted = planted.to_string_lossy().replace('/', "\\");
    for (name, value) in &data.env {
        let value = value.to_string_lossy().to_lowercase().replace('/', "\\");
        assert!(
            !value.contains(&planted),
            "{name:?} carries a parent PATH entry"
        );
    }
}

fn clear() -> ManagedPresence {
    ManagedPresence {
        files: [Presence::Absent; 3],
        machine: Presence::Absent,
        user: Presence::Absent,
    }
}

#[test]
fn a_present_or_unknown_managed_source_or_an_unverified_cap_refuses_the_launch() {
    let policy = reviewed(V2_1_306).expect("reviewed").policy;
    assert_eq!(may_launch(policy, clear()), Ok(()));
    for at in 0..5 {
        for presence in [Presence::Present, Presence::Indeterminate] {
            let mut managed = clear();
            match at {
                0..=2 => managed.files[at] = presence,
                3 => managed.machine = presence,
                _ => managed.user = presence,
            }
            assert_eq!(
                may_launch(policy, managed),
                Err(PolicyFailed::ManagedPolicy),
                "{managed:?}"
            );
        }
    }
    let veto = LaunchPolicy {
        server_managed: ServerManaged::RequireSignal,
        ..policy
    };
    assert_eq!(may_launch(veto, clear()), Err(PolicyFailed::ManagedPolicy));
    let uncapped = LaunchPolicy {
        turn_cap: TurnCap::Unverified,
        ..policy
    };
    assert_eq!(may_launch(uncapped, clear()), Err(PolicyFailed::TurnCap));
    let mut managed = clear();
    managed.user = Presence::Present;
    assert_eq!(
        may_launch(uncapped, managed),
        Err(PolicyFailed::ManagedPolicy),
        "the managed check comes first"
    );
}

#[test]
fn launch_data_debug_prints_counts_only() {
    let tools: BTreeSet<String> = ["mcp__kumochi__planted"].map(String::from).into();
    let mcp = PathBuf::from(r"C:\planted\mcp.json");
    let data = launch_data(
        &folders(),
        &system(),
        &EntryPoint::Question {
            mcp_config: &mcp,
            allowed_tools: &tools,
            model: "planted",
            effort: None,
            max_turns: MAX_NATIVE_TURNS,
        },
    );
    let printed = format!("{data:?}");
    assert_eq!(
        printed,
        format!("LaunchData {{ args: {}, env: 20 }}", data.args.len())
    );
}
