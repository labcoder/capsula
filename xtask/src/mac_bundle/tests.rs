use super::*;

const TEAM: &str = "ABCDE12345";
const NOW: &str = "2026-11-26T06:00:00Z";
const BRAIN: &str = "ai.kumochi.dev.brain";
const GROUP: &str = "ai.kumochi.dev.instance";
const SHA1: &str = "0123456789abcdef0123456789ABCDEF01234567";

fn args(list: &[&str]) -> Result<Args> {
    parse(list.iter().map(|&arg| arg.to_owned()))
}

fn profile(entitlements: Value) -> Profile {
    Profile {
        team: TEAM.to_owned(),
        entitlements,
        expires: "2027-11-23T04:40:22Z".to_owned(),
        platforms: vec!["OSX".to_owned()],
    }
}

fn granted(groups: &[&str]) -> Value {
    json!({ APP_ID: format!("{TEAM}.{BRAIN}"), TEAM_ID: TEAM, GROUPS: groups })
}

fn refusal(profile: &Profile) -> String {
    check(profile, BRAIN, GROUP, NOW)
        .expect_err("refused")
        .to_string()
}

#[test]
fn every_input_is_named_once_and_the_identity_is_an_exact_hash() {
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
        refused(&["--team", "x"]),
        "mac-bundle doesn't take \"--team\""
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
fn a_current_macos_profile_for_exactly_the_app_passes() {
    for groups in [
        vec![format!("{TEAM}.*")],
        vec![format!("{TEAM}.{BRAIN}"), format!("{TEAM}.{GROUP}")],
    ] {
        let groups: Vec<&str> = groups.iter().map(String::as_str).collect();
        check(&profile(granted(&groups)), BRAIN, GROUP, NOW).expect("allowed");
    }
}

#[test]
fn a_profile_that_doesnt_authorize_the_app_is_refused() {
    let wildcard = format!("{TEAM}.*");
    let mut expired = profile(granted(&[&wildcard]));
    expired.expires = NOW.to_owned();
    assert_eq!(refusal(&expired), "it has expired");

    let mut ios = profile(granted(&[&wildcard]));
    ios.platforms = vec!["iOS".to_owned()];
    assert_eq!(refusal(&ios), "it isn't a macOS profile");

    let mut other_team = granted(&[&wildcard]);
    other_team[TEAM_ID] = json!("ZYXWV98765");
    assert_eq!(
        refusal(&profile(other_team)),
        "its team doesn't match its entitlements"
    );

    for app in [
        format!("{TEAM}.*"),
        format!("{TEAM}.ai.kumochi.dev.shell"),
        BRAIN.to_owned(),
    ] {
        let mut entitlements = granted(&[&wildcard]);
        entitlements[APP_ID] = json!(app);
        assert_eq!(
            refusal(&profile(entitlements)),
            "it isn't for exactly ai.kumochi.dev.brain"
        );
    }

    let own_only = format!("{TEAM}.{BRAIN}");
    let foreign = "ZYXWV98765.*";
    for groups in [vec![own_only.as_str()], vec![foreign], vec![]] {
        let expected = if groups.is_empty() || groups == [foreign] {
            ".ai.kumochi.dev.brain"
        } else {
            ".ai.kumochi.dev.instance"
        };
        assert_eq!(
            refusal(&profile(granted(&groups))),
            format!("it doesn't allow the keychain group {expected}")
        );
    }
    let mut no_groups = granted(&[]);
    no_groups.as_object_mut().expect("an object").remove(GROUPS);
    assert_eq!(
        refusal(&profile(no_groups)),
        "it doesn't allow the keychain group .ai.kumochi.dev.brain"
    );
}

#[test]
fn each_role_is_signed_for_its_own_group_and_the_shared_one() {
    assert_eq!(
        entitlements(TEAM, BRAIN, GROUP),
        json!({
            "com.apple.application-identifier": "ABCDE12345.ai.kumochi.dev.brain",
            "com.apple.developer.team-identifier": "ABCDE12345",
            "keychain-access-groups": ["ABCDE12345.ai.kumochi.dev.brain", "ABCDE12345.ai.kumochi.dev.instance"],
        })
    );
}

#[test]
fn only_the_brain_is_a_background_app_and_both_plists_read_back() {
    let brain = Role {
        id: BRAIN,
        exe: BRAIN_EXE,
        profile: Path::new("b"),
        background: true,
    };
    let shell = Role {
        id: "ai.kumochi.dev.shell",
        exe: SHELL_EXE,
        profile: Path::new("s"),
        background: false,
    };
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
    assert!(plist(&json!({ "n": 1 })).is_err());
}
