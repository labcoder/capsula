//! A local task's binding to the server its first stored turn arrived under.

use kumochi_protocol::message::{RunId, ThreadId};
use rusqlite::{Connection, params};

use super::tests::store_at;
use super::{Store, StoreError};
use crate::context::ConnectionTag;
use crate::test_support::Scratch;

fn opened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
}

/// A task saved on the local route with no runs or events, as a new local task starts.
fn fresh(conn: &Connection, id: i64, connection: &str) {
    conn.execute(
        "INSERT INTO threads (id, title, created_ms, updated_ms, model, effort, connection)
         VALUES (?1, 'local', 1, 1, 'local-model', 'low', ?2)",
        params![id, connection],
    )
    .expect("a fresh task");
}

fn tag_of(conn: &Connection, id: i64) -> Option<Vec<u8>> {
    conn.query_row("SELECT local_tag FROM threads WHERE id = ?1", [id], |row| {
        row.get(0)
    })
    .expect("the thread")
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("count")
}

#[test]
fn a_fresh_local_task_binds_to_its_first_server_and_keeps_it() {
    let scratch = Scratch::new("local-bind-fresh");
    let mut store = opened(&scratch);
    fresh(store.conn_for_tests(), 2, "local_chat");
    let first = ConnectionTag(7);
    assert!(
        store
            .begin_local_turn(ThreadId(2), first, RunId(1), "first")
            .expect("stored")
    );
    assert_eq!(
        tag_of(store.conn_for_tests(), 2),
        Some(7_u64.to_be_bytes().to_vec())
    );
    assert!(
        store
            .begin_local_turn(ThreadId(2), first, RunId(2), "again")
            .expect("stored")
    );
    assert!(
        !store
            .begin_local_turn(ThreadId(2), ConnectionTag(9), RunId(3), "moved")
            .expect("refused")
    );
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let conn = store.conn_for_tests();
    assert_eq!(tag_of(conn, 2), Some(7_u64.to_be_bytes().to_vec()));
    assert_eq!(count(conn, "SELECT count(*) FROM runs WHERE thread = 2"), 2);
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM events WHERE thread = 2 AND kind = 'user'"
        ),
        2
    );
    assert_eq!(
        count(conn, "SELECT count(*) FROM events WHERE text = 'moved'"),
        0
    );
}

#[test]
fn the_largest_tag_round_trips() {
    let scratch = Scratch::new("local-bind-max");
    let mut store = opened(&scratch);
    fresh(store.conn_for_tests(), 2, "local_chat");
    let tag = ConnectionTag(u64::MAX);
    assert!(
        store
            .begin_local_turn(ThreadId(2), tag, RunId(1), "first")
            .expect("stored")
    );
    assert!(
        store
            .begin_local_turn(ThreadId(2), tag, RunId(2), "again")
            .expect("stored")
    );
    assert_eq!(tag_of(store.conn_for_tests(), 2), Some(vec![0xff; 8]));
}

/// The zero tag, a cloud task, the main thread and a missing thread bind and store nothing.
#[test]
fn only_a_local_task_and_a_real_tag_bind() {
    let scratch = Scratch::new("local-bind-refused");
    let mut store = opened(&scratch);
    fresh(store.conn_for_tests(), 2, "local_chat");
    fresh(store.conn_for_tests(), 3, "chatgpt");
    let main: i64 = store
        .conn_for_tests()
        .query_row("SELECT id FROM threads WHERE kind = 'main'", [], |row| {
            row.get(0)
        })
        .expect("main");
    store
        .conn_for_tests()
        .execute(
            "UPDATE threads SET model = 'local-model', effort = 'low', connection = 'local_chat'
             WHERE id = ?1",
            [main],
        )
        .expect("a main thread saved local");
    let main = ThreadId(u64::try_from(main).expect("positive"));
    for (thread, tag) in [
        (ThreadId(2), ConnectionTag(0)),
        (ThreadId(3), ConnectionTag(7)),
        (main, ConnectionTag(7)),
        (ThreadId(40), ConnectionTag(7)),
    ] {
        assert!(
            !store
                .begin_local_turn(thread, tag, RunId(1), "refused")
                .expect("refused"),
            "{thread:?}"
        );
    }
    let conn = store.conn_for_tests();
    assert_eq!(count(conn, "SELECT count(*) FROM runs"), 0);
    assert_eq!(
        count(conn, "SELECT count(*) FROM events WHERE text = 'refused'"),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM threads WHERE local_tag IS NOT NULL"
        ),
        0
    );
}

/// A V14 task that was saved local after it had history opens unbound and is never adopted.
#[test]
fn a_local_task_with_history_from_before_binding_is_never_adopted() {
    let scratch = Scratch::new("local-bind-v14");
    let db = store_at(scratch.dir(), 14);
    let conn = Connection::open(&db).expect("raw");
    fresh(&conn, 2, "local_chat");
    conn.execute_batch(
        "INSERT INTO runs (id, thread, state, started_ms) VALUES (1, 2, 'done', 1);
         INSERT INTO events (thread, run, kind, text, created_ms) VALUES (2, 1, 'user', 'old', 1);",
    )
    .expect("history from before binding");
    drop(conn);
    let mut store = Store::open_existing(&db).expect("migrated");
    assert_eq!(tag_of(store.conn_for_tests(), 2), None);
    assert!(
        !store
            .begin_local_turn(ThreadId(2), ConnectionTag(7), RunId(2), "new")
            .expect("refused")
    );
    let conn = store.conn_for_tests();
    assert_eq!(tag_of(conn, 2), None);
    assert_eq!(count(conn, "SELECT count(*) FROM runs WHERE thread = 2"), 1);
    assert_eq!(
        count(conn, "SELECT count(*) FROM events WHERE thread = 2"),
        1
    );
}

/// A turn whose run id isn't new fails, and the binding it would have stamped goes with it.
#[test]
fn a_failed_turn_leaves_its_task_unbound() {
    let scratch = Scratch::new("local-bind-rollback");
    let mut store = opened(&scratch);
    fresh(store.conn_for_tests(), 2, "local_chat");
    store
        .conn_for_tests()
        .execute(
            "INSERT INTO sqlite_sequence (name, seq) SELECT 'runs', 5
             WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'runs')",
            [],
        )
        .expect("runs used up to 5");
    assert!(matches!(
        store.begin_local_turn(ThreadId(2), ConnectionTag(7), RunId(5), "stale"),
        Err(StoreError::StaleId)
    ));
    assert_eq!(tag_of(store.conn_for_tests(), 2), None);
    assert_eq!(
        count(store.conn_for_tests(), "SELECT count(*) FROM runs"),
        0
    );
}
