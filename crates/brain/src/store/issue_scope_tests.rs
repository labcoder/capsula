use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use kumochi_protocol::message::{ApprovalRev, Decision, IssueUrl, LookupResult, StateVersion};
use rusqlite::{
    Connection,
    hooks::{Action as HookAction, Wal},
};
use tokio::time::Instant;

use super::actions_tests::{
    A, body, draft, issue, mark, opened, repo, snapshot, thread, title, user,
};
use super::{Store, StoreError};
use crate::connections::github::issues::CreateOutcome;
use crate::issues::{Action, Answer, Snapshot, Stage, Write, WriteScope};
use crate::test_support::Scratch;

fn consent(action: &Action) -> Write {
    Write::Consent {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
        live: Snapshot {
            capacity: true,
            account: Some(user(7)),
            repos: vec![repo()],
        },
    }
}

fn events(store: &Store) -> Vec<String> {
    store.conn.prepare(
        "SELECT json_array(id, thread, run, kind, text, body, created_ms) FROM events ORDER BY id",
    ).unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<Result<_, _>>().unwrap()
}

fn changed(answer: Result<Answer, StoreError>) -> Action {
    match answer {
        Ok(Answer::Changed(action)) => action,
        other => panic!("expected a committed change: {other:?}"),
    }
}

fn restored_connection(store: &Store) {
    let wait: i64 = store
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(wait, 1234);
    let sum: i64 = store
        .conn
        .query_row(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 10000)
         SELECT sum(x) FROM n",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sum, 50_005_000);
    assert!(store.conn.is_autocommit());
}

#[test]
fn cancelled_or_expired_issue_scopes_leave_actions_revisions_and_events_untouched() {
    for expired in [false, true] {
        let scratch = Scratch::new("issue-scope-admission");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        store
            .conn
            .busy_timeout(Duration::from_millis(1234))
            .unwrap();
        let scope =
            WriteScope::new(Instant::now() + Duration::from_secs(if expired { 0 } else { 60 }));
        if !expired {
            scope.clone().cancel();
        }
        assert!(scope.stopped());
        let before = snapshot(&store.conn);
        let before_events = events(&store);
        assert!(matches!(
            store.issue_write(&consent(&action), Some(&scope)),
            Err(StoreError::Issue)
        ));
        assert_eq!(snapshot(&store.conn), before);
        assert_eq!(events(&store), before_events);
        assert_eq!(store.action(action.draft).unwrap(), Some(action));
        restored_connection(&store);
    }
}

enum Cut {
    Cancelled,
    Expired,
    Event,
    Committed,
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the same real transaction fixture covers all cancellation boundaries"
)]
fn cancellation_after_the_action_event_insert_rolls_back_the_whole_issue_transaction() {
    for (rearming, cut) in [
        (false, Cut::Event),
        (true, Cut::Cancelled),
        (true, Cut::Expired),
        (true, Cut::Event),
        (true, Cut::Committed),
    ] {
        let scratch = Scratch::new("issue-scope-rollback");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        store
            .conn
            .busy_timeout(Duration::from_millis(1234))
            .unwrap();
        let before = snapshot(&store.conn);
        let before_events = events(&store);
        let limit = if matches!(cut, Cut::Expired) {
            Duration::ZERO
        } else {
            Duration::from_secs(60)
        };
        let scope = WriteScope::new(Instant::now() + limit);
        let cancel = scope.clone();
        let reached = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&reached);
        match cut {
            Cut::Cancelled => scope.cancel(),
            Cut::Expired => (),
            Cut::Event => store
                .conn
                .update_hook(Some(
                    move |kind: HookAction, database: &str, table: &str, _row: i64| {
                        if kind == HookAction::SQLITE_INSERT
                            && database == "main"
                            && table == "events"
                        {
                            mark.store(true, Ordering::SeqCst);
                            cancel.cancel();
                        }
                    },
                ))
                .unwrap(),
            Cut::Committed => store
                .conn
                .wal_hook(Some(move |_wal: &mut Wal, _pages: i32| {
                    mark.store(true, Ordering::SeqCst);
                    cancel.cancel();
                    Ok(())
                }))
                .unwrap(),
        }
        let write = if rearming {
            super::rearm_tests::rearm(&action)
        } else {
            consent(&action)
        };
        let result = store.issue_write(&write, Some(&scope));
        store
            .conn
            .update_hook(None::<fn(HookAction, &str, &str, i64)>)
            .unwrap();
        store
            .conn
            .wal_hook(None::<fn(&mut Wal, i32) -> rusqlite::Result<()>>)
            .unwrap();
        assert!(scope.stopped());
        assert_eq!(
            reached.load(Ordering::SeqCst),
            matches!(cut, Cut::Event | Cut::Committed)
        );
        if matches!(cut, Cut::Committed) {
            let expected = Action {
                version: StateVersion(2),
                approval: Some(ApprovalRev(2)),
                ..action
            };
            assert_eq!(changed(result), expected);
            let reopened = Store::open(&scratch.db()).unwrap();
            assert_eq!(reopened.action(expected.draft).unwrap(), Some(expected));
            assert_eq!(snapshot(&store.conn).1, 2);
            assert_eq!(events(&store).len(), before_events.len() + 1);
        } else {
            assert!(matches!(result, Err(StoreError::Issue)));
            assert_eq!(snapshot(&store.conn), before);
            assert_eq!(events(&store), before_events);
            let reader = Connection::open(scratch.db()).unwrap();
            assert_eq!(snapshot(&reader), before);
            restored_connection(&store);
            let after =
                changed(store.edit_draft(action.draft, &title("After"), &body("B"), Some(&repo())));
            assert_eq!(
                after,
                Action {
                    title: title("After"),
                    version: StateVersion(2),
                    approval: Some(ApprovalRev(2)),
                    ..action
                }
            );
        }
        restored_connection(&store);
    }
}

#[test]
fn cancellation_from_the_wal_commit_hook_cannot_erase_a_committed_answer() {
    let scratch = Scratch::new("issue-scope-committed");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let action = draft(&mut store, owner);
    store
        .conn
        .busy_timeout(Duration::from_millis(1234))
        .unwrap();
    let journal: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    let before_events = events(&store);
    let scope = WriteScope::new(Instant::now() + Duration::from_secs(60));
    let cancel = scope.clone();
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&commits);
    store
        .conn
        .wal_hook(Some(move |_wal: &mut Wal, _pages: i32| {
            observed.fetch_add(1, Ordering::SeqCst);
            cancel.cancel();
            Ok(())
        }))
        .unwrap();
    let result = store.issue_write(&consent(&action), Some(&scope));
    store
        .conn
        .wal_hook(None::<fn(&mut Wal, i32) -> rusqlite::Result<()>>)
        .unwrap();
    let after = changed(result);
    let expected = Action {
        stage: Stage::Preflight { unresolved: None },
        version: StateVersion(2),
        approval: None,
        consumed: Some(ApprovalRev(1)),
        ..action
    };
    assert_eq!(after, expected);
    assert!(scope.stopped());
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.action(expected.draft).unwrap(),
        Some(expected.clone())
    );
    let reopened = Store::open(&scratch.db()).unwrap();
    assert_eq!(reopened.action(expected.draft).unwrap(), Some(expected));
    assert_eq!(events(&store).len(), before_events.len() + 1);
    assert_eq!(snapshot(&store.conn).1, 1);
    restored_connection(&store);
}

#[test]
fn unscoped_result_and_compensation_writes_settle_posting_after_logical_cancellation() {
    for receipt in [false, true] {
        let scratch = Scratch::new("issue-scope-settlement");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        store
            .conn
            .busy_timeout(Duration::from_millis(1234))
            .unwrap();
        let scope = WriteScope::new(Instant::now() + Duration::from_secs(60));
        let preflight = changed(store.issue_write(&consent(&action), Some(&scope)));
        let posting = changed(store.issue_write(
            &Write::Preflighted {
                draft: action.draft,
                at: preflight.version,
                result: Ok(mark(A)),
            },
            Some(&scope),
        ));
        assert_eq!(posting.version, StateVersion(3));
        assert_eq!(
            posting.stage,
            Stage::Posting {
                mark: mark(A),
                unresolved: None
            }
        );
        scope.clone().cancel();
        assert!(scope.stopped());
        let write = if receipt {
            Write::Posted {
                draft: action.draft,
                at: posting.version,
                outcome: CreateOutcome::Created {
                    number: 17,
                    url: IssueUrl::new("https://github.com/octo/repo/issues/17").unwrap(),
                },
            }
        } else {
            Write::Interrupted {
                draft: action.draft,
                at: posting.version,
            }
        };
        let before = snapshot(&store.conn);
        let before_events = events(&store);
        assert!(matches!(
            store.issue_write(&write, Some(&scope)),
            Err(StoreError::Issue)
        ));
        assert_eq!(snapshot(&store.conn), before);
        assert_eq!(events(&store), before_events);
        let after = changed(store.issue_write(&write, None));
        let expected = if receipt {
            Action {
                version: StateVersion(4),
                stage: Stage::Filed {
                    number: issue(17),
                    confirmed: false,
                    maybe_duplicate: false,
                },
                ..posting
            }
        } else {
            Action {
                version: StateVersion(4),
                stage: Stage::Unknown {
                    mark: mark(A),
                    lookup: LookupResult::Pending,
                },
                approval: Some(ApprovalRev(2)),
                uncertain: 1,
                ..posting
            }
        };
        assert_eq!(after, expected);
        assert_eq!(store.action(action.draft).unwrap(), Some(expected));
        assert_eq!(events(&store).len(), before_events.len() + 1);
        assert!(scope.stopped());
        restored_connection(&store);
    }
}
