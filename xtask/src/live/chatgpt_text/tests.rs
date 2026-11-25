use std::fs;

use kumochi_protocol::constants::ProductIdentity;

use super::{PAIR, PLAN, Startup, prepared, startup};
use crate::live::{Check, Entry, entry};

#[test]
fn the_first_text_command_has_an_explicit_dev_only_budget() {
    for (n, budget) in [("1", 1), ("60", 60)] {
        let words = ["chatgpt-text", "first-text", "--budget", n].map(str::to_owned);
        assert_eq!(
            entry(ProductIdentity::DEV, &words).expect("explicit budget"),
            Entry::Prepare(Check::ChatGptText(budget))
        );
        let inner = ["chatgpt-text", "first-text", "--budget", n, "--inner"].map(str::to_owned);
        assert_eq!(
            entry(ProductIdentity::DEV, &inner).expect("explicit inner"),
            Entry::Inner(Check::ChatGptText(budget))
        );
        for identity in [ProductIdentity::RELEASE, ProductIdentity::GATE] {
            for args in [words.as_slice(), inner.as_slice()] {
                assert!(entry(identity, args).is_err(), "{identity:?}: {args:?}");
            }
        }
    }
    for value in ["0", "61", "", "-1", "+1", "unlimited"] {
        let words = ["chatgpt-text", "first-text", "--budget", value].map(str::to_owned);
        assert!(entry(ProductIdentity::DEV, &words).is_err(), "{value:?}");
    }
}

pub(super) const TEXT_ONLY: &str =
    "[claude_code]\nenabled=false\n[voice]\nhold_to_talk=false\nscreen_holds=false\n";
pub(super) const SAFE_PLAN: &str = "model_budget=19\nconnections=chatgpt\nchatgpt_startup_identity_validation=one possible check not visible in metadata (discovery and up to two signing-key requests)\n";

/// The identities of a prepared profile pair, for tests that don't read real folders.
pub(super) const PAIR_FIXTURE: [u8; 24] = [7; 24];

/// `plan` as made from [`PAIR_FIXTURE`].
pub(super) fn fixture(plan: String) -> Startup {
    Startup {
        plan,
        pair: PAIR_FIXTURE,
    }
}

pub(super) fn clean_metadata() -> (rusqlite::Connection, rusqlite::Connection) {
    let db = rusqlite::Connection::open_in_memory().expect("fixture history");
    db.execute_batch(
        "CREATE TABLE threads (kind TEXT, state TEXT, model TEXT);
        INSERT INTO threads VALUES ('main','live',NULL);
        CREATE TABLE runs (id); CREATE TABLE events (id); CREATE TABLE summaries (id);
        CREATE TABLE actions (stage); CREATE TABLE purge_pending (id);",
    )
    .expect("clean history");
    let authority = rusqlite::Connection::open_in_memory().expect("fixture authority");
    authority.execute_batch(r#"PRAGMA user_version = 3;
        CREATE TABLE generations (kind TEXT, state TEXT);
        INSERT INTO generations VALUES ('host','active'),('chatgpt','active');
        CREATE TABLE control (id, marker);
        INSERT INTO control VALUES (1,'{"v":1,"sign_out":null,"sweep":false,"request_pending":false,"outcome_unknown":false,"notice":false}');"#)
        .expect("token-free authority");
    (db, authority)
}

#[test]
fn an_ordinary_profile_claude_cleanup_marker_refuses_startup() {
    let folder = crate::scratch::dir("first-text-cleanup-marker");
    startup::check_claude_cleanup(&folder).expect("no marker");
    fs::write(
        folder.join(kumochi_protocol::constants::CLAUDE_CODE_DELETING),
        [],
    )
    .expect("metadata fixture");
    assert_eq!(
        startup::check_claude_cleanup(&folder)
            .expect_err("cleanup owed")
            .to_string(),
        "Claude Code cleanup is pending or unknown"
    );
}

#[test]
fn prepare_records_the_inspectable_plan_in_the_fresh_folder() {
    let folder = crate::scratch::dir("first-text-prepare");
    prepared(&folder, &fixture(SAFE_PLAN.to_owned())).expect("prepared plan");
    assert_eq!(
        fs::read_to_string(folder.join(PLAN)).expect("plan"),
        SAFE_PLAN
    );
    assert_eq!(fs::read(folder.join(PAIR)).expect("pair"), PAIR_FIXTURE);
}

/// The override pair as a fixture: `local` and `roaming` under `profile`, set as named.
fn variables(
    profile: &std::path::Path,
    set: [bool; 2],
) -> impl Fn(&str) -> Option<std::ffi::OsString> {
    let profile = profile.to_owned();
    move |name| match name {
        "KUMOCHI_LIVE_TEXT_PROFILE" => Some(profile.clone().into()),
        "KUMOCHI_GATE_LOCALAPPDATA" if set[0] => Some(profile.join("local").into()),
        "KUMOCHI_GATE_APPDATA" if set[1] => Some(profile.join("roaming").into()),
        _ => None,
    }
}

#[test]
fn a_half_pair_or_a_profile_outside_local_app_data_is_refused() {
    let profile = crate::scratch::local("first-text-profile");
    for half in ["local", "roaming"] {
        fs::create_dir(profile.join(half)).expect("half");
    }
    let (local, roaming) =
        startup::profile(variables(&profile, [true, true])).expect("a whole pair below the anchor");
    assert_eq!(local.file_name(), Some("local".as_ref()));
    assert_eq!(roaming.file_name(), Some("roaming".as_ref()));
    for set in [[true, false], [false, true], [false, false]] {
        assert_eq!(
            startup::profile(variables(&profile, set))
                .expect_err("half a pair")
                .to_string(),
            "first-text needs both KUMOCHI_GATE_LOCALAPPDATA and KUMOCHI_GATE_APPDATA"
        );
    }
    let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", profile.display()));
    let dotted = profile
        .join("..")
        .join(profile.file_name().expect("a named profile"));
    for spelled in [&verbatim, &dotted] {
        assert_eq!(
            startup::profile(variables(spelled, [true, true]))
                .expect_err("not as plainly given")
                .to_string(),
            "profile folders must be absolute paths on a local drive"
        );
    }
    // The checkout is outside Local AppData on Rez and in CI.
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(
        startup::profile(variables(outside, [true, true]))
            .expect_err("outside the anchor")
            .to_string(),
        "the disposable profile must lie below local app data, outside its own data"
    );
}

#[test]
fn identities_tell_two_profile_pairs_apart_and_refuse_a_link() {
    let first = crate::scratch::local("first-text-identity-a");
    let second = crate::scratch::local("first-text-identity-b");
    for profile in [&first, &second] {
        for half in ["local", "roaming"] {
            fs::create_dir(profile.join(half)).expect("half");
        }
    }
    let pair = |profile: &std::path::Path| {
        startup::identities(&profile.join("local"), &profile.join("roaming")).map(|(pair, _)| pair)
    };
    let one = pair(&first).expect("the first pair");
    assert_eq!(pair(&first).expect("the same pair again"), one);
    assert_ne!(pair(&second).expect("the second pair"), one);
    let linked = crate::scratch::local("first-text-identity-linked");
    fs::create_dir(linked.join("local")).expect("half");
    fs::create_dir(linked.join("roaming")).expect("an empty folder");
    kumochi_storage::set_junction(&linked.join("roaming"), &second.join("roaming"))
        .expect("a junction");
    assert_eq!(
        pair(&linked).expect_err("a linked half").to_string(),
        "cannot hold the profile folders"
    );
}
