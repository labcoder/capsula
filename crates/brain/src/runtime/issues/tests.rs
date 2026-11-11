use super::*;
use crate::connections::clock::SystemClock;
use crate::connections::github::{
    GitHubEndpoint, Page,
    api::GitHubUser,
    device::{Input as GitHubInput, Repositories},
};
use crate::connections::http_client::{
    Endpoint, Http, HttpError, Request, Response, ResponseMeta, SharedHttp,
};
use crate::issues::{Action, Mark, Stage, Write};
use crate::runtime::account::github_tests::drive;
use crate::runtime::{Seen, StoreOp};
use kumochi_protocol::{
    Secret,
    message::{
        ActionState, ApprovalRev, GitHubLogin, GitHubView, IssueUrl, LookupResult, StateVersion,
    },
};
use kumochi_scene::text::Bounded;
use serde_json::json;
use std::num::NonZeroU64;
use zeroize::Zeroizing;

#[derive(Default)]
struct FilingServer {
    factories: AtomicUsize,
    gets: AtomicUsize,
    posts: AtomicUsize,
    receipt: AtomicUsize,
    posted: Notify,
    lose_response: bool,
}

impl Http for Arc<FilingServer> {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        assert!(request.fields.is_empty());
        let destination = request.endpoint.url().unwrap().into_owned();
        let (date, body) = match request.endpoint {
            Endpoint::GitHub(GitHubEndpoint::User { .. }) => {
                (100, json!({"id":7,"login":"owner-sentinel"}))
            }
            Endpoint::GitHub(GitHubEndpoint::Installations { page, .. }) => {
                assert_eq!(page, Page::new(1).unwrap());
                (100, json!({"total_count":1,"installations":[{"id":9}]}))
            }
            Endpoint::GitHub(GitHubEndpoint::Repositories {
                installation, page, ..
            }) => {
                assert_eq!(installation, 9);
                assert_eq!(page, Page::new(1).unwrap());
                (
                    100,
                    json!({"total_count":2,"repositories":[
                        {"full_name":"owner/other"},{"full_name":"owner/repo"}
                    ]}),
                )
            }
            Endpoint::GitHub(GitHubEndpoint::Issues {
                repo,
                creator,
                since_s,
                page,
                ..
            }) => {
                assert_eq!(repo.as_str(), "owner/repo");
                assert_eq!(creator.as_str(), "owner-sentinel");
                assert_eq!(page, Page::new(1).unwrap());
                if self.receipt.load(Ordering::Acquire) == 0 {
                    assert_eq!(since_s, 99);
                    (120, json!([]))
                } else {
                    assert_eq!(self.receipt.load(Ordering::Acquire), 17);
                    assert_eq!(since_s, 119);
                    (
                        130,
                        json!([{
                            "number":17,"title":"  Local café  ","body":"first\n\tcafé\n",
                            "created_at":"1970-01-01T00:02:01Z"
                        }]),
                    )
                }
            }
            Endpoint::GitHub(GitHubEndpoint::CreateIssue { repo, body, .. }) => {
                assert_eq!(
                    destination,
                    "https://api.github.com/repos/owner/repo/issues"
                );
                assert_eq!(repo.as_str(), "owner/repo");
                assert_eq!(
                    body.expose().bytes_for_test(),
                    r#"{"title":"  Local café  ","body":"first\n\tcafé\n"}"#.as_bytes()
                );
                assert_eq!(self.posts.fetch_add(1, Ordering::SeqCst), 0);
                self.receipt.store(17, Ordering::Release);
                self.posted.notify_one();
                if self.lose_response {
                    return std::future::pending().await;
                }
                return Ok(Response {
                    status: 201,
                    request_id: None,
                    meta: ResponseMeta::default(),
                    body: Secret::new(Zeroizing::new(
                        br#"{"number":17,"html_url":"https://github.com/owner/repo/issues/17"}"#
                            .to_vec(),
                    )),
                });
            }
            _ => panic!("unexpected typed filing endpoint"),
        };
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(Response {
            status: 200,
            request_id: None,
            meta: ResponseMeta {
                date_s: Some(date),
                ..ResponseMeta::default()
            },
            body: Secret::new(Zeroizing::new(serde_json::to_vec(&body).unwrap())),
        })
    }
}

async fn filing_owner(scratch: &Scratch, server: &Arc<FilingServer>) -> (Brain, Action) {
    let action = {
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let thread = store.main_thread().unwrap();
        store
            .create_draft(
                thread,
                &IssueTitle::new("  Local café  ").unwrap(),
                &IssueBody::new("first\r\n\tcafé\r\n").unwrap(),
                Some(&RepoName::new("owner/repo").unwrap()),
                Some(&GitHubUser {
                    id: 7,
                    login: GitHubLogin::new("owner-sentinel").unwrap(),
                }),
            )
            .unwrap()
    };
    assert_eq!(action.title.as_str(), "  Local café  ");
    assert_eq!(action.body.as_str(), "first\n\tcafé\n");
    assert_eq!(action.version, StateVersion(1));
    assert_eq!(action.approval, Some(ApprovalRev(1)));
    let mut net = crate::test_support::net();
    net.clock = Arc::new(SystemClock);
    let http = Arc::clone(server);
    net.http = Arc::new(move || {
        http.factories.fetch_add(1, Ordering::SeqCst);
        Ok(SharedHttp::new(Arc::clone(&http)))
    });
    let mut accounts = owner_accounts(scratch, ObservedStore::default(), net);
    accounts.github.enable();
    accounts.github_session(Some(1));
    saving_owner(&mut accounts).await;
    settle_owner(&mut accounts).await;
    let generation = accounts.github.machine().stamp().generation;
    drive(
        &mut accounts,
        GitHubInput::Repos {
            generation,
            result: Ok(Repositories {
                names: Bounded::new(vec![
                    RepoName::new("owner/other").unwrap(),
                    RepoName::new("owner/repo").unwrap(),
                ])
                .unwrap(),
            }),
        },
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    (brain_with_github_owner(scratch, accounts), action)
}

async fn open_filing(shell: &mut Shell<DuplexStream>) {
    assert_eq!(
        shell.control().await,
        Some(ControlToShell::GitHub(GitHubView::Connected {
            login: GitHubLogin::new("owner-sentinel").unwrap(),
            repos: Bounded::new(vec![
                RepoName::new("owner/other").unwrap(),
                RepoName::new("owner/repo").unwrap(),
            ])
            .unwrap(),
            truncated: false,
            default: None,
        }))
    );
    assert!(shell.scenes().await.is_empty());
}

#[tokio::test]
async fn wire_file_commits_one_literal_post_and_replayed_consent_cannot_send_again() {
    let scratch = Scratch::new("github-filing-wire-file");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
    let consent = ControlToBrain::Issue {
        request: 101,
        op: IssueOp::Consent {
            draft: action.draft,
            hash: action.hash(),
            approval: ApprovalRev(1),
            decision: Decision::File,
        },
    };
    let filed = Some(ControlToShell::ActionState {
        request: Some(101),
        draft: action.draft,
        version: StateVersion(4),
        state: ActionState::Filed {
            number: 17,
            url: IssueUrl::new("https://github.com/owner/repo/issues/17").unwrap(),
            confirmed_by_user: false,
            maybe_duplicate: false,
        },
        approval: None,
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        shell.send(&consent).await;
        for version in [2, 3] {
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::ActionState {
                    request: None,
                    draft: action.draft,
                    version: StateVersion(version),
                    state: ActionState::Sending,
                    approval: None,
                })
            );
        }
        assert_eq!(shell.control().await, filed);
        shell.send(&consent).await;
        assert_eq!(shell.control().await, filed);
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    let ((control, data), mut replacement) = duplex();
    let client = async {
        open_filing(&mut replacement).await;
        replacement.send(&consent).await;
        assert_eq!(replacement.control().await, filed);
        replacement.control_out.shutdown().await.unwrap();
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let stored = Store::open(&scratch.db())
        .unwrap()
        .action(action.draft)
        .unwrap();
    assert_eq!(
        stored,
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
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    assert_eq!(server.receipt.load(Ordering::Acquire), 17);
}

#[tokio::test]
async fn wire_eof_during_held_consent_consumes_approval_and_records_known_unsent_failure() {
    let scratch = Scratch::new("github-filing-wire-admission-eof");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = filing_owner(&scratch, &server).await;
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
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        shell
            .send(&ControlToBrain::Issue {
                request: 301,
                op: IssueOp::Consent {
                    draft: action.draft,
                    hash: action.hash(),
                    approval: ApprovalRev(1),
                    decision: Decision::File,
                },
            })
            .await;
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
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
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
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one wire regression compares cancellation before and after real POST handoff"
)]
async fn wire_eof_before_handoff_is_failed_but_a_lost_post_stays_unknown_without_retry() {
    for after_handoff in [false, true] {
        let scratch = Scratch::new("github-filing-wire-eof");
        let server = Arc::new(FilingServer {
            lose_response: after_handoff,
            ..FilingServer::default()
        });
        let (mut brain, action) = filing_owner(&scratch, &server).await;
        let mut observed = brain.observe();
        let release = (!after_handoff).then(|| {
            brain.hold_store(|op| {
                matches!(
                    op,
                    StoreOp::Issue {
                        write: Write::Preflighted { .. },
                        ..
                    }
                )
            })
        });
        let mark = Mark {
            at_s: NonZeroU64::new(120).unwrap(),
            presend: Bounded::default(),
        };
        let consent = ControlToBrain::Issue {
            request: if after_handoff { 201 } else { 200 },
            op: IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: ApprovalRev(1),
                decision: Decision::File,
            },
        };
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            open_filing(&mut shell).await;
            shell.send(&consent).await;
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::ActionState {
                    request: None,
                    draft: action.draft,
                    version: StateVersion(2),
                    state: ActionState::Sending,
                    approval: None,
                })
            );
            if after_handoff {
                timeout(PATIENCE, server.posted.notified()).await.unwrap();
                assert_eq!(
                    shell.control().await,
                    Some(ControlToShell::ActionState {
                        request: None,
                        draft: action.draft,
                        version: StateVersion(3),
                        state: ActionState::Sending,
                        approval: None,
                    })
                );
            } else {
                timeout(PATIENCE, async {
                    loop {
                        if matches!(
                            observed.recv().await.unwrap(),
                            Seen::StoreOpStarted(StoreOp::Issue {
                                write: Write::Preflighted { .. },
                                ..
                            })
                        ) {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
            let stored = Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap();
            assert_eq!(
                stored,
                Some(Action {
                    version: StateVersion(if after_handoff { 3 } else { 2 }),
                    stage: if after_handoff {
                        Stage::Posting {
                            mark: mark.clone(),
                            unresolved: None,
                        }
                    } else {
                        Stage::Preflight { unresolved: None }
                    },
                    approval: None,
                    consumed: Some(ApprovalRev(1)),
                    ..action.clone()
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
        if let Some(release) = release {
            release.send(()).unwrap();
        }
        assert!(
            timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
                .await
                .unwrap()
        );
        let mut settled = Action {
            version: StateVersion(if after_handoff { 4 } else { 3 }),
            stage: if after_handoff {
                Stage::Unknown {
                    mark: mark.clone(),
                    lookup: LookupResult::Pending,
                }
            } else {
                Stage::Failed {
                    reason: kumochi_protocol::message::FailReason::Interrupted,
                }
            },
            approval: Some(ApprovalRev(2)),
            consumed: Some(ApprovalRev(1)),
            uncertain: u32::from(after_handoff),
            ..action.clone()
        };
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(settled.clone())
        );
        let terminal = Some(ControlToShell::ActionState {
            request: Some(if after_handoff { 201 } else { 200 }),
            draft: action.draft,
            version: settled.version,
            state: if after_handoff {
                ActionState::Unknown {
                    lookup: LookupResult::Pending,
                    maybe_duplicate: false,
                }
            } else {
                ActionState::Failed(kumochi_protocol::message::FailReason::Interrupted)
            },
            approval: Some(ApprovalRev(2)),
        });
        let ((control, data), mut replacement) = duplex();
        let client = async {
            open_filing(&mut replacement).await;
            replacement.send(&consent).await;
            assert_eq!(replacement.control().await, terminal);
            if after_handoff {
                replacement
                    .send(&ControlToBrain::Issue {
                        request: 202,
                        op: IssueOp::CheckAgain {
                            draft: action.draft,
                        },
                    })
                    .await;
                assert_eq!(
                    replacement.control().await,
                    Some(ControlToShell::ActionState {
                        request: None,
                        draft: action.draft,
                        version: StateVersion(4),
                        state: ActionState::Unknown {
                            lookup: LookupResult::Pending,
                            maybe_duplicate: false
                        },
                        approval: Some(ApprovalRev(2)),
                    })
                );
                let candidate = LookupResult::Unplaced {
                    number: 17,
                    url: IssueUrl::new("https://github.com/owner/repo/issues/17").unwrap(),
                };
                let candidate_state = |request| {
                    Some(ControlToShell::ActionState {
                        request,
                        draft: action.draft,
                        version: StateVersion(5),
                        state: ActionState::Unknown {
                            lookup: candidate.clone(),
                            maybe_duplicate: false,
                        },
                        approval: Some(ApprovalRev(3)),
                    })
                };
                assert_eq!(replacement.control().await, candidate_state(Some(202)));
                replacement
                    .send(&ControlToBrain::Issue {
                        request: 203,
                        op: IssueOp::CheckAgain {
                            draft: action.draft,
                        },
                    })
                    .await;
                assert_eq!(replacement.control().await, candidate_state(None));
                assert_eq!(replacement.control().await, candidate_state(Some(203)));
                settled.version = StateVersion(5);
                settled.stage = Stage::Unknown {
                    mark: mark.clone(),
                    lookup: candidate,
                };
                settled.approval = Some(ApprovalRev(3));
            }
            replacement.control_out.shutdown().await.unwrap();
            assert_eq!(replacement.control().await, None);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(action.draft)
                .unwrap(),
            Some(settled)
        );
        assert_eq!(
            server.factories.load(Ordering::SeqCst),
            if after_handoff { 3 } else { 1 }
        );
        assert_eq!(
            server.gets.load(Ordering::SeqCst),
            if after_handoff { 12 } else { 4 }
        );
        assert_eq!(
            server.posts.load(Ordering::SeqCst),
            usize::from(after_handoff)
        );
        assert_eq!(
            server.receipt.load(Ordering::Acquire),
            if after_handoff { 17 } else { 0 }
        );
    }
}
