use std::fmt::Write as _;
use std::path::Path;

use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use crate::test_support::Scratch;

use super::{
    Deletion, EventKind, Failure, Interruption, NewEvent, Purge, Recovered, RunEnd, Store,
    StoreError, checkpoint_now,
};

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
            known: 2
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
            finished_deletions: 1,
            purge_pending: true,
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

/// The FTS segments, in order: a rebuild replaces them all.
fn segments(store: &Store) -> String {
    store
        .conn
        .query_row(
            "SELECT group_concat(segid) FROM (SELECT DISTINCT segid FROM events_fts_idx ORDER BY segid)",
            [],
            |row| row.get(0),
        )
        .expect("segments")
}

/// A deleted thread whose purge gave way to one queued piece of work, still counted.
fn owed_purge(store: &mut Store) -> ThreadId {
    turn(store, Err("kept"), "kept");
    let (thread, _) = plant(store);
    store.begin_delete(thread).expect("begin");
    store.queue().push(|| ());
    assert_eq!(
        store.finish_delete(thread).expect("finish"),
        Deletion::Pending
    );
    thread
}

#[test]
fn a_purge_gives_way_to_queued_work_and_stays_owed_until_it_finishes() {
    let scratch = Scratch::new("yield");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    let before = segments(&store);
    assert_eq!(store.purge().expect("purge"), Purge::Yielded);
    assert_eq!(segments(&store), before, "nothing ran");
    drop(store);

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.purge_pending(), "owed across a restart");
    assert_eq!(store.purge().expect("purge"), Purge::Done);
    assert_ne!(segments(&store), before, "the index was rebuilt");
    assert_eq!(rows(&store.conn, "purge_pending"), 0);
    drop(store);
    assert!(!Store::open(&scratch.db()).expect("reopen").purge_pending());
}

#[test]
fn work_queued_during_a_purge_step_interrupts_it() {
    let scratch = Scratch::new("interrupt");
    let mut store = Store::open(&scratch.db()).expect("store");
    owed_purge(&mut store);
    store.queue().pop();
    let before = segments(&store);
    // Work arrives while the first step is running.
    let queue = store.queue();
    let mut queued = false;
    store
        .conn
        .progress_handler(
            1,
            Some(move || {
                if !std::mem::replace(&mut queued, true) {
                    queue.push(|| ());
                }
                false
            }),
        )
        .expect("handler");
    assert_eq!(store.purge().expect("purge"), Purge::Yielded);
    assert_eq!(segments(&store), before, "the rebuild rolled back");
    assert!(store.purge_pending());

    store
        .conn
        .progress_handler(0, None::<fn() -> bool>)
        .expect("no handler");
    store.queue().pop();
    assert_eq!(store.purge().expect("retry"), Purge::Done);
    assert_ne!(segments(&store), before);
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

/// Where each deleted thread's marker or tokens still appear in the raw files.
fn raw_remnants(db: &Path, deleted: &[usize]) -> Vec<String> {
    let mut found = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        let Ok(mut bytes) = std::fs::read(&path) else {
            continue;
        };
        bytes.make_ascii_lowercase();
        for thread in deleted {
            for pattern in [format!("mark{thread:04}q"), format!("tk{thread:04}x")] {
                let hits = bytes
                    .windows(pattern.len())
                    .filter(|window| *window == pattern.as_bytes())
                    .count();
                if hits > 0 {
                    found.push(format!("{pattern} x{hits} in store.db{suffix}"));
                }
            }
        }
    }
    found
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
    // On the bundled SQLite this seed leaves thread 57's cells in a page gap when VACUUM is
    // skipped, and FTS keys of several threads when the rebuild is; most seeds reach only the
    // keys. runs/CAPS-20/logs/6b-mutation-*.txt records both.
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
                assert_eq!(
                    store.finish_delete(thread).expect("finish"),
                    Deletion::Finished
                );
                deleted.push(t);
            }
        }
    }
    let kept = live[0];
    let kept_id = ids[kept].expect("a turn");
    drop(store);

    assert_eq!(raw_remnants(&scratch.db(), &deleted), Vec::<String>::new());
    let store = Store::open(&scratch.db()).expect("reopen");
    store
        .conn
        .execute_batch("INSERT INTO events_fts (events_fts) VALUES ('integrity-check')")
        .expect("the index matches the events");
    assert_eq!(index_remnants(&store.conn, &deleted), Vec::<String>::new());
    assert_eq!(
        store
            .search(&format!("mark{kept:04}q"), 10)
            .expect("search"),
        vec![kept_id],
        "the kept threads are still found"
    );
}

/// Rez measurement for the pull request, not a check: the purge's cost by store size, with and
/// without media blobs the size of slice 6's captures (architecture v1 §6.1: two capture images
/// total about 121 KB). Run it with
/// `cargo test --release -p capsula-brain -- --ignored --nocapture measure_purge`; set
/// `MEASURE_ONLY` to one store's name to run just that one.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
fn measure_purge() {
    use std::time::{Duration, Instant};
    // (name, threads, events per thread, bytes per event, captures per thread)
    let stores = [
        ("text 2k events", 100, 20, 2000, 0),
        ("text 20k events", 1000, 20, 2000, 0),
        ("text 100k events", 2000, 50, 2000, 0),
        ("text 20k events + 1k captures", 1000, 20, 2000, 1),
        ("text 20k events + 4k captures", 1000, 20, 2000, 4),
    ];
    let only = std::env::var("MEASURE_ONLY").ok();
    let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do ";
    let capture: Vec<u8> = (0..121_000_u32)
        .map(|i| i.wrapping_mul(2_654_435_761).to_be_bytes()[0])
        .collect();
    for (name, threads, events, bytes, captures) in stores {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let scratch = Scratch::new("measure-purge");
        let mut store = Store::open(&scratch.db()).expect("store");
        let text = words.repeat(bytes / words.len());
        {
            let tx = store.conn.transaction().expect("tx");
            // Stands in for slice 6's assets table, which doesn't exist yet.
            tx.execute_batch("CREATE TABLE media (thread INTEGER NOT NULL, bytes BLOB NOT NULL)")
                .expect("media");
            for t in 1..=threads {
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
                for e in 0..events {
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
        }
        assert!(checkpoint_now(&store.conn).expect("settled"));
        let size = std::fs::metadata(scratch.db()).expect("size").len();
        let thread = ThreadId(1);
        store
            .conn
            .execute("DELETE FROM media WHERE thread = 1", [])
            .expect("media gone");
        store.begin_delete(thread).expect("begin");
        let started = Instant::now();
        assert_eq!(
            store.finish_delete(thread).expect("finish"),
            Deletion::Finished
        );
        let total = started.elapsed();
        let time = |sql: &str| {
            let started = Instant::now();
            store.conn.execute_batch(sql).expect("step");
            started.elapsed()
        };
        let rebuild = time("INSERT INTO events_fts (events_fts) VALUES ('rebuild')");
        let vacuum = time("VACUUM");
        let started = Instant::now();
        assert!(checkpoint_now(&store.conn).expect("checkpoint"));
        let checkpoint = started.elapsed();
        // How long queued work waits when it arrives 50 ms into a purge.
        let other = ThreadId(2);
        store.begin_delete(other).expect("begin");
        store.queue().push(|| ());
        assert_eq!(
            store.finish_delete(other).expect("finish"),
            Deletion::Pending
        );
        store.queue().pop();
        let queue = store.queue();
        let arrival = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            queue.push(Instant::now)
        });
        assert_eq!(store.purge().expect("purge"), Purge::Yielded);
        let gave_way = arrival.join().expect("arrived").elapsed();
        eprintln!(
            "{name}: {} MiB; finish_delete {total:.1?} (rebuild {rebuild:.1?}, VACUUM {vacuum:.1?}, checkpoint {checkpoint:.1?}); work queued mid-purge waited {gave_way:.2?}",
            size >> 20
        );
    }
}
