//! The ledger's one write: atomic with the call's own write, live threads only, and purged.

use kumochi_protocol::message::{
    Deleted, IssueBody, IssueTitle, RepoName, RunId, ThreadId, ThreadTitle,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::{Value, json};

use super::{Store, StoreError};
use crate::connections::github::api::GitHubUser;
use crate::dispatch::{
    CommitCall, Committed, LedgerKey, LocalWrite, Route, Then, ToolError, ToolOutput,
};
use crate::store::{Interruption, RunEnd};
use crate::test_support::{SENTINEL, Scratch};

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

fn probe_call(thread: ThreadId, run: RunId, call: &str) -> CommitCall {
    CommitCall {
        key: LedgerKey {
            run,
            route: Route::Model,
            call: call.into(),
        },
        thread,
        tool: "probe",
        hash: [7; 32],
        write: LocalWrite::None,
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
fn deleting_a_thread_removes_its_calls() {
    let scratch = Scratch::new("calls-purge");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let mut mcp = probe_call(thread, run, "m:n:1");
    mcp.key.route = Route::Mcp(crate::bridge::LaunchId(0xab));
    assert_eq!(
        store
            .commit_call(&probe_call(thread, run, "c1"))
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
        let mut focus = probe_call(thread, run, call);
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
    let mut create = probe_call(thread, run, call);
    create.tool = "create_task";
    create.output = ToolOutput::Text(format!("Created task {}.", task.0));
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
    drop(store);
    let (store, high) = reopened(&scratch);
    assert_eq!(high, fresh, "the second reservation stays burned");
    assert_eq!(task_row(&store.conn, first), row("Trip plan"));
    let ledger: String = store
        .conn
        .query_row(
            "SELECT json_array(run, thread, route, call, tool, hex(hash), result) FROM tool_calls",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ledger).unwrap(),
        json!([
            run.0,
            thread.0,
            "model",
            "c1",
            "create_task",
            "07".repeat(32),
            format!(r#"{{"text":"Created task {}."}}"#, first.0),
        ])
    );
    assert_eq!(rows(&store.conn).len(), 1);
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
    assert_eq!(title(&store.conn, thread), "Before");
    assert_eq!(rows(&store.conn), []);
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
    assert_eq!(super::tests::purge(&mut store), crate::store::Swap::Done);
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

fn drafting(thread: ThreadId, run: RunId, call: &str) -> CommitCall {
    let mut draft = probe_call(thread, run, call);
    draft.tool = "draft_issue";
    draft.hash = [9; 32];
    draft.write = LocalWrite::DraftIssue {
        title: IssueTitle::new("  Local café  ").unwrap(),
        body: IssueBody::new("first\r\n\tcafé\r\n").unwrap(),
        repo: Some(RepoName::new("owner/repo").unwrap()),
        account: Some(GitHubUser {
            id: u64::MAX,
            login: kumochi_protocol::message::GitHubLogin::new("owner").unwrap(),
        }),
    };
    draft.output = ToolOutput::Text("Local draft saved for user review.".to_owned());
    draft.then = Some(Then::IssueDrafted);
    draft
}

fn draft_events(conn: &Connection) -> Vec<Value> {
    conn.prepare(
        "SELECT json_array(thread, run, kind, text, body) FROM events
         WHERE kind IN ('draft', 'action') ORDER BY id",
    )
    .unwrap()
    .query_map([], |row| row.get::<_, String>(0))
    .unwrap()
    .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
    .collect()
}

#[test]
fn a_draft_call_commits_exact_normalized_action_event_and_ledger_contents() {
    let scratch = Scratch::new("calls-draft-content");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    task(&mut store);
    let call = drafting(thread, run, "draft-1");
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    drop(store);
    let conn = Connection::open(scratch.db()).unwrap();
    let action: String = conn
        .query_row(
            "SELECT json_array(id, thread, version, stage, repo, account_id, account_login,
         title, body, approval, consumed, uncertain) FROM actions",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&action).unwrap(),
        json!([
            1,
            thread.0,
            1,
            "{\"kind\":\"draft\"}",
            "owner/repo",
            "18446744073709551615",
            "owner",
            "  Local café  ",
            "first\n\tcafé\n",
            1,
            null,
            0
        ])
    );
    let (actions, approval) = super::actions_tests::snapshot(&conn);
    assert_eq!(actions.len(), 1);
    assert_eq!(approval, 1);
    assert_eq!(
        draft_events(&conn),
        [json!([
            thread.0,
            null,
            "draft",
            "  Local café  ",
            "{\"draft\":1}"
        ])]
    );
    assert_eq!(
        rows(&conn),
        [(
            "model".to_owned(),
            "draft-1".to_owned(),
            r#"{"text":"Local draft saved for user review."}"#.to_owned(),
        )]
    );
    let ledger: String = conn
        .query_row(
            "SELECT json_array(run, thread, route, call, tool, hex(hash), result) FROM tool_calls",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ledger).unwrap(),
        json!([
            run.0,
            thread.0,
            "model",
            "draft-1",
            "draft_issue",
            "09".repeat(32),
            "{\"text\":\"Local draft saved for user review.\"}"
        ])
    );
    assert_eq!(title(&conn, thread), "Before");
}

#[test]
fn an_unbound_draft_uses_only_its_callers_thread_and_stays_unbound() {
    let scratch = Scratch::new("calls-draft-unbound");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let mut call = drafting(thread, run, "unbound");
    let LocalWrite::DraftIssue { repo, account, .. } = &mut call.write else {
        panic!("draft fixture");
    };
    *repo = None;
    *account = None;
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    let action = store
        .action(kumochi_protocol::message::DraftId(1))
        .unwrap()
        .unwrap();
    assert_eq!(action.thread, thread);
    assert_eq!(action.repo, None);
    assert_eq!(action.account, None);
    assert_eq!(action.stage, crate::issues::Stage::Draft);
    assert_eq!(
        action.approval,
        Some(kumochi_protocol::message::ApprovalRev(1))
    );
    assert_eq!(action.consumed, None);
    assert_eq!(action.uncertain, 0);
    assert_eq!(draft_events(&store.conn).len(), 1);
    assert_eq!(rows(&store.conn).len(), 1);
}

#[test]
fn duplicate_draft_keys_never_create_a_second_action_event_or_result() {
    let scratch = Scratch::new("calls-draft-duplicate");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    let first = drafting(thread, run, SENTINEL);
    assert_eq!(store.commit_call(&first).unwrap(), Committed::Done);
    let before = super::actions_tests::snapshot(&store.conn);
    let events = draft_events(&store.conn);
    let ledger = rows(&store.conn);
    let mut changed = first.clone();
    let LocalWrite::DraftIssue { title, body, .. } = &mut changed.write else {
        panic!("draft fixture");
    };
    *title = IssueTitle::new("replacement must not persist").unwrap();
    *body = IssueBody::new("different body").unwrap();
    for duplicate in [&first, &changed] {
        let error = store.commit_call(duplicate).unwrap_err();
        assert!(matches!(error, StoreError::Call));
        let bytes = format!("{:?}", SENTINEL.as_bytes());
        for shown in [error.to_string(), format!("{error:?}")] {
            for private in [
                "UNIQUE",
                "constraint",
                "tool_calls",
                SENTINEL,
                bytes.trim_matches(['[', ']']),
            ] {
                assert!(!shown.contains(private), "{shown}");
            }
        }
        assert_eq!(super::actions_tests::snapshot(&store.conn), before);
        assert_eq!(draft_events(&store.conn), events);
        assert_eq!(rows(&store.conn), ledger);
        assert_eq!(title(&store.conn, thread), "Before");
    }
    assert_eq!(before.0.len(), 1);
    assert_eq!(before.1, 1);
    assert_eq!(events.len(), 1);
    assert_eq!(
        ledger,
        [(
            "model".to_owned(),
            SENTINEL.to_owned(),
            r#"{"text":"Local draft saved for user review."}"#.to_owned(),
        )]
    );
}

#[test]
fn a_ledger_failure_after_action_and_event_insertion_rolls_back_all_three() {
    let scratch = Scratch::new("calls-draft-trigger");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER reject_draft_ledger BEFORE INSERT ON tool_calls
         WHEN new.tool = 'draft_issue'
         AND (SELECT count(*) FROM actions) = 1
         AND (SELECT count(*) FROM events WHERE kind = 'draft') = 1
         AND (SELECT last FROM approval_mark WHERE id = 1) = 1
         BEGIN SELECT RAISE(ABORT, 'private-ledger-boundary-sentinel'); END;",
        )
        .unwrap();
    let before = super::actions_tests::snapshot(&store.conn);
    let error = store
        .commit_call(&drafting(thread, run, "blocked"))
        .unwrap_err();
    assert!(matches!(error, StoreError::Call));
    assert_eq!(super::actions_tests::snapshot(&store.conn), before);
    assert_eq!(draft_events(&store.conn), Vec::<Value>::new());
    assert_eq!(rows(&store.conn), []);
    assert!(!format!("{error:?} {error}").contains("private-ledger-boundary-sentinel"));
    store
        .conn
        .execute_batch("DROP TRIGGER reject_draft_ledger")
        .unwrap();
    assert_eq!(
        store
            .commit_call(&drafting(thread, run, "after-trigger"))
            .unwrap(),
        Committed::Done
    );
    let (actions, approval) = super::actions_tests::snapshot(&store.conn);
    assert_eq!(actions.len(), 1);
    assert_eq!(approval, 1);
    assert_eq!(draft_events(&store.conn).len(), 1);
    assert_eq!(rows(&store.conn).len(), 1);
}

#[test]
fn draft_output_bounds_commit_or_roll_back_the_complete_transaction() {
    let exact = format!("a{}", "é".repeat(32_762));
    for prior_success in [false, true] {
        let scratch = Scratch::new("calls-draft-output");
        let mut store = opened(&scratch);
        let (thread, run) = task(&mut store);
        if prior_success {
            let mut fits = drafting(thread, run, "fits");
            fits.output = ToolOutput::Text(exact.clone());
            assert_eq!(fits.output.stored().len(), 65_536);
            assert_eq!(store.commit_call(&fits).unwrap(), Committed::Done);
            let (actions, approval) = super::actions_tests::snapshot(&store.conn);
            assert_eq!(actions.len(), 1);
            assert_eq!(approval, 1);
            assert_eq!(draft_events(&store.conn).len(), 1);
            assert_eq!(
                rows(&store.conn),
                [(
                    "model".to_owned(),
                    "fits".to_owned(),
                    format!(r#"{{"text":"{exact}"}}"#),
                )]
            );
        }

        let before = super::actions_tests::snapshot(&store.conn);
        let events = draft_events(&store.conn);
        let ledger = rows(&store.conn);
        let mut over = drafting(thread, run, "over");
        over.output = ToolOutput::Text(format!("a{exact}"));
        assert_eq!(over.output.stored().len(), 65_537);
        let error = store.commit_call(&over).unwrap_err();
        assert!(matches!(error, StoreError::Call));
        for shown in [error.to_string(), format!("{error:?}")] {
            assert!(!shown.contains("CHECK"), "{shown}");
            assert!(!shown.contains("constraint"), "{shown}");
        }
        assert_eq!(super::actions_tests::snapshot(&store.conn), before);
        assert_eq!(draft_events(&store.conn), events);
        assert_eq!(rows(&store.conn), ledger);
        assert_eq!(title(&store.conn, thread), "Before");
    }
}

#[test]
fn draft_admission_requires_the_matching_active_run_and_live_thread() {
    for case in [
        "missing run",
        "missing thread",
        "wrong run",
        "wrong thread",
        "done run",
        "canceled run",
        "deleting thread",
        "deleted thread",
        "large run",
        "large thread",
    ] {
        let scratch = Scratch::new("calls-draft-owner");
        let mut store = opened(&scratch);
        let (thread, run) = task(&mut store);
        let (other, other_run) = task(&mut store);
        let mut call = drafting(thread, run, "lost-owner");
        match case {
            "missing run" => call.key.run = RunId(other_run.0 + 1),
            "missing thread" => call.thread = ThreadId(other.0 + 1),
            "wrong run" => call.key.run = other_run,
            "wrong thread" => call.thread = other,
            "done run" => store.end_run(thread, run, &[], RunEnd::Done).unwrap(),
            "canceled run" => store
                .end_run(thread, run, &[], RunEnd::Interrupted(Interruption::Stop))
                .unwrap(),
            "deleting thread" => {
                store.begin_delete(thread).unwrap();
            }
            "deleted thread" => assert_eq!(store.delete(thread).outcome, Deleted::Removed),
            "large run" => call.key.run = RunId(u64::MAX),
            "large thread" => call.thread = ThreadId(u64::MAX),
            _ => unreachable!(),
        }
        let before = super::actions_tests::snapshot(&store.conn);
        let events = draft_events(&store.conn);
        let ledger = rows(&store.conn);
        assert_eq!(
            store.commit_call(&call).unwrap(),
            Committed::Failed,
            "{case}"
        );
        assert_eq!(
            super::actions_tests::snapshot(&store.conn),
            before,
            "{case}"
        );
        assert_eq!(draft_events(&store.conn), events, "{case}");
        assert_eq!(rows(&store.conn), ledger, "{case}");
        assert_eq!(
            store
                .commit_call(&drafting(other, other_run, "after-refusal"))
                .unwrap(),
            Committed::Done,
            "{case}"
        );
        assert_eq!(super::actions_tests::snapshot(&store.conn).0.len(), 1);
        assert_eq!(draft_events(&store.conn).len(), 1);
        assert_eq!(rows(&store.conn).len(), 1);
    }
}

#[test]
fn a_non_draft_call_keeps_its_existing_ended_run_behavior() {
    let scratch = Scratch::new("calls-existing-ended");
    let mut store = opened(&scratch);
    let (thread, run) = task(&mut store);
    store.end_run(thread, run, &[], RunEnd::Done).unwrap();
    let created = ThreadId(thread.0 + 1);
    assert_eq!(
        store
            .commit_call(&creating(thread, run, "legacy", created))
            .unwrap(),
        Committed::Done
    );
    drop(store);
    let (store, high) = reopened(&scratch);
    assert_eq!(high, created);
    assert_eq!(
        task_row(&store.conn, created),
        Some((
            "Trip plan".to_owned(),
            "model".to_owned(),
            "thread".to_owned(),
            "live".to_owned(),
        ))
    );
    assert_eq!(
        rows(&store.conn),
        [(
            "model".to_owned(),
            "legacy".to_owned(),
            format!(r#"{{"text":"Created task {}."}}"#, created.0),
        )]
    );
}

#[test]
fn outer_commit_call_debug_redacts_draft_content_as_text_and_byte_lists() {
    let mut call = drafting(ThreadId(1), RunId(1), SENTINEL);
    call.tool = SENTINEL;
    call.write = LocalWrite::DraftIssue {
        title: IssueTitle::new(SENTINEL).unwrap(),
        body: IssueBody::new(SENTINEL).unwrap(),
        repo: Some(RepoName::new(&format!("{SENTINEL}/{SENTINEL}")).unwrap()),
        account: Some(GitHubUser {
            id: 7,
            login: kumochi_protocol::message::GitHubLogin::new(SENTINEL).unwrap(),
        }),
    };
    call.output = ToolOutput::Text(SENTINEL.to_owned());
    let debug = format!("{:?}", Ok::<_, StoreError>(call));
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    assert!(!debug.contains(SENTINEL));
    assert!(!debug.contains(&byte_list));
    assert!(!debug.contains(byte_list.trim_start_matches('[').trim_end_matches(']')));
}
