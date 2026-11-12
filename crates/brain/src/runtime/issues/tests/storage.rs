use super::*;
use crate::coordinator::{Effect, Input};

async fn settle(brain: &mut Brain) -> Vec<ControlToShell> {
    let mut controls = Vec::new();
    timeout(PATIENCE, async {
        while brain.filing.pending() {
            let effects = tokio::select! {
                Some(input) = brain.inputs.recv() => brain.handle(input).unwrap_or_default(),
                event = brain.filing.next() => brain.issue_event(event),
            };
            controls.extend(effects.iter().filter_map(|effect| match effect {
                Effect::Control(control) => Some(control.clone()),
                _ => None,
            }));
            brain.apply(effects, None).unwrap();
        }
    })
    .await
    .unwrap();
    controls
}

#[tokio::test]
async fn a_failed_discard_store_write_must_settle_its_correlated_attempt() {
    let scratch = Scratch::new("github-filing-discard-storage-failure");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_discard BEFORE UPDATE OF stage ON actions
         WHEN json_extract(NEW.stage, '$.kind') = 'discarded'
         BEGIN SELECT RAISE(ABORT, 'test discard save refused'); END;",
    )
    .unwrap();
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            901,
            &IssueOp::Discard {
                draft: action.draft,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(
        settle(&mut brain).await,
        vec![ControlToShell::IssueUnavailable {
            request: 901,
            draft: action.draft
        }]
    );
    assert!(!brain.filing.pending());
    drop(conn);
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(action)
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn every_non_job_operation_and_an_admitted_recovery_read_settle_a_store_error() {
    for operation in 0..6 {
        let scratch = Scratch::new("github-filing-shared-storage-failure");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch("ALTER TABLE actions RENAME TO unavailable_actions;")
            .unwrap();
        let op = match operation {
            0 => IssueOp::Discard {
                draft: action.draft,
            },
            1..=3 => IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: ApprovalRev(1),
                decision: match operation {
                    1 => Decision::File,
                    2 => Decision::FileAnyway,
                    _ => Decision::ThisIsTheIssue,
                },
            },
            _ => IssueOp::CheckAgain {
                draft: action.draft,
            },
        };
        let epoch = (operation == 5).then_some(brain.epoch);
        let effects = brain
            .issue_op(brain.issue_session().unwrap(), 902, &op, epoch)
            .unwrap();
        brain.apply(effects, None).unwrap();
        assert_eq!(
            settle(&mut brain).await,
            vec![ControlToShell::IssueUnavailable {
                request: 902,
                draft: action.draft
            }]
        );
        assert!(!brain.filing.pending());
        conn.execute_batch("ALTER TABLE unavailable_actions RENAME TO actions;")
            .unwrap();
        drop(conn);
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(action)
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn a_missing_action_finishes_each_operation_without_claiming_authoritative_state() {
    for operation in 0..3 {
        let scratch = Scratch::new("github-filing-missing-action-terminal");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let missing = kumochi_protocol::message::DraftId(u64::MAX);
        let op = match operation {
            0 => IssueOp::Discard { draft: missing },
            1 => IssueOp::CheckAgain { draft: missing },
            _ => IssueOp::Consent {
                draft: missing,
                hash: action.hash(),
                approval: ApprovalRev(1),
                decision: Decision::File,
            },
        };
        let effects = brain
            .issue_op(brain.issue_session().unwrap(), 903, &op, Some(brain.epoch))
            .unwrap();
        brain.apply(effects, None).unwrap();
        assert_eq!(
            settle(&mut brain).await,
            vec![ControlToShell::IssueUnavailable {
                request: 903,
                draft: missing
            }]
        );
        assert!(!brain.filing.pending());
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(action)
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one trigger scenario distinguishes successful compensation from storage remaining unavailable"
)]
async fn a_failed_approval_write_is_spent_only_if_its_compensation_commits() {
    for keep_failing in [false, true] {
        let scratch = Scratch::new("github-filing-approval-storage-failure");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER refuse_approval BEFORE UPDATE ON actions
             BEGIN SELECT RAISE(ABORT, 'test approval save refused'); END;",
        )
        .unwrap();
        let consent = IssueOp::Consent {
            draft: action.draft,
            hash: action.hash(),
            approval: ApprovalRev(1),
            decision: Decision::File,
        };
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                904,
                &consent,
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
        let failed = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(failed, Input::IssueWritten { reply: Err(()), .. }));
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(action.clone())
        );
        if !keep_failing {
            conn.execute_batch("DROP TRIGGER refuse_approval;").unwrap();
        }
        let effects = brain.handle(failed).unwrap();
        assert!(matches!(
            &effects[..],
            [Effect::Store(StoreOp::Issue {
                write: Write::CancelledConsent { .. },
                scope: None,
                ..
            })]
        ));
        brain.apply(effects, None).unwrap();
        let terminal = settle(&mut brain).await;
        let saved = if keep_failing {
            assert_eq!(
                terminal,
                vec![ControlToShell::IssueUnavailable {
                    request: 904,
                    draft: action.draft
                }]
            );
            action.clone()
        } else {
            assert_eq!(
                terminal,
                vec![ControlToShell::ActionState {
                    request: Some(904),
                    draft: action.draft,
                    version: StateVersion(3),
                    state: ActionState::Failed(kumochi_protocol::message::FailReason::Interrupted),
                    approval: Some(ApprovalRev(2)),
                }]
            );
            Action {
                version: StateVersion(3),
                stage: Stage::Failed {
                    reason: kumochi_protocol::message::FailReason::Interrupted,
                },
                approval: Some(ApprovalRev(2)),
                consumed: Some(ApprovalRev(1)),
                ..action.clone()
            }
        };
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(saved.clone())
        );
        assert!(!brain.filing.pending());
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
        if keep_failing {
            conn.execute_batch("DROP TRIGGER refuse_approval;").unwrap();
            let effects = brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    905,
                    &consent,
                    Some(brain.epoch),
                )
                .unwrap();
            brain.apply(effects, None).unwrap();
            assert_eq!(
                settle(&mut brain).await,
                vec![
                    ControlToShell::ActionState {
                        request: None,
                        draft: action.draft,
                        version: StateVersion(2),
                        state: ActionState::Sending,
                        approval: None
                    },
                    ControlToShell::ActionState {
                        request: None,
                        draft: action.draft,
                        version: StateVersion(3),
                        state: ActionState::Sending,
                        approval: None
                    },
                    ControlToShell::ActionState {
                        request: Some(905),
                        draft: action.draft,
                        version: StateVersion(4),
                        state: ActionState::Filed {
                            number: 17,
                            url: IssueUrl::new("https://github.com/owner/repo/issues/17").unwrap(),
                            confirmed_by_user: false,
                            maybe_duplicate: false
                        },
                        approval: None
                    },
                ]
            );
            assert_eq!(server.factories.load(Ordering::SeqCst), 1);
            assert_eq!(server.gets.load(Ordering::SeqCst), 4);
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
            assert_eq!(server.receipt.load(Ordering::Acquire), 17);
        } else {
            let effects = brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    905,
                    &consent,
                    Some(brain.epoch),
                )
                .unwrap();
            brain.apply(effects, None).unwrap();
            assert_eq!(
                settle(&mut brain).await,
                vec![ControlToShell::ActionState {
                    request: Some(905),
                    draft: action.draft,
                    version: StateVersion(3),
                    state: ActionState::Failed(kumochi_protocol::message::FailReason::Interrupted),
                    approval: Some(ApprovalRev(2)),
                }]
            );
            assert_eq!(server.factories.load(Ordering::SeqCst), 0);
            assert_eq!(server.gets.load(Ordering::SeqCst), 0);
            assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        }
        drop(conn);
        brain.close().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one actual POST and two failed SQL settlements must preserve uncertainty through duplicate consent and recovery"
)]
async fn a_storage_terminal_after_posting_cannot_unlock_a_second_post() {
    let scratch = Scratch::new("github-filing-posting-storage-terminal");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let (posting, _) = super::authority::posting_acknowledgement(&mut brain, &action).await;
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_post_result BEFORE UPDATE OF stage ON actions
         WHEN json_extract(NEW.stage, '$.kind') IN ('filed', 'unknown')
         BEGIN SELECT RAISE(ABORT, 'test result and cleanup refused'); END;",
    )
    .unwrap();
    let effects = brain.handle(posting).unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(
        settle(&mut brain).await,
        vec![ControlToShell::IssueUnavailable {
            request: 601,
            draft: action.draft
        }]
    );
    let posted = Action {
        version: StateVersion(3),
        stage: Stage::Posting {
            mark: Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::default(),
            },
            unresolved: None,
        },
        approval: None,
        consumed: Some(ApprovalRev(1)),
        ..action.clone()
    };
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(posted.clone())
    );
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            906,
            &IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: ApprovalRev(1),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(
        settle(&mut brain).await,
        vec![ControlToShell::ActionState {
            request: Some(906),
            draft: action.draft,
            version: StateVersion(3),
            state: ActionState::Sending,
            approval: None
        }]
    );
    let second = {
        let mut store = Store::open(&scratch.db()).unwrap();
        store
            .create_draft(
                action.thread,
                &IssueTitle::new("  Local café  ").unwrap(),
                &IssueBody::new("first\n\tcafé\n").unwrap(),
                Some(&RepoName::new("owner/repo").unwrap()),
                Some(&GitHubUser {
                    id: 7,
                    login: GitHubLogin::new("owner-sentinel").unwrap(),
                }),
            )
            .unwrap()
    };
    assert_eq!(second.approval, Some(ApprovalRev(2)));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            907,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(2),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(
        settle(&mut brain).await,
        vec![
            ControlToShell::ActionState {
                request: Some(907),
                draft: second.draft,
                version: StateVersion(2),
                state: ActionState::Draft,
                approval: Some(ApprovalRev(3))
            },
            ControlToShell::IssueRefused {
                request: 907,
                draft: second.draft,
                version: StateVersion(2),
                approval: Some(ApprovalRev(3)),
                reason: kumochi_protocol::message::IssueRefused::UnresolvedDraft {
                    draft: action.draft
                }
            },
        ]
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
    conn.execute_batch("DROP TRIGGER refuse_post_result;")
        .unwrap();
    drop(conn);
    brain.close().await;
    let mut reopened = Store::open(&scratch.db()).unwrap();
    reopened.recover().unwrap();
    assert_eq!(
        reopened.action(action.draft).unwrap(),
        Some(Action {
            version: StateVersion(4),
            stage: Stage::Unknown {
                mark: Mark {
                    at_s: NonZeroU64::new(120).unwrap(),
                    presend: Bounded::default()
                },
                lookup: LookupResult::Pending,
            },
            approval: Some(ApprovalRev(4)),
            consumed: Some(ApprovalRev(1)),
            uncertain: 1,
            ..action
        })
    );
}
