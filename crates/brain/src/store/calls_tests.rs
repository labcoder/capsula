//! The ledger's one write: atomic with the call's own write, live threads only, and purged.

use kumochi_protocol::message::{Deleted, RunId, ThreadId, ThreadTitle};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::{Store, StoreError};
use crate::dispatch::{CommitCall, Committed, LedgerKey, LocalWrite, Route, ToolError, ToolOutput};
use crate::store::RunEnd;
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
        then: None,
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
    assert_eq!(
        store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("recorded"),
        Committed::Done
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
    assert_eq!(
        store.commit_call(&first).expect("recorded"),
        Committed::Done
    );
    let error = store
        .commit_call(&renaming(thread, run, "c1"))
        .expect_err("the key is recorded already");
    assert!(matches!(error, StoreError::Call), "{error:?}");
    for shown in [error.to_string(), format!("{error:?}")] {
        for sql in ["UNIQUE", "constraint", "tool_calls", "c1"] {
            assert!(!shown.contains(sql), "{shown}");
        }
    }
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
    assert_eq!(store.commit_call(&fits).expect("recorded"), Committed::Done);
    let mut over = renaming(thread, run, "over");
    over.output = ToolOutput::Text(format!("a{at}"));
    assert_eq!(over.output.stored().len(), 65_537);
    let error = store.commit_call(&over).expect_err("over the cap");
    assert!(matches!(error, StoreError::Call), "{error:?}");
    for shown in [error.to_string(), format!("{error:?}")] {
        assert!(!shown.contains("CHECK"), "{shown}");
        assert!(!shown.contains("constraint"), "{shown}");
    }
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
    assert_eq!(
        store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("no error"),
        Committed::Failed
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
    assert_eq!(
        store
            .commit_call(&renaming(thread, run, "c1"))
            .expect("recorded"),
        Committed::Done
    );
    assert_eq!(store.commit_call(&mcp).expect("recorded"), Committed::Done);
    assert_eq!(
        rows(&store.conn)[1].0,
        "mcp:000000000000000000000000000000ab"
    );
    assert_eq!(store.delete(thread).outcome, Deleted::Removed);
    assert_eq!(rows(&store.conn), []);
}

#[test]
fn a_focus_commits_only_while_its_target_is_live() {
    let scratch = Scratch::new("calls-focus");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let (other, _) = task(&mut store);
    let focus = |call: &str, target: ThreadId| {
        let mut focus = renaming(thread, run, call);
        focus.write = LocalWrite::Focus(target);
        focus
    };
    assert_eq!(
        store.commit_call(&focus("live", other)).expect("recorded"),
        Committed::Done
    );
    assert_eq!(
        store
            .commit_call(&focus("missing", ThreadId(other.0 + 1)))
            .expect("no error"),
        Committed::Refused(ToolError::NoTask)
    );
    assert_eq!(
        store
            .commit_call(&focus("too-big", ThreadId(u64::MAX)))
            .expect("no error"),
        Committed::Refused(ToolError::NoTask)
    );
    store.begin_delete(other).expect("hidden");
    assert_eq!(
        store
            .commit_call(&focus("deleting", other))
            .expect("no error"),
        Committed::Refused(ToolError::NoTask)
    );
    assert_eq!(
        rows(&store.conn),
        [(
            "model".to_owned(),
            "live".to_owned(),
            r#"{"text":"probed"}"#.to_owned()
        )]
    );
    assert_eq!(title(&store.conn, thread), "Before");
}

fn creating(thread: ThreadId, run: RunId, call: &str, task: ThreadId) -> CommitCall {
    let mut create = renaming(thread, run, call);
    create.write = LocalWrite::CreateTask {
        thread: task,
        title: ThreadTitle::new("Trip plan").expect("valid"),
        focus: false,
    };
    create
}

/// `id`'s title, title source, kind and state, if it has a row.
fn task_row(conn: &Connection, id: ThreadId) -> Option<(String, String, String, String)> {
    conn.query_row(
        "SELECT title, title_source, kind, state FROM threads WHERE id = ?1",
        params![i64::try_from(id.0).expect("small")],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .expect("read")
}

fn reopened(scratch: &Scratch) -> (Store, ThreadId) {
    let store = opened(scratch);
    let (high, _) = store.high_water().expect("high water");
    (store, high)
}

#[test]
fn a_created_task_commits_with_its_row_and_a_repeated_key_burns_its_fresh_id() {
    let scratch = Scratch::new("calls-create");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let first = ThreadId(thread.0 + 1);
    assert_eq!(
        store
            .commit_call(&creating(thread, run, "c1", first))
            .expect("recorded"),
        Committed::Done
    );
    let row = |title: &str| {
        Some((
            title.to_owned(),
            "model".to_owned(),
            "thread".to_owned(),
            "live".to_owned(),
        ))
    };
    assert_eq!(task_row(&store.conn, first), row("Trip plan"));
    let fresh = ThreadId(first.0 + 1);
    let error = store
        .commit_call(&creating(thread, run, "c1", fresh))
        .expect_err("the key is recorded already");
    assert!(matches!(error, StoreError::Call), "{error:?}");
    assert_eq!(task_row(&store.conn, fresh), None);
    assert_eq!(rows(&store.conn).len(), 1);
    drop(store);
    let (store, high) = reopened(&scratch);
    assert_eq!(high, fresh, "the second reservation stays burned");
    assert_eq!(task_row(&store.conn, first), row("Trip plan"));
}

#[test]
fn a_reservation_whose_task_never_commits_stays_issued() {
    let scratch = Scratch::new("calls-reserve");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let error = store
        .commit_call(&creating(thread, run, "c0", thread))
        .expect_err("not above the high-water");
    assert!(matches!(error, StoreError::StaleId), "{error:?}");
    store.begin_delete(thread).expect("hidden");
    let reserved = ThreadId(thread.0 + 1);
    assert_eq!(
        store
            .commit_call(&creating(thread, run, "c1", reserved))
            .expect("no error"),
        Committed::Failed
    );
    assert_eq!(task_row(&store.conn, reserved), None);
    drop(store);
    let (mut store, high) = reopened(&scratch);
    assert_eq!(high, reserved);
    let (_, last_run) = store.high_water().expect("high water");
    let next_run = RunId(last_run.0 + 1);
    assert!(matches!(
        store.begin_turn(reserved, Some("Again"), next_run, "ask"),
        Err(StoreError::StaleId)
    ));
    store
        .begin_turn(ThreadId(reserved.0 + 1), Some("Again"), next_run, "ask")
        .expect("the next id is free");
}

#[test]
fn a_reservation_with_no_row_behind_it_survives_a_purge_and_a_reopen() {
    let scratch = Scratch::new("calls-reserve-purge");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    assert_eq!(store.delete(thread).outcome, Deleted::Removed);
    let (high, _) = store.high_water().expect("high water");
    let reserved = ThreadId(high.0 + 2);
    store.reserve(reserved).expect("reserved");
    let max: i64 = store
        .conn
        .query_row("SELECT max(id) FROM threads", [], |row| row.get(0))
        .expect("max");
    assert!(u64::try_from(max).expect("positive") < reserved.0);
    super::tests::purge(&mut store);
    drop(store);
    let (mut store, high) = reopened(&scratch);
    assert_eq!(high, reserved);
    let (_, last_run) = store.high_water().expect("high water");
    let next = ThreadId(high.0 + 1);
    store
        .begin_turn(next, Some("After"), RunId(last_run.0 + 1), "ask")
        .expect("allocated");
    assert!(next.0 > reserved.0);
}
