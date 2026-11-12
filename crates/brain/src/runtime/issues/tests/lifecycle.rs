use super::*;
use std::future::Future as _;
use std::sync::Mutex;
use std::task::Poll;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one actual-SQL deadline scenario covers both durable pre-handoff waits"
)]
async fn one_deadline_cancels_held_consent_or_posting_without_an_eof_or_http_handoff() {
    for admission in [true, false] {
        let scratch = Scratch::new("github-filing-held-sql-deadline");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let mut observed = brain.observe();
        let release = if admission {
            brain.hold_store(|op| {
                matches!(
                    op,
                    StoreOp::Issue {
                        write: Write::Consent { .. },
                        ..
                    }
                )
            })
        } else {
            brain.hold_store(|op| {
                matches!(
                    op,
                    StoreOp::Issue {
                        write: Write::Preflighted { .. },
                        ..
                    }
                )
            })
        };
        let effects = brain
            .issue_op(
                401,
                &IssueOp::Consent {
                    draft: action.draft,
                    hash: action.hash(),
                    approval: ApprovalRev(1),
                    decision: Decision::File,
                },
                Some(brain.epoch),
            )
            .unwrap();
        let deadline = brain.filing.deadline_for_test().unwrap();
        brain.apply(effects, None).unwrap();
        let scope = timeout(PATIENCE, async {
            loop {
                tokio::select! {
                    Some(seen) = observed.recv() => {
                        if let Seen::StoreOpStarted(StoreOp::Issue { write, scope: Some(scope), .. }) = seen
                            && matches!((admission, write), (true, Write::Consent { .. }) | (false, Write::Preflighted { .. }))
                        {
                            break scope;
                        }
                    }
                    Some(input) = brain.inputs.recv() => brain.offline(input),
                    event = brain.filing.next() => {
                        let effects = brain.issue_event(event);
                        brain.apply(effects, None).unwrap();
                    }
                }
            }
        }).await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        let before_deadline = deadline - Duration::from_secs(1);
        tokio::time::advance(before_deadline.saturating_duration_since(Instant::now())).await;
        assert_eq!(Instant::now(), before_deadline);
        assert!(!scope.stopped());
        {
            let next = brain.filing.next();
            tokio::pin!(next);
            let state = std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
            assert!(matches!(state, Poll::Pending));
        }
        let after_deadline = deadline + Duration::from_secs(1);
        tokio::time::advance(after_deadline.saturating_duration_since(Instant::now())).await;
        assert_eq!(Instant::now(), after_deadline);
        let event = {
            let next = brain.filing.next();
            tokio::pin!(next);
            let state = std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
            let Poll::Ready(event) = state else {
                panic!("the original 60-second deadline must fire without shell loss");
            };
            event
        };
        assert_eq!(brain.issue_event(event).len(), 0);
        assert!(scope.stopped());
        tokio::time::resume();
        release.send(()).unwrap();
        assert!(
            timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
                .await
                .unwrap()
        );
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(Action {
                version: StateVersion(3),
                stage: Stage::Failed {
                    reason: kumochi_protocol::message::FailReason::Interrupted
                },
                approval: Some(ApprovalRev(2)),
                consumed: Some(ApprovalRev(1)),
                ..action
            })
        );
        assert_eq!(
            server.factories.load(Ordering::SeqCst),
            usize::from(!admission)
        );
        assert_eq!(
            server.gets.load(Ordering::SeqCst),
            if admission { 0 } else { 4 }
        );
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn close_and_quit_join_a_canceled_constructor_before_closing_the_sql_writer() {
    for quit in [false, true] {
        let scratch = Scratch::new("github-filing-retained-factory");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let entered = Arc::new(Notify::new());
        let started = Arc::clone(&entered);
        let http = Arc::clone(&server);
        let (release, resume) = std::sync::mpsc::channel();
        let resume = Mutex::new(resume);
        brain.account.net.http = Arc::new(move || {
            http.factories.fetch_add(1, Ordering::SeqCst);
            started.notify_one();
            resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            Ok(SharedHttp::new(Arc::clone(&http)))
        });
        let consent = IssueOp::Consent {
            draft: action.draft,
            hash: action.hash(),
            approval: ApprovalRev(1),
            decision: Decision::File,
        };
        let effects = brain.issue_op(501, &consent, Some(brain.epoch)).unwrap();
        brain.apply(effects, None).unwrap();
        timeout(PATIENCE, async {
            loop {
                tokio::select! {
                    () = entered.notified() => break,
                    Some(input) = brain.inputs.recv() => brain.offline(input),
                }
            }
        })
        .await
        .unwrap();
        brain.filing.cancel();
        let effects = brain.issue_op(502, &consent, Some(brain.epoch)).unwrap();
        brain.apply(effects, None).unwrap();
        tokio::time::pause();
        let closing = async move {
            if quit {
                assert_eq!(
                    brain.quit(Instant::now()).await,
                    crate::runtime::Quit::Incomplete
                );
            } else {
                brain.close().await;
            }
        };
        tokio::pin!(closing);
        let state = std::future::poll_fn(|cx| Poll::Ready(closing.as_mut().poll(cx))).await;
        assert!(matches!(state, Poll::Pending));
        tokio::time::advance(Duration::from_secs(61)).await;
        let state = std::future::poll_fn(|cx| Poll::Ready(closing.as_mut().poll(cx))).await;
        assert!(matches!(state, Poll::Pending));
        tokio::time::resume();
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(tokio::spawn(async { 17 }).await.unwrap(), 17);
        release.send(()).unwrap();
        timeout(PATIENCE, closing).await.unwrap();
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(Action {
                version: StateVersion(3),
                stage: Stage::Failed {
                    reason: kumochi_protocol::message::FailReason::Interrupted
                },
                approval: Some(ApprovalRev(2)),
                consumed: Some(ApprovalRev(1)),
                ..action
            })
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn the_original_deadline_after_post_handoff_records_unknown_and_replay_never_resends() {
    let scratch = Scratch::new("github-filing-post-deadline");
    let server = Arc::new(FilingServer {
        lose_response: true,
        ..FilingServer::default()
    });
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let consent = IssueOp::Consent {
        draft: action.draft,
        hash: action.hash(),
        approval: ApprovalRev(1),
        decision: Decision::File,
    };
    let effects = brain.issue_op(601, &consent, Some(brain.epoch)).unwrap();
    brain.apply(effects, None).unwrap();
    timeout(PATIENCE, async {
        loop {
            tokio::select! {
                () = server.posted.notified() => break,
                Some(input) = brain.inputs.recv() => brain.offline(input),
                event = brain.filing.next() => {
                    let effects = brain.issue_event(event);
                    brain.apply(effects, None).unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    let event = {
        let next = brain.filing.next();
        tokio::pin!(next);
        let state = std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
        let Poll::Ready(event) = state else {
            panic!("the one filing deadline must end a lost POST response");
        };
        event
    };
    assert_eq!(brain.issue_event(event).len(), 0);
    tokio::time::resume();
    assert!(
        timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
            .await
            .unwrap()
    );
    let unknown = Action {
        version: StateVersion(4),
        stage: Stage::Unknown {
            mark: Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::default(),
            },
            lookup: LookupResult::Pending,
        },
        approval: Some(ApprovalRev(2)),
        consumed: Some(ApprovalRev(1)),
        uncertain: 1,
        ..action.clone()
    };
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(unknown.clone())
    );
    let effects = brain.issue_op(602, &consent, Some(brain.epoch)).unwrap();
    brain.apply(effects, None).unwrap();
    timeout(PATIENCE, async {
        while brain.filing.pending() {
            tokio::select! {
                Some(input) = brain.inputs.recv() => brain.offline(input),
                event = brain.filing.next() => {
                    let effects = brain.issue_event(event);
                    brain.apply(effects, None).unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(unknown)
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real send proves that Unknown and dismissed uncertainty both block another draft"
)]
async fn another_draft_consumes_each_approval_but_cannot_repeat_an_unknown_or_dismissed_send() {
    let scratch = Scratch::new("github-filing-unresolved-same-payload");
    let server = Arc::new(FilingServer {
        lose_response: true,
        ..FilingServer::default()
    });
    let (mut brain, first) = filing_owner(&scratch, &server).await;
    let effects = brain
        .issue_op(
            701,
            &IssueOp::Consent {
                draft: first.draft,
                hash: first.hash(),
                approval: ApprovalRev(1),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap();
    brain.apply(effects, None).unwrap();
    timeout(PATIENCE, async {
        loop {
            tokio::select! {
                () = server.posted.notified() => break,
                Some(input) = brain.inputs.recv() => brain.offline(input),
                event = brain.filing.next() => {
                    let effects = brain.issue_event(event);
                    brain.apply(effects, None).unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
            .await
            .unwrap()
    );
    let second = {
        let mut store = Store::open(&scratch.db()).unwrap();
        store
            .create_draft(
                first.thread,
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
    assert_eq!(second.version, StateVersion(1));
    assert_eq!(second.approval, Some(ApprovalRev(3)));
    for (attempt, approval, request) in [(1, 3, 711), (2, 4, 713)] {
        if attempt == 2 {
            let effects = brain
                .issue_op(
                    712,
                    &IssueOp::Discard { draft: first.draft },
                    Some(brain.epoch),
                )
                .unwrap();
            brain.apply(effects, None).unwrap();
            assert!(
                timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
                    .await
                    .unwrap()
            );
            assert_eq!(
                Store::open(&scratch.db())
                    .unwrap()
                    .action(first.draft)
                    .unwrap(),
                Some(Action {
                    version: StateVersion(5),
                    stage: Stage::Discarded,
                    approval: None,
                    consumed: Some(ApprovalRev(1)),
                    uncertain: 1,
                    ..first.clone()
                })
            );
        }
        let effects = brain
            .issue_op(
                request,
                &IssueOp::Consent {
                    draft: second.draft,
                    hash: second.hash(),
                    approval: ApprovalRev(approval),
                    decision: Decision::File,
                },
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
        let mut notices = Vec::new();
        timeout(PATIENCE, async {
            while brain.filing.pending() {
                let effects = tokio::select! {
                    Some(input) = brain.inputs.recv() => brain.handle(input).unwrap_or_default(),
                    event = brain.filing.next() => brain.issue_event(event),
                };
                notices.extend(effects.iter().filter_map(|effect| match effect {
                    crate::coordinator::Effect::Control(control) => Some(control.clone()),
                    _ => None,
                }));
                brain.apply(effects, None).unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(
            notices,
            vec![
                ControlToShell::ActionState {
                    request: Some(request),
                    draft: second.draft,
                    version: StateVersion(attempt + 1),
                    state: ActionState::Draft,
                    approval: Some(ApprovalRev(approval + 1))
                },
                ControlToShell::IssueRefused {
                    request,
                    draft: second.draft,
                    version: StateVersion(attempt + 1),
                    approval: Some(ApprovalRev(approval + 1)),
                    reason: kumochi_protocol::message::IssueRefused::UnresolvedDraft {
                        draft: first.draft
                    }
                },
            ]
        );
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(second.draft)
                .unwrap(),
            Some(Action {
                version: StateVersion(attempt + 1),
                approval: Some(ApprovalRev(approval + 1)),
                consumed: Some(ApprovalRev(approval)),
                ..second.clone()
            })
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 4);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_eq!(server.receipt.load(Ordering::Acquire), 17);
    }
    brain.close().await;
}
