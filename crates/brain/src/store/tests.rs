use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kumochi_protocol::message::{Deleted, RunId, ThreadId, ThreadKind};
use rusqlite::Connection;

use crate::context::ConnectionTag;
use crate::engines::{CallId, Opaque};
use crate::test_support::Scratch;

use super::{
    Cancel, EventKind, Failure, Interruption, NewEvent, Recovered, RunEnd, SceneTarget, Store,
    StoreError, Swap, checkpoint_now, purge as steps,
};

const MARKER: &str = "zqxplantedmarker";

/// Every byte of the store's files, ASCII-lowercased.
pub(super) fn raw_bytes(db: &Path) -> Vec<u8> {
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

/// A thread with the marker in its title, a user turn, assistant text, a step body and its own
/// summary.
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
    // A stored round: a native item and a result, each holding the marker.
    let item = format!(r#"{{"type":"message","content":"{MARKER}"}}"#);
    let call = CallId::new("call-1").expect("short");
    store
        .append_round(
            (thread, run),
            ConnectionTag(7),
            None,
            &[Opaque::new(item.into_bytes()).expect("small")],
            &[(&call, MARKER)],
        )
        .expect("round");
    // A scene of one step: its `scene_begin`, a `scene_step` whose label is the marker, and its
    // `scene_cleared`.
    let scene = store
        .commit_step(
            (thread, run),
            None,
            SceneTarget::New { evict: None },
            0,
            MARKER,
        )
        .expect("step");
    store.clear_scenes(&[scene]).expect("cleared");
    let covered = kumochi_protocol::message::EventId(1);
    assert!(
        store
            .store_summary(
                thread,
                super::Generation(0),
                (covered, covered),
                &format!("summary {MARKER}"),
                None
            )
            .expect("summary")
    );
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

/// What the store's thread does with an owed purge, in one go.
pub(super) fn purge(store: &mut Store) -> Swap {
    let copied = clean_copy(store);
    let swapped = store.swap(&copied).expect("swap");
    store.leftovers().remove().expect("purge files removed");
    swapped
}

/// A clean copy of a store owing a purge.
fn clean_copy(store: &Store) -> super::Copied {
    store
        .owed()
        .expect("owed")
        .expect("a purge is owed and no run is active")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("nothing stopped it")
}

/// `db` with `suffix` appended.
fn beside(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// The purge's copies left beside the store.
fn purge_files(db: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(db.parent().expect("a folder"))
        .expect("folder")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.to_string_lossy().contains(".purge-"))
        .collect()
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
            known: 5
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
    let threads = store
        .pages()
        .thread_page(None, 50)
        .map(|(rows, _)| rows)
        .expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, thread);
    assert_eq!(threads[0].title, format!("Title {MARKER}"));
    assert!(
        !format!("{threads:?}").contains(MARKER),
        "Debug leaves titles out"
    );
    assert_eq!(
        store.pages().search_ids(MARKER).expect("search"),
        vec![thread]
    );
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
    assert_eq!(
        store.pages().search_ids("RED fox").expect("search"),
        vec![kept]
    );
    assert!(
        store
            .pages()
            .search_ids("fox the")
            .expect("search")
            .is_empty(),
        "a phrase, in order"
    );
    assert!(
        store
            .pages()
            .search_ids(r#"" OR * NEAR("#)
            .expect("syntax is text")
            .is_empty(),
        "FTS5 syntax in the words is matched literally"
    );
    assert!(store.pages().search_ids("").expect("empty").is_empty());
    assert!(
        store
            .pages()
            .search_ids("red\0fox")
            .expect("NUL")
            .is_empty()
    );
    store.begin_delete(doomed).expect("begin");
    assert!(store.pages().search_ids(MARKER).expect("search").is_empty());
    assert_eq!(
        store
            .pages()
            .thread_page(None, 50)
            .map(|(rows, _)| rows)
            .expect("threads")
            .len(),
        1
    );
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
    store.finish_delete(b).expect("b deletes cleanly");
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
    assert_eq!(
        store.pages().search_ids("partial").expect("search"),
        vec![thread]
    );
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
    let threads = store
        .pages()
        .thread_page(None, 50)
        .map(|(rows, _)| rows)
        .expect("threads");
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
    let (rows, _) = store.pages().thread_page(None, 50).expect("threads");
    let listed: Vec<_> = rows.iter().map(|row| row.kind).collect();
    assert_eq!(listed, [ThreadKind::Main], "only the main thread is listed");
    assert!(store.pages().search_ids(MARKER).expect("search").is_empty());
}

/// The deleted rows are zeroed in every file. At this scale no page splits and no FTS segment
/// spans two leaves, so it can't see the stale copies that page gaps and `events_fts_idx` keys
/// keep at real sizes; the purge PR's test covers those.
#[test]
fn a_deleted_threads_rows_are_zeroed_in_the_files() {
    let scratch = Scratch::new("delete");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (kept, run) = turn(&mut store, Err("kept"), "unrelated");
    store.end_run(kept, run, &[], RunEnd::Done).expect("ended");
    let (thread, _) = plant(&mut store);
    assert!(
        contains_marker(&scratch.db()),
        "the marker is on disk before the delete"
    );
    let deleted_scene: i64 = store
        .conn
        .query_row(
            "SELECT max(id) FROM events WHERE kind = 'scene_begin'",
            [],
            |row| row.get(0),
        )
        .expect("scene id");

    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()), "found while open");
    assert_eq!(rows(&store.conn, "summaries"), 0, "its own summary went");
    for table in ["events", "runs", "threads"] {
        assert_eq!(
            rows(&store.conn, table),
            1,
            "{table} keeps only the other thread"
        );
    }
    drop(store);
    assert!(!contains_marker(&scratch.db()), "found after close");

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.pages().search_ids(MARKER).expect("search").is_empty());
    assert_eq!(
        store.pages().search_ids("unrelated").expect("search"),
        vec![kept]
    );
    let next = turn(&mut store, Err("next"), "x");
    let scene = store
        .commit_step(next, None, SceneTarget::New { evict: None }, 0, "next")
        .expect("step");
    assert!(
        i64::try_from(scene.0).expect("small") > deleted_scene,
        "scene ids aren't reused"
    );
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
            purge_pending: true,
            ..Recovered::default()
        },
        "the deleted thread's run isn't counted as crashed"
    );
    // The dead brain's connection goes, as its process does; while it held the file, the swap
    // couldn't move the copy over it.
    drop(crashed);
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()));
    assert!(matches!(
        store.finish_delete(thread),
        Err(StoreError::NoThread)
    ));
}

#[test]
fn a_checkpoint_a_reader_blocks_keeps_the_old_file_and_the_purge_owed() {
    let scratch = Scratch::new("blocked");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    // After the copy's drain, another connection writes and then reads with that write still in
    // the WAL. The store's own connection wrote nothing, so the copy isn't stale, but the WAL
    // can't be emptied under the reader.
    let reader = Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("CREATE TABLE elsewhere (x); BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");

    let started = std::time::Instant::now();
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Blocked);
    store.leftovers().remove().expect("purge files removed");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the checkpoint didn't wait out the busy timeout"
    );
    let wait: i64 = store
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("timeout");
    assert_eq!(wait, 5000, "other work keeps its busy timeout");
    assert!(contains_marker(&scratch.db()), "the old file stays");
    assert!(store.purge_pending());
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());

    drop(reader);
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()));
    assert!(!store.recover().expect("recover").purge_pending);
}

#[test]
fn a_copy_that_cant_replace_the_file_keeps_the_old_store_serving() {
    let scratch = Scratch::new("held");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let holder = Connection::open(scratch.db()).expect("holder");
    holder
        .query_row("SELECT count(*) FROM threads", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("opened");
    let copied = clean_copy(&store);
    assert!(matches!(store.swap(&copied), Err(StoreError::Purge(_))));
    assert!(store.purge_pending());
    assert_eq!(rows(&store.conn, "purge_pending"), 1);
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")]
    );
    let (thread, run) = turn(&mut store, Err("served"), "still served");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");

    drop(holder);
    assert_eq!(purge(&mut store), Swap::Done);
    assert_eq!(
        store.pages().search_ids("served").expect("search"),
        vec![thread]
    );
}

#[test]
fn an_ending_stores_its_events_and_outcome_together_or_not_at_all() {
    let scratch = Scratch::new("ending");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = turn(&mut store, Err("t"), "hi");
    let (other, other_run) = turn(&mut store, Err("u"), "hey");
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
    // A failure after the tail is inserted takes the tail back with it.
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER refuse_outcome BEFORE UPDATE ON runs BEGIN
                 SELECT RAISE(ABORT, 'refused');
             END;",
        )
        .expect("trigger");
    assert!(matches!(
        store.end_run(thread, run, &tail, RunEnd::Done),
        Err(StoreError::Sql(_))
    ));
    assert_eq!(rows(&store.conn, "events"), 2, "only the two turns");
    assert_eq!(run_row(&store, run), ("active".to_owned(), None));
    store
        .conn
        .execute_batch("DROP TRIGGER refuse_outcome")
        .expect("drop");
    store
        .end_run(other, other_run, &tail, RunEnd::Done)
        .expect("a live thread's ending");
    assert_eq!(run_row(&store, other_run), ("done".to_owned(), None));
    let stored: Vec<(i64, Option<String>)> = store
        .conn
        .prepare("SELECT run, text FROM events WHERE kind != 'user' ORDER BY id")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(
        stored,
        [(
            super::sql_id(other_run.0, StoreError::NoRun).expect("id"),
            Some("tail".to_owned())
        )]
    );
    store.begin_delete(thread).expect("begin");
    assert!(matches!(
        store.end_run(thread, run, &tail, RunEnd::Done),
        Err(StoreError::NoThread)
    ));
    assert_eq!(run_row(&store, run), ("active".to_owned(), None));
    assert_eq!(
        rows(&store.conn, "events"),
        3,
        "the two turns and the other's tail"
    );
    store
        .end_run(thread, run, &[], RunEnd::Failed(Failure::Store))
        .expect("an outcome alone needs no live thread");
    assert_eq!(
        run_row(&store, run),
        ("failed".to_owned(), Some("store".to_owned()))
    );
}

/// A store owing the purge of a planted thread, beside a kept thread whose run ended.
fn owed_purge(store: &mut Store) -> ThreadId {
    let (kept, run) = turn(store, Err("kept"), "kept");
    store.end_run(kept, run, &[], RunEnd::Done).expect("ended");
    let (thread, _) = plant(store);
    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
    thread
}

#[test]
fn a_purge_waits_while_a_run_is_active() {
    let scratch = Scratch::new("active");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let (thread, run) = turn(&mut store, Err("busy"), "busy");
    assert!(store.owed().expect("owed").is_none());
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    assert!(store.owed().expect("owed").is_some());
}

#[test]
fn a_write_after_the_snapshot_keeps_the_old_file_and_the_purge_owed() {
    let scratch = Scratch::new("stale");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    let (late, run) = turn(&mut store, Err("late"), "written after the snapshot");
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Stale);
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")]
    );
    assert!(store.purge_pending());
    assert!(contains_marker(&scratch.db()), "the old file stays");

    store.end_run(late, run, &[], RunEnd::Done).expect("ended");
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()));
    assert_eq!(
        store.pages().search_ids("snapshot").expect("search"),
        vec![late]
    );
}

#[test]
fn a_stopped_copy_leaves_nothing_and_the_purge_owed() {
    let scratch = Scratch::new("stopped");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let owed = store.owed().expect("owed").expect("idle");
    assert!(
        owed.copy(&{
            let cancel = Cancel::default();
            cancel.stop_and_release();
            Arc::new(cancel)
        })
        .expect("copy")
        .is_none()
    );
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
    assert!(store.purge_pending());
}

#[test]
fn a_failed_copy_leaves_nothing_and_a_retry_finishes() {
    let scratch = Scratch::new("copy-fails");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    // The copy carries the trigger, so clearing its owed row fails after the snapshot.
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER keep_owed BEFORE DELETE ON purge_pending BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trigger");
    let owed = store.owed().expect("owed").expect("idle");
    assert!(matches!(
        owed.copy(&Arc::new(Cancel::default())),
        Err(StoreError::Sql(_))
    ));
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
    assert_eq!(
        rows(&store.conn, "threads"),
        1,
        "the deleted rows stay gone"
    );
    assert_eq!(rows(&store.conn, "purge_pending"), 1);
    assert!(store.purge_pending());

    store
        .conn
        .execute_batch("DROP TRIGGER keep_owed")
        .expect("drop");
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()));
}

#[test]
fn a_failed_swap_restores_the_busy_timeout_and_a_retry_finishes() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let scratch = Scratch::new("swap-fails");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    store
        .conn
        .execute_batch("PRAGMA busy_timeout = 1234")
        .expect("timeout");
    let copied = clean_copy(&store);
    store
        .conn
        .authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Pragma {
                pragma_name: "wal_checkpoint",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .expect("authorizer");
    assert!(matches!(store.swap(&copied), Err(StoreError::Sql(_))));
    let wait: i64 = store
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("timeout");
    assert_eq!(wait, 1234);
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")]
    );
    assert_eq!(rows(&store.conn, "purge_pending"), 1);
    assert!(store.purge_pending());
    assert!(contains_marker(&scratch.db()), "the old file stays");

    store
        .conn
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("no authorizer");
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!contains_marker(&scratch.db()));
}

#[test]
fn a_copy_left_by_a_crash_is_never_swapped_in_and_the_next_copy_replaces_it() {
    let scratch = Scratch::new("crashed-copy");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    // The brain dies before the swap, with only the clean copy left.
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")]
    );
    drop((copied, store));

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.leftovers().exist());
    assert!(store.purge_pending());
    assert_eq!(purge(&mut store), Swap::Done);
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
    assert_eq!(rows(&store.conn, "purge_pending"), 0);
}

#[test]
fn a_store_whose_reopen_failed_serves_again_once_its_file_opens() {
    let scratch = Scratch::new("reopen-fails");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    let wal = store.swap_failing_reopen(&copied);
    assert!(store.reconnect().is_err());
    assert!(
        store.high_water().is_err(),
        "it fails closed rather than serve another file"
    );

    std::fs::remove_dir(&wal).expect("out of the way");
    store.reconnect().expect("the file opens again");
    assert!(!store.purge_pending(), "read from the clean file");
    let (thread, run) = turn(&mut store, Err("after"), "served again");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    store.leftovers().remove().expect("removed");
    assert!(!contains_marker(&scratch.db()));
    assert_eq!(
        store.pages().search_ids("served").expect("search"),
        vec![thread]
    );
}

#[test]
fn a_reader_holding_the_wal_defers_the_copy_until_it_lets_go() {
    let scratch = Scratch::new("drain-held");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let reader = Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");
    // A write past the reader's snapshot, which no checkpoint can copy while it reads.
    let (thread, run) = turn(&mut store, Err("later"), "later");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy");
    assert!(copied.is_none(), "the swap would have had the WAL to copy");
    assert!(store.purge_pending());
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());

    drop(reader);
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(!store.purge_pending());
    assert!(!contains_marker(&scratch.db()));
}

#[test]
fn a_removal_that_leaves_a_file_behind_isnt_finished() {
    let scratch = Scratch::new("removal-leaves");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
    let old = beside(&scratch.db(), ".purge-old");
    // As a file another process holds with delete sharing keeps its name after a deletion
    // succeeds, where the file system doesn't remove names at once.
    let removed = store.leftovers().remove_with(|path| {
        if path == old {
            Ok(())
        } else {
            std::fs::remove_file(path)
        }
    });
    assert!(removed.is_err());
    assert!(old.exists());
    store.leftovers().remove().expect("removed");
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
}

#[test]
fn without_hard_links_the_swap_still_replaces_the_store() {
    let scratch = Scratch::new("no-links");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    let swapped = store.swap_with(&copied, |_, _| {
        Err(std::io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED.cast_signed(),
        ))
    });
    assert_eq!(swapped.expect("swap"), Swap::Done);
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
    assert!(!contains_marker(&scratch.db()));
}

#[test]
fn a_link_that_fails_otherwise_keeps_the_old_store() {
    let scratch = Scratch::new("link-fails");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    let swapped = store.swap_with(&copied, |_, _| {
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
    });
    assert!(matches!(swapped, Err(StoreError::Purge(_))));
    assert!(store.purge_pending());
    assert_eq!(rows(&store.conn, "purge_pending"), 1, "the old file serves");
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")]
    );
}

#[test]
fn a_move_that_fails_after_the_link_keeps_the_old_store_under_one_name() {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 1;
    let scratch = Scratch::new("move-fails");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    // The clean copy can't move while it's open without delete sharing.
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(beside(&scratch.db(), ".purge-clean"))
        .expect("held");
    assert!(matches!(store.swap(&copied), Err(StoreError::Purge(_))));
    assert_eq!(
        purge_files(&scratch.db()),
        vec![beside(&scratch.db(), ".purge-clean")],
        "the old store's second name is gone"
    );
    assert!(store.purge_pending());
    let (thread, run) = turn(&mut store, Err("served"), "still served");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    drop(holder);
    assert_eq!(purge(&mut store), Swap::Done);
    assert_eq!(
        store.pages().search_ids("served").expect("search"),
        vec![thread]
    );
}

#[test]
fn a_reopen_never_creates_a_missing_store_and_its_old_copy_stays() {
    let scratch = Scratch::new("reopen-missing");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let copied = clean_copy(&store);
    let old = beside(&scratch.db(), ".purge-old");
    // The store's name goes missing after the old store's second name is made.
    let swapped = store.swap_with(&copied, |db, old| {
        std::fs::hard_link(db, old)?;
        std::fs::remove_file(db)?;
        Err(std::io::Error::other("in the way"))
    });
    assert!(swapped.is_err());
    assert!(store.reconnect().is_err());
    assert!(!scratch.db().exists(), "no empty store was made");
    assert!(store.leftovers().remove().is_err());
    assert!(old.exists(), "the only copy stays");
}

#[test]
fn a_missing_store_beside_its_old_copy_fails_closed() {
    let scratch = Scratch::new("missing");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    drop(store);
    let old = beside(&scratch.db(), ".purge-old");
    std::fs::rename(scratch.db(), &old).expect("moved away");
    assert!(matches!(
        Store::open(&scratch.db()),
        Err(StoreError::Retired)
    ));
    assert!(old.exists(), "the old copy stays");
    assert!(!scratch.db().exists(), "no empty store was made");
}

#[test]
fn compacting_leaves_out_what_the_snapshot_freed_and_keeps_its_rows() {
    let scratch = Scratch::new("compact");
    std::fs::create_dir_all(scratch.db().parent().expect("a folder")).expect("folder");
    let snapshot = beside(&scratch.db(), ".purge-snapshot");
    let conn = Connection::open(&snapshot).expect("snapshot");
    conn.execute_batch(&format!(
        "PRAGMA secure_delete = OFF;
         CREATE TABLE t (x TEXT);
         INSERT INTO t VALUES ('{MARKER}' || zeroblob(5000));
         INSERT INTO t VALUES ('kept');
         DELETE FROM t WHERE x <> 'kept';"
    ))
    .expect("freed bytes");
    drop(conn);
    let has_marker = |path: &Path| {
        std::fs::read(path)
            .expect("readable")
            .windows(MARKER.len())
            .any(|window| window == MARKER.as_bytes())
    };
    assert!(
        has_marker(&snapshot),
        "the freed row's bytes stayed in the snapshot"
    );

    assert!(steps::compact(&scratch.db(), &Arc::new(Cancel::default())).expect("compact"));
    let clean = beside(&scratch.db(), ".purge-clean");
    assert!(!has_marker(&clean));
    let kept: String = Connection::open(&clean)
        .expect("clean")
        .query_row("SELECT x FROM t", [], |row| row.get(0))
        .expect("the live row");
    assert_eq!(kept, "kept");
}

#[test]
fn a_leftover_that_cant_be_removed_doesnt_stop_the_start() {
    let scratch = Scratch::new("stuck-copy");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    drop(store);
    // A folder in the copy's place can't be removed as a file.
    let mut clean = scratch.db().into_os_string();
    clean.push(".purge-clean");
    std::fs::create_dir(&clean).expect("in the way");

    let store = Store::open(&scratch.db()).expect("the store still opens");
    assert!(store.purge_pending());
    assert!(matches!(
        store
            .owed()
            .expect("owed")
            .expect("idle")
            .copy(&Arc::new(Cancel::default())),
        Err(StoreError::Purge(_))
    ));
    std::fs::remove_dir(&clean).expect("out of the way");
}

#[test]
fn a_thread_whose_deletion_keeps_failing_doesnt_hold_the_purge() {
    let scratch = Scratch::new("stuck-delete");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    // Its run stays active: recovery fails only live threads' runs.
    let (stuck, _) = turn(&mut store, Err("stuck"), "stuck");
    store.begin_delete(stuck).expect("begin");
    assert!(store.owed().expect("owed").is_some());
}

#[test]
fn a_stop_ends_a_running_statement() {
    let scratch = Scratch::new("stop-mid");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = turn(&mut store, Err("big"), "big");
    let events: Vec<NewEvent<'_>> = (0..2000)
        .map(|_| NewEvent {
            kind: EventKind::AssistantText,
            text: Some("some words to copy"),
            body: None,
        })
        .collect();
    store.append(thread, run, &events).expect("events");
    let mut reader = Connection::open(scratch.db()).expect("reader");
    steps::stoppable(&mut reader, &{
        let cancel = Cancel::default();
        cancel.stop_and_release();
        Arc::new(cancel)
    })
    .expect("handler");
    let mut copy = scratch.db().into_os_string();
    copy.push(".purge-snapshot");
    let copy = copy.into_string().expect("a UTF-8 path");
    assert!(
        !steps::finished(reader.execute("VACUUM INTO ?1", [&copy])).expect("stopped, not failed")
    );
    drop(reader);
    store.leftovers().remove().expect("removed");
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
}

/// A tiny xorshift generator, so the remnants test builds the same store every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n).expect("small")).expect("below n")
    }

    fn between(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }

    /// Between `low` and `high` bytes of words, with the thread's marker once and its own tokens sprinkled in.
    fn text(&mut self, thread: usize, (low, high): (usize, usize), words: &[String]) -> String {
        let len = self.between(low, high);
        let mut text = String::new();
        let mark_at = self.below(len);
        let mut marked = false;
        while text.len() < len {
            if !marked && text.len() >= mark_at {
                write!(text, "MARK{thread:04}Q ").expect("a String takes any write");
                marked = true;
            } else if self.below(100) < 8 {
                write!(text, "tk{thread:04}x{:04} ", self.below(10_000))
                    .expect("a String takes any write");
            } else {
                text.push_str(&words[self.below(words.len())]);
                text.push(' ');
            }
        }
        text
    }
}

/// Where each deleted thread's marker or tokens still appear in any file of the store's folder,
/// in one pass over each file.
fn raw_remnants(db: &Path, deleted: &[usize]) -> Vec<String> {
    let mut found = std::collections::BTreeMap::<String, usize>::new();
    let folder = std::fs::read_dir(db.parent().expect("a folder")).expect("the store's folder");
    for entry in folder {
        let path = entry.expect("a folder entry").path();
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let mut bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("can't scan {}: {error}", path.display()));
        bytes.make_ascii_lowercase();
        for at in 0..bytes.len() {
            let rest = &bytes[at..];
            let (prefix, end) = if rest.starts_with(b"mark") {
                ("mark", b'q')
            } else if rest.starts_with(b"tk") {
                ("tk", b'x')
            } else {
                continue;
            };
            let Some(digits) = rest.get(prefix.len()..prefix.len() + 4) else {
                continue;
            };
            if rest.get(prefix.len() + 4) != Some(&end) || !digits.iter().all(u8::is_ascii_digit) {
                continue;
            }
            let thread: usize = std::str::from_utf8(digits)
                .expect("digits")
                .parse()
                .expect("a number");
            if deleted.contains(&thread) {
                let pattern = format!("{prefix}{thread:04}{} in {name}", char::from(end));
                *found.entry(pattern).or_default() += 1;
            }
        }
    }
    found
        .into_iter()
        .map(|(pattern, hits)| format!("{pattern} x{hits}"))
        .collect()
}

/// `events_fts_idx` keys that still name a deleted thread's tokens.
fn index_remnants(conn: &Connection, deleted: &[usize]) -> Vec<String> {
    let keys: Vec<Vec<u8>> = conn
        .prepare("SELECT term FROM events_fts_idx")
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("keys")
        .collect::<Result<_, _>>()
        .expect("read");
    keys.iter()
        .filter_map(|key| {
            let term = String::from_utf8_lossy(key.get(1..)?).into_owned();
            deleted
                .iter()
                .any(|thread| {
                    term.starts_with(&format!("mark{thread:04}"))
                        || term.starts_with(&format!("tk{thread:04}"))
                })
                .then_some(term)
        })
        .collect()
}

/// CAPS-22's remnants scan: a store big enough to split pages and build multi-leaf FTS
/// segments, written one event per transaction as the runtime writes it, with threads deleted
/// through the store, then the raw files and the index keys searched for anything of theirs.
#[test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn deleted_threads_leave_nothing_in_the_files_or_the_index_at_scale() {
    const THREADS: usize = 100;
    const ROUNDS: usize = 10;
    const DELETED_PER_ROUND: usize = 6;
    let scratch = Scratch::new("remnants");
    let mut store = Store::open(&scratch.db()).expect("store");
    // Fsyncs don't change what the pages hold, and skipping them keeps the test short.
    store
        .conn
        .execute_batch("PRAGMA synchronous = OFF")
        .expect("no fsync");
    // Most seeds leave only FTS keys behind; on the bundled SQLite this one also leaves a deleted
    // cell in a page gap, which the test checks before purging.
    let mut rng = Rng(0x3C6E_F372_FE94_F82B);
    let syllables = [
        "ka", "lo", "mi", "ne", "ru", "sa", "ti", "vo", "be", "da", "fe", "go", "hu", "ji", "pe",
        "qu", "ze", "wa", "xi", "yo",
    ];
    let words: Vec<String> = (0..4000)
        .map(|_| {
            (0..rng.between(2, 4))
                .map(|_| syllables[rng.below(syllables.len())])
                .collect()
        })
        .collect();
    let mut ids: Vec<Option<ThreadId>> = vec![None; THREADS];
    let mut live: Vec<usize> = (0..THREADS).collect();
    let mut deleted = Vec::new();
    for round in 0..ROUNDS {
        for &t in &live {
            if ids[t].is_some() && rng.below(10) >= 6 {
                continue;
            }
            let user = rng.text(t, (40, 600), &words);
            let title = format!("Title MARK{t:04}Q");
            let (thread, run) = turn(&mut store, ids[t].ok_or(title.as_str()), &user);
            ids[t] = Some(thread);
            let size = if rng.below(10) == 0 {
                rng.between(4000, 12_000)
            } else {
                rng.between(200, 2500)
            };
            let answer = rng.text(t, (size, size), &words);
            let pad = rng.text(t, (50, 700), &words);
            let body =
                serde_json::json!({ "label": format!("MARK{t:04}Q"), "pad": pad }).to_string();
            // One transaction per event, as the runtime commits text and steps.
            for (kind, text, body) in [
                (EventKind::AssistantText, answer.as_str(), None),
                (EventKind::SceneStep, "label", Some(body.as_str())),
            ] {
                let event = NewEvent {
                    kind,
                    text: Some(text),
                    body,
                };
                store.append(thread, run, &[event]).expect("appended");
            }
            store
                .end_run(thread, run, &[], RunEnd::Done)
                .expect("ended");
        }
        if round == ROUNDS / 2 {
            let widest: i64 = store
                .conn
                .query_row(
                    "SELECT max(n) FROM (SELECT count(*) AS n FROM events_fts_idx GROUP BY segid)",
                    [],
                    |row| row.get(0),
                )
                .expect("segments");
            assert!(widest >= 2, "an FTS segment spans {widest} leaves");
        }
        if round >= ROUNDS / 2 {
            for _ in 0..DELETED_PER_ROUND {
                let t = live.remove(rng.below(live.len()));
                let thread = ids[t].expect("every thread had a turn");
                store.begin_delete(thread).expect("begin");
                store.finish_delete(thread).expect("finish");
                deleted.push(t);
            }
        }
    }
    let kept = live[0];
    let kept_id = ids[kept].expect("a turn");

    // Each half of the purge is needed: the keys survive until a rebuild, and on a copy of the
    // file a cell in a page gap survives the rebuild.
    assert!(!index_remnants(&store.conn, &deleted).is_empty());
    assert!(
        checkpoint_now(&store.conn).expect("checkpoint"),
        "the copy needs the whole file"
    );
    let unvacuumed = Scratch::new("remnants-copy");
    std::fs::create_dir_all(unvacuumed.db().parent().expect("a folder")).expect("folder");
    std::fs::copy(scratch.db(), unvacuumed.db()).expect("copy");
    let copy = Connection::open(unvacuumed.db()).expect("copy opens");
    copy.execute_batch("INSERT INTO events_fts (events_fts) VALUES ('rebuild')")
        .expect("rebuild");
    assert_eq!(index_remnants(&copy, &deleted), Vec::<String>::new());
    drop(copy);
    assert!(
        !raw_remnants(&unvacuumed.db(), &deleted).is_empty(),
        "no page gap kept a deleted cell, so this seed no longer shows why the file is replaced"
    );

    let copied = clean_copy(&store);
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
    // Until it's removed, the old store keeps the deleted text, and only it does.
    let left = raw_remnants(&scratch.db(), &deleted);
    assert!(!left.is_empty());
    assert!(
        left.iter()
            .all(|found| found.contains(" in store.db.purge-old ")),
        "{left:?}"
    );
    store.leftovers().remove().expect("removed");
    assert_eq!(purge_files(&scratch.db()), Vec::<PathBuf>::new());
    // Read while the store is open, before closing checkpoints and removes the WAL.
    assert_eq!(raw_remnants(&scratch.db(), &deleted), Vec::<String>::new());
    store
        .conn
        .execute_batch("INSERT INTO events_fts (events_fts) VALUES ('integrity-check')")
        .expect("the index matches the events");
    assert_eq!(index_remnants(&store.conn, &deleted), Vec::<String>::new());
    drop(store);
    assert_eq!(raw_remnants(&scratch.db(), &deleted), Vec::<String>::new());
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store
            .pages()
            .search_ids(&format!("mark{kept:04}q"))
            .expect("search"),
        vec![kept_id],
        "the kept threads are still found"
    );
}

/// A measured store's threads, events per thread, bytes per event and captures per thread.
pub(crate) type Shape = (usize, usize, usize, usize);

/// The stores the purge is measured on.
pub(crate) const MEASURED_STORES: [(&str, Shape); 4] = [
    ("text 20k events", (1000, 20, 2000, 0)),
    ("text 100k events", (2000, 50, 2000, 0)),
    ("text 20k events + 4k captures", (1000, 20, 2000, 4)),
    ("text 20k events + 8k captures", (1000, 20, 2000, 8)),
];

/// Builds one of [`MEASURED_STORES`], with its WAL emptied.
pub(crate) fn measured_store(
    scratch: &Scratch,
    (threads, events, bytes, captures): Shape,
) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do ";
    let text = words.repeat(bytes / words.len());
    let capture: Vec<u8> = (0..121_000_u32)
        .map(|i| i.wrapping_mul(2_654_435_761).to_be_bytes()[0])
        .collect();
    let tx = store.conn.transaction().expect("tx");
    // Stands in for slice 6's assets table, which doesn't exist yet.
    tx.execute_batch("CREATE TABLE media (thread INTEGER NOT NULL, bytes BLOB NOT NULL)")
        .expect("media");
    let small = |n: usize| i64::try_from(n).expect("small");
    for t in 1..=small(threads) {
        tx.execute(
            "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (?1, 't', 0, 0)",
            [t],
        )
        .expect("thread");
        tx.execute(
            "INSERT INTO runs (id, thread, state, started_ms) VALUES (?1, ?1, 'done', 0)",
            [t],
        )
        .expect("run");
        for e in 0..small(events) {
            tx.execute(
                "INSERT INTO events (thread, run, kind, text, created_ms)
                 VALUES (?1, ?1, 'assistant_text', ?2 || ?3, 0)",
                rusqlite::params![t, e, text],
            )
            .expect("event");
        }
        for _ in 0..captures {
            tx.execute(
                "INSERT INTO media (thread, bytes) VALUES (?1, ?2)",
                rusqlite::params![t, capture],
            )
            .expect("capture");
        }
    }
    tx.commit().expect("built");
    assert!(checkpoint_now(&store.conn).expect("settled"));
    store
}

/// A store op the way a run makes them: a turn, then its ending.
fn measured_op(store: &mut Store) -> [std::time::Duration; 2] {
    let started = std::time::Instant::now();
    let (thread, run) = turn(store, Err("measured"), "a measured turn");
    let turned = started.elapsed();
    let started = std::time::Instant::now();
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    [turned, started.elapsed()]
}

/// Deletes `thread` as a run would, its captures included.
fn delete_measured(store: &mut Store, thread: usize) {
    store
        .conn
        .execute(
            "DELETE FROM media WHERE thread = ?1",
            [i64::try_from(thread).expect("small")],
        )
        .expect("media gone");
    let thread = ThreadId(u64::try_from(thread).expect("small"));
    store.begin_delete(thread).expect("begin");
    store.finish_delete(thread).expect("finish");
}

/// Rez measurement for the pull request, not a check: the swap after a reader kept a deletion out
/// of the store's file. A reader holds a snapshot while a tenth of the store's threads, their
/// captures included, are deleted, so no checkpoint can copy the deletion into the file; it then
/// lets go, a copy is made with nothing else running, and the WAL's size, the whole swap and the op
/// right behind it are logged. Three rounds a store, each deleting the next tenth. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_swap_after`; set
/// `MEASURE_ONLY` to one store's name to run just that one.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
fn measure_swap_after_a_held_reader() {
    use std::time::Instant;
    let only = std::env::var("MEASURE_ONLY").ok();
    for (name, shape) in MEASURED_STORES {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let scratch = Scratch::new("measure-wal");
        let mut store = measured_store(&scratch, shape);
        let size = std::fs::metadata(scratch.db()).expect("size").len();
        let tenth = shape.0 / 10;
        for round in 0..3 {
            let reader = Connection::open(scratch.db()).expect("reader");
            reader
                .execute_batch("BEGIN; SELECT count(*) FROM events;")
                .expect("snapshot");
            for thread in round * tenth + 1..=(round + 1) * tenth {
                delete_measured(&mut store, thread);
            }
            drop(reader);
            // The copy's first step drains the WAL with a passive checkpoint on its own
            // connection. In the last round that's done apart first, with ops meanwhile, since an
            // op makes the copy stale; the ops start once the helper's connection is open, as an
            // op that came first would checkpoint the WAL itself (each store's last line).
            let drained = (round == 2).then(|| {
                let db = scratch.db();
                let (ready, opened) = std::sync::mpsc::channel();
                let helper = std::thread::spawn(move || {
                    let conn = Connection::open_with_flags(
                        &db,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .expect("helper");
                    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
                        .expect("read");
                    ready.send(()).expect("heard");
                    let started = Instant::now();
                    let frames: (i64, i64, i64) = conn
                        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })
                        .expect("drained");
                    (started.elapsed(), frames)
                });
                opened.recv().expect("opened");
                let mut ops = Vec::new();
                while !helper.is_finished() {
                    ops.extend(measured_op(&mut store));
                }
                let (drain, (busy, frames, copied)) = helper.join().expect("drained");
                let worst = ops.iter().max().copied().unwrap_or_default();
                format!(
                    "; drained apart first in {drain:.1?} (busy {busy}, {copied} of {frames} \
                     frames) with {} ops meanwhile, worst {worst:.1?}",
                    ops.len()
                )
            });
            let started = Instant::now();
            let copied = store
                .owed()
                .expect("owed")
                .expect("idle")
                .copy(&Arc::new(Cancel::default()))
                .expect("copy")
                .expect("not stopped");
            let copy = started.elapsed();
            let wal = std::fs::metadata(beside(&scratch.db(), "-wal")).map_or(0, |wal| wal.len());
            let started = Instant::now();
            assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
            let swap = started.elapsed();
            let first = measured_op(&mut store);
            eprintln!(
                "{name} ({} MiB) round {}: {tenth} threads deleted behind a reader{}; copy \
                 {copy:.1?}; WAL file at the swap {:.1} MiB; swap {swap:.1?}; op right behind it \
                 {:.1?} + {:.1?}",
                size >> 20,
                round + 1,
                drained.unwrap_or_default(),
                f64::from(u32::try_from(wal >> 10).expect("under 4 TiB")) / 1024.0,
                first[0],
                first[1],
            );
            store.leftovers().remove().expect("removed");
        }
        // With no purge at all: SQLite's automatic checkpoint makes the first write after the
        // reader lets go copy the whole deletion on the store's thread.
        let reader = Connection::open(scratch.db()).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM events;")
            .expect("snapshot");
        for thread in 3 * tenth + 1..=4 * tenth {
            delete_measured(&mut store, thread);
        }
        drop(reader);
        let wal = std::fs::metadata(beside(&scratch.db(), "-wal")).map_or(0, |wal| wal.len());
        let first = measured_op(&mut store);
        eprintln!(
            "{name}: with no purge, {tenth} threads deleted behind a reader; WAL file {:.1} MiB; \
             the first op after the reader let go {:.1?} + {:.1?}",
            f64::from(u32::try_from(wal >> 10).expect("under 4 TiB")) / 1024.0,
            first[0],
            first[1],
        );
    }
}

/// Rez measurement for the pull request, not a check: how long the store thread spends on one
/// `Store::delete` of a thread, its captures included, by store size. Normally, and right after
/// a reader that held a snapshot while a twentieth of the store's threads were deleted lets go,
/// so the delete may have to checkpoint that WAL itself. Reports p50, p95 and the worst sample
/// against the ~100 ms foreground bound. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_big_delete`; set
/// `MEASURE_ONLY` to one store's name to run just that one.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
fn measure_big_delete() {
    use std::time::{Duration, Instant};
    const SAMPLES: usize = 20;
    const HELD: usize = 5;
    let only = std::env::var("MEASURE_ONLY").ok();
    let timed = |store: &mut Store, thread: usize| {
        let started = Instant::now();
        store
            .conn
            .execute(
                "DELETE FROM media WHERE thread = ?1",
                [i64::try_from(thread).expect("small")],
            )
            .expect("media gone");
        let deletion = store.delete(ThreadId(u64::try_from(thread).expect("small")));
        let took = started.elapsed();
        assert_eq!(deletion.outcome, Deleted::Removed);
        took
    };
    let report = |name: &str, size: u64, how: &str, mut samples: Vec<Duration>| {
        samples.sort();
        let at = |percent: usize| samples[(samples.len() - 1) * percent / 100];
        eprintln!(
            "{name} ({} MiB) {how}: {} deletes, p50 {:.1?}, p95 {:.1?}, worst {:.1?}",
            size >> 20,
            samples.len(),
            at(50),
            at(95),
            samples[samples.len() - 1],
        );
    };
    for (name, shape) in MEASURED_STORES {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let scratch = Scratch::new("measure-delete");
        let mut store = measured_store(&scratch, shape);
        let size = std::fs::metadata(scratch.db()).expect("size").len();
        let threads = shape.0;
        let normal = (1..=SAMPLES)
            .map(|thread| timed(&mut store, thread))
            .collect();
        report(name, size, "normally", normal);
        let fill = threads / 20;
        let mut next = SAMPLES + 1;
        let mut held = Vec::new();
        let mut wal_mib = 0.0_f64;
        for _ in 0..HELD {
            let reader = Connection::open(scratch.db()).expect("reader");
            reader
                .execute_batch("BEGIN; SELECT count(*) FROM events;")
                .expect("snapshot");
            for thread in next..next + fill {
                delete_measured(&mut store, thread);
            }
            next += fill;
            let wal = std::fs::metadata(beside(&scratch.db(), "-wal")).map_or(0, |wal| wal.len());
            wal_mib = wal_mib.max(f64::from(u32::try_from(wal >> 10).expect("small")) / 1024.0);
            drop(reader);
            held.push(timed(&mut store, next));
            next += 1;
        }
        report(
            name,
            size,
            &format!("after a reader held a WAL of up to {wal_mib:.1} MiB"),
            held,
        );
    }
}

/// Rez measurement for the pull request, not a check: the purge's phases by store size, with and
/// without media blobs the size of slice 6's captures (architecture v1 §6.1: two capture images
/// total about 121 KB), and how long store ops take while each phase runs. Five purges a store;
/// in each, ops run during a copy that's then thrown away, and a second copy is swapped in
/// with nothing else running, followed by one op. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_purge`; set
/// `MEASURE_ONLY` to one store's name to run just that one.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
#[expect(clippy::too_many_lines, reason = "one measurement, read top to bottom")]
fn measure_purge() {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};
    const PHASES: [&str; 3] = ["snapshot", "rebuild", "compact"];
    let only = std::env::var("MEASURE_ONLY").ok();
    let op = measured_op;
    let worst = |ops: &[Duration]| {
        let (at, worst) = ops
            .iter()
            .enumerate()
            .max_by_key(|(_, took)| **took)
            .map_or((0, Duration::ZERO), |(at, took)| (at, *took));
        format!("{} ops, worst {worst:.1?} at op {at}", ops.len())
    };
    // Removes the purge's files on a helper, as the store's thread does, with ops meanwhile.
    let while_removing = |store: &mut Store| -> (Duration, Vec<Duration>) {
        let leftovers = store.leftovers();
        let helper = std::thread::spawn(move || {
            let started = Instant::now();
            leftovers.remove().expect("removed");
            started.elapsed()
        });
        let mut ops = Vec::new();
        while !helper.is_finished() {
            ops.extend(op(store));
            std::thread::sleep(Duration::from_millis(10));
        }
        (helper.join().expect("removed"), ops)
    };
    for (name, shape) in MEASURED_STORES {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let scratch = Scratch::new("measure-purge");
        let mut store = measured_store(&scratch, shape);
        let size = std::fs::metadata(scratch.db()).expect("size").len();
        // The same ops with no purge running, for comparison.
        let alone = |store: &mut Store| -> String {
            let mut took: Vec<(Duration, usize)> = (0..500)
                .flat_map(|_| op(store))
                .enumerate()
                .map(|(at, took)| (took, at))
                .collect();
            took.sort_unstable();
            format!(
                "1,000 ops with no purge running: median {:.1?}, 99th percentile {:.1?}, worst {:.1?} at op {}",
                took[took.len() / 2].0,
                took[took.len() * 99 / 100].0,
                took[took.len() - 1].0,
                took[took.len() - 1].1
            )
        };
        eprintln!("{name}: {} MiB; {}", size >> 20, alone(&mut store));
        for purge_number in 1..=5 {
            let thread = ThreadId(purge_number);
            store
                .conn
                .execute(
                    "DELETE FROM media WHERE thread = ?1",
                    [i64::try_from(purge_number).expect("small")],
                )
                .expect("media gone");
            store.begin_delete(thread).expect("begin");
            store.finish_delete(thread).expect("finish");

            // Ops while each phase of a copy runs. An op counts for a phase when that phase was
            // running both when it started and when it ended; one that crosses phases is listed
            // apart, and every op counts for the worst.
            let db = scratch.db();
            let phase = Arc::new(AtomicU8::new(0));
            let running = Arc::clone(&phase);
            let helper = std::thread::spawn(move || {
                let stop = Arc::new(Cancel::default());
                let mut took = Vec::new();
                for (at, step) in [steps::snapshot, steps::clean, steps::compact]
                    .into_iter()
                    .enumerate()
                {
                    running.store(u8::try_from(at).expect("three") + 1, Ordering::SeqCst);
                    let started = Instant::now();
                    assert!(step(&db, &stop).expect("step"));
                    took.push(started.elapsed());
                }
                running.store(0, Ordering::SeqCst);
                took
            });
            while phase.load(Ordering::SeqCst) == 0 && !helper.is_finished() {
                std::thread::yield_now();
            }
            let mut during: [Vec<Duration>; 3] = Default::default();
            let mut crossing = Vec::new();
            let mut all = Vec::new();
            loop {
                let before = phase.load(Ordering::SeqCst);
                if before == 0 {
                    break;
                }
                let took = op(&mut store);
                let after = phase.load(Ordering::SeqCst);
                all.extend(took);
                if after == before {
                    during[usize::from(before) - 1].extend(took);
                } else {
                    crossing.push(format!(
                        "{:.1?} {}->{}",
                        took[0] + took[1],
                        PHASES[usize::from(before) - 1],
                        PHASES
                            .get(usize::from(after).wrapping_sub(1))
                            .unwrap_or(&"end")
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let took = helper.join().expect("copied");
            // The copy those ops made stale is removed off the thread, as the store's thread does
            // after a stale or blocked swap.
            let (unused, unused_ops) = while_removing(&mut store);

            let owed = store.owed().expect("owed").expect("idle");
            let started = Instant::now();
            let copied = owed
                .copy(&Arc::new(Cancel::default()))
                .expect("copy")
                .expect("not stopped");
            let copy = started.elapsed();
            let started = Instant::now();
            assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
            let swap = started.elapsed();
            let first = op(&mut store);
            let (old, old_ops) = while_removing(&mut store);
            assert!(!store.leftovers().exist());
            let mut line = format!("{name} purge {purge_number}:");
            for ((phase, took), ops) in PHASES.iter().zip(&took).zip(&during) {
                write!(line, " {phase} {took:.1?} ({});", worst(ops))
                    .expect("a String takes any write");
            }
            write!(
                line,
                " every op during the copy: {}; crossing phases: [{}]; removing the unused copy \
                 {unused:.1?} ({}); quiet copy {copy:.1?}; swap {swap:.1?}; first op after it \
                 {:.1?} + {:.1?}; removing the old store {old:.1?} ({})",
                worst(&all),
                crossing.join(", "),
                worst(&unused_ops),
                first[0],
                first[1],
                worst(&old_ops),
            )
            .expect("a String takes any write");
            eprintln!("{line}");
        }
        eprintln!("{name} after the purges: {}", alone(&mut store));
    }
}

#[test]
fn a_delete_reports_how_far_it_got_and_the_threads_newest_run() {
    let scratch = Scratch::new("delete-outcomes");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, first) = turn(&mut store, Err("t"), "x");
    store
        .end_run(thread, first, &[], RunEnd::Done)
        .expect("ended");
    let (_, newest) = turn(&mut store, Ok(thread), "y");
    let (other, other_run) = turn(&mut store, Err("u"), "z");
    assert!(other_run > newest);
    let done = store.delete(thread);
    assert_eq!(
        (done.outcome, done.last_run),
        (Deleted::Removed, Some(newest))
    );
    assert!(done.error.is_none());
    let again = store.delete(thread);
    assert_eq!(
        (again.outcome, again.last_run),
        (Deleted::Removed, None),
        "already gone"
    );

    store
        .conn
        .execute_batch(
            "CREATE TRIGGER keep_threads BEFORE DELETE ON threads BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trap");
    let hidden = store.delete(other);
    assert_eq!(
        (hidden.outcome, hidden.last_run),
        (Deleted::Hidden, Some(other_run))
    );
    assert!(hidden.error.is_some());
    assert!(
        store
            .pages()
            .thread_page(None, 50)
            .map(|(rows, _)| rows)
            .expect("threads")
            .is_empty(),
        "hidden"
    );
    store
        .conn
        .execute_batch("DROP TRIGGER keep_threads;")
        .expect("untrapped");
    let finished = store.delete(other);
    assert_eq!(
        (finished.outcome, finished.last_run),
        (Deleted::Removed, Some(other_run)),
        "a hidden thread is removed"
    );
}

#[test]
fn a_delete_that_cant_hide_the_thread_leaves_it_live() {
    let scratch = Scratch::new("delete-not-hidden");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, _) = turn(&mut store, Err("t"), "x");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER no_hide BEFORE UPDATE OF state ON threads BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trap");
    let deletion = store.delete(thread);
    assert_eq!(
        (deletion.outcome, deletion.last_run),
        (Deleted::NotHidden, None)
    );
    assert!(deletion.error.is_some());
    let threads = store
        .pages()
        .thread_page(None, 50)
        .map(|(rows, _)| rows)
        .expect("threads");
    assert_eq!(threads.len(), 1, "still live");
}
