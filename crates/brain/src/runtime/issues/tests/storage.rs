use super::*;
use crate::coordinator::{Effect, Input};

pub(super) async fn settle(brain: &mut Brain) -> Vec<ControlToShell> {
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
            kind: IssueUnavailableKind::Action,
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
        let (mut brain, action) = if (1..=3).contains(&operation) {
            reviewed_filing_owner(&scratch, &server).await
        } else {
            filing_owner(&scratch, &server).await
        };
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
                approval: action.approval.unwrap(),
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
        let mut replies: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Control(frame) => Some(frame.clone()),
                _ => None,
            })
            .collect();
        brain.apply(effects, None).unwrap();
        replies.extend(settle(&mut brain).await);
        assert_eq!(
            replies,
            vec![ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
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
        let mut replies: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Control(frame) => Some(frame.clone()),
                _ => None,
            })
            .collect();
        brain.apply(effects, None).unwrap();
        replies.extend(settle(&mut brain).await);
        assert_eq!(
            replies,
            vec![ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
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
        let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER refuse_approval BEFORE UPDATE ON actions
             BEGIN SELECT RAISE(ABORT, 'test approval save refused'); END;",
        )
        .unwrap();
        let consent = IssueOp::Consent {
            draft: action.draft,
            hash: action.hash(),
            approval: ApprovalRev(2),
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
        assert_eq!(brain.filing.review_counts(), (0, 0));
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
                write: Write::CancelledConsent {
                    at: StateVersion(2),
                    approval: ApprovalRev(2),
                    ..
                },
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
                    kind: IssueUnavailableKind::Action,
                    request: 904,
                    draft: action.draft,
                }]
            );
            action.clone()
        } else {
            assert_eq!(
                terminal,
                vec![ControlToShell::ActionState {
                    request: Some(904),
                    draft: action.draft,
                    version: StateVersion(4),
                    state: ActionState::Failed(kumochi_protocol::message::FailReason::Interrupted),
                    approval: Some(ApprovalRev(3)),
                }]
            );
            Action {
                version: StateVersion(4),
                stage: Stage::Failed {
                    reason: kumochi_protocol::message::FailReason::Interrupted,
                },
                approval: Some(ApprovalRev(3)),
                consumed: Some(ApprovalRev(2)),
                ..action.clone()
            }
        };
        assert!(!brain.filing.pending());
        if keep_failing {
            conn.execute_batch("DROP TRIGGER refuse_approval;").unwrap();
        }
        for request in [904, 905] {
            let effects = brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    request,
                    &consent,
                    Some(brain.epoch),
                )
                .unwrap();
            assert_eq!(
                effects,
                vec![Effect::Control(ControlToShell::IssueUnavailable {
                    kind: IssueUnavailableKind::Action,
                    request,
                    draft: action.draft,
                })]
            );
            assert_eq!(brain.filing.deadline_for_test(), None);
            assert_eq!(brain.filing.review_counts(), (0, 0));
            assert_eq!(
                Store::open(&scratch.db())
                    .unwrap()
                    .action(action.draft)
                    .unwrap(),
                Some(saved.clone())
            );
            assert_eq!(server.factories.load(Ordering::SeqCst), 0);
            assert_eq!(server.gets.load(Ordering::SeqCst), 0);
            assert_eq!(server.posts.load(Ordering::SeqCst), 0);
            assert_eq!(server.receipt.load(Ordering::Acquire), 0);
        }
        brain.prepare_issue_session(Incarnation(1));
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                0,
                &consent,
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 0,
                draft: action.draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(saved.clone())
        );
        drop(conn);
        brain.close().await;
        let (restarted, other) = filing_owner(&scratch, &server).await;
        brain = restarted;
        assert_ne!(other.draft, action.draft);
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                0,
                &consent,
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 0,
                draft: action.draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(saved.clone())
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
        let fresh = rearm_filing(&scratch, &mut brain, &saved, 906).await;
        assert_ne!(fresh.approval, action.approval);
        assert_ne!(fresh.approval, other.approval);
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                907,
                &IssueOp::Consent {
                    draft: fresh.draft,
                    hash: fresh.hash(),
                    approval: fresh.approval.unwrap(),
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
                    request: None,
                    draft: fresh.draft,
                    version: StateVersion(fresh.version.0 + 1),
                    state: ActionState::Sending,
                    approval: None,
                },
                ControlToShell::ActionState {
                    request: None,
                    draft: fresh.draft,
                    version: StateVersion(fresh.version.0 + 2),
                    state: ActionState::Sending,
                    approval: None,
                },
                ControlToShell::ActionState {
                    request: Some(907),
                    draft: fresh.draft,
                    version: StateVersion(fresh.version.0 + 3),
                    state: ActionState::Filed {
                        number: 17,
                        url: IssueUrl::new("https://github.com/owner/repo/issues/17").unwrap(),
                        confirmed_by_user: false,
                        maybe_duplicate: false,
                    },
                    approval: None,
                },
            ]
        );
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(fresh.draft)
                .unwrap(),
            Some(Action {
                version: StateVersion(fresh.version.0 + 3),
                stage: Stage::Filed {
                    number: NonZeroU64::new(17).unwrap(),
                    confirmed: false,
                    maybe_duplicate: false,
                },
                approval: None,
                consumed: fresh.approval,
                ..fresh
            })
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 4);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_eq!(server.receipt.load(Ordering::Acquire), 17);
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
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
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
            kind: IssueUnavailableKind::AttemptNotSaved,
            request: 601,
            draft: action.draft
        }]
    );
    let posted = Action {
        version: StateVersion(4),
        stage: Stage::Posting {
            mark: Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::default(),
            },
            unresolved: None,
        },
        approval: None,
        consumed: Some(ApprovalRev(2)),
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
                approval: ApprovalRev(2),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 906,
            draft: action.draft,
        })]
    );
    brain.apply(effects, None).unwrap();
    assert!(!brain.filing.pending());
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
    assert_eq!(second.approval, Some(ApprovalRev(3)));
    let second = rearm_filing(&scratch, &mut brain, &second, 907).await;
    assert_eq!(second.version, StateVersion(2));
    assert_eq!(second.approval, Some(ApprovalRev(4)));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            908,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(4),
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
                request: Some(908),
                draft: second.draft,
                version: StateVersion(3),
                state: ActionState::Draft,
                approval: Some(ApprovalRev(5))
            },
            ControlToShell::IssueRefused {
                request: 908,
                draft: second.draft,
                version: StateVersion(3),
                approval: Some(ApprovalRev(5)),
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
            version: StateVersion(5),
            stage: Stage::Unknown {
                mark: Mark {
                    at_s: NonZeroU64::new(120).unwrap(),
                    presend: Bounded::default()
                },
                lookup: LookupResult::Pending,
            },
            approval: Some(ApprovalRev(6)),
            consumed: Some(ApprovalRev(2)),
            uncertain: 1,
            ..action
        })
    );
}
