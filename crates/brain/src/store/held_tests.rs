use kumochi_protocol::message::{HeldScreen, PageEventKind, RunId, ScreenClass, ThreadId};
use rusqlite::Connection;

use super::{EventKind, NewEvent, RunEnd, Store, schema};
use crate::test_support::Scratch;

fn screen(store: &Store, run: RunId) -> Option<String> {
    store
        .conn
        .query_row(
            "SELECT screen FROM runs WHERE id = ?1",
            [i64::try_from(run.0).expect("small id")],
            |row| row.get(0),
        )
        .expect("the run's row")
}

/// A new thread whose one run's turn carried `held`.
fn held_run(store: &mut Store, held: Option<HeldScreen>) -> (ThreadId, RunId) {
    let (high_thread, high_run) = store.high_water().expect("high water");
    let (thread, run) = (ThreadId(high_thread.0 + 1), RunId(high_run.0 + 1));
    store
        .begin_held_turn(thread, Some("t"), run, "words", held)
        .expect("turn");
    (thread, run)
}

/// The kinds on the thread's page, oldest first.
fn kinds(store: &Store, thread: ThreadId) -> Vec<PageEventKind> {
    let (rows, _) = store.pages().events_page(thread, None, 10).expect("page");
    rows.iter().rev().map(|row| row.kind).collect()
}

#[test]
fn a_v7_store_reopens_at_v8_with_its_runs_screen_unknown() {
    let scratch = Scratch::new("held-upgrade");
    std::fs::create_dir_all(scratch.db().parent().expect("a folder")).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    schema::migrate_to(&mut conn, 7).expect("V7");
    conn.execute_batch(
        "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1);
         INSERT INTO runs (id, thread, state, started_ms, ended_ms) VALUES (7, 5, 'done', 1, 2);
         INSERT INTO events (id, thread, run, kind, text, created_ms)
             VALUES (9, 5, 7, 'user', 'words', 3);",
    )
    .expect("V7 rows");
    drop(conn);
    let store = Store::open(&scratch.db()).expect("opens at the current schema");
    let version: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 8);
    assert_eq!(screen(&store, RunId(7)), None);
    assert_eq!(kinds(&store, ThreadId(5)), [PageEventKind::User]);
}

#[test]
fn a_held_turns_screen_is_stored_with_it_and_read_back_on_the_user_turn_only() {
    let scratch = Scratch::new("held-stored");
    let mut store = Store::open(&scratch.db()).expect("store");
    let not_included = HeldScreen::NotIncluded;
    for (held, column) in [
        (HeldScreen::Attached, "attached"),
        (not_included(ScreenClass::NotReady), "not_ready"),
        (not_included(ScreenClass::NothingThere), "nothing_there"),
        (not_included(ScreenClass::CantShare), "cant_share"),
        (not_included(ScreenClass::Unreadable), "unreadable"),
        (not_included(ScreenClass::TooLarge), "too_large"),
        (not_included(ScreenClass::Changed), "changed"),
        (not_included(ScreenClass::ModelCantSee), "model_cant_see"),
    ] {
        let (thread, run) = held_run(&mut store, Some(held));
        assert_eq!(screen(&store, run).as_deref(), Some(column));
        let answer = NewEvent {
            kind: EventKind::AssistantText,
            text: Some("answer"),
            body: None,
        };
        store
            .end_run(thread, run, &[answer], RunEnd::Done)
            .expect("ended");
        assert_eq!(
            kinds(&store, thread),
            [
                PageEventKind::HeldTurn { screen: held },
                PageEventKind::AssistantText
            ]
        );
    }
    let (thread, run) = held_run(&mut store, None);
    assert_eq!(screen(&store, run), None);
    assert_eq!(kinds(&store, thread), [PageEventKind::User]);
}

#[test]
fn a_value_this_build_does_not_know_reads_as_a_plain_turn() {
    let scratch = Scratch::new("held-unknown");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = held_run(&mut store, None);
    store
        .conn
        .execute(
            "UPDATE runs SET screen = 'a_later_class' WHERE id = ?1",
            [i64::try_from(run.0).expect("small id")],
        )
        .expect("a newer value");
    assert_eq!(kinds(&store, thread), [PageEventKind::User]);
}
