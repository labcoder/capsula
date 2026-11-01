use std::path::{Path, PathBuf};

use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use super::{EventKind, Recovered, Store, StoreError};

/// A store file in its own temp folder, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("capsula-store-{}-{name}", std::process::id()));
        // A folder left by an earlier aborted run would make the test start from old data.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("old scratch folder removable");
        }
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("store.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("scratch folder left behind: {error}");
        }
    }
}

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

/// A thread with the marker in its title, a user turn, assistant text and a step label.
fn plant(store: &mut Store) -> ThreadId {
    let thread = store
        .create_thread(&format!("Title {MARKER}"))
        .expect("thread");
    let run = store.start_run(thread).expect("run");
    store
        .append(
            thread,
            Some(run),
            EventKind::User,
            Some(&format!("ask {MARKER}")),
            None,
        )
        .expect("user");
    store
        .append(
            thread,
            Some(run),
            EventKind::AssistantText,
            Some(&format!("ANSWER {}", MARKER.to_uppercase())),
            None,
        )
        .expect("text");
    store
        .append(
            thread,
            Some(run),
            EventKind::SceneStep,
            Some("label"),
            Some(&format!(r#"{{"label":"{MARKER}"}}"#)),
        )
        .expect("step");
    thread
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
    let thread = plant(&mut store);
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let threads = store.threads().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, thread);
    assert_eq!(threads[0].title, format!("Title {MARKER}"));
    assert_eq!(store.search(MARKER, 10).expect("search"), vec![thread]);
}

#[test]
fn search_takes_any_text_and_skips_threads_being_deleted() {
    let scratch = Scratch::new("search");
    let mut store = Store::open(&scratch.db()).expect("store");
    let kept = store.create_thread("kept").expect("thread");
    store
        .append(kept, None, EventKind::User, Some("the red fox"), None)
        .expect("event");
    let doomed = plant(&mut store);
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
    let thread = store.create_thread("t").expect("thread");
    let generation = store.begin_delete(thread).expect("begin");
    assert_eq!(generation.0, 1);
    assert!(matches!(store.start_run(thread), Err(StoreError::NoThread)));
    assert!(matches!(
        store.append(thread, None, EventKind::User, Some("late"), None),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.begin_delete(thread),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.start_run(ThreadId(u64::MAX)),
        Err(StoreError::NoThread)
    ));
}

#[test]
fn events_go_only_to_an_active_run_of_their_thread_and_never_change() {
    let scratch = Scratch::new("runs");
    let mut store = Store::open(&scratch.db()).expect("store");
    let a = store.create_thread("a").expect("thread");
    let b = store.create_thread("b").expect("thread");
    let run_b = store.start_run(b).expect("run");
    assert!(matches!(
        store.append(a, Some(run_b), EventKind::User, Some("x"), None),
        Err(StoreError::NoRun)
    ));
    store
        .conn
        .execute(
            "UPDATE runs SET state = 'interrupted' WHERE id = ?1",
            [super::sql_id(run_b.0).expect("id")],
        )
        .expect("end");
    assert!(matches!(
        store.append(b, Some(run_b), EventKind::SceneStep, Some("late"), None),
        Err(StoreError::NoRun)
    ));
    store
        .append(b, None, EventKind::User, Some("kept"), None)
        .expect("event");
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
    let thread = store.create_thread("t").expect("thread");
    let ended = store.start_run(thread).expect("run");
    store
        .conn
        .execute(
            "UPDATE runs SET state = 'done' WHERE id = ?1",
            [super::sql_id(ended.0).expect("id")],
        )
        .expect("end");
    let active = store.start_run(thread).expect("run");
    store
        .append(
            thread,
            Some(active),
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
    let state = |run: RunId| -> (String, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT state, reason FROM runs WHERE id = ?1",
                [super::sql_id(run.0).expect("id")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("run row")
    };
    assert_eq!(state(ended), ("done".to_owned(), None));
    assert_eq!(
        state(active),
        ("failed".to_owned(), Some("crash".to_owned()))
    );
    assert_eq!(store.search("partial", 10).expect("search"), vec![thread]);
    assert_eq!(store.recover().expect("again"), Recovered::default());
    drop(crashed);
}

#[test]
fn run_ids_are_never_reused_after_a_deletion() {
    let scratch = Scratch::new("runids");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = store.create_thread("t").expect("thread");
    let highest = store.start_run(thread).expect("run");
    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen");
    let next = store.create_thread("u").expect("thread");
    assert!(next > thread, "thread ids aren't reused either");
    assert!(store.start_run(next).expect("run") > highest);
}

#[test]
fn a_deleted_thread_leaves_no_copy_in_the_files() {
    let scratch = Scratch::new("delete");
    let mut store = Store::open(&scratch.db()).expect("store");
    let kept = store.create_thread("kept").expect("thread");
    let kept_run = store.start_run(kept).expect("run");
    store
        .append(
            kept,
            Some(kept_run),
            EventKind::User,
            Some("unrelated"),
            None,
        )
        .expect("event");
    let thread = plant(&mut store);
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
    let thread = plant(&mut store);
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
