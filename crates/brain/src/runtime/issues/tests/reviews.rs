use super::*;
use crate::coordinator::{Effect, Input};
use crate::issues::Answer as IssueAnswer;
use kumochi_protocol::message::{ContentHash, DraftId, IssueCardsStatus, IssueSession};
use std::time::Duration;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "real rearms are retired before SQL or after commit by both cancellation and reconnect"
)]
async fn retired_pending_reviews_never_offer_a_late_saved_revision() {
    for (after_commit, disconnect) in [(false, false), (true, false), (false, true), (true, true)] {
        let scratch = Scratch::new("github-review-retired-pending");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let mut observed = brain.observe();
        let mut release = Some(brain.hold_store(|op| {
            matches!(
                op,
                StoreOp::Issue {
                    write: Write::Rearm { .. },
                    ..
                }
            )
        }));
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                1,
                &IssueOp::Rearm {
                    draft: action.draft,
                    version: action.version,
                    hash: action.hash(),
                },
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
        let scope = timeout(PATIENCE, async {
            loop {
                if let Seen::StoreOpStarted(StoreOp::Issue {
                    write: Write::Rearm { .. },
                    scope: Some(scope),
                    ..
                }) = observed.recv().await.unwrap()
                {
                    break scope;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(brain.filing.review_counts(), (0, 1));
        let mut acknowledged = None;
        if after_commit {
            release.take().unwrap().send(()).unwrap();
            acknowledged = Some(
                timeout(PATIENCE, brain.inputs.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        if disconnect {
            brain.prepare_issue_session(Incarnation(1));
        } else {
            let effects = brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    2,
                    &IssueOp::CancelReview {
                        draft: action.draft,
                        review: 1,
                    },
                    Some(brain.epoch),
                )
                .unwrap();
            assert_eq!(effects, Vec::new());
            brain.apply(effects, None).unwrap();
        }
        assert!(scope.stopped());
        assert_eq!(brain.filing.review_counts(), (0, 1));
        if !after_commit {
            release.take().unwrap().send(()).unwrap();
            acknowledged = Some(
                timeout(PATIENCE, brain.inputs.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        let acknowledged = acknowledged.unwrap();
        let saved = if after_commit {
            let expected = Action {
                version: StateVersion(2),
                approval: Some(ApprovalRev(2)),
                ..action.clone()
            };
            assert!(matches!(
                &acknowledged,
                Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. } if stored == &expected
            ));
            expected
        } else {
            assert!(matches!(
                acknowledged,
                Input::IssueWritten { reply: Err(()), .. }
            ));
            action.clone()
        };
        let effects = brain.handle(acknowledged).unwrap();
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Data(_)))
        );
        brain.apply(effects, None).unwrap();
        assert_eq!(brain.filing.review_counts(), (0, 0));
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                3,
                &IssueOp::Consent {
                    draft: saved.draft,
                    hash: saved.hash(),
                    approval: saved.approval.unwrap(),
                    decision: Decision::File,
                },
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 3,
                draft: saved.draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(saved)
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
        brain.close().await;
    }
}

#[tokio::test]
async fn a_committed_review_past_its_deadline_cannot_register_without_a_timer_event() {
    let scratch = Scratch::new("github-review-late-commit-deadline");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            1,
            &IssueOp::Rearm {
                draft: action.draft,
                version: action.version,
                hash: action.hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    let acknowledged = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let saved = Action {
        version: StateVersion(2),
        approval: Some(ApprovalRev(2)),
        ..action
    };
    assert!(matches!(
        &acknowledged,
        Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. } if stored == &saved
    ));
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    let effects = brain.handle(acknowledged).unwrap();
    tokio::time::resume();
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(_)))
    );
    assert_eq!(brain.filing.review_counts(), (0, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            2,
            &IssueOp::Consent {
                draft: saved.draft,
                hash: saved.hash(),
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
            request: 2,
            draft: saved.draft,
        })]
    );
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(saved.draft)
            .unwrap(),
        Some(saved)
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "supersession between SQL commit and acknowledgement must preserve only the newer review"
)]
async fn a_superseded_committed_review_never_offers_its_known_revision() {
    let scratch = Scratch::new("github-review-superseded-commit");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            1,
            &IssueOp::Rearm {
                draft: action.draft,
                version: action.version,
                hash: action.hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    let old = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let first = Action {
        version: StateVersion(2),
        approval: Some(ApprovalRev(2)),
        ..action
    };
    assert!(matches!(
        &old, Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. } if stored == &first
    ));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            2,
            &IssueOp::Rearm {
                draft: first.draft,
                version: first.version,
                hash: first.hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    let effects = brain.handle(old).unwrap();
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(_)))
    );
    brain.apply(effects, None).unwrap();
    assert_eq!(brain.filing.review_counts(), (0, 1));
    let newest = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let second = Action {
        version: StateVersion(3),
        approval: Some(ApprovalRev(3)),
        ..first.clone()
    };
    assert!(matches!(
        &newest, Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. } if stored == &second
    ));
    assert_eq!(
        brain.handle(newest).unwrap(),
        vec![Effect::Data(DataToShell::IssueReview {
            request: 2,
            card: IssueCard {
                draft: second.draft,
                thread: second.thread,
                repo: second.repo.clone(),
                account: second.account.as_ref().map(|user| user.login.clone()),
                title: second.title.clone(),
                body: second.body.clone(),
                hash: second.hash(),
                approval: Some(ApprovalRev(3)),
                version: StateVersion(3),
                state: ActionState::Draft,
            },
        })]
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            3,
            &IssueOp::Consent {
                draft: first.draft,
                hash: first.hash(),
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
            request: 3,
            draft: first.draft,
        })]
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    let current = brain.issue_session().unwrap();
    assert_eq!(
        brain.issue_op(
            IssueSession {
                epoch: 0,
                ..current
            },
            u64::MAX,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(3),
                decision: Decision::File,
            },
            Some(brain.epoch),
        ),
        None
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            4,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(3),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(
        super::storage::settle(&mut brain).await,
        vec![
            ControlToShell::ActionState {
                request: None,
                draft: second.draft,
                version: StateVersion(4),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: None,
                draft: second.draft,
                version: StateVersion(5),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: Some(4),
                draft: second.draft,
                version: StateVersion(6),
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
    assert_eq!(brain.filing.review_counts(), (0, 0));
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(second.draft)
            .unwrap(),
        Some(Action {
            version: StateVersion(6),
            stage: Stage::Filed {
                number: NonZeroU64::new(17).unwrap(),
                confirmed: false,
                maybe_duplicate: false,
            },
            approval: None,
            consumed: Some(ApprovalRev(3)),
            ..second
        })
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one bounded queue proves the combined cap, pending cancellation ownership and reusable capacity"
)]
async fn offered_and_pending_reviews_share_sixteen_slots_without_lifetime_tombstones() {
    let scratch = Scratch::new("github-review-combined-capacity");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let mut drafts = vec![action.clone()];
    {
        let mut store = Store::open(&scratch.db()).unwrap();
        for _ in 1..17 {
            drafts.push(
                store
                    .create_draft(
                        action.thread,
                        &action.title,
                        &action.body,
                        action.repo.as_ref(),
                        action.account.as_ref(),
                    )
                    .unwrap(),
            );
        }
    }
    for (index, draft) in drafts.iter_mut().enumerate().take(8) {
        *draft = rearm_filing(
            &scratch,
            &mut brain,
            draft,
            u64::try_from(index + 1).unwrap(),
        )
        .await;
    }
    assert_eq!(brain.filing.review_counts(), (8, 0));
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Rearm { .. },
                ..
            }
        )
    });
    for (index, draft) in drafts.iter().enumerate().take(16).skip(8) {
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                u64::try_from(index + 1).unwrap(),
                &IssueOp::Rearm {
                    draft: draft.draft,
                    version: draft.version,
                    hash: draft.hash(),
                },
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
    }
    timeout(PATIENCE, async {
        loop {
            if matches!(
                observed.recv().await.unwrap(),
                Seen::StoreOpStarted(StoreOp::Issue {
                    write: Write::Rearm { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(brain.filing.review_counts(), (8, 8));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            100,
            &IssueOp::CancelReview {
                draft: drafts[8].draft,
                review: 9,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(effects, Vec::new());
    assert_eq!(brain.filing.review_counts(), (8, 8));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            101,
            &IssueOp::Rearm {
                draft: drafts[16].draft,
                version: drafts[16].version,
                hash: drafts[16].hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::ReviewCapacity,
            request: 101,
            draft: drafts[16].draft,
        })]
    );
    for request in 102..118 {
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                request,
                &IssueOp::Consent {
                    draft: action.draft,
                    hash: action.hash(),
                    approval: ApprovalRev(u64::MAX - request),
                    decision: Decision::File,
                },
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
        assert_eq!(brain.filing.review_counts(), (8, 8));
        assert_eq!(brain.filing.deadline_for_test(), None);
    }
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            120,
            &IssueOp::CancelReview {
                draft: drafts[0].draft,
                review: 1,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(effects, Vec::new());
    assert_eq!(brain.filing.review_counts(), (7, 8));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            121,
            &IssueOp::Rearm {
                draft: drafts[16].draft,
                version: drafts[16].version,
                hash: drafts[16].hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(brain.filing.review_counts(), (7, 9));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            122,
            &IssueOp::Rearm {
                draft: drafts[0].draft,
                version: drafts[0].version,
                hash: drafts[0].hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::ReviewCapacity,
            request: 122,
            draft: drafts[0].draft,
        })]
    );
    release.send(()).unwrap();
    let mut offered = 0;
    for _ in 0..9 {
        let saved = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        let effects = brain.handle(saved).unwrap();
        offered += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Data(DataToShell::IssueReview { .. })))
            .count();
        brain.apply(effects, None).unwrap();
    }
    assert_eq!(offered, 8);
    assert_eq!(brain.filing.review_counts(), (15, 0));
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(drafts[8].draft)
            .unwrap(),
        Some(drafts[8].clone())
    );
    let mut current = rearm_filing(&scratch, &mut brain, &drafts[8], 200).await;
    let mut review = 200;
    assert_eq!(brain.filing.review_counts(), (16, 0));
    for cycle in 0..20 {
        let cancel = 201 + 2 * cycle;
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                cancel,
                &IssueOp::CancelReview {
                    draft: current.draft,
                    review,
                },
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(effects, Vec::new());
        assert_eq!(brain.filing.review_counts(), (15, 0));
        let before = current.approval;
        review = cancel + 1;
        current = rearm_filing(&scratch, &mut brain, &current, review).await;
        assert!(current.approval.unwrap().0 > before.unwrap().0);
        assert_eq!(brain.filing.review_counts(), (16, 0));
    }
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    brain.close().await;
}

#[tokio::test]
async fn the_first_wrong_binding_unsupported_decision_or_old_request_retires_the_offer() {
    for case in 0..5 {
        let scratch = Scratch::new("github-review-first-refusal");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
        assert_eq!(brain.filing.review_counts(), (1, 0));
        let draft = if case == 1 {
            DraftId(999)
        } else {
            action.draft
        };
        let request = u64::from(case != 4);
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                request,
                &IssueOp::Consent {
                    draft,
                    hash: if case == 0 {
                        ContentHash([0x91; 32])
                    } else {
                        action.hash()
                    },
                    approval: ApprovalRev(2),
                    decision: match case {
                        2 => Decision::FileAnyway,
                        3 => Decision::ThisIsTheIssue,
                        _ => Decision::File,
                    },
                },
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request,
                draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                2,
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
                request: 2,
                draft: action.draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert!(!brain.filing.pending());
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
        brain.close().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a held real admission makes another offered review busy without constructing either HTTP client"
)]
async fn a_busy_first_confirmation_retires_its_offer_before_sql_refuses_capacity() {
    let scratch = Scratch::new("github-review-busy-refusal");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let second = {
        let mut store = Store::open(&scratch.db()).unwrap();
        store
            .create_draft(
                action.thread,
                &action.title,
                &action.body,
                action.repo.as_ref(),
                action.account.as_ref(),
            )
            .unwrap()
    };
    let second = rearm_filing(&scratch, &mut brain, &second, 1).await;
    assert_eq!(second.approval, Some(ApprovalRev(4)));
    assert_eq!(brain.filing.review_counts(), (2, 0));
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Consent { .. },
                ..
            }
        )
    });
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            2,
            &IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: ApprovalRev(2),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    timeout(PATIENCE, async {
        loop {
            if matches!(
                observed.recv().await.unwrap(),
                Seen::StoreOpStarted(StoreOp::Issue {
                    write: Write::Consent { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            3,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(4),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert!(matches!(&effects[..], [Effect::Store(StoreOp::Issue {
        write: Write::Consent { at: StateVersion(2), approval: ApprovalRev(4), live, .. }, ..
    })] if !live.capacity));
    assert_eq!(brain.filing.review_counts(), (0, 0));
    brain.apply(effects, None).unwrap();
    brain.filing.cancel();
    release.send(()).unwrap();
    let controls = super::storage::settle(&mut brain).await;
    let second_controls: Vec<_> = controls.into_iter().filter(|frame| matches!(frame,
        ControlToShell::ActionState { draft, .. } | ControlToShell::IssueRefused { draft, .. }
            if *draft == second.draft
    )).collect();
    assert_eq!(
        second_controls,
        vec![
            ControlToShell::ActionState {
                request: Some(3),
                draft: second.draft,
                version: StateVersion(3),
                state: ActionState::Draft,
                approval: Some(ApprovalRev(5)),
            },
            ControlToShell::IssueRefused {
                request: 3,
                draft: second.draft,
                version: StateVersion(3),
                approval: Some(ApprovalRev(5)),
                reason: kumochi_protocol::message::IssueRefused::NotAllowedNow,
            },
        ]
    );
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            4,
            &IssueOp::Consent {
                draft: second.draft,
                hash: second.hash(),
                approval: ApprovalRev(4),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 4,
            draft: second.draft,
        })]
    );
    assert_eq!(brain.filing.review_counts(), (0, 0));
    let store = Store::open(&scratch.db()).unwrap();
    assert_eq!(
        store.action(second.draft).unwrap(),
        Some(Action {
            version: StateVersion(3),
            approval: Some(ApprovalRev(5)),
            consumed: Some(ApprovalRev(4)),
            ..second
        })
    );
    assert_eq!(
        store.action(action.draft).unwrap(),
        Some(Action {
            version: StateVersion(4),
            stage: Stage::Failed {
                reason: kumochi_protocol::message::FailReason::Interrupted
            },
            approval: Some(ApprovalRev(6)),
            consumed: Some(ApprovalRev(2)),
            ..action
        })
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    brain.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "ordinary full-card hydration cannot restore a saved approval after either reconnect or restart"
)]
async fn ordinary_saved_card_reads_never_rebuild_an_offer_after_reconnect_or_restart() {
    for restart in [false, true] {
        let scratch = Scratch::new("github-review-card-read-unarmed");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
        if restart {
            brain.close().await;
            let (restarted, other) = filing_owner(&scratch, &server).await;
            assert_ne!(other.draft, action.draft);
            brain = restarted;
        } else {
            brain.prepare_issue_session(Incarnation(1));
        }
        assert_eq!(brain.filing.review_counts(), (0, 0));
        let session = brain.issue_session().unwrap();
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            open_filing(&mut shell).await;
            shell.credit(64 * 1024).await;
            shell
                .data_out
                .write_all(
                    &frame::encode(
                        &DataToBrain::IssueCardsRequest {
                            request: 87,
                            drafts: Bounded::new(vec![action.draft]).unwrap(),
                        },
                        Pipe::Data,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                shell.data().await,
                DataToShell::IssueCardRead {
                    request: 87,
                    card: IssueCard {
                        draft: action.draft,
                        thread: action.thread,
                        repo: action.repo.clone(),
                        account: action.account.as_ref().map(|user| user.login.clone()),
                        title: action.title.clone(),
                        body: action.body.clone(),
                        hash: action.hash(),
                        approval: Some(ApprovalRev(2)),
                        version: StateVersion(2),
                        state: ActionState::Draft,
                    },
                }
            );
            assert_eq!(
                shell.data().await,
                DataToShell::IssueCardsDone {
                    request: 87,
                    status: IssueCardsStatus::Complete,
                }
            );
            shell
                .send(&ControlToBrain::Issue {
                    session,
                    request: 0,
                    op: IssueOp::Consent {
                        draft: action.draft,
                        hash: action.hash(),
                        approval: ApprovalRev(2),
                        decision: Decision::File,
                    },
                })
                .await;
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::IssueUnavailable {
                    kind: IssueUnavailableKind::Action,
                    request: 0,
                    draft: action.draft,
                })
            );
            shell.control_out.shutdown().await.unwrap();
            assert_eq!(shell.control().await, None);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
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
        brain.close().await;
    }
}

#[tokio::test]
async fn cancellation_by_the_opener_retires_an_offer_before_its_revision_is_delivered() {
    let scratch = Scratch::new("github-review-cancel-undelivered-offer");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let reviewed = rearm_filing(&scratch, &mut brain, &action, 1).await;
    assert_eq!(brain.filing.review_counts(), (1, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            2,
            &IssueOp::CancelReview {
                draft: action.draft,
                review: 1,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(effects, Vec::new());
    assert_eq!(brain.filing.review_counts(), (0, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            3,
            &IssueOp::Consent {
                draft: reviewed.draft,
                hash: reviewed.hash(),
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
            request: 3,
            draft: reviewed.draft,
        })]
    );
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(reviewed)
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "ordered supersession must keep the invalidated SQL reservation while only its successor can offer"
)]
async fn supersession_before_sql_keeps_the_old_reservation_until_its_acknowledgement() {
    let scratch = Scratch::new("github-review-superseded-before-sql");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Rearm { .. },
                ..
            }
        )
    });
    let rearm = IssueOp::Rearm {
        draft: action.draft,
        version: action.version,
        hash: action.hash(),
    };
    let effects = brain
        .issue_op(brain.issue_session().unwrap(), 1, &rearm, Some(brain.epoch))
        .unwrap();
    brain.apply(effects, None).unwrap();
    timeout(PATIENCE, async {
        loop {
            if matches!(
                observed.recv().await.unwrap(),
                Seen::StoreOpStarted(StoreOp::Issue {
                    write: Write::Rearm { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let effects = brain
        .issue_op(brain.issue_session().unwrap(), 2, &rearm, Some(brain.epoch))
        .unwrap();
    brain.apply(effects, None).unwrap();
    assert_eq!(brain.filing.review_counts(), (0, 2));
    release.send(()).unwrap();
    let old = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(old, Input::IssueWritten { reply: Err(()), .. }));
    let effects = brain.handle(old).unwrap();
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(_)))
    );
    assert_eq!(brain.filing.review_counts(), (0, 1));
    let new = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let reviewed = Action {
        version: StateVersion(2),
        approval: Some(ApprovalRev(2)),
        ..action.clone()
    };
    assert!(matches!(
        &new, Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. } if stored == &reviewed
    ));
    assert_eq!(
        brain.handle(new).unwrap(),
        vec![Effect::Data(DataToShell::IssueReview {
            request: 2,
            card: IssueCard {
                draft: reviewed.draft,
                thread: reviewed.thread,
                repo: reviewed.repo.clone(),
                account: reviewed.account.as_ref().map(|user| user.login.clone()),
                title: reviewed.title.clone(),
                body: reviewed.body.clone(),
                hash: reviewed.hash(),
                approval: Some(ApprovalRev(2)),
                version: StateVersion(2),
                state: ActionState::Draft,
            },
        })]
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            3,
            &IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: ApprovalRev(1),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 3,
            draft: action.draft,
        })]
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(reviewed)
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}

#[tokio::test]
async fn exhausted_review_allocators_and_versions_never_register_permission() {
    for allocator in [false, true] {
        let scratch = Scratch::new("github-review-exhausted-metadata");
        let server = Arc::new(FilingServer::default());
        let (mut brain, mut action) = filing_owner(&scratch, &server).await;
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        if allocator {
            conn.execute(
                "UPDATE approval_mark SET last = 9223372036854775807 WHERE id = 1",
                [],
            )
            .unwrap();
        } else {
            conn.execute(
                "UPDATE actions SET version = 9223372036854775807 WHERE id = ?1",
                [i64::try_from(action.draft.0).unwrap()],
            )
            .unwrap();
            action.version = StateVersion(9_223_372_036_854_775_807);
        }
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                1,
                &IssueOp::Rearm {
                    draft: action.draft,
                    version: action.version,
                    hash: action.hash(),
                },
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
        let saved = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(saved, Input::IssueWritten { reply: Err(()), .. }));
        assert_eq!(
            brain.handle(saved).unwrap(),
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::ReviewNotSaved,
                request: 1,
                draft: action.draft,
            })]
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                2,
                &IssueOp::Consent {
                    draft: action.draft,
                    hash: action.hash(),
                    approval: ApprovalRev(1),
                    decision: Decision::File,
                },
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 2,
                draft: action.draft,
            })]
        );
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(action)
        );
        assert_eq!(brain.filing.review_counts(), (0, 0));
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert_eq!(server.factories.load(Ordering::SeqCst), 0);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        drop(conn);
        brain.close().await;
    }
}

#[tokio::test]
async fn a_pending_review_deadline_cancels_sql_but_keeps_its_reservation_until_ack() {
    let scratch = Scratch::new("github-review-pending-deadline");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Rearm { .. },
                ..
            }
        )
    });
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            1,
            &IssueOp::Rearm {
                draft: action.draft,
                version: action.version,
                hash: action.hash(),
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    let scope = timeout(PATIENCE, async {
        loop {
            if let Seen::StoreOpStarted(StoreOp::Issue {
                write: Write::Rearm { .. },
                scope: Some(scope),
                ..
            }) = observed.recv().await.unwrap()
            {
                break scope;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(!scope.stopped());
    tokio::time::advance(Duration::from_secs(1)).await;
    let event = timeout(Duration::from_secs(1), brain.filing.next())
        .await
        .unwrap();
    let effects = brain.issue_event(event);
    tokio::time::resume();
    assert_eq!(
        effects,
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            request: 1,
            draft: action.draft,
            kind: IssueUnavailableKind::ReviewNotSaved,
        })]
    );
    assert!(scope.stopped());
    assert_eq!(brain.filing.review_counts(), (0, 1));
    assert_eq!(brain.filing.deadline_for_test(), None);
    release.send(()).unwrap();
    let saved = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(saved, Input::IssueWritten { reply: Err(()), .. }));
    assert_eq!(brain.handle(saved).unwrap(), Vec::new());
    assert_eq!(brain.filing.review_counts(), (0, 0));
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
    brain.close().await;
}

#[tokio::test]
async fn a_full_reply_queue_consumes_a_located_offer_before_refusing_the_request() {
    let scratch = Scratch::new("github-review-full-reply-queue");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let mut writes = Vec::new();
    let cap = u64::try_from(crate::runtime::CONTROL_QUEUE).unwrap();
    for request in 1..=cap {
        writes.extend(
            brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    request,
                    &IssueOp::Discard {
                        draft: DraftId(999),
                    },
                    Some(brain.epoch),
                )
                .unwrap(),
        );
    }
    let consent = IssueOp::Consent {
        draft: action.draft,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
    };
    assert_eq!(
        brain.issue_op(
            brain.issue_session().unwrap(),
            cap + 1,
            &consent,
            Some(brain.epoch),
        ),
        None
    );
    assert_eq!(brain.filing.review_counts(), (0, 0));
    assert_eq!(brain.filing.deadline_for_test(), None);
    brain.apply(writes, None).unwrap();
    assert_eq!(
        super::storage::settle(&mut brain).await,
        (1..=cap)
            .map(|request| ControlToShell::IssueUnavailable {
                request,
                draft: DraftId(999),
                kind: IssueUnavailableKind::Action,
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        brain.issue_op(
            brain.issue_session().unwrap(),
            cap + 2,
            &consent,
            Some(brain.epoch),
        ),
        Some(vec![Effect::Control(ControlToShell::IssueUnavailable {
            request: cap + 2,
            draft: action.draft,
            kind: IssueUnavailableKind::Action,
        })])
    );
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(action)
    );
    assert_eq!(brain.filing.deadline_for_test(), None);
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}
