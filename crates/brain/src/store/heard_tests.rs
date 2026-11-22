use kumochi_protocol::message::{Deleted, RunId, SegmentId, ThreadId};
use rusqlite::Connection;

use super::{RunEnd, Store, schema};
use crate::test_support::Scratch;
use crate::voice::reply::Heard;

/// The run's stored point: segment, sample and text, each NULL when unknown.
type Stored = (Option<i64>, Option<i64>, Option<i64>);

/// A `tool_calls` row, in column order.
type Call = (i64, i64, String, String, String, Vec<u8>, String, i64);

fn point(conn: &Connection, run: RunId) -> Stored {
    conn.query_row(
        "SELECT heard_segment, heard_sample, heard_text FROM runs WHERE id = ?1",
        [i64::try_from(run.0).expect("small id")],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .expect("the run's row")
}

fn heard(segment: u32, sample: u32, text: usize) -> Heard {
    Heard {
        segment: SegmentId(segment),
        sample,
        text,
    }
}

fn set(segment: i64, sample: i64, text: i64) -> Stored {
    (Some(segment), Some(sample), Some(text))
}

/// A thread with one active run.
fn active_run(store: &mut Store) -> (ThreadId, RunId) {
    let (high_thread, high_run) = store.high_water().expect("high water");
    let (thread, run) = (ThreadId(high_thread.0 + 1), RunId(high_run.0 + 1));
    store
        .begin_turn(thread, Some("t"), run, "hi")
        .expect("turn");
    (thread, run)
}

/// A thread with one run, ended `Done`.
fn run(store: &mut Store) -> (ThreadId, RunId) {
    let (thread, run) = active_run(store);
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    (thread, run)
}

#[test]
fn a_v5_store_reopens_at_v6_with_its_runs_unknown() {
    let scratch = Scratch::new("heard-upgrade");
    std::fs::create_dir_all(scratch.db().parent().expect("a folder")).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    schema::migrate_to(&mut conn, 5).expect("V5");
    conn.execute_batch(
        "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1);
         INSERT INTO runs (id, thread, state, started_ms, ended_ms) VALUES (7, 5, 'done', 1, 2);
         INSERT INTO events (id, thread, run, kind, text, created_ms)
             VALUES (9, 5, 7, 'user', 'findable words', 3);
         INSERT INTO tool_calls (run, thread, route, call, tool, hash, result, created_ms)
             VALUES (7, 5, 'model', 'c1', 'draw', zeroblob(32), '{}', 4);",
    )
    .expect("V5 rows");
    drop(conn);
    let store = Store::open(&scratch.db()).expect("opens at the current schema");
    let version: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 11);
    assert_eq!(point(&store.conn, RunId(7)), (None, None, None));
    let state: String = store
        .conn
        .query_row("SELECT state FROM runs WHERE id = 7", [], |row| row.get(0))
        .expect("kept run");
    assert_eq!(state, "done");
    let event: (i64, i64, String, String, i64) = store
        .conn
        .query_row(
            "SELECT thread, run, kind, text, created_ms FROM events WHERE id = 9",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("kept event");
    assert_eq!(event, (5, 7, "user".into(), "findable words".into(), 3));
    let hits: Vec<i64> = store
        .conn
        .prepare("SELECT rowid FROM events_fts WHERE events_fts MATCH 'findable'")
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("hits")
        .collect::<Result<_, _>>()
        .expect("rowids");
    assert_eq!(hits, [9], "the index still finds it");
    let call: Call = store
        .conn
        .query_row(
            "SELECT run, thread, route, call, tool, hash, result, created_ms FROM tool_calls",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .expect("kept call");
    assert_eq!(
        call,
        (
            7,
            5,
            "model".into(),
            "c1".into(),
            "draw".into(),
            vec![0; 32],
            "{}".into(),
            4
        )
    );
}

#[test]
fn the_three_columns_are_set_together_or_not_at_all() {
    let scratch = Scratch::new("heard-pairs");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (_, ended) = run(&mut store);
    let id = i64::try_from(ended.0).expect("small id");
    for partial in [
        "UPDATE runs SET heard_segment = 1 WHERE id = ?1",
        "UPDATE runs SET heard_sample = 1 WHERE id = ?1",
        "UPDATE runs SET heard_text = 1 WHERE id = ?1",
        "UPDATE runs SET heard_segment = 1, heard_sample = 1 WHERE id = ?1",
        "UPDATE runs SET heard_sample = 1, heard_text = 1 WHERE id = ?1",
        "UPDATE runs SET heard_segment = 1, heard_text = 1 WHERE id = ?1",
    ] {
        assert!(store.conn.execute(partial, [id]).is_err(), "{partial}");
        assert_eq!(point(&store.conn, ended), (None, None, None), "{partial}");
    }
    for negative in [(-1, 0, 0), (0, -1, 0), (0, 0, -1)] {
        let (segment, sample, text) = negative;
        assert!(
            store
                .conn
                .execute(
                    "UPDATE runs SET heard_segment = ?2, heard_sample = ?3, heard_text = ?4
                     WHERE id = ?1",
                    [id, segment, sample, text],
                )
                .is_err(),
            "{negative:?}"
        );
        assert_eq!(
            point(&store.conn, ended),
            (None, None, None),
            "{negative:?}"
        );
    }
    store
        .conn
        .execute(
            "UPDATE runs SET heard_segment = 0, heard_sample = 0, heard_text = 0 WHERE id = ?1",
            [id],
        )
        .expect("all zero is a point");
    assert_eq!(point(&store.conn, ended), set(0, 0, 0));
    store
        .conn
        .execute(
            "UPDATE runs SET heard_segment = NULL, heard_sample = NULL, heard_text = NULL
             WHERE id = ?1",
            [id],
        )
        .expect("all NULL is unknown");
    assert_eq!(point(&store.conn, ended), (None, None, None));
    store.heard(ended, heard(1, 2, 3)).expect("written");
    assert_eq!(point(&store.conn, ended), set(1, 2, 3));
    assert!(
        store
            .conn
            .execute("UPDATE runs SET heard_text = NULL WHERE id = ?1", [id])
            .is_err(),
        "one column can't be cleared alone"
    );
    assert_eq!(point(&store.conn, ended), set(1, 2, 3));
}

#[test]
fn the_point_only_moves_forward() {
    let scratch = Scratch::new("heard-forward");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (_, ended) = run(&mut store);
    store.heard(ended, heard(1, 100, 10)).expect("first");
    assert_eq!(point(&store.conn, ended), set(1, 100, 10));
    for behind in [heard(1, 100, 99), heard(1, 99, 99), heard(0, 500, 99)] {
        store.heard(ended, behind).expect("refused quietly");
        assert_eq!(point(&store.conn, ended), set(1, 100, 10), "{behind:?}");
    }
    store.heard(ended, heard(1, 101, 10)).expect("later sample");
    assert_eq!(point(&store.conn, ended), set(1, 101, 10));
    store.heard(ended, heard(2, 0, 20)).expect("later segment");
    assert_eq!(point(&store.conn, ended), set(2, 0, 20));
}

#[test]
fn an_active_missing_or_deleted_run_is_left_alone() {
    let scratch = Scratch::new("heard-alone");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (_, active) = active_run(&mut store);
    store.heard(active, heard(0, 1, 1)).expect("no error");
    assert_eq!(point(&store.conn, active), (None, None, None), "active");

    let (thread, ended) = run(&mut store);
    store
        .conn
        .execute(
            "UPDATE threads SET state = 'deleting' WHERE id = ?1",
            [i64::try_from(thread.0).expect("small id")],
        )
        .expect("hidden");
    store.heard(ended, heard(0, 1, 1)).expect("no error");
    assert_eq!(point(&store.conn, ended), (None, None, None), "deleting");

    let runs = |store: &Store| -> i64 {
        store
            .conn
            .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
            .expect("count")
    };
    let before = runs(&store);
    store.heard(RunId(999), heard(0, 1, 1)).expect("no error");
    assert_eq!(runs(&store), before, "no row is made for a missing run");

    let (thread, deleted) = run(&mut store);
    assert_eq!(store.delete(thread).outcome, Deleted::Removed);
    let before = runs(&store);
    store.heard(deleted, heard(0, 1, 1)).expect("no error");
    assert_eq!(runs(&store), before, "a deleted run isn't made again");
}

#[test]
fn a_stored_point_survives_a_restart() {
    let scratch = Scratch::new("heard-restart");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (_, ended) = run(&mut store);
    store.heard(ended, heard(3, 4, 5)).expect("written");
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopens");
    assert_eq!(point(&store.conn, ended), set(3, 4, 5));
}
