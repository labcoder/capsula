use super::*;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one framed session proves failed storage cannot revive an old consent but a fresh confirmation can file"
)]
async fn wire_replay_after_both_approval_writes_fail_requires_a_new_request() {
    let scratch = Scratch::new("github-filing-failed-admission-replay");
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
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        let mut replies = Vec::new();
        let mut work = Vec::new();
        let mut saved = Vec::new();
        for (attempt, request) in [904, 904, 905].into_iter().enumerate() {
            shell
                .send(&ControlToBrain::Issue {
                    request,
                    op: consent.clone(),
                })
                .await;
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
                    break;
                }
            }
            replies.push(frames);
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
    let (ended, (replies, work, saved)) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(
        replies[0],
        vec![ControlToShell::IssueUnavailable {
            request: 904,
            draft: action.draft,
        }]
    );
    assert_eq!(
        replies[1],
        vec![ControlToShell::IssueUnavailable {
            request: 904,
            draft: action.draft,
        }],
        "the exact original frame is terminal even after SQL recovers"
    );
    assert_eq!(work, vec![(0, 0, 0, 0), (0, 0, 0, 0), (1, 4, 1, 17)]);
    assert_eq!(saved[0], Some(action.clone()));
    assert_eq!(saved[1], Some(action.clone()));
    assert_eq!(
        replies[2],
        vec![
            ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(2),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(3),
                state: ActionState::Sending,
                approval: None,
            },
            ControlToShell::ActionState {
                request: Some(905),
                draft: action.draft,
                version: StateVersion(4),
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
        saved[2],
        Some(Action {
            version: StateVersion(4),
            stage: Stage::Filed {
                number: NonZeroU64::new(17).unwrap(),
                confirmed: false,
                maybe_duplicate: false,
            },
            approval: None,
            consumed: Some(ApprovalRev(1)),
            ..action
        })
    );
}
