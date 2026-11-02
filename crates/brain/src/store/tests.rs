use std::path::{Path, PathBuf};

use capsula_protocol::constants;
use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use super::{
    Deletion, EventKind, Failure, Interruption, NewEvent, Recovered, RunEnd, Store, StoreError,
};

/// A store file in its own temp folder, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "{}-store-{}-{name}",
            constants::OBJECT_PREFIX,
            std::process::id()
        ));
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
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some(&answer),
                body: None,
            }],
        )
        .expect("text");
    let body = format!(r#"{{"label":"{MARKER}"}}"#);
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::SceneStep,
                text: Some("label"),
                body: Some(&body),
            }],
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
            [super::sql_id(run.0, StoreError::NoRun).expect("id")],
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
    assert!(
        !format!("{threads:?}").contains(MARKER),
        "Debug leaves titles out"
    );
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
    assert!(store.search("red\0fox", 10).expect("NUL").is_empty());
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
        store.append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("late"),
                body: None
            }]
        ),
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
        store.append(
            a,
            run_b,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("x"),
                body: None
            }]
        ),
        Err(StoreError::NoRun)
    ));
    assert!(matches!(
        store.append(
            b,
            RunId(u64::MAX),
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("x"),
                body: None
            }]
        ),
        Err(StoreError::NoRun)
    ));
    store
        .end_run(b, run_b, &[], RunEnd::Interrupted(Interruption::Stop))
        .expect("end");
    assert!(matches!(
        store.append(
            b,
            run_b,
            &[NewEvent {
                kind: EventKind::SceneStep,
                text: Some("late"),
                body: None
            }]
        ),
        Err(StoreError::NoRun)
    ));
    assert!(matches!(
        store.end_run(b, run_b, &[], RunEnd::Done),
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
    assert_eq!(
        store.finish_delete(b).expect("b deletes cleanly"),
        Deletion::Finished
    );
}

#[test]
fn recovery_fails_active_runs_and_keeps_ended_ones() {
    let scratch = Scratch::new("recover");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, ended) = turn(&mut store, Err("t"), "one");
    store
        .end_run(thread, ended, &[], RunEnd::Failed(Failure::Budget))
        .expect("end");
    let (_, active) = turn(&mut store, Ok(thread), "two");
    store
        .append(
            thread,
            active,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("partial"),
                body: None,
            }],
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
    let newest_event = |store: &Store| -> i64 {
        store
            .conn
            .query_row("SELECT max(id) FROM events", [], |row| row.get(0))
            .expect("max id")
    };
    let deleted_event = newest_event(&store);
    store.begin_delete(thread).expect("begin");
    assert_eq!(
        store.finish_delete(thread).expect("finish"),
        Deletion::Finished
    );
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
    turn(&mut store, Err("u"), "y");
    assert!(
        newest_event(&store) > deleted_event,
        "event ids aren't reused either"
    );
}

#[test]
fn a_deletion_that_fails_at_commit_leaves_the_thread_live() {
    let scratch = Scratch::new("commitfail");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, _) = turn(&mut store, Err("t"), "x");
    // The deletion's own update breaks a foreign key that is only checked at commit.
    store
        .conn
        .execute_batch(
            "CREATE TABLE fence (
                 thread INTEGER REFERENCES threads (id) DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER fence_on_delete AFTER UPDATE OF state ON threads BEGIN
                 INSERT INTO fence VALUES (-1);
             END;",
        )
        .expect("trap");
    assert!(matches!(
        store.begin_delete(thread),
        Err(StoreError::Sql(_))
    ));
    let threads = store.threads().expect("threads");
    assert_eq!(threads.len(), 1, "the thread is still live");
    assert_eq!(threads[0].id, thread);
    turn(&mut store, Ok(thread), "still takes work");
}

#[test]
fn a_deletion_that_fails_again_at_start_stays_hidden_and_is_reported() {
    let scratch = Scratch::new("stuck");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, _) = plant(&mut store);
    store.begin_delete(thread).expect("begin");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER keep_threads BEFORE DELETE ON threads BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trap");
    assert_eq!(
        store.recover().expect("recover"),
        Recovered {
            failed_deletions: 1,
            ..Recovered::default()
        },
        "the stuck thread's run isn't counted as crashed"
    );
    assert!(store.threads().expect("threads").is_empty());
    assert!(store.search(MARKER, 10).expect("search").is_empty());
}

/// The deleted rows are zeroed in every file. At this scale no page splits and no FTS segment
/// spans two leaves, so it can't see the stale copies that page gaps and `events_fts_idx` keys
/// keep at real sizes; the purge PR's test covers those.
#[test]
fn a_deleted_threads_rows_are_zeroed_in_the_files() {
    let scratch = Scratch::new("delete");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (kept, _) = turn(&mut store, Err("kept"), "unrelated");
    let (thread, _) = plant(&mut store);
    assert!(
        contains_marker(&scratch.db()),
        "the marker is on disk before the delete"
    );

    store.begin_delete(thread).expect("begin");
    assert_eq!(
        store.finish_delete(thread).expect("finish"),
        Deletion::Finished
    );
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

#[test]
fn a_checkpoint_a_reader_blocks_leaves_the_deletion_pending() {
    let scratch = Scratch::new("blocked");
    let mut store = Store::open(&scratch.db()).expect("store");
    // The checkpoint would otherwise wait out the 5 s busy timeout before giving up.
    store
        .conn
        .busy_timeout(std::time::Duration::ZERO)
        .expect("timeout");
    let (first, _) = plant(&mut store);
    // Without the marker, so the raw scan below can only find the first thread's residue.
    let (second, _) = turn(&mut store, Err("plain"), "plain");
    store.begin_delete(first).expect("begin");
    store.begin_delete(second).expect("begin");
    let reader = Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");

    assert_eq!(
        store.finish_delete(first).expect("finish"),
        Deletion::Pending
    );
    assert!(
        contains_marker(&scratch.db()),
        "the old pages are still in the files"
    );
    assert_eq!(
        store.recover().expect("recover"),
        Recovered {
            pending_deletions: 1,
            ..Recovered::default()
        }
    );

    reader.execute_batch("COMMIT").expect("end snapshot");
    assert_eq!(store.recover().expect("retry"), Recovered::default());
    assert!(!contains_marker(&scratch.db()));
}

#[test]
fn an_ending_stores_its_events_and_outcome_together_or_not_at_all() {
    let scratch = Scratch::new("ending");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = turn(&mut store, Err("t"), "hi");
    let (other, _) = turn(&mut store, Err("u"), "hey");
    let tail = [NewEvent {
        kind: EventKind::AssistantText,
        text: Some("tail"),
        body: None,
    }];
    let other_updated = |store: &Store| -> i64 {
        store
            .conn
            .query_row(
                "SELECT updated_ms FROM threads WHERE id = ?1",
                [super::sql_id(other.0, StoreError::NoThread).expect("id")],
                |row| row.get(0),
            )
            .expect("updated_ms")
    };
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 0", [])
        .expect("reset");
    assert!(matches!(
        store.end_run(other, run, &tail, RunEnd::Done),
        Err(StoreError::NoRun)
    ));
    assert_eq!(
        other_updated(&store),
        0,
        "the touch rolled back with the rest"
    );
    store.begin_delete(thread).expect("begin");
    assert!(matches!(
        store.end_run(thread, run, &tail, RunEnd::Done),
        Err(StoreError::NoThread)
    ));
    assert_eq!(run_row(&store, run), ("active".to_owned(), None));
    assert_eq!(rows(&store.conn, "events"), 2, "only the two turns");
    store
        .end_run(thread, run, &[], RunEnd::Failed(Failure::Store))
        .expect("an outcome alone needs no live thread");
    assert_eq!(
        run_row(&store, run),
        ("failed".to_owned(), Some("store".to_owned()))
    );
}
