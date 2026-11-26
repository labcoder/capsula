use super::*;

const TEAM: &str = "ABCDE12345";
const NOW: &str = "2026-11-26T06:00:00Z";
const BRAIN: &str = "ai.kumochi.dev.brain";
const GROUP: &str = "ai.kumochi.dev.instance";
const SHA1: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

fn args(list: &[&str]) -> Result<Args> {
    parse(list.iter().map(|&arg| arg.to_owned()))
}

fn profile(entitlements: Value) -> Profile {
    Profile {
        bytes: Vec::new(),
        team: TEAM.to_owned(),
        entitlements,
        expires: "2027-11-23T04:40:22Z".to_owned(),
        platforms: vec!["OSX".to_owned()],
        certificates: vec![
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF".to_owned(),
            SHA1.to_owned(),
        ],
    }
}

fn granted(groups: &[&str]) -> Value {
    json!({ APP_ID: format!("{TEAM}.{BRAIN}"), TEAM_ID: TEAM, GROUPS: groups })
}

fn wildcard() -> Profile {
    profile(granted(&[&format!("{TEAM}.*")]))
}

fn refusal(profile: &Profile) -> String {
    check(profile, BRAIN, GROUP, SHA1, NOW)
        .expect_err("refused")
        .to_string()
}

fn role(id: &'static str, exe: &'static str) -> Role<'static> {
    Role {
        name: "role",
        id,
        exe,
        profile: wildcard(),
        background: false,
    }
}

#[test]
fn every_input_is_named_once_and_the_identity_is_an_exact_hash() {
    let lower = SHA1.to_ascii_lowercase();
    let full = [
        "--identity",
        lower.as_str(),
        "--shell-profile",
        "s",
        "--brain-profile",
        "b",
        "--out",
        "o",
    ];
    assert_eq!(
        args(&full).expect("complete"),
        Args {
            identity: SHA1.to_owned(),
            shell_profile: "s".into(),
            brain_profile: "b".into(),
            out: "o".into(),
        }
    );
    let refused = |list: &[&str]| args(list).expect_err("refused").to_string();
    assert_eq!(refused(&full[2..]), "mac-bundle needs --identity");
    assert_eq!(refused(&full[..6]), "mac-bundle needs --out");
    assert_eq!(
        refused(&[&full[..], &["--out", "p"]].concat()),
        "--out given twice"
    );
    assert_eq!(refused(&["--out"]), "--out needs a value");
    assert_eq!(
        refused(&[SHA1, "--out", "o"]),
        "mac-bundle doesn't take that argument"
    );
    for identity in [
        &SHA1[1..],
        &format!("{}g", &SHA1[1..]),
        "Apple Development: Someone",
    ] {
        let mut list = full.to_vec();
        list[1] = identity;
        assert_eq!(
            refused(&list),
            "--identity is a 40-digit SHA-1 certificate hash"
        );
    }
}

#[test]
fn a_current_macos_profile_for_exactly_the_app_and_certificate_passes() {
    let exact = [format!("{TEAM}.{BRAIN}"), format!("{TEAM}.{GROUP}")];
    for groups in [vec![format!("{TEAM}.*")], exact.to_vec()] {
        let groups: Vec<&str> = groups.iter().map(String::as_str).collect();
        check(&profile(granted(&groups)), BRAIN, GROUP, SHA1, NOW).expect("allowed");
    }
}

#[test]
fn a_profile_that_doesnt_authorize_the_app_is_refused() {
    let mut expired = wildcard();
    expired.expires = NOW.to_owned();
    assert_eq!(refusal(&expired), "it has expired");
    for text in [
        "2027-11-23 04:40:22Z",
        "2027-11-23T04:40:22",
        "9999-99-99T99:99:99+0",
    ] {
        let mut odd = wildcard();
        odd.expires = text.to_owned();
        assert_eq!(refusal(&odd), "its expiry isn't a UTC time");
    }

    let mut ios = wildcard();
    ios.platforms = vec!["iOS".to_owned()];
    assert_eq!(refusal(&ios), "it isn't a macOS profile");

    for team in ["ABCDE1234", "ABCDE123456", "abcde12345", "ABCDE<1234"] {
        let mut odd = wildcard();
        odd.team = team.to_owned();
        assert_eq!(refusal(&odd), "its team isn't a team identifier");
    }

    let mut other_team = wildcard();
    other_team.entitlements[TEAM_ID] = json!("ZYXWV98765");
    assert_eq!(
        refusal(&other_team),
        "its team doesn't match its entitlements"
    );

    for app in [
        format!("{TEAM}.*"),
        format!("{TEAM}.ai.kumochi.dev.shell"),
        BRAIN.to_owned(),
    ] {
        let mut odd = wildcard();
        odd.entitlements[APP_ID] = json!(app);
        assert_eq!(refusal(&odd), "it isn't for exactly ai.kumochi.dev.brain");
    }

    let mut other_certificate = wildcard();
    other_certificate.certificates.pop();
    assert_eq!(
        refusal(&other_certificate),
        "it doesn't authorize the signing certificate"
    );

    let own_only = format!("{TEAM}.{BRAIN}");
    for (groups, missing) in [
        (vec![own_only.as_str()], GROUP),
        (vec!["ZYXWV98765.*"], BRAIN),
        (vec![], BRAIN),
    ] {
        assert_eq!(
            refusal(&profile(granted(&groups))),
            format!("it doesn't allow the keychain group {missing}")
        );
    }
    let mut no_groups = wildcard();
    no_groups
        .entitlements
        .as_object_mut()
        .expect("an object")
        .remove(GROUPS);
    assert_eq!(
        refusal(&no_groups),
        "it doesn't allow the keychain group ai.kumochi.dev.brain"
    );
}

#[test]
fn each_role_is_signed_for_its_own_group_and_the_shared_one() {
    assert_eq!(
        entitlements(TEAM, BRAIN, GROUP),
        json!({
            "com.apple.application-identifier": "ABCDE12345.ai.kumochi.dev.brain",
            "com.apple.developer.team-identifier": "ABCDE12345",
            "keychain-access-groups": [
                "ABCDE12345.ai.kumochi.dev.brain",
                "ABCDE12345.ai.kumochi.dev.instance"
            ],
        })
    );
}

#[test]
fn only_the_brain_is_a_background_app_and_both_plists_read_back() {
    let brain = Role {
        background: true,
        ..role(BRAIN, BRAIN_EXE)
    };
    let shell = role("ai.kumochi.dev.shell", SHELL_EXE);
    let version = env!("CARGO_PKG_VERSION");
    let base = |id: &str, exe: &str| {
        json!({
            "CFBundleIdentifier": id,
            "CFBundleExecutable": exe,
            "CFBundlePackageType": "APPL",
            "CFBundleVersion": version,
            "CFBundleShortVersionString": version,
        })
    };
    let mut background = base(BRAIN, "kumochid");
    background["LSBackgroundOnly"] = json!(true);
    assert_eq!(info(&brain), background);
    assert_eq!(info(&shell), base("ai.kumochi.dev.shell", "kumochi"));
    for value in [
        info(&brain),
        info(&shell),
        entitlements(TEAM, BRAIN, "a&b<c>"),
    ] {
        let text = plist(&value).expect("a plist");
        assert_eq!(to_json(text.as_bytes()).expect("plutil reads it"), value);
    }
    assert_eq!(
        plist(&json!({ "n": 1 })).expect_err("refused").to_string(),
        "a bundle property list holds no number or null"
    );
}

fn here(package: &str) -> String {
    format!("path+file:///Users/a%20b/kumochi/crates/x#{package}@0.1.2")
}

#[test]
fn the_workspace_members_are_cargos_exact_ids() {
    let metadata = json!({
        "workspace_members": [here("kumochi-shell"), here("kumochi-brain")],
        "packages": [
            { "name": "kumochi-shell", "id": here("kumochi-shell") },
            { "name": "kumochi-brain", "id": here("kumochi-brain") },
            { "name": "kumochi-brain", "id": "registry+https://example.invalid#kumochi-brain@9.0.0" },
        ],
    });
    assert_eq!(
        members(&metadata).expect("members"),
        [
            ("kumochi-shell".to_owned(), here("kumochi-shell")),
            ("kumochi-brain".to_owned(), here("kumochi-brain")),
        ]
    );
}

#[test]
fn only_each_workspace_binary_artifact_is_taken() {
    let members = [
        ("kumochi-shell".to_owned(), here("kumochi-shell")),
        ("kumochi-brain".to_owned(), here("kumochi-brain")),
    ];
    let artifact = |reason: &str, id: &str, name: &str, kind: &str, exe: &str| {
        json!({
            "reason": reason,
            "package_id": id,
            "target": { "name": name, "kind": [kind] },
            "executable": exe,
        })
        .to_string()
    };
    let (shell_id, brain_id) = (here("kumochi-shell"), here("kumochi-brain"));
    let shell = artifact(
        "compiler-artifact",
        &shell_id,
        "kumochi",
        "bin",
        "/t/kumochi",
    );
    let brain = artifact(
        "compiler-artifact",
        &brain_id,
        "kumochid",
        "bin",
        "/t/kumochid",
    );
    let lines = |extra: &[String]| {
        [
            &[shell.clone(), "not json".to_owned(), brain.clone()][..],
            extra,
        ]
        .concat()
        .join("\n")
    };
    let taken = |extra: &[String]| artifacts(lines(extra).as_bytes(), &members);
    assert_eq!(
        taken(&[]).expect("both"),
        [
            ("kumochi".to_owned(), PathBuf::from("/t/kumochi")),
            ("kumochid".to_owned(), PathBuf::from("/t/kumochid")),
        ]
    );
    for decoy in [
        artifact("build-script-executed", &brain_id, "kumochid", "bin", "/x"),
        artifact(
            "compiler-artifact",
            &here("kumochi-brainx"),
            "kumochid",
            "bin",
            "/x",
        ),
        artifact(
            "compiler-artifact",
            "path+file:///elsewhere/crates/brain#kumochi-brain@0.1.2",
            "kumochid",
            "bin",
            "/x",
        ),
        artifact(
            "compiler-artifact",
            "registry+https://example.invalid#kumochi-brain@9.0.0",
            "kumochid",
            "bin",
            "/x",
        ),
        artifact("compiler-artifact", &brain_id, "kumochid", "test", "/x"),
    ] {
        assert_eq!(
            taken(std::slice::from_ref(&decoy)).expect("decoy ignored")[1].1,
            PathBuf::from("/t/kumochid")
        );
    }
    let twice = artifact("compiler-artifact", &brain_id, "kumochid", "bin", "/y");
    assert_eq!(
        taken(&[twice]).expect_err("ambiguous").to_string(),
        "Cargo built more than one kumochid"
    );
    assert_eq!(
        artifacts(shell.as_bytes(), &members)
            .expect_err("missing")
            .to_string(),
        "Cargo built no kumochid"
    );
    assert_eq!(
        artifacts(lines(&[]).as_bytes(), &members[..1])
            .expect_err("not a member")
            .to_string(),
        "the workspace has no kumochi-brain"
    );
}

#[test]
fn a_child_tool_runs_bounded_with_large_input_and_output_and_hides_its_errors() {
    let big = vec![b'k'; 300_000];
    assert_eq!(pipe("/bin/cat", &[], &big).expect("echoed"), big);
    let mut noisy = Command::new("/bin/sh");
    noisy.args(["-c", "head -c 200000 /dev/zero >&2; exit 4"]);
    assert_eq!(
        bounded("sh", &mut noisy, None, LAUNCH_WAIT)
            .expect_err("failed")
            .to_string(),
        "sh failed (exit status: 4, 200000 bytes of stderr withheld)"
    );
    let mut stalled = Command::new("/bin/sleep");
    stalled.arg("30");
    let started = Instant::now();
    assert_eq!(
        bounded("sleep", &mut stalled, None, Duration::from_millis(200))
            .expect_err("stalled")
            .to_string(),
        "sleep didn't finish within 200ms"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut forked = Command::new("/bin/sh");
    forked.args(["-c", "/bin/sleep 30 & exit 0"]);
    let started = Instant::now();
    assert_eq!(
        bounded("sh", &mut forked, None, Duration::from_millis(300))
            .expect_err("a helper kept the pipe")
            .to_string(),
        "sh's output didn't end within 300ms"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the helper's group was stopped"
    );
    assert_eq!(
        strings("Platform", br#"{"not":"a list"}"#)
            .expect_err("refused")
            .to_string(),
        "the profile's Platform has the wrong type"
    );
    assert_eq!(
        object("Entitlements", br#""ABCDE12345""#)
            .expect_err("refused")
            .to_string(),
        "the profile's Entitlements has the wrong type"
    );
}

#[test]
fn the_launch_needs_exit_69_in_time_and_always_reaps() {
    let sh = |script: &str| {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    };
    launch(&mut sh("exit 69"), LAUNCH_WAIT).expect("69 in time");
    let refused = |script: &str, wait| {
        launch(&mut sh(script), wait)
            .expect_err("refused")
            .to_string()
    };
    assert_eq!(
        refused("exit 3", LAUNCH_WAIT),
        "the signed brain ended with exit status: 3, not 69"
    );
    assert_eq!(
        refused("kill -9 $$", LAUNCH_WAIT),
        "the signed brain ended with signal: 9 (SIGKILL), not 69"
    );
    let started = Instant::now();
    assert_eq!(
        refused("exec /bin/sleep 30", Duration::from_millis(200)),
        "the signed brain didn't exit within 200ms"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stalled child was killed, not waited out"
    );
}
