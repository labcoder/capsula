use std::fs;

use kumochi_protocol::constants::ProductIdentity;

use super::{PLAN, prepared, startup};
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
    prepared(&folder, SAFE_PLAN).expect("prepared plan");
    assert_eq!(
        fs::read_to_string(folder.join(PLAN)).expect("plan"),
        SAFE_PLAN
    );
}
