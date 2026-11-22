use super::*;
use crate::coordinator::{Effect, Input};
use crate::issues::{Answer as IssueAnswer, WriteScope};
use kumochi_protocol::message::FailReason;

pub(super) async fn posting_acknowledgement(
    brain: &mut Brain,
    action: &Action,
) -> (Input, WriteScope) {
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Preflighted { .. },
                ..
            }
        )
    });
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            601,
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
    let scope = timeout(PATIENCE, async {
        loop {
            tokio::select! {
                Some(seen) = observed.recv() => {
                    if let Seen::StoreOpStarted(StoreOp::Issue {
                        write: Write::Preflighted { .. }, scope: Some(scope), ..
                    }) = seen {
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
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    let acknowledged = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let Input::IssueWritten {
        reply: Ok(IssueAnswer::Changed(posting)),
        ..
    } = &acknowledged
    else {
        panic!("Posting must be committed before the final authority check");
    };
    assert_eq!(
        posting,
        &Action {
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
        }
    );
    assert!(!scope.stopped());
    (acknowledged, scope)
}

#[tokio::test]
async fn final_permit_rechecks_same_generation_repository_authority_after_posting_commits() {
    use crate::connections::github::api::ApiError;

    for listing_failed in [false, true] {
        let scratch = Scratch::new("github-filing-final-repository-authority");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
        let stamp = brain.account.github.machine().stamp();
        let (posting, scope) = posting_acknowledgement(&mut brain, &action).await;
        let _updates = brain.account.github.machine_mut().on(
            GitHubInput::Repos {
                generation: stamp.generation,
                result: if listing_failed {
                    Err(ApiError::Http(HttpError::Connect))
                } else {
                    Ok(Repositories {
                        names: Bounded::default(),
                        ids: Vec::new(),
                    })
                },
            },
            Instant::now(),
        );
        assert_eq!(brain.account.github.machine().stamp(), stamp);
        assert_eq!(
            brain.account.github.machine().user(),
            action.account.as_ref()
        );
        assert_eq!(
            brain.account.github.machine().repositories(),
            if listing_failed {
                None
            } else {
                Some([].as_slice())
            }
        );
        assert!(!scope.stopped());
        let effects = brain.handle(posting).unwrap();
        assert!(matches!(
            &effects[..],
            [Effect::Control(ControlToShell::ActionState {
                request: None,
                version: StateVersion(4),
                state: ActionState::Sending,
                approval: None,
                ..
            }), Effect::Store(StoreOp::Issue {
                write: Write::NotSent { draft, at: StateVersion(4), reason: FailReason::NotConnected },
                scope: None,
                ..
            })] if *draft == action.draft
        ));
        assert!(!scope.stopped());
        brain.apply(effects, None).unwrap();
        let saved = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            brain.handle(saved).unwrap(),
            vec![Effect::Control(ControlToShell::ActionState {
                request: Some(601),
                draft: action.draft,
                version: StateVersion(5),
                state: ActionState::Failed(FailReason::NotConnected),
                approval: Some(ApprovalRev(3)),
            })]
        );
        assert!(!brain.filing.pending());
        assert_eq!(brain.account.github.machine().stamp(), stamp);
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(Action {
                version: StateVersion(5),
                stage: Stage::Failed {
                    reason: FailReason::NotConnected,
                },
                approval: Some(ApprovalRev(3)),
                consumed: Some(ApprovalRev(2)),
                ..action
            })
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 4);
        assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn wall_expiry_after_posting_commits_refuses_handoff_in_the_same_generation() {
    use crate::connections::clock::FixedClock;

    let scratch = Scratch::new("github-filing-final-wall-expiry");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let stamp = brain.account.github.machine().stamp();
    let (posting, scope) = posting_acknowledgement(&mut brain, &action).await;
    let (_, _, expires_at_s) = brain
        .account
        .github
        .machine()
        .issue_access(7, &RepoName::new("owner/repo").unwrap())
        .unwrap();
    brain.account.net.clock = Arc::new(FixedClock(expires_at_s));
    assert!(!scope.stopped());
    assert_eq!(brain.account.github.machine().stamp(), stamp);
    let effects = brain.handle(posting).unwrap();
    brain.apply(effects, None).unwrap();
    let completed = timeout(PATIENCE, brain.filing.next()).await.unwrap();
    assert!(scope.stopped());
    let effects = brain.issue_event(completed);
    assert!(matches!(
        &effects[..],
        [Effect::Store(StoreOp::Issue {
            write: Write::NotSent { draft, at: StateVersion(4), reason: FailReason::Interrupted },
            scope: None,
            ..
        })] if *draft == action.draft
    ));
    brain.apply(effects, None).unwrap();
    let saved = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        brain.handle(saved).unwrap(),
        vec![Effect::Control(ControlToShell::ActionState {
            request: Some(601),
            draft: action.draft,
            version: StateVersion(5),
            state: ActionState::Failed(FailReason::Interrupted),
            approval: Some(ApprovalRev(3)),
        })]
    );
    assert!(!brain.filing.pending());
    assert_eq!(brain.account.github.machine().stamp(), stamp);
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(Action {
            version: StateVersion(5),
            stage: Stage::Failed {
                reason: FailReason::Interrupted,
            },
            approval: Some(ApprovalRev(3)),
            consumed: Some(ApprovalRev(2)),
            ..action
        })
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn final_permit_rechecks_same_generation_account_expiry_after_posting_commits() {
    use crate::connections::clock::FixedClock;

    let scratch = Scratch::new("github-filing-final-account-expiry");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let stamp = brain.account.github.machine().stamp();
    let (posting, scope) = posting_acknowledgement(&mut brain, &action).await;
    let (_, _, expires_at_s) = brain
        .account
        .github
        .machine()
        .issue_access(7, &RepoName::new("owner/repo").unwrap())
        .unwrap();
    brain
        .account
        .github
        .machine_mut()
        .clock_for_test(Arc::new(FixedClock(expires_at_s)));
    assert_eq!(brain.account.github.machine().stamp(), stamp);
    assert_eq!(brain.account.github.machine().user(), None);
    assert!(!scope.stopped());
    let effects = brain.handle(posting).unwrap();
    assert!(matches!(
        &effects[..],
        [Effect::Control(ControlToShell::ActionState {
            request: None, version: StateVersion(4), state: ActionState::Sending,
            approval: None, ..
        }), Effect::Store(StoreOp::Issue {
            write: Write::NotSent { draft, at: StateVersion(4), reason: FailReason::NotConnected },
            scope: None, ..
        })] if *draft == action.draft
    ));
    assert!(!scope.stopped());
    brain.apply(effects, None).unwrap();
    let saved = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        brain.handle(saved).unwrap(),
        vec![Effect::Control(ControlToShell::ActionState {
            request: Some(601),
            draft: action.draft,
            version: StateVersion(5),
            state: ActionState::Failed(FailReason::NotConnected),
            approval: Some(ApprovalRev(3)),
        })]
    );
    assert!(!brain.filing.pending());
    assert_eq!(brain.account.github.machine().stamp(), stamp);
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(Action {
            version: StateVersion(5),
            stage: Stage::Failed {
                reason: FailReason::NotConnected
            },
            approval: Some(ApprovalRev(3)),
            consumed: Some(ApprovalRev(2)),
            ..action
        })
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real SQL failure must retain the committed POST evidence through replay"
)]
async fn a_sql_result_save_failure_after_one_post_becomes_unknown_and_replay_cannot_post() {
    use crate::connections::github::issues::CreateOutcome;

    let scratch = Scratch::new("github-filing-result-save-failure");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let (posting, _) = posting_acknowledgement(&mut brain, &action).await;
    let effects = brain.handle(posting).unwrap();
    brain.apply(effects, None).unwrap();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Issue {
                write: Write::Posted { .. },
                ..
            }
        )
    });
    let event = timeout(PATIENCE, brain.filing.next()).await.unwrap();
    let effects = brain.issue_event(event);
    let [
        Effect::Store(StoreOp::Issue {
            write, scope: None, ..
        }),
    ] = &effects[..]
    else {
        panic!("the real 201 must queue its result write");
    };
    assert_eq!(
        write,
        &Write::Posted {
            draft: action.draft,
            at: StateVersion(4),
            outcome: CreateOutcome::Created {
                number: 17,
                url: IssueUrl::new("https://github.com/owner/repo/issues/17").unwrap(),
            },
        }
    );
    brain.apply(effects, None).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_filed_result BEFORE UPDATE OF stage ON actions
         WHEN json_extract(NEW.stage, '$.kind') = 'filed'
         BEGIN SELECT RAISE(ABORT, 'test result save refused'); END;",
    )
    .unwrap();
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
    release.send(()).unwrap();
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
        Some(Action {
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
        })
    );
    let effects = brain.handle(failed).unwrap();
    assert!(matches!(
        &effects[..],
        [Effect::Store(StoreOp::Issue {
            write: Write::Interrupted { draft, at: StateVersion(4) },
            scope: None,
            ..
        })] if *draft == action.draft
    ));
    brain.apply(effects, None).unwrap();
    let cleanup = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    let settled = Action {
        version: StateVersion(5),
        stage: Stage::Unknown {
            mark: Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::default(),
            },
            lookup: LookupResult::Pending,
        },
        approval: Some(ApprovalRev(3)),
        consumed: Some(ApprovalRev(2)),
        uncertain: 1,
        ..action.clone()
    };
    assert!(matches!(
        &cleanup,
        Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. }
            if stored == &settled
    ));
    let terminal = vec![Effect::Control(ControlToShell::ActionState {
        request: Some(601),
        draft: action.draft,
        version: StateVersion(5),
        state: ActionState::Unknown {
            lookup: LookupResult::Pending,
            maybe_duplicate: false,
        },
        approval: Some(ApprovalRev(3)),
    })];
    assert_eq!(brain.handle(cleanup).unwrap(), terminal);
    assert!(!brain.filing.pending());
    conn.execute_batch("DROP TRIGGER refuse_filed_result;")
        .unwrap();
    let effects = brain
        .issue_op(
            brain.issue_session().unwrap(),
            601,
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
            request: 601,
            draft: action.draft,
        })]
    );
    assert_eq!(brain.filing.deadline_for_test(), None);
    assert!(!brain.filing.pending());
    brain.close().await;
    let mut reopened = Store::open(&scratch.db()).unwrap();
    reopened.recover().unwrap();
    assert_eq!(reopened.action(action.draft).unwrap(), Some(settled));
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
}

struct UnauthorizedPost(Arc<FilingServer>);

impl Http for UnauthorizedPost {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        if let Endpoint::GitHub(GitHubEndpoint::CreateIssue { repo, body, .. }) = &request.endpoint
        {
            assert!(request.fields.is_empty());
            assert_eq!(
                request.endpoint.url().unwrap().into_owned(),
                "https://api.github.com/repos/owner/repo/issues"
            );
            assert_eq!(repo.as_str(), "owner/repo");
            assert_eq!(
                body.expose().bytes_for_test(),
                r#"{"title":"  Local café  ","body":"first\n\tcafé\n"}"#.as_bytes()
            );
            assert_eq!(self.0.posts.fetch_add(1, Ordering::SeqCst), 0);
            return Ok(Response {
                status: 401,
                request_id: None,
                meta: ResponseMeta::default(),
                body: Secret::new(Zeroizing::new(b"{}".to_vec())),
            });
        }
        self.0.execute(request).await
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the same completed HTTP 401 must revoke its owner but preserve a replacement"
)]
async fn a_real_post_401_revokes_only_the_generation_that_sent_it() {
    for replacement in [false, true] {
        let scratch = Scratch::new("github-filing-post-401-generation");
        let server = Arc::new(FilingServer::default());
        let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
        let http = Arc::clone(&server);
        brain.account.net.http = Arc::new(move || {
            http.factories.fetch_add(1, Ordering::SeqCst);
            Ok(SharedHttp::new(UnauthorizedPost(Arc::clone(&http))))
        });
        let sent = brain.account.github.machine().stamp();
        let (posting, _) = posting_acknowledgement(&mut brain, &action).await;
        let effects = brain.handle(posting).unwrap();
        brain.apply(effects, None).unwrap();
        let completed = timeout(PATIENCE, brain.filing.next()).await.unwrap();
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        if replacement {
            drive(&mut brain.account, GitHubInput::Op(GitHubOp::SignOut));
            settle_owner(&mut brain.account).await;
            saving_owner(&mut brain.account).await;
            settle_owner(&mut brain.account).await;
            let generation = brain.account.github.machine().stamp().generation;
            drive(
                &mut brain.account,
                GitHubInput::Repos {
                    generation,
                    result: Ok(Repositories {
                        names: Bounded::new(vec![RepoName::new("owner/repo").unwrap()]).unwrap(),
                        ids: Vec::new(),
                    }),
                },
            );
            assert_ne!(brain.account.github.machine().stamp(), sent);
        }
        let current = brain.account.github.machine().stamp();
        let effects = brain.issue_event(completed);
        let frames: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Control(frame) => Some(frame.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            frames,
            if replacement {
                vec![ControlToShell::GitHub(GitHubView::Connected {
                    login: GitHubLogin::new("owner-sentinel").unwrap(),
                    repos: Bounded::new(vec![RepoName::new("owner/repo").unwrap()]).unwrap(),
                    truncated: false,
                    default: None,
                })]
            } else {
                vec![ControlToShell::GitHub(GitHubView::NeedsSignIn {
                    login: GitHubLogin::new("owner-sentinel").unwrap(),
                })]
            }
        );
        if replacement {
            assert_eq!(brain.account.github.machine().stamp(), current);
            assert_eq!(
                brain.account.github.issue_snapshot().account,
                action.account
            );
            assert_eq!(
                brain.account.github.issue_snapshot().repos,
                vec![RepoName::new("owner/repo").unwrap()]
            );
        } else {
            assert_eq!(
                brain.account.github.machine().stamp().generation,
                sent.generation + 1
            );
            assert_eq!(brain.account.github.issue_snapshot().account, None);
            assert_eq!(brain.account.github.issue_snapshot().repos, Vec::new());
        }
        brain.apply(effects, None).unwrap();
        let saved = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        let settled = Action {
            version: StateVersion(5),
            stage: if replacement {
                Stage::Unknown {
                    mark: Mark {
                        at_s: NonZeroU64::new(120).unwrap(),
                        presend: Bounded::default(),
                    },
                    lookup: LookupResult::Pending,
                }
            } else {
                Stage::Failed {
                    reason: FailReason::NotConnected,
                }
            },
            approval: Some(ApprovalRev(3)),
            consumed: Some(ApprovalRev(2)),
            uncertain: u32::from(replacement),
            ..action.clone()
        };
        assert!(matches!(
            &saved,
            Input::IssueWritten { reply: Ok(IssueAnswer::Changed(stored)), .. }
                if stored == &settled
        ));
        assert_eq!(
            brain.handle(saved).unwrap(),
            vec![Effect::Control(ControlToShell::ActionState {
                request: Some(601),
                draft: action.draft,
                version: StateVersion(5),
                state: if replacement {
                    ActionState::Unknown {
                        lookup: LookupResult::Pending,
                        maybe_duplicate: false,
                    }
                } else {
                    ActionState::Failed(FailReason::NotConnected)
                },
                approval: Some(ApprovalRev(3)),
            })]
        );
        assert!(!brain.filing.pending());
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(settled)
        );
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 4);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_eq!(server.receipt.load(Ordering::Acquire), 0);
    }
}
