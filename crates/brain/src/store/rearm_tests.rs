use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use kumochi_protocol::message::{
    ApprovalRev, ContentHash, Decision, Deleted, DraftId, FailReason, IssueRefused, StateVersion,
};
use rusqlite::{
    Connection,
    hooks::{Action as HookAction, Wal},
};
use tokio::time::Instant;

use super::actions_tests::{
    A, ID, draft, opened, posting, repo, reviewed, seed, snapshot, thread, unknown, user,
};
use super::{Store, StoreError};
use crate::issues::{Action, Answer, Snapshot, Stage, Write, WriteScope};
use crate::test_support::Scratch;

fn rearm(action: &Action) -> Write {
    Write::Rearm {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
        live: unlisted(),
    }
}

fn unlisted() -> Snapshot {
    Snapshot {
        capacity: false,
        account: None,
        repos: Vec::new(),
        repo_ids: Vec::new(),
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
        repo_ids: vec![ID],
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
        let created = reviewed(&mut store, owner);
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
                        live: unlisted(),
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
                    live: unlisted(),
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
                        live: unlisted(),
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
fn rearm_scopes_roll_back_before_commit_but_keep_a_late_committed_answer() {
    for boundary in ["cancelled", "expired", "event", "committed"] {
        let scratch = Scratch::new("issue-rearm-scope");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        let before = (snapshot(&store.conn), events(&store));
        let scope = WriteScope::new(
            Instant::now() + Duration::from_secs(if boundary == "expired" { 0 } else { 60 }),
        );
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&hits);
        let cancel = scope.clone();
        match boundary {
            "cancelled" => scope.cancel(),
            "expired" => {}
            "event" => store
                .conn
                .update_hook(Some(
                    move |kind: HookAction, database: &str, table: &str, _row: i64| {
                        if kind == HookAction::SQLITE_INSERT
                            && database == "main"
                            && table == "events"
                        {
                            observed.fetch_add(1, Ordering::SeqCst);
                            cancel.cancel();
                        }
                    },
                ))
                .unwrap(),
            "committed" => store
                .conn
                .wal_hook(Some(move |_wal: &mut Wal, _pages: i32| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    cancel.cancel();
                    Ok(())
                }))
                .unwrap(),
            _ => unreachable!(),
        }
        let result = store.issue_write(&rearm(&action), Some(&scope));
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
            hits.load(Ordering::SeqCst),
            usize::from(matches!(boundary, "event" | "committed"))
        );
        if boundary == "committed" {
            let expected = Action {
                version: StateVersion(2),
                approval: Some(ApprovalRev(2)),
                ..action
            };
            assert_eq!(changed(result), expected);
            assert_eq!(
                Store::open(&scratch.db())
                    .unwrap()
                    .action(expected.draft)
                    .unwrap(),
                Some(expected)
            );
            assert_eq!(snapshot(&store.conn).1, 2);
            assert_eq!(events(&store).len(), before.1.len() + 1);
        } else {
            assert!(matches!(result, Err(StoreError::Issue)));
            assert_eq!((snapshot(&store.conn), events(&store)), before);
            assert_eq!(snapshot(&Connection::open(scratch.db()).unwrap()), before.0);
        }
    }
}

#[test]
fn rearm_counters_reach_the_limit_once_and_never_wrap_or_burn_a_rolled_back_mint() {
    for allocator in [false, true] {
        let scratch = Scratch::new("issue-rearm-exhaustion");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        store
            .conn
            .execute_batch(if allocator {
                "UPDATE approval_mark SET last = 9223372036854775806;"
            } else {
                "UPDATE actions SET version = 9223372036854775806;"
            })
            .unwrap();
        let current = store.action(action.draft).unwrap().unwrap();
        let fresh = changed(store.issue_write(&rearm(&current), None));
        assert_eq!(
            fresh,
            Action {
                version: StateVersion(if allocator {
                    2
                } else {
                    9_223_372_036_854_775_807
                }),
                approval: Some(ApprovalRev(if allocator {
                    9_223_372_036_854_775_807
                } else {
                    2
                })),
                ..action
            }
        );
        let before = (snapshot(&store.conn), events(&store));
        assert!(matches!(
            store.issue_write(&rearm(&fresh), None),
            Err(StoreError::Issue)
        ));
        assert_eq!((snapshot(&store.conn), events(&store)), before);
        assert_eq!(
            snapshot(&store.conn).1,
            if allocator { i64::MAX } else { 2 }
        );
    }
}

#[test]
fn writer_consent_and_cancellation_check_version_before_consuming_a_current_approval() {
    for cancelled in [false, true] {
        let scratch = Scratch::new("issue-consent-version");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let created = reviewed(&mut store, owner);
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
        let blocker = reviewed(&mut store, owner);
        seed(&store, blocker.draft, &unknown(r#""Pending""#), 1);
        let blocker = store.action(blocker.draft).unwrap().unwrap();
        let other = reviewed(&mut store, owner);
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
    let write = Write::Rearm {
        draft: DraftId(23),
        at: StateVersion(17),
        hash: ContentHash([83; 32]),
        live: unlisted(),
    };
    assert_eq!(format!("{write:?}"), "IssueRearm");
    let scratch = Scratch::new("issue-rearm-debug");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let action = draft(&mut store, owner);
    assert_eq!(format!("{:?}", consent(&action, false)), "IssueConsent");
    assert_eq!(
        format!("{:?}", consent(&action, true)),
        "IssueCancelledConsent"
    );
}

/// OSC-648: a draft's first preview that finds its name listed binds that id and a later preview
/// keeps it; consent refuses an offer that bound none or a name now listed under another id, and
/// choosing another repository drops the id.
#[test]
fn a_preview_binds_the_listed_id_once_and_consent_only_verifies_it() {
    use kumochi_protocol::message::RepoName;
    use std::num::NonZeroU64;

    let scratch = Scratch::new("issue-rearm-repo-id");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let listing = |id| Snapshot {
        capacity: true,
        account: Some(user(7)),
        repos: vec![repo()],
        repo_ids: vec![NonZeroU64::new(id).unwrap()],
    };
    let review = |action: &Action, id| Write::Rearm {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
        live: listing(id),
    };
    let file = |action: &Action, id| Write::Consent {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
        live: listing(id),
    };
    let unbound = draft(&mut store, owner);
    let Answer::Changed(unbound) = store.issue_write(&rearm(&unbound), None).unwrap() else {
        panic!("an unlisted preview commits");
    };
    assert_eq!(unbound.repo_id, None);
    assert!(matches!(
        store.issue_write(&file(&unbound, 7), None).unwrap(),
        Answer::Refused(IssueRefused::RepoNotListed, refused) if refused.repo_id.is_none()
    ));
    let action = draft(&mut store, owner);
    let Answer::Changed(first) = store.issue_write(&review(&action, 7), None).unwrap() else {
        panic!("the first preview commits");
    };
    assert_eq!(first.repo_id, NonZeroU64::new(7));
    let Answer::Changed(again) = store.issue_write(&review(&first, 9), None).unwrap() else {
        panic!("a second preview commits");
    };
    assert_eq!(again.repo_id, NonZeroU64::new(7));
    assert!(matches!(
        store.issue_write(&file(&again, 9), None).unwrap(),
        Answer::Refused(IssueRefused::RepoNotListed, refused)
            if refused.repo_id == NonZeroU64::new(7)
    ));
    let other = RepoName::new("octo/other").unwrap();
    let Answer::Changed(edited) = store
        .edit_draft(again.draft, &again.title, &again.body, Some(&other))
        .unwrap()
    else {
        panic!("another repository is an edit");
    };
    assert_eq!(edited.repo_id, None);
}

/// OSC-648: a draft saved before V11 has no id bound, so File is refused, uses up its approval and
/// never reaches Preflight, until a fresh review binds the listed id; then it files as any other.
#[test]
fn a_draft_from_before_the_id_files_only_after_a_fresh_review() {
    let scratch = Scratch::new("issue-v10-draft");
    std::fs::create_dir_all(scratch.dir()).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    super::schema::migrate_to(&mut conn, 10).expect("V10");
    conn.execute_batch(
        "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1);
         INSERT INTO actions (thread, version, stage, repo, account_id, account_login, title,
         body, approval, uncertain, created_ms)
         VALUES (5, 1, '{\"kind\":\"draft\"}', 'octo/repo', '7', 'octo', 'T', 'B', 1, 0, 0);
         UPDATE approval_mark SET last = 1 WHERE id = 1;",
    )
    .expect("a draft saved before V11");
    drop(conn);
    let mut store = opened(&scratch);
    let saved = store.action(DraftId(1)).expect("reads").expect("kept");
    assert_eq!(
        (saved.repo_id, saved.approval),
        (None, Some(ApprovalRev(1)))
    );
    let listing = Snapshot {
        capacity: true,
        account: Some(user(7)),
        repos: vec![repo()],
        repo_ids: vec![ID],
    };
    let file = |action: &Action| Write::Consent {
        draft: action.draft,
        at: action.version,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
        live: listing.clone(),
    };
    let idless = Write::Consent {
        draft: saved.draft,
        at: saved.version,
        hash: saved.hash(),
        approval: ApprovalRev(1),
        decision: Decision::File,
        live: Snapshot {
            repo_ids: Vec::new(),
            ..listing.clone()
        },
    };
    let Answer::Refused(IssueRefused::RepoNotListed, refused) =
        store.issue_write(&idless, None).unwrap()
    else {
        panic!("no id bound and none listed still isn't the reviewed repository");
    };
    assert_eq!(refused.consumed, Some(ApprovalRev(1)));
    let Answer::Refused(IssueRefused::RepoNotListed, refused) =
        store.issue_write(&file(&refused), None).unwrap()
    else {
        panic!("an unreviewed draft from before V11 can't file");
    };
    assert_eq!(
        (&refused.stage, refused.consumed, refused.repo_id),
        (&Stage::Draft, Some(ApprovalRev(2)), None)
    );
    let reviewed = changed(store.issue_write(
        &Write::Rearm {
            draft: refused.draft,
            at: refused.version,
            hash: refused.hash(),
            live: listing.clone(),
        },
        None,
    ));
    assert_eq!(reviewed.repo_id, Some(ID));
    assert_eq!(
        changed(store.issue_write(&file(&reviewed), None)).stage,
        Stage::Preflight { unresolved: None }
    );
}
