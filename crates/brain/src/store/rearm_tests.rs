use kumochi_protocol::message::{
    ApprovalRev, ContentHash, Decision, Deleted, DraftId, FailReason, IssueRefused, StateVersion,
};
use rusqlite::Connection;

use super::actions_tests::{
    A, draft, opened, posting, repo, seed, snapshot, thread, unknown, user,
};
use super::{Store, StoreError};
use crate::issues::{Action, Answer, Snapshot, Stage, Write};
use crate::test_support::Scratch;

pub(super) fn rearm(action: &Action) -> Write {
    Write::Rearm {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "one test matrix constructs both ordinary and cancelled writer operations"
)]
fn consent(action: &Action, cancelled: bool) -> Write {
    let live = Snapshot {
        capacity: true,
        account: Some(user(7)),
        repos: vec![repo()],
    };
    if cancelled {
        Write::CancelledConsent {
            draft: action.draft,
            at: action.version,
            hash: action.hash(),
            approval: action.approval.unwrap(),
            live,
        }
    } else {
        Write::Consent {
            draft: action.draft,
            at: action.version,
            hash: action.hash(),
            approval: action.approval.unwrap(),
            decision: Decision::File,
            live,
        }
    }
}

fn changed(answer: Result<Answer, StoreError>) -> Action {
    match answer {
        Ok(Answer::Changed(action)) => action,
        other => panic!("expected a committed change: {other:?}"),
    }
}

fn events(store: &Store) -> Vec<String> {
    store.conn.prepare(
        "SELECT json_array(id, thread, run, kind, text, body, created_ms) FROM events ORDER BY id",
    ).unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<Result<_, _>>().unwrap()
}

#[test]
fn rearm_mints_without_changing_draft_or_failed_content_and_survives_restart() {
    for failed in [false, true] {
        let scratch = Scratch::new("issue-rearm-restart");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let created = draft(&mut store, owner);
        let action = if failed {
            let action = changed(store.issue_write(&consent(&created, true), None));
            assert_eq!(
                action,
                Action {
                    version: StateVersion(3),
                    stage: Stage::Failed {
                        reason: FailReason::Interrupted
                    },
                    approval: Some(ApprovalRev(2)),
                    consumed: Some(ApprovalRev(1)),
                    ..created
                }
            );
            action
        } else {
            created
        };
        let before_events = events(&store).len();
        let fresh = changed(store.issue_write(&rearm(&action), None));
        assert_eq!(
            fresh,
            Action {
                version: StateVersion(if failed { 4 } else { 2 }),
                approval: Some(ApprovalRev(if failed { 3 } else { 2 })),
                ..action.clone()
            }
        );
        drop(store);
        let mut store = opened(&scratch);
        assert_eq!(store.action(action.draft).unwrap(), Some(fresh.clone()));
        let newer = changed(store.issue_write(&rearm(&fresh), None));
        assert_eq!(
            newer,
            Action {
                version: StateVersion(if failed { 5 } else { 3 }),
                approval: Some(ApprovalRev(if failed { 4 } else { 3 })),
                ..action
            }
        );
        assert_eq!(snapshot(&store.conn).1, if failed { 4 } else { 3 });
        assert_eq!(events(&store).len(), before_events + 2);
    }
}

#[test]
fn stale_rearm_versions_hashes_and_gone_rows_do_not_mint_or_change_any_row() {
    let scratch = Scratch::new("issue-rearm-binding");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let action = draft(&mut store, owner);
    let before = (snapshot(&store.conn), events(&store));
    for at in [StateVersion(0), StateVersion(2)] {
        assert_eq!(
            store
                .issue_write(
                    &Write::Rearm {
                        draft: action.draft,
                        at,
                        hash: action.hash(),
                    },
                    None
                )
                .unwrap(),
            Answer::Refused(IssueRefused::StaleApproval, action.clone())
        );
        assert_eq!((snapshot(&store.conn), events(&store)), before);
    }
    assert_eq!(
        store
            .issue_write(
                &Write::Rearm {
                    draft: action.draft,
                    at: StateVersion(1),
                    hash: ContentHash([0; 32]),
                },
                None
            )
            .unwrap(),
        Answer::Refused(IssueRefused::ContentChanged, action.clone())
    );
    assert_eq!((snapshot(&store.conn), events(&store)), before);
    for draft in [DraftId(0), DraftId(2), DraftId(u64::MAX)] {
        assert_eq!(
            store
                .issue_write(
                    &Write::Rearm {
                        draft,
                        at: StateVersion(1),
                        hash: action.hash(),
                    },
                    None
                )
                .unwrap(),
            Answer::Gone
        );
        assert_eq!((snapshot(&store.conn), events(&store)), before);
    }
    assert_eq!(store.delete(owner).outcome, Deleted::Removed);
    let before = (snapshot(&store.conn), events(&store));
    assert_eq!(
        store.issue_write(&rearm(&action), None).unwrap(),
        Answer::Gone
    );
    assert_eq!((snapshot(&store.conn), events(&store)), before);
}

#[test]
fn rearm_cannot_supersede_sending_uncertain_settled_or_discarded_actions() {
    for (stage, uncertain) in [
        (r#"{"kind":"preflight","unresolved":null}"#.to_owned(), 0),
        (posting(A), 0),
        (unknown(r#""Pending""#), 1),
        (
            r#"{"kind":"filed","number":17,"confirmed":false}"#.to_owned(),
            0,
        ),
        (r#"{"kind":"discarded"}"#.to_owned(), 0),
        (r#"{"kind":"discarded"}"#.to_owned(), 1),
    ] {
        let scratch = Scratch::new("issue-rearm-stage");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        seed(&store, action.draft, &stage, uncertain);
        let current = store.action(action.draft).unwrap().unwrap();
        let before = (snapshot(&store.conn), events(&store));
        assert_eq!(
            store.issue_write(&rearm(&current), None).unwrap(),
            Answer::Refused(IssueRefused::NotAllowedNow, current)
        );
        assert_eq!((snapshot(&store.conn), events(&store)), before);
    }
}

#[test]
fn rearm_event_failure_rolls_back_the_mint_version_and_content_atomically() {
    let scratch = Scratch::new("issue-rearm-rollback");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let action = draft(&mut store, owner);
    let before = (snapshot(&store.conn), events(&store));
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER reject_rearm_event BEFORE INSERT ON events
         WHEN new.kind = 'action' BEGIN SELECT RAISE(ABORT, 'rearm event trap'); END;",
        )
        .unwrap();
    assert!(matches!(
        store.issue_write(&rearm(&action), None),
        Err(StoreError::Issue)
    ));
    assert_eq!((snapshot(&store.conn), events(&store)), before);
    assert_eq!(snapshot(&Connection::open(scratch.db()).unwrap()), before.0);
    store
        .conn
        .execute_batch("DROP TRIGGER temp.reject_rearm_event")
        .unwrap();
    let fresh = changed(store.issue_write(&rearm(&action), None));
    assert_eq!(
        fresh,
        Action {
            version: StateVersion(2),
            approval: Some(ApprovalRev(2)),
            ..action
        }
    );
    assert_eq!(snapshot(&store.conn).1, 2);
    assert_eq!(events(&store).len(), before.1.len() + 1);
}

#[test]
fn writer_consent_and_cancellation_check_version_before_consuming_a_current_approval() {
    for cancelled in [false, true] {
        let scratch = Scratch::new("issue-consent-version");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let created = draft(&mut store, owner);
        let fresh = changed(store.issue_write(&rearm(&created), None));
        let before = (snapshot(&store.conn), events(&store));
        let mut stale = consent(&fresh, cancelled);
        match &mut stale {
            Write::Consent { at, .. } | Write::CancelledConsent { at, .. } => *at = StateVersion(1),
            _ => unreachable!(),
        }
        assert_eq!(
            store.issue_write(&stale, None).unwrap(),
            Answer::Refused(IssueRefused::StaleApproval, fresh.clone())
        );
        assert_eq!((snapshot(&store.conn), events(&store)), before);
        assert_eq!(
            store.issue_write(&consent(&created, true), None).unwrap(),
            Answer::Refused(IssueRefused::StaleApproval, fresh.clone())
        );
        assert_eq!((snapshot(&store.conn), events(&store)), before);
        let admitted = changed(store.issue_write(&consent(&fresh, cancelled), None));
        assert_eq!(
            admitted,
            Action {
                version: StateVersion(if cancelled { 4 } else { 3 }),
                stage: if cancelled {
                    Stage::Failed {
                        reason: FailReason::Interrupted,
                    }
                } else {
                    Stage::Preflight { unresolved: None }
                },
                approval: cancelled.then_some(ApprovalRev(3)),
                consumed: Some(ApprovalRev(2)),
                ..created
            }
        );
    }
}

#[test]
fn version_checked_writes_preserve_the_identical_unresolved_guard() {
    for cancelled in [false, true] {
        let scratch = Scratch::new("issue-rearm-unresolved");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let blocker = draft(&mut store, owner);
        seed(&store, blocker.draft, &unknown(r#""Pending""#), 1);
        let blocker = store.action(blocker.draft).unwrap().unwrap();
        let other = draft(&mut store, owner);
        let fresh = changed(store.issue_write(&rearm(&other), None));
        let refused = store
            .issue_write(&consent(&fresh, cancelled), None)
            .unwrap();
        assert_eq!(
            refused,
            Answer::Refused(
                IssueRefused::UnresolvedDraft {
                    draft: blocker.draft
                },
                Action {
                    version: StateVersion(3),
                    approval: Some(ApprovalRev(4)),
                    consumed: Some(ApprovalRev(3)),
                    ..other
                },
            )
        );
        assert_eq!(store.action(blocker.draft).unwrap(), Some(blocker));
        assert_eq!(snapshot(&store.conn).1, 4);
    }
}

#[test]
fn rearm_and_versioned_writes_have_content_free_debug() {
    let marker = b"private-issue-approval-hash-text";
    let hash = ContentHash(*marker);
    let write = Write::Rearm {
        draft: DraftId(23),
        at: StateVersion(17),
        hash,
    };
    let make_consent = |cancelled| {
        let live = Snapshot {
            capacity: true,
            account: Some(user(7)),
            repos: vec![repo()],
        };
        if cancelled {
            Write::CancelledConsent {
                draft: DraftId(23),
                at: StateVersion(17),
                hash,
                approval: ApprovalRev(29),
                live,
            }
        } else {
            Write::Consent {
                draft: DraftId(23),
                at: StateVersion(17),
                hash,
                approval: ApprovalRev(29),
                decision: Decision::File,
                live,
            }
        }
    };
    let text = std::str::from_utf8(marker).unwrap();
    let bytes = format!("{marker:?}");
    for operation in [write, make_consent(false), make_consent(true)] {
        let debug = format!("{operation:?}");
        assert!(!debug.contains(text));
        assert!(!debug.contains(&bytes));
        assert!(!debug.contains("octo"));
    }
}
