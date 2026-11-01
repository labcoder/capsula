use std::path::Path;

use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use crate::test_support::Scratch;

use super::{EventKind, Failure, Interruption, Recovered, RunEnd, Store, StoreError};

const MARKER: &str = "zqxplantedmarker";

/// Every byte of the store's files, ASCII-lowercased.
fn raw_bytes(db: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        match std::fs::read(&path) {
            Ok(file) => bytes.extend(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("can't scan {}: {error}", path.display()),
        }
    }
    bytes.make_ascii_lowercase();
    bytes
}

fn contains_marker(db: &Path) -> bool {
    raw_bytes(db)
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes())
}

/// Starts a turn with the next ids, in a new thread titled `title` or in `thread`.
fn turn(store: &mut Store, thread: Result<ThreadId, &str>, text: &str) -> (ThreadId, RunId) {
    let (high_thread, high_run) = store.high_water().expect("high water");
    let run = RunId(high_run.0 + 1);
    let (thread, title) = match thread {
        Ok(thread) => (thread, None),
        Err(title) => (ThreadId(high_thread.0 + 1), Some(title)),
    };
    store.begin_turn(thread, title, run, text).expect("turn");
    (thread, run)
}

/// A thread with the marker in its title, a user turn, assistant text and a step body.
fn plant(store: &mut Store) -> (ThreadId, RunId) {
    let (thread, run) = turn(
        store,
        Err(&format!("Title {MARKER}")),
        &format!("ask {MARKER}"),
    );
    let answer = format!("ANSWER {}", MARKER.to_uppercase());
    store
        .append(thread, run, EventKind::AssistantText, Some(&answer), None)
        .expect("text");
    let body = format!(r#"{{"label":"{MARKER}"}}"#);
    store
        .append(
            thread,
            run,
            EventKind::SceneStep,
            Some("label"),
            Some(&body),
        )
        .expect("step");
    (thread, run)
}

/// Whether the WAL holds frames that weren't checkpointed, as after a crash.
fn wal_is_full(db: &Path) -> bool {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).is_ok_and(|meta| meta.len() > 0)
}

fn rows(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count")
}

fn run_row(store: &Store, run: RunId) -> (String, Option<String>) {
    store
        .conn
        .query_row(
            "SELECT state, reason FROM runs WHERE id = ?1",
            [super::sql_id(run.0).expect("id")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("run row")
}

#[test]
fn a_newer_schema_is_refused() {
    let scratch = Scratch::new("newer");
    drop(Store::open(&scratch.db()).expect("fresh store"));
    let conn = Connection::open(scratch.db()).expect("raw open");
    conn.pragma_update(None, "user_version", 99).expect("bump");
    drop(conn);
    assert!(matches!(
        Store::open(&scratch.db()),
        Err(StoreError::NewerSchema {
            found: 99,
            known: 1
        })
    ));
}

#[test]
fn reopening_keeps_threads_and_events() {
    let scratch = Scratch::new("reopen");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, _) = plant(&mut store);
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let threads = store.threads().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, thread);
    assert_eq!(threads[0].title, format!("Title {MARKER}"));
    assert_eq!(store.search(MARKER, 10).expect("search"), vec![thread]);
}

#[test]
fn a_turn_needs_new_ids_and_a_live_thread() {
    let scratch = Scratch::new("ids");
    let mut store = Store::open(&scratch.db()).expect("store");
    assert_eq!(store.high_water().expect("empty"), (ThreadId(0), RunId(0)));
    let (thread, run) = turn(&mut store, Err("t"), "first");
    assert_eq!((thread, run), (ThreadId(1), RunId(1)));
    assert!(matches!(
        store.begin_turn(ThreadId(1), Some("again"), RunId(2), "x"),
        Err(StoreError::StaleId)
    ));
    assert!(matches!(
        store.begin_turn(thread, None, run, "x"),
        Err(StoreError::StaleId)
    ));
    assert!(matches!(
        store.begin_turn(ThreadId(9), None, RunId(9), "x"),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.begin_turn(ThreadId(u64::MAX), None, RunId(9), "x"),
        Err(StoreError::NoThread)
    ));
    assert_eq!(
        store.high_water().expect("refusals issue nothing"),
        (ThreadId(1), RunId(1))
    );
}

#[test]
fn search_takes_any_text_and_skips_threads_being_deleted() {
    let scratch = Scratch::new("search");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (kept, _) = turn(&mut store, Err("kept"), "the red fox");
    let (doomed, _) = plant(&mut store);
    assert_eq!(store.search("RED fox", 10).expect("search"), vec![kept]);
    assert!(
        store.search("fox the", 10).expect("search").is_empty(),
        "a phrase, in order"
    );
    assert!(
        store
            .search(r#"" OR * NEAR("#, 10)
            .expect("syntax is text")
            .is_empty(),
        "FTS5 syntax in the words is matched literally"
    );
    assert!(store.search("", 10).expect("empty").is_empty());
    store.begin_delete(doomed).expect("begin");
    assert!(store.search(MARKER, 10).expect("search").is_empty());
    assert_eq!(store.threads().expect("threads").len(), 1);
}

#[test]
fn a_thread_being_deleted_takes_no_new_work() {
    let scratch = Scratch::new("fenced");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = turn(&mut store, Err("t"), "hi");
    let generation = store.begin_delete(thread).expect("begin");
    assert_eq!(generation.0, 1);
    assert!(matches!(
        store.begin_turn(thread, None, RunId(run.0 + 1), "late"),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.append(thread, run, EventKind::AssistantText, Some("late"), None),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.begin_delete(thread),
        Err(StoreError::NoThread)
    ));
}

#[test]
fn events_go_only_to_an_active_run_of_their_thread_and_never_change() {
    let scratch = Scratch::new("runs");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (a, _) = turn(&mut store, Err("a"), "x");
    let (b, run_b) = turn(&mut store, Err("b"), "y");
    assert!(matches!(
        store.append(a, run_b, EventKind::AssistantText, Some("x"), None),
        Err(StoreError::NoRun)
    ));
    store
        .end_run(run_b, RunEnd::Interrupted(Interruption::Stop))
        .expect("end");
    assert!(matches!(
        store.append(b, run_b, EventKind::SceneStep, Some("late"), None),
        Err(StoreError::NoRun)
    ));
    assert!(matches!(
        store.end_run(run_b, RunEnd::Done),
        Err(StoreError::NoRun)
    ));
    assert_eq!(
        run_row(&store, run_b),
        ("interrupted".to_owned(), Some("stop".to_owned())),
        "the first outcome stays"
    );
    assert!(
        store
            .conn
            .execute("UPDATE events SET text = 'changed'", [])
            .is_err(),
        "events are append-only"
    );
    store.begin_delete(b).expect("begin");
    store.finish_delete(b).expect("b deletes cleanly");
}

#[test]
fn recovery_fails_active_runs_and_keeps_ended_ones() {
    let scratch = Scratch::new("recover");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, ended) = turn(&mut store, Err("t"), "one");
    store
        .end_run(ended, RunEnd::Failed(Failure::Budget))
        .expect("end");
    let (_, active) = turn(&mut store, Ok(thread), "two");
    store
        .append(
            thread,
            active,
            EventKind::AssistantText,
            Some("partial"),
            None,
        )
        .expect("committed text");
    // The brain dies: its connection never closes, so nothing is checkpointed.
    let crashed = store;
    assert!(wal_is_full(&scratch.db()));

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store.recover().expect("recover"),
        Recovered {
            crashed_runs: 1,
            ..Recovered::default()
        }
    );
    assert_eq!(
        run_row(&store, ended),
        ("failed".to_owned(), Some("budget".to_owned()))
    );
    assert_eq!(
        run_row(&store, active),
        ("failed".to_owned(), Some("crash".to_owned()))
    );
    assert_eq!(store.search("partial", 10).expect("search"), vec![thread]);
    assert_eq!(store.recover().expect("again"), Recovered::default());
    drop(crashed);
}

#[test]
fn ids_are_never_reused_after_a_deletion() {
    let scratch = Scratch::new("runids");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = turn(&mut store, Err("t"), "x");
    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(store.high_water().expect("high water"), (thread, run));
    assert!(matches!(
        store.begin_turn(thread, Some("reused"), RunId(run.0 + 1), "x"),
        Err(StoreError::StaleId)
    ));
    assert!(matches!(
        store.begin_turn(ThreadId(thread.0 + 1), Some("u"), run, "x"),
        Err(StoreError::StaleId)
    ));
}

#[test]
fn a_deleted_thread_leaves_no_copy_in_the_files() {
    let scratch = Scratch::new("delete");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (kept, _) = turn(&mut store, Err("kept"), "unrelated");
    let (thread, _) = plant(&mut store);
    assert!(
        contains_marker(&scratch.db()),
        "the marker is on disk before the delete"
    );

    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
    assert!(!contains_marker(&scratch.db()), "found while open");
    for table in ["events", "runs", "threads"] {
        assert_eq!(
            rows(&store.conn, table),
            1,
            "{table} keeps only the other thread"
        );
    }
    drop(store);
    assert!(!contains_marker(&scratch.db()), "found after close");

    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.search(MARKER, 10).expect("search").is_empty());
    assert_eq!(store.search("unrelated", 10).expect("search"), vec![kept]);
}

#[test]
fn a_deletion_cut_short_by_a_crash_finishes_at_start() {
    let scratch = Scratch::new("cutshort");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, _) = plant(&mut store);
    store.begin_delete(thread).expect("begin");
    // The brain dies here, between the two transactions, without closing its connection.
    let crashed = store;
    assert!(wal_is_full(&scratch.db()));

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store.recover().expect("recover"),
        Recovered {
            finished_deletions: 1,
            ..Recovered::default()
        },
        "the deleted thread's run isn't counted as crashed"
    );
    assert!(!contains_marker(&scratch.db()));
    assert!(matches!(
        store.finish_delete(thread),
        Err(StoreError::NoThread)
    ));
    drop(crashed);
}
