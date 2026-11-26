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
        prefix: TEAM.to_owned(),
        all_devices: false,
        devices: Some(1),
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
    check(profile, BRAIN, GROUP, SHA1, NOW, Mode::Dev)
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
            mode: Mode::Dev,
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
        check(
            &profile(granted(&groups)),
            BRAIN,
            GROUP,
            SHA1,
            NOW,
            Mode::Dev,
        )
        .expect("allowed");
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
        entitlements(TEAM, TEAM, BRAIN, GROUP),
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
        entitlements(TEAM, TEAM, BRAIN, "a&b<c>"),
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
        "sh didn't finish within 300ms"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the helper's group was stopped"
    );
    let mut escaped = Command::new("/bin/sh");
    escaped.args([
        "-c",
        "/usr/bin/perl -MPOSIX -e 'POSIX::setsid() > 0 or exit 1; sleep 4' & exit 0",
    ]);
    let started = Instant::now();
    assert_eq!(
        bounded("sh", &mut escaped, None, Duration::from_millis(300))
            .expect_err("a helper outside the group kept the pipe")
            .to_string(),
        "sh's output stayed open after it was stopped"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the task gave up after the grace, not when the helper exited"
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

const PREFIX: &str = "PFX1234567";

/// A brain profile whose App ID prefix isn't its team, as legacy App IDs have.
fn prefixed() -> Profile {
    Profile {
        prefix: PREFIX.to_owned(),
        ..profile(json!({
            APP_ID: format!("{PREFIX}.{BRAIN}"),
            TEAM_ID: TEAM,
            GROUPS: [format!("{PREFIX}.*")],
        }))
    }
}

const SIGNED_DISPLAY: &str = "Executable=/x/kumochid.app/Contents/MacOS/kumochid
Identifier=ai.kumochi.dev.brain
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=269600 flags=0x10000(runtime) hashes=8414+7 location=embedded
Hash type=sha256 size=32
Hash choices=sha256
Signature size=4783
";

fn audit_refusal(profile: &Profile, signed: &Value, display: &str) -> String {
    audited(profile, signed, display.as_bytes(), BRAIN, GROUP)
        .expect_err("refused")
        .to_string()
}

#[test]
fn beta_is_a_flag_given_at_most_once() {
    let full = [
        "--identity",
        SHA1,
        "--shell-profile",
        "s",
        "--brain-profile",
        "b",
        "--out",
        "o",
    ];
    let beta = args(&[&["--beta"][..], &full].concat()).expect("beta");
    assert_eq!(beta.mode, Mode::Beta);
    assert_eq!(args(&full).expect("dev").mode, Mode::Dev);
    assert_eq!(
        args(&[&full[..], &["--beta"]].concat())
            .expect("beta last")
            .identity,
        SHA1
    );
    assert_eq!(
        args(&[&["--beta", "--beta"][..], &full].concat())
            .expect_err("refused")
            .to_string(),
        "--beta given twice"
    );
}

#[test]
fn each_mode_takes_only_its_own_kind_of_profile() {
    let mut developer_id = wildcard();
    developer_id.all_devices = true;
    developer_id.devices = None;
    check(&developer_id, BRAIN, GROUP, SHA1, NOW, Mode::Beta).expect("beta takes Developer ID");
    let mut both = wildcard();
    both.all_devices = true;
    both.devices = Some(2);
    assert_eq!(
        check(&both, BRAIN, GROUP, SHA1, NOW, Mode::Beta)
            .expect_err("refused")
            .to_string(),
        "it isn't a Developer ID profile"
    );
    for devices in [None, Some(0)] {
        let mut deviceless = wildcard();
        deviceless.devices = devices;
        assert_eq!(
            refusal(&deviceless),
            "it isn't a development profile: it lists no devices"
        );
    }
    assert_eq!(
        check(&wildcard(), BRAIN, GROUP, SHA1, NOW, Mode::Beta)
            .expect_err("refused")
            .to_string(),
        "it isn't a Developer ID profile"
    );
    assert_eq!(
        refusal(&developer_id),
        "it's a Developer ID profile, which needs --beta"
    );
}

#[test]
fn the_app_id_and_groups_follow_the_profiles_prefix_not_its_team() {
    check(&prefixed(), BRAIN, GROUP, SHA1, NOW, Mode::Dev).expect("prefix, not team");
    assert_eq!(
        entitlements(PREFIX, TEAM, BRAIN, GROUP),
        json!({
            "com.apple.application-identifier": "PFX1234567.ai.kumochi.dev.brain",
            "com.apple.developer.team-identifier": "ABCDE12345",
            "keychain-access-groups": [
                "PFX1234567.ai.kumochi.dev.brain",
                "PFX1234567.ai.kumochi.dev.instance"
            ],
        })
    );
    let mut team_app = prefixed();
    team_app.entitlements[APP_ID] = json!(format!("{TEAM}.{BRAIN}"));
    assert_eq!(
        refusal(&team_app),
        "it isn't for exactly ai.kumochi.dev.brain"
    );
    let mut team_groups = prefixed();
    team_groups.entitlements[GROUPS] = json!([format!("{TEAM}.*")]);
    assert_eq!(
        refusal(&team_groups),
        "it doesn't allow the keychain group ai.kumochi.dev.brain"
    );
    for odd in ["PFX123456", "pfx1234567", "PFX12345678", "PFX.234567"] {
        let mut profile = prefixed();
        profile.prefix = odd.to_owned();
        assert_eq!(refusal(&profile), "its App ID prefix isn't one");
    }
}

/// A decoded profile as `security cms -D` prints one, with a certificate whose bytes are "abc".
fn fixture(all_devices: &str, prefixes: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>AppIDName</key><string>Kumochi Dev Brain</string>
<key>ApplicationIdentifierPrefix</key><array>{prefixes}</array>
<key>DeveloperCertificates</key><array><data>YWJj</data></array>
<key>Entitlements</key><dict>
<key>com.apple.application-identifier</key><string>PFX1234567.ai.kumochi.dev.brain</string>
<key>com.apple.developer.team-identifier</key><string>ABCDE12345</string>
<key>keychain-access-groups</key><array><string>PFX1234567.*</string></array>
</dict>
<key>ExpirationDate</key><date>2027-11-23T04:40:22Z</date>
<key>Platform</key><array><string>OSX</string></array>
{all_devices}
<key>TeamIdentifier</key><array><string>ABCDE12345</string></array>
</dict></plist>
"#
    )
}

#[test]
fn a_decoded_profile_reads_into_exact_fields() {
    let one = "<string>PFX1234567</string>";
    let text = fixture("<key>ProvisionsAllDevices</key><true/>", one);
    let read = read_profile(b"signed".to_vec(), text.as_bytes()).expect("read");
    assert_eq!(read.bytes, b"signed");
    assert_eq!(read.team, "ABCDE12345");
    assert_eq!(read.prefix, "PFX1234567");
    assert!(read.all_devices);
    assert_eq!(read.devices, None);
    assert_eq!(read.expires, "2027-11-23T04:40:22Z");
    assert_eq!(read.platforms, ["OSX"]);
    // SHA-1 of "abc", from FIPS 180-1's own example.
    assert_eq!(
        read.certificates,
        ["A9993E364706816ABA3E25717850C26C9CD0D89D"]
    );
    assert_eq!(
        read.entitlements,
        json!({
            APP_ID: "PFX1234567.ai.kumochi.dev.brain",
            TEAM_ID: "ABCDE12345",
            GROUPS: ["PFX1234567.*"],
        })
    );
    let devices = "<key>ProvisionedDevices</key><array><string>00008103-0001</string></array>";
    let development = read_profile(Vec::new(), fixture(devices, one).as_bytes()).expect("read");
    assert!(!development.all_devices);
    assert_eq!(development.devices, Some(1));
    let listless = "<key>ProvisionedDevices</key><string>00008103-0001</string>";
    assert_eq!(
        read_profile(Vec::new(), fixture(listless, one).as_bytes())
            .map(drop)
            .expect_err("refused")
            .to_string(),
        "the profile's ProvisionedDevices has the wrong type"
    );
    let no_all = "<key>ProvisionsAllDevices</key><false/>";
    assert!(
        !read_profile(Vec::new(), fixture(no_all, one).as_bytes())
            .expect("read")
            .all_devices
    );
    let mistyped = "<key>ProvisionsAllDevices</key><string>true</string>";
    assert_eq!(
        read_profile(Vec::new(), fixture(mistyped, one).as_bytes())
            .map(drop)
            .expect_err("refused")
            .to_string(),
        "the profile's ProvisionsAllDevices has the wrong type"
    );
    for prefixes in ["", "<string>PFX1234567</string><string>ABCDE12345</string>"] {
        assert_eq!(
            read_profile(Vec::new(), fixture("", prefixes).as_bytes())
                .map(drop)
                .expect_err("refused")
                .to_string(),
            "the profile names one App ID prefix"
        );
    }
}

#[test]
fn a_signed_app_passes_only_without_a_debugger_with_the_runtime_and_its_profiles_ids() {
    let profile = prefixed();
    let signed = entitlements(PREFIX, TEAM, BRAIN, GROUP);
    audited(&profile, &signed, SIGNED_DISPLAY.as_bytes(), BRAIN, GROUP).expect("shippable");

    for value in [json!(true), json!(false)] {
        let mut planted = signed.clone();
        planted[GET_TASK_ALLOW] = value;
        assert_eq!(
            audit_refusal(&profile, &planted, SIGNED_DISPLAY),
            "it's signed with get-task-allow"
        );
    }

    let mut team_prefixed = signed.clone();
    team_prefixed[APP_ID] = json!(format!("{TEAM}.{BRAIN}"));
    assert_eq!(
        audit_refusal(&profile, &team_prefixed, SIGNED_DISPLAY),
        "its App ID isn't its profile's prefix and ai.kumochi.dev.brain"
    );
    let mut other_team = signed.clone();
    other_team[TEAM_ID] = json!("ZYXWV98765");
    assert_eq!(
        audit_refusal(&profile, &other_team, SIGNED_DISPLAY),
        "its team isn't its profile's"
    );
    for groups in [
        json!([format!("{TEAM}.{BRAIN}"), format!("{TEAM}.{GROUP}")]),
        json!([format!("{PREFIX}.{BRAIN}")]),
        json!([format!("{PREFIX}.{GROUP}"), format!("{PREFIX}.{BRAIN}")]),
        json!([
            format!("{PREFIX}.{BRAIN}"),
            format!("{PREFIX}.{GROUP}"),
            format!("{PREFIX}.x")
        ]),
    ] {
        let mut mismatched = signed.clone();
        mismatched[GROUPS] = groups;
        assert_eq!(
            audit_refusal(&profile, &mismatched, SIGNED_DISPLAY),
            "its keychain groups aren't its profile's"
        );
    }
    let mut narrow = profile_with_groups(&[&format!("{PREFIX}.{BRAIN}")]);
    narrow.prefix = PREFIX.to_owned();
    assert_eq!(
        audit_refusal(&narrow, &signed, SIGNED_DISPLAY),
        "its keychain groups aren't its profile's"
    );
    let mut extra = signed.clone();
    extra["com.apple.security.cs.disable-library-validation"] = json!(true);
    assert_eq!(
        audit_refusal(&profile, &extra, SIGNED_DISPLAY),
        "it carries entitlements beyond its App ID, team and groups"
    );

    let unhardened = SIGNED_DISPLAY.replace("flags=0x10000(runtime)", "flags=0x0(none)");
    assert_eq!(
        audit_refusal(&profile, &signed, &unhardened),
        "it isn't signed with the hardened runtime"
    );
}

fn profile_with_groups(groups: &[&str]) -> Profile {
    profile(json!({
        APP_ID: format!("{PREFIX}.{BRAIN}"),
        TEAM_ID: TEAM,
        GROUPS: groups,
    }))
}

#[test]
fn the_runtime_flag_is_read_from_exactly_one_code_directory() {
    let line = |flags: &str| {
        format!("CodeDirectory v=20500 size=1 flags={flags} hashes=1+7 location=embedded\n")
    };
    for (display, expected) in [
        (line("0x10000(runtime)"), true),
        (line("0x10002(adhoc,runtime)"), true),
        (line("0x0(none)"), false),
        (line("0x2(adhoc)"), false),
        (
            [line("0x10000(runtime)"), line("0x0(none)")].concat(),
            false,
        ),
        (
            [line("0x10000(runtime)"), line("0x10000(runtime)")].concat(),
            false,
        ),
        (line("10000(runtime)"), false),
        (line("0xzz(runtime)"), false),
        ("Identifier=ai.kumochi.dev.brain\n".to_owned(), false),
        (String::new(), false),
    ] {
        assert_eq!(hardened(display.as_bytes()), expected, "{display:?}");
    }
}

#[test]
fn a_planted_debugger_entitlement_and_a_missing_runtime_are_read_from_a_real_signature() {
    let root = crate::scratch::dir("mac-bundle-signature");
    let app = root.join("Probe.app");
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).expect("layout");
    // The test binary itself: a thin Mach-O, as the bundle's binaries are.
    let exe = std::env::current_exe().expect("this test binary");
    std::fs::copy(exe, contents.join("MacOS/probe")).expect("executable");
    let info = json!({
        "CFBundleIdentifier": BRAIN,
        "CFBundleExecutable": "probe",
        "CFBundlePackageType": "APPL",
    });
    std::fs::write(contents.join("Info.plist"), plist(&info).expect("info")).expect("info");
    let mut planted = entitlements(PREFIX, TEAM, BRAIN, GROUP);
    planted[GET_TASK_ALLOW] = json!(true);
    let file = root.join("planted.entitlements");
    std::fs::write(&file, plist(&planted).expect("entitlements")).expect("entitlements");
    let file = file.to_str().expect("a text path");
    // Ad-hoc, so no identity, keychain or network is involved, and nothing is launched.
    let adhoc = |runtime: bool| {
        let mut args = vec![
            "--digest-algorithm=sha256",
            "--force",
            "--sign",
            "-",
            "--timestamp=none",
            "--entitlements",
            file,
        ];
        if runtime {
            args.extend(["--options", "runtime"]);
        }
        command("/usr/bin/codesign", &args, &app).expect("ad-hoc signed");
        signature(&app).expect("read back")
    };
    let (signed, display) = adhoc(true);
    assert_eq!(signed, planted);
    assert!(thin(&display));
    assert!(hardened(&display));
    assert_eq!(
        audited(&prefixed(), &signed, &display, BRAIN, GROUP)
            .expect_err("refused")
            .to_string(),
        "it's signed with get-task-allow"
    );
    assert!(one_digest(&display));
    let (_, display) = adhoc(false);
    assert!(!hardened(&display));
}

#[test]
fn only_a_single_architecture_signature_counts_as_checked() {
    let thin_line = "Format=app bundle with Mach-O thin (arm64)\n";
    let universal = "Format=app bundle with Mach-O universal (x86_64 arm64)\n";
    assert!(thin(thin_line.as_bytes()));
    assert!(thin(SIGNED_DISPLAY.as_bytes()));
    for display in [
        universal.to_owned(),
        String::new(),
        [thin_line, thin_line].concat(),
    ] {
        assert!(!thin(display.as_bytes()), "{display:?}");
    }
    let widened = SIGNED_DISPLAY.replace("Mach-O thin (arm64)", "Mach-O universal (x86_64 arm64)");
    assert_eq!(
        audit_refusal(
            &prefixed(),
            &entitlements(PREFIX, TEAM, BRAIN, GROUP),
            &widened
        ),
        "it isn't a single-architecture app"
    );
}

#[test]
fn another_roles_profile_embedded_with_the_same_team_prefix_and_groups_is_refused() {
    let shell = "ai.kumochi.dev.shell";
    let signed = entitlements(PREFIX, TEAM, shell, GROUP);
    assert_eq!(
        audited(
            &prefixed(),
            &signed,
            SIGNED_DISPLAY.as_bytes(),
            shell,
            GROUP
        )
        .expect_err("refused")
        .to_string(),
        "its embedded profile isn't for ai.kumochi.dev.shell"
    );
    let mut teamless = prefixed();
    teamless.entitlements[TEAM_ID] = json!("ZYXWV98765");
    assert_eq!(
        audit_refusal(
            &teamless,
            &entitlements(PREFIX, TEAM, BRAIN, GROUP),
            SIGNED_DISPLAY
        ),
        "its team isn't its profile's"
    );
}

#[test]
fn a_slice_signed_with_alternate_digests_is_refused() {
    for (choices, expected) in [
        ("Hash choices=sha256\n", true),
        ("Hash choices=sha1,sha256\n", false),
        ("Hash choices=\n", false),
        ("Hash choices=sha256\nHash choices=sha256\n", false),
        ("", false),
    ] {
        assert_eq!(one_digest(choices.as_bytes()), expected, "{choices:?}");
    }
    let doubled = SIGNED_DISPLAY.replace("Hash choices=sha256", "Hash choices=sha1,sha256");
    assert_eq!(
        audit_refusal(
            &prefixed(),
            &entitlements(PREFIX, TEAM, BRAIN, GROUP),
            &doubled
        ),
        "it isn't signed with exactly one code directory"
    );

    let root = crate::scratch::dir("mac-bundle-digests");
    let exe = root.join("probe");
    std::fs::copy(std::env::current_exe().expect("this test binary"), &exe).expect("copy");
    let file = root.join("probe.entitlements");
    let granted = entitlements(PREFIX, TEAM, BRAIN, GROUP);
    std::fs::write(&file, plist(&granted).expect("entitlements")).expect("entitlements");
    let file = file.to_str().expect("a text path");
    let sign = |digests: &str| {
        command(
            "/usr/bin/codesign",
            &[
                "--force",
                "--sign",
                "-",
                "--timestamp=none",
                "--entitlements",
                file,
                digests,
            ],
            &exe,
        )
        .expect("ad-hoc signed");
        signature(&exe).expect("read back").1
    };
    assert!(one_digest(&sign("--digest-algorithm=sha256")));
    let both = sign("--digest-algorithm=sha1,sha256");
    assert!(thin(&both));
    assert!(!one_digest(&both));
}

#[test]
fn only_top_level_keys_mark_a_profiles_kind() {
    let one = "<string>PFX1234567</string>";
    let nested = fixture("", one).replace(
        "<key>keychain-access-groups</key>",
        "<key>ProvisionedDevices</key><array><string>z</string></array><key>ProvisionsAllDevices</key><true/><key>keychain-access-groups</key>",
    );
    let read = read_profile(Vec::new(), nested.as_bytes()).expect("read");
    assert_eq!(read.devices, None);
    assert!(!read.all_devices);
}
