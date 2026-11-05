//! The ledger's one write: atomic with the call's own write, live threads only, and purged.

use kumochi_protocol::message::{Deleted, RunId, ThreadId};
use rusqlite::{Connection, params};

use super::Store;
use crate::dispatch::{CommitCall, LedgerKey, LocalWrite, Route, ToolOutput};
use crate::test_support::Scratch;

fn opened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
}

/// A new task titled "Before" with run 1's turn in it.
fn task(store: &mut Store) -> (ThreadId, RunId) {
    let (thread, run) = store.high_water().expect("high water");
    let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(thread, Some("Before"), run, "ask")
        .expect("turn");
    (thread, run)
}

fn renaming(thread: ThreadId, run: RunId, call: &str) -> CommitCall {
    CommitCall {
        key: LedgerKey {
            run,
            route: Route::Model,
            call: call.into(),
        },
        thread,
        tool: "probe",
        hash: [7; 32],
        write: LocalWrite::Title("After".to_owned()),
        output: ToolOutput::Text("probed".to_owned()),
    }
}

fn title(conn: &Connection, thread: ThreadId) -> String {
    conn.query_row(
        "SELECT title FROM threads WHERE id = ?1",
        params![i64::try_from(thread.0).expect("small")],
        |row| row.get(0),
    )
    .expect("title")
}

fn rows(conn: &Connection) -> Vec<(String, String, String)> {
    let mut query = conn
        .prepare("SELECT route, call, result FROM tool_calls ORDER BY rowid")
        .expect("query");
    query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

#[test]
fn a_call_and_its_write_commit_together_and_survive_a_restart() {
    let scratch = Scratch::new("calls-commit");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    assert!(
        store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("recorded")
    );
    drop(store);
    let conn = Connection::open(scratch.db()).expect("reopen");
    assert_eq!(title(&conn, thread), "After");
    assert_eq!(
        rows(&conn),
        [(
            "model".to_owned(),
            "c1".to_owned(),
            r#"{"text":"probed"}"#.to_owned()
        )]
    );
}

#[test]
fn a_failed_record_rolls_back_the_calls_write() {
    let scratch = Scratch::new("calls-rollback");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let mut first = renaming(thread, run, "c1");
    first.write = LocalWrite::None;
    assert!(store.commit_call(&first).expect("recorded"));
    assert!(
        store.commit_call(&renaming(thread, run, "c1")).is_err(),
        "the key is recorded already"
    );
    assert_eq!(title(&store.conn, thread), "Before");
    assert_eq!(rows(&store.conn).len(), 1);
}

#[test]
fn the_ledger_caps_a_result_in_bytes_and_rolls_its_write_back_when_over() {
    let scratch = Scratch::new("calls-cap");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    // `{"text":""}` is 11 bytes and each two-byte letter counts twice: 65,536 and 65,537
    // bytes, in far fewer characters.
    let at = format!("a{}", "\u{e9}".repeat(32_762));
    let mut fits = renaming(thread, run, "fits");
    fits.write = LocalWrite::None;
    fits.output = ToolOutput::Text(at.clone());
    assert_eq!(fits.output.stored().len(), 65_536);
    assert!(store.commit_call(&fits).expect("recorded"));
    let mut over = renaming(thread, run, "over");
    over.output = ToolOutput::Text(format!("a{at}"));
    assert_eq!(over.output.stored().len(), 65_537);
    assert!(store.commit_call(&over).is_err(), "over the cap");
    assert_eq!(title(&store.conn, thread), "Before");
    assert_eq!(
        rows(&store.conn),
        [(
            "model".to_owned(),
            "fits".to_owned(),
            format!(r#"{{"text":"{at}"}}"#)
        )]
    );
}

#[test]
fn a_call_in_a_thread_being_deleted_writes_nothing() {
    let scratch = Scratch::new("calls-deleting");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    store.begin_delete(thread).expect("hidden");
    assert!(
        !store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("no error")
    );
    assert_eq!(title(&store.conn, thread), "Before");
    assert_eq!(rows(&store.conn), []);
}

#[test]
fn deleting_a_thread_removes_its_calls() {
    let scratch = Scratch::new("calls-purge");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let mut mcp = renaming(thread, run, "m:n:1");
    mcp.key.route = Route::Mcp(crate::bridge::LaunchId(0xab));
    mcp.write = LocalWrite::None;
    assert!(
        store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("recorded")
    );
    assert!(store.commit_call(&mcp).expect("recorded"));
    assert_eq!(
        rows(&store.conn)[1].0,
        "mcp:000000000000000000000000000000ab"
    );
    assert_eq!(store.delete(thread).outcome, Deleted::Removed);
    assert_eq!(rows(&store.conn), []);
}
