use super::*;

async fn final_reply(shell: &mut Shell<DuplexStream>) -> Vec<ControlToShell> {
    let mut frames = Vec::new();
    loop {
        let reply = shell.control().await.unwrap();
        let terminal = matches!(
            reply,
            ControlToShell::ActionState {
                request: Some(_),
                ..
            } | ControlToShell::IssueUnavailable { .. }
        );
        frames.push(reply);
        if terminal {
            return frames;
        }
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "old-frame replay ends its replacement before fresh confirmation in a new framed session"
)]
async fn wire_replay_after_both_approval_writes_fail_requires_a_new_request() {
    let scratch = Scratch::new("github-filing-failed-admission-replay");
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
    let original = ControlToBrain::Issue {
        session: brain.issue_session().unwrap(),
        request: 904,
        op: consent.clone(),
    };
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        let mut replies = Vec::new();
        let mut work = Vec::new();
        let mut saved = Vec::new();
        for attempt in 0..2 {
            shell.send(&original).await;
            replies.push(final_reply(&mut shell).await);
            work.push((
                server.factories.load(Ordering::SeqCst),
                server.gets.load(Ordering::SeqCst),
                server.posts.load(Ordering::SeqCst),
                server.receipt.load(Ordering::Acquire),
            ));
            saved.push(
                Store::open(&scratch.db())
                    .unwrap()
                    .action(action.draft)
                    .unwrap(),
            );
            if attempt == 0 {
                conn.execute_batch("DROP TRIGGER refuse_approval;").unwrap();
            }
        }
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
        (replies, work, saved)
    };
    let (ended, (mut replies, mut work, mut saved)) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    brain.prepare_issue_session(kumochi_protocol::message::Incarnation(1));
    let ((control, data), mut replacement) = duplex();
    let client = async {
        open_filing(&mut replacement).await;
        replacement.send(&original).await;
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(action.clone())
    );
    let session = brain.prepare_issue_session(kumochi_protocol::message::Incarnation(1));
    let ((control, data), mut fresh_shell) = duplex();
    let client = async {
        open_filing(&mut fresh_shell).await;
        fresh_shell
            .send(&ControlToBrain::Issue {
                session,
                request: 0,
                op: consent.clone(),
            })
            .await;
        let frames = final_reply(&mut fresh_shell).await;
        fresh_shell.control_out.shutdown().await.unwrap();
        assert_eq!(fresh_shell.control().await, None);
        frames
    };
    let (ended, fresh_frames) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    replies.push(fresh_frames);
    work.push((
        server.factories.load(Ordering::SeqCst),
        server.gets.load(Ordering::SeqCst),
        server.posts.load(Ordering::SeqCst),
        server.receipt.load(Ordering::Acquire),
    ));
    saved.push(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
    );
    brain.close().await;
    assert_eq!(
        replies[0],
        vec![ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 904,
            draft: action.draft,
        }]
    );
    assert_eq!(
        replies[1],
        vec![ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 904,
            draft: action.draft,
        }],
        "the exact original frame is terminal even after SQL recovers"
    );
    assert_eq!(work, vec![(0, 0, 0, 0), (0, 0, 0, 0), (0, 0, 0, 0)]);
    assert_eq!(saved[0], Some(action.clone()));
    assert_eq!(saved[1], Some(action.clone()));
    assert_eq!(
        replies[2],
        vec![ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 0,
            draft: action.draft
        }]
    );
    assert_eq!(saved[2], Some(action.clone()));
    let (mut restarted, other) = filing_owner(&scratch, &server).await;
    assert_ne!(other.draft, action.draft);
    let effects = restarted
        .issue_op(
            restarted.issue_session().unwrap(),
            0,
            &consent,
            Some(restarted.epoch),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![crate::coordinator::Effect::Control(
            ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 0,
                draft: action.draft,
            }
        )]
    );
    assert_eq!(restarted.filing.review_counts(), (0, 0));
    assert_eq!(restarted.filing.deadline_for_test(), None);
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(action.clone())
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    let fresh = rearm_filing(&scratch, &mut restarted, &action, 1).await;
    assert_ne!(fresh.approval, action.approval);
    assert_ne!(fresh.approval, other.approval);
    let effects = restarted
        .issue_op(
            restarted.issue_session().unwrap(),
            2,
            &IssueOp::Consent {
                draft: fresh.draft,
                hash: fresh.hash(),
                approval: fresh.approval.unwrap(),
                decision: Decision::File,
            },
            Some(restarted.epoch),
        )
        .unwrap();
    restarted.apply(effects, None).unwrap();
    assert_eq!(
        super::storage::settle(&mut restarted).await,
        vec![
            ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(4),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(5),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: Some(2),
                draft: action.draft,
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
    restarted.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(fresh.draft)
            .unwrap(),
        Some(Action {
            version: StateVersion(6),
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
}
