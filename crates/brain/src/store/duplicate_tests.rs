use kumochi_protocol::message::{
    ApprovalRev, Decision, DraftId, FailReason, GitHubLogin, IssueRefused, RepoName, RunId,
    StateVersion, ThreadId,
};

use super::actions_tests::{
    A, body, mark, opened, posting, repo, seed, snapshot, title, uncertain, unknown, user,
};
use super::{RunEnd, Store, StoreError};
use crate::connections::github::api::GitHubUser;
use crate::dispatch::{CommitCall, Committed, LedgerKey, LocalWrite, Route, Then, ToolOutput};
use crate::issues::{Action, Answer, Live, Stage};
use crate::test_support::Scratch;

fn drafted(store: &mut Store, key: &str, raw_body: &str) -> Action {
    let (thread, run) = store.high_water().unwrap();
    let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store.begin_turn(thread, Some("task"), run, "ask").unwrap();
    let call = CommitCall {
        key: LedgerKey {
            run,
            route: Route::Model,
            call: key.into(),
        },
        thread,
        tool: "draft_issue",
        hash: [9; 32],
        write: LocalWrite::DraftIssue {
            title: title("Same café"),
            body: body(raw_body),
            repo: Some(repo()),
            account: Some(user(7)),
        },
        output: ToolOutput::Text(
            "Saved a local issue draft for user review. Nothing was filed on GitHub.".to_owned(),
        ),
        then: Some(Then::IssueDrafted),
    };
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    let id: i64 = store
        .conn
        .query_row(
            "SELECT id FROM actions WHERE thread = ?1",
            [i64::try_from(thread.0).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    store.end_run(thread, run, &[], RunEnd::Done).unwrap();
    store
        .action(DraftId(u64::try_from(id).unwrap()))
        .unwrap()
        .unwrap()
}

fn changed(answer: Result<Answer, StoreError>) -> Action {
    match answer {
        Ok(Answer::Changed(action)) => action,
        other => panic!("expected a change: {other:?}"),
    }
}

fn file(store: &mut Store, action: &Action, account: u64) -> Result<Answer, StoreError> {
    let (account, repos) = (
        user(account),
        [repo(), RepoName::new("octo/other").unwrap()],
    );
    store.consent(
        action.draft,
        &action.hash(),
        action.approval.unwrap(),
        Decision::File,
        &Live {
            capacity: true,
            account: Some(&account),
            repos: &repos,
        },
    )
}

fn unknown_once(store: &mut Store, action: &Action) -> Action {
    let preflight = changed(file(store, action, 7));
    let posting = changed(store.preflighted(action.draft, preflight.version, Ok(mark(A))));
    changed(store.posted(action.draft, posting.version, &uncertain()))
}

fn events(store: &Store) -> Vec<(String, String, String)> {
    store
        .conn
        .prepare("SELECT kind, text, body FROM events WHERE kind IN ('draft','action') ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn unresolved_content_blocks_distinct_keys_before_or_after_creation_and_dismissal() {
    for (case, early, dismissed, original) in [
        ("unknown", false, false, ApprovalRev(3)),
        ("dismissed unknown", false, true, ApprovalRev(3)),
        ("already existing draft", true, false, ApprovalRev(2)),
    ] {
        let scratch = Scratch::new("duplicate-unresolved");
        let mut store = opened(&scratch);
        let a = drafted(&mut store, "draft-a", "first\r\n\tcafé\r\n");
        let early_b = early.then(|| drafted(&mut store, "draft-b", "first\n\tcafé\n"));
        let a = unknown_once(&mut store, &a);
        assert_eq!(a.uncertain, 1);
        assert!(matches!(a.stage, Stage::Unknown { .. }));
        let a = if dismissed {
            changed(store.discard_draft(a.draft))
        } else {
            a
        };
        if dismissed {
            assert_eq!(a.stage, Stage::Discarded);
            assert_eq!(a.uncertain, 1);
        }
        let b = early_b.unwrap_or_else(|| drafted(&mut store, "draft-b", "first\n\tcafé\n"));
        assert_eq!((a.draft, b.draft), (DraftId(1), DraftId(2)));
        assert_eq!((a.thread, b.thread), (ThreadId(2), ThreadId(3)));
        assert_eq!(a.body.as_str(), "first\n\tcafé\n");
        assert_eq!(b.body.as_str(), "first\n\tcafé\n");
        assert_eq!(b.version, StateVersion(1));
        assert_eq!(b.stage, Stage::Draft);
        assert_eq!(b.approval, Some(original));
        let before = snapshot(&store.conn);
        assert_eq!(before.1, 3);
        let mut expected_events = events(&store);
        let Answer::Refused(why, after) = file(&mut store, &b, 7).unwrap() else {
            panic!("{case}: the other draft's File must be refused");
        };
        assert_eq!(why, IssueRefused::UnresolvedDraft { draft: a.draft });
        let expected = Action {
            version: StateVersion(2),
            approval: Some(ApprovalRev(4)),
            consumed: Some(original),
            ..b.clone()
        };
        assert_eq!(after, expected, "{case}");
        assert_eq!(store.action(a.draft).unwrap(), Some(a));
        assert_eq!(store.action(b.draft).unwrap(), Some(expected.clone()));
        let after = snapshot(&store.conn);
        assert_eq!(after.0.len(), 2);
        assert_eq!(after.0[0], before.0[0], "{case}: A is untouched");
        assert_eq!(after.1, 4);
        expected_events.push(("action".into(), "Same café".into(), "{\"draft\":2}".into()));
        assert_eq!(events(&store), expected_events);
        let sending: i64 = store.conn.query_row(
            "SELECT count(*) FROM actions WHERE json_extract(stage, '$.kind') IN ('preflight','posting')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(sending, 0, "{case}: B never admits another preflight");
        let Answer::Unchanged(replayed) = file(&mut store, &b, 7).unwrap() else {
            panic!("{case}: the consumed approval is a no-op");
        };
        assert_eq!(replayed, expected);
        assert_eq!(snapshot(&store.conn), after);
        assert_eq!(events(&store), expected_events);
        let keys: Vec<(i64, i64, String, String)> = store
            .conn
            .prepare("SELECT run, thread, route, call FROM tool_calls ORDER BY run")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            keys,
            [
                (1, 2, "model".into(), "draft-a".into()),
                (2, 3, "model".into(), "draft-b".into()),
            ]
        );
    }
}

#[test]
fn a_refused_file_retry_restores_failed_and_keeps_its_consumed_approval_spent() {
    let scratch = Scratch::new("duplicate-failed-retry");
    let mut store = opened(&scratch);
    let a = drafted(&mut store, "draft-a", "first\r\n\tcafé\r\n");
    let b = drafted(&mut store, "draft-b", "first\n\tcafé\n");
    let preflight = changed(file(&mut store, &b, 7));
    let b = changed(store.preflighted(b.draft, preflight.version, Err(FailReason::Network)));
    assert_eq!(b.approval, Some(ApprovalRev(3)));
    assert_eq!(
        b.stage,
        Stage::Failed {
            reason: FailReason::Network
        }
    );
    let a = unknown_once(&mut store, &a);
    let before = snapshot(&store.conn);
    let mut expected_events = events(&store);
    let Answer::Refused(why, after) = file(&mut store, &b, 7).unwrap() else {
        panic!("an unresolved identical send blocks the retry");
    };
    assert_eq!(why, IssueRefused::UnresolvedDraft { draft: a.draft });
    let expected = Action {
        version: StateVersion(4),
        approval: Some(ApprovalRev(5)),
        consumed: Some(ApprovalRev(3)),
        ..b.clone()
    };
    assert_eq!(after, expected);
    assert_eq!(snapshot(&store.conn).0[0], before.0[0]);
    expected_events.push(("action".into(), "Same café".into(), "{\"draft\":2}".into()));
    assert_eq!(events(&store), expected_events);
    let frozen = snapshot(&store.conn);
    let Answer::Unchanged(replayed) = file(&mut store, &b, 7).unwrap() else {
        panic!("the consumed failed-stage approval is a no-op");
    };
    assert_eq!(replayed, expected);
    assert_eq!(snapshot(&store.conn), frozen);
    assert_eq!(events(&store), expected_events);
}

#[test]
fn content_and_numeric_account_differences_are_not_general_semantic_deduplication() {
    for case in ["title", "body", "repo", "account", "login only"] {
        let scratch = Scratch::new("duplicate-content-contrast");
        let mut store = opened(&scratch);
        let a = drafted(&mut store, "draft-a", "first\r\n\tcafé\r\n");
        let a = unknown_once(&mut store, &a);
        let b = drafted(&mut store, "draft-b", "first\n\tcafé\n");
        let b = match case {
            "title" => changed(store.edit_draft(
                b.draft,
                &title("Same café!"),
                &body("first\n\tcafé\n"),
                Some(&repo()),
            )),
            "body" => changed(store.edit_draft(
                b.draft,
                &title("Same café"),
                &body("first\n\tcafé!\n"),
                Some(&repo()),
            )),
            "repo" => changed(store.edit_draft(
                b.draft,
                &title("Same café"),
                &body("first\n\tcafé\n"),
                Some(&RepoName::new("octo/other").unwrap()),
            )),
            "account" | "login only" => {
                let account = GitHubUser {
                    id: if case == "account" { 8 } else { 7 },
                    login: GitHubLogin::new(if case == "account" { "octo" } else { "renamed" })
                        .unwrap(),
                };
                let mut changes = store.bind_drafts(Some(&account), None).unwrap();
                assert_eq!(changes.len(), 1);
                changes.pop().unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(b.version, StateVersion(2));
        assert_eq!(b.approval, Some(ApprovalRev(4)));
        let before = snapshot(&store.conn);
        let account = if case == "account" { 8 } else { 7 };
        let expected = if case == "login only" {
            let Answer::Refused(why, after) = file(&mut store, &b, account).unwrap() else {
                panic!("changing the login does not change numeric account identity");
            };
            assert_eq!(why, IssueRefused::UnresolvedDraft { draft: a.draft });
            assert_eq!(after.stage, Stage::Draft);
            assert_eq!(after.approval, Some(ApprovalRev(5)));
            after
        } else {
            let after = changed(file(&mut store, &b, account));
            assert_eq!(after.stage, Stage::Preflight { unresolved: None }, "{case}");
            assert_eq!(after.approval, None);
            after
        };
        assert_eq!(expected.version, StateVersion(3));
        assert_eq!(expected.consumed, Some(ApprovalRev(4)));
        assert_eq!(expected.title, b.title);
        assert_eq!(expected.body, b.body);
        assert_eq!(expected.repo, b.repo);
        assert_eq!(expected.account, b.account);
        assert_eq!(snapshot(&store.conn).0[0], before.0[0]);
    }
}

#[test]
fn only_retained_unresolved_stages_block_a_different_drafts_file() {
    let posting = posting(A);
    let unknown = unknown(r#""Pending""#);
    for (stage, count, blocked, hidden) in [
        (r#"{"kind":"preflight","unresolved":null}"#, 0, true, false),
        (posting.as_str(), 0, true, false),
        (unknown.as_str(), 1, true, false),
        (r#"{"kind":"discarded"}"#, 1, true, false),
        (
            r#"{"kind":"filed","number":9,"confirmed":false}"#,
            1,
            false,
            false,
        ),
        (r#"{"kind":"failed","reason":"Network"}"#, 0, false, false),
        (r#"{"kind":"discarded"}"#, 0, false, false),
        (unknown.as_str(), 1, true, true),
    ] {
        let scratch = Scratch::new("duplicate-stage-boundary");
        let mut store = opened(&scratch);
        let a = drafted(&mut store, "draft-a", "first\r\n\tcafé\r\n");
        seed(&store, a.draft, stage, count);
        if hidden {
            store.begin_delete(a.thread).unwrap();
        }
        let b = drafted(&mut store, "draft-b", "first\n\tcafé\n");
        assert_eq!(b.approval, Some(ApprovalRev(2)));
        let before = snapshot(&store.conn);
        let mut expected_events = events(&store);
        let after = if blocked {
            let Answer::Refused(why, after) = file(&mut store, &b, 7).unwrap() else {
                panic!("{stage}: unresolved content blocks admission");
            };
            assert_eq!(why, IssueRefused::UnresolvedDraft { draft: a.draft });
            assert_eq!(after.stage, Stage::Draft);
            assert_eq!(after.approval, Some(ApprovalRev(3)));
            after
        } else {
            let after = changed(file(&mut store, &b, 7));
            assert_eq!(
                after.stage,
                Stage::Preflight { unresolved: None },
                "{stage}"
            );
            assert_eq!(after.approval, None);
            after
        };
        assert_eq!(after.version, StateVersion(2));
        assert_eq!(after.consumed, Some(ApprovalRev(2)));
        assert_eq!(after.title, b.title);
        assert_eq!(after.body, b.body);
        assert_eq!(after.repo, b.repo);
        assert_eq!(after.account, b.account);
        assert_eq!(
            snapshot(&store.conn).0[0],
            before.0[0],
            "{stage}: A is untouched"
        );
        expected_events.push(("action".into(), "Same café".into(), "{\"draft\":2}".into()));
        assert_eq!(events(&store), expected_events);
    }
}

#[test]
fn hiding_keeps_the_guard_but_completed_history_deletion_forgets_its_evidence() {
    let scratch = Scratch::new("duplicate-deletion");
    let mut store = opened(&scratch);
    let a = drafted(&mut store, "draft-a", "first\r\n\tcafé\r\n");
    let a = unknown_once(&mut store, &a);
    store.begin_delete(a.thread).unwrap();
    let b = drafted(&mut store, "draft-b", "first\n\tcafé\n");
    let Answer::Refused(why, b) = file(&mut store, &b, 7).unwrap() else {
        panic!("hiding does not remove a retained unresolved send");
    };
    assert_eq!(why, IssueRefused::UnresolvedDraft { draft: DraftId(1) });
    assert_eq!(b.stage, Stage::Draft);
    assert_eq!(b.version, StateVersion(2));
    assert_eq!(b.approval, Some(ApprovalRev(4)));
    assert_eq!(
        store.delete(a.thread).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    let retained: i64 = store
        .conn
        .query_row("SELECT count(*) FROM actions WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(retained, 0);
    let admitted = changed(file(&mut store, &b, 7));
    assert_eq!(admitted.draft, DraftId(2));
    assert_eq!(admitted.stage, Stage::Preflight { unresolved: None });
    assert_eq!(admitted.version, StateVersion(3));
    assert_eq!(admitted.approval, None);
    assert_eq!(admitted.consumed, Some(ApprovalRev(4)));
}
