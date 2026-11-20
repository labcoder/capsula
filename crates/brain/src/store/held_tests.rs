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

/// The value after `screen`, through every value. The match is exhaustive, so a new class or
/// value breaks the build here until it's chained in, and then fails the test until `ALL` has it.
fn after(screen: HeldScreen) -> Option<HeldScreen> {
    Some(HeldScreen::NotIncluded(match screen {
        HeldScreen::NotIncluded(ScreenClass::ModelCantSee) => return Some(HeldScreen::Withdrawn),
        HeldScreen::Withdrawn => return Some(HeldScreen::Screen),
        HeldScreen::Screen => return None,
        HeldScreen::Attached => ScreenClass::NotReady,
        HeldScreen::NotIncluded(ScreenClass::NotReady) => ScreenClass::NothingThere,
        HeldScreen::NotIncluded(ScreenClass::NothingThere) => ScreenClass::CantShare,
        HeldScreen::NotIncluded(ScreenClass::CantShare) => ScreenClass::Unreadable,
        HeldScreen::NotIncluded(ScreenClass::Unreadable) => ScreenClass::TooLarge,
        HeldScreen::NotIncluded(ScreenClass::TooLarge) => ScreenClass::Changed,
        HeldScreen::NotIncluded(ScreenClass::Changed) => ScreenClass::ModelCantSee,
    }))
}

#[test]
fn every_value_reads_back_as_itself() {
    let mut next = Some(HeldScreen::Attached);
    let mut values = 0;
    while let Some(screen) = next {
        assert_eq!(super::held::read(super::held::column(screen)), Some(screen));
        values += 1;
        next = after(screen);
    }
    assert_eq!(values, 10);
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
    assert_eq!(version, 9);
    assert_eq!(screen(&store, RunId(7)), None);
    assert_eq!(kinds(&store, ThreadId(5)), [PageEventKind::User]);
}

#[test]
fn a_held_turns_screen_is_stored_with_it_and_read_back_on_the_user_turn_only() {
    let scratch = Scratch::new("held-stored");
    let mut store = Store::open(&scratch.db()).expect("store");
    let not_included = HeldScreen::NotIncluded;
    for (held, column) in [
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

#[test]
fn a_withdrawal_marks_only_an_attached_turn() {
    let scratch = Scratch::new("held-withdrawn");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, attached) = held_run(&mut store, Some(HeldScreen::Attached));
    let (_, closed) = held_run(
        &mut store,
        Some(HeldScreen::NotIncluded(ScreenClass::NotReady)),
    );
    let (_, plain) = held_run(&mut store, None);
    assert_eq!(
        screen(&store, attached).as_deref(),
        Some("pending_attached")
    );
    for run in [attached, closed, plain] {
        store.withdraw_screen(run).expect("withdrawn");
    }
    assert_eq!(screen(&store, attached).as_deref(), Some("withdrawn"));
    assert_eq!(screen(&store, closed).as_deref(), Some("not_ready"));
    assert_eq!(screen(&store, plain), None);
    assert_eq!(
        kinds(&store, thread),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Withdrawn
        }]
    );
}

#[test]
fn a_screen_picture_is_stored_as_screen_and_an_attached_row_still_reads_as_the_window() {
    let scratch = Scratch::new("held-screen");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = held_run(&mut store, Some(HeldScreen::Screen));
    assert_eq!(screen(&store, run).as_deref(), Some("pending_screen"));
    store.sent_screen(run).expect("sent");
    assert_eq!(screen(&store, run).as_deref(), Some("screen"));
    assert_eq!(
        kinds(&store, thread),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Screen
        }]
    );
    let (older_thread, older) = held_run(&mut store, None);
    store
        .conn
        .execute(
            "UPDATE runs SET screen = 'attached' WHERE id = ?1",
            [i64::try_from(older.0).expect("small id")],
        )
        .expect("a row an earlier build wrote");
    assert_eq!(
        kinds(&store, older_thread),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Attached
        }]
    );
}

#[test]
fn an_unsent_picture_is_cleared_and_every_other_value_kept() {
    let scratch = Scratch::new("held-forgotten");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, attached) = held_run(&mut store, Some(HeldScreen::Attached));
    let (_, picture) = held_run(&mut store, Some(HeldScreen::Screen));
    let (_, other_attached) = held_run(&mut store, Some(HeldScreen::Attached));
    let (_, other_picture) = held_run(&mut store, Some(HeldScreen::Screen));
    let (_, closed) = held_run(
        &mut store,
        Some(HeldScreen::NotIncluded(ScreenClass::NotReady)),
    );
    let (_, withdrawn) = held_run(&mut store, Some(HeldScreen::Withdrawn));
    let (_, sent) = held_run(&mut store, Some(HeldScreen::Attached));
    store.sent_screen(sent).expect("sent");
    store.forget_screen(attached).expect("cleared");
    assert_eq!(screen(&store, attached), None);
    assert_eq!(screen(&store, picture).as_deref(), Some("pending_screen"));
    assert_eq!(
        screen(&store, other_attached).as_deref(),
        Some("pending_attached")
    );
    for run in [picture, closed, withdrawn, sent] {
        store.forget_screen(run).expect("cleared");
    }
    assert_eq!(screen(&store, attached), None);
    assert_eq!(screen(&store, picture), None);
    assert_eq!(
        screen(&store, other_attached).as_deref(),
        Some("pending_attached")
    );
    assert_eq!(
        screen(&store, other_picture).as_deref(),
        Some("pending_screen")
    );
    assert_eq!(
        screen(&store, sent).as_deref(),
        Some("attached"),
        "a sent mark stays"
    );
    assert_eq!(screen(&store, closed).as_deref(), Some("not_ready"));
    assert_eq!(screen(&store, withdrawn).as_deref(), Some("withdrawn"));
    assert_eq!(kinds(&store, thread), [PageEventKind::User]);
}

#[test]
fn a_screen_row_is_withdrawn_like_an_attached_one() {
    let scratch = Scratch::new("held-screen-withdrawn");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = held_run(&mut store, Some(HeldScreen::Screen));
    store.withdraw_screen(run).expect("withdrawn");
    assert_eq!(screen(&store, run).as_deref(), Some("withdrawn"));
    assert_eq!(
        kinds(&store, thread),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Withdrawn
        }]
    );
}

/// Opens the store at `scratch` the way a start after a crash does.
fn reopened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("reopened");
    store.recover().expect("recovered");
    store
}

#[test]
fn a_pending_picture_reads_as_a_plain_turn_until_a_send_may_have_written_it() {
    let scratch = Scratch::new("held-pending");
    let mut store = Store::open(&scratch.db()).expect("store");
    for (held, pending) in [
        (HeldScreen::Attached, "pending_attached"),
        (HeldScreen::Screen, "pending_screen"),
    ] {
        let (thread, run) = held_run(&mut store, Some(held));
        assert_eq!(screen(&store, run).as_deref(), Some(pending));
        assert_eq!(kinds(&store, thread), [PageEventKind::User]);
        store.sent_screen(run).expect("sent");
        store.sent_screen(run).expect("sent again");
        assert_eq!(
            kinds(&store, thread),
            [PageEventKind::HeldTurn { screen: held }]
        );
    }
    let (thread, closed) = held_run(
        &mut store,
        Some(HeldScreen::NotIncluded(ScreenClass::ModelCantSee)),
    );
    let (_, plain) = held_run(&mut store, None);
    store.sent_screen(closed).expect("nothing pending");
    store.sent_screen(plain).expect("nothing pending");
    assert_eq!(screen(&store, closed).as_deref(), Some("model_cant_see"));
    assert_eq!(screen(&store, plain), None);
    assert_eq!(
        kinds(&store, thread),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::NotIncluded(ScreenClass::ModelCantSee)
        }]
    );
}

#[test]
fn a_picture_still_pending_at_a_crash_is_recovered_as_sent() {
    let scratch = Scratch::new("held-pending-crash");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (live, _) = held_run(&mut store, Some(HeldScreen::Attached));
    let (ended, run) = held_run(&mut store, Some(HeldScreen::Screen));
    store
        .end_run(ended, run, &[], RunEnd::Done)
        .expect("ended with its send unsettled");
    drop(store);
    let store = reopened(&scratch);
    assert_eq!(
        kinds(&store, live),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Attached
        }]
    );
    assert_eq!(
        kinds(&store, ended),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Screen
        }]
    );
}

#[test]
fn a_picture_cleared_after_an_unsent_send_is_not_recovered_as_sent() {
    let scratch = Scratch::new("held-unsent-crash");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (forgotten, run) = held_run(&mut store, Some(HeldScreen::Attached));
    store.forget_screen(run).expect("cleared");
    let (withdrawn, run) = held_run(&mut store, Some(HeldScreen::Screen));
    store.withdraw_screen(run).expect("withdrawn");
    drop(store);
    let store = reopened(&scratch);
    assert_eq!(kinds(&store, forgotten), [PageEventKind::User]);
    assert_eq!(
        kinds(&store, withdrawn),
        [PageEventKind::HeldTurn {
            screen: HeldScreen::Withdrawn
        }]
    );
}
