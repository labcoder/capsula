use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;

use super::*;

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
        },
    );
    assert!(
        !strings(&bare.args).contains(&"--allowedTools"),
        "no tools, no switch to swallow the next one"
    );
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

    // Cargo puts the test binary's own folder on the parent's PATH: the planted entry.
    let planted = std::env::current_exe().expect("test binary");
    let planted = planted
        .parent()
        .expect("its folder")
        .to_string_lossy()
        .to_lowercase();
    let parent = std::env::var_os("PATH").expect("a parent PATH");
    assert!(
        std::env::split_paths(&parent)
            .any(|entry| entry.to_string_lossy().to_lowercase() == planted),
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
        },
    );
    for (name, value) in &data.env {
        assert!(
            !value.to_string_lossy().to_lowercase().contains(&planted),
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
        },
    );
    let printed = format!("{data:?}");
    assert_eq!(
        printed,
        format!("LaunchData {{ args: {}, env: 17 }}", data.args.len())
    );
}
