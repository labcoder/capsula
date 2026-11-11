use super::*;
use kumochi_protocol::message::{
    Decision, ErrorKey, GitHubOp, IssueBody, IssueOp, IssueTitle, RepoName,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn operations() -> [GitHubOp; 9] {
    [
        GitHubOp::Connect,
        GitHubOp::CancelConnect,
        GitHubOp::SignOut,
        GitHubOp::RetryCleanup,
        GitHubOp::RefreshRepos,
        GitHubOp::SetDefaultRepo(RepoName::new("sentinel/private-repository").unwrap()),
        GitHubOp::Connect,
        GitHubOp::CancelConnect,
        GitHubOp::SignOut,
    ]
}

fn counts(store: &Store) -> (i64, i64) {
    store
        .conn_for_tests()
        .query_row(
            "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM events)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

struct RejectLaunch(Arc<AtomicUsize>);

impl crate::connections::oauth::Launcher for RejectLaunch {
    fn launch(
        &self,
        _: &crate::connections::oauth::LaunchTarget,
    ) -> Result<(), crate::connections::oauth::LaunchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::oauth::LaunchError::Launch)
    }
}

#[tokio::test]
async fn live_refusals_and_offline_controls_never_start_work_or_change_approval() {
    let scratch = Scratch::new("github-refusal");
    let (action, before) = {
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let thread = store.main_thread().unwrap();
        let action = store
            .create_draft(
                thread,
                &IssueTitle::new("saved title").unwrap(),
                &IssueBody::new("saved body").unwrap(),
                None,
                None,
            )
            .unwrap();
        (action, counts(&store))
    };
    let mut brain = patient_brain(&scratch, "");
    let (jobs, queued) = std::sync::mpsc::channel();
    brain.account.jobs = Some(jobs);
    let attempts = Arc::new(AtomicUsize::new(0));
    let http_attempts = Arc::clone(&attempts);
    brain.account.net.http = Arc::new(move || {
        http_attempts.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::http_client::HttpError::Configuration)
    });
    brain.account.net.launcher = Arc::new(RejectLaunch(Arc::clone(&attempts)));
    let issue = ControlToBrain::Issue(IssueOp::Consent {
        draft: action.draft,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
    });
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert!(shell.scenes().await.is_empty());
        shell.send(&issue).await;
        for op in operations() {
            shell.send(&ControlToBrain::GitHub(op)).await;
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::GitHubError(
                    ErrorKey::GitHubUnavailable.into()
                ))
            );
        }
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    for op in operations() {
        brain.control_offline(ControlToBrain::GitHub(op), 1, &mut tap);
    }
    brain.control_offline(issue, 1, &mut tap);
    let ((control, data), mut replacement) = duplex();
    let client = async {
        assert!(replacement.scenes().await.is_empty());
        replacement.control_out.shutdown().await.unwrap();
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    brain.close().await;
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    let store = Store::open(&scratch.db()).unwrap();
    assert_eq!(store.action(action.draft).unwrap(), Some(action));
    assert_eq!(counts(&store), before);
}

#[tokio::test]
async fn one_slot_refusal_output_is_exact_and_full_or_closed_output_loses_the_connection() {
    let scratch = Scratch::new("github-output-full");
    let mut brain = patient_brain(&scratch, "");
    let (out, mut frames) = mpsc::channel(1);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let mut ping = Some(tokio::time::Instant::now());
    let was_ping = ping;
    assert!(matches!(
        brain.control_frame(
            ControlToBrain::GitHub(GitHubOp::Connect),
            1,
            &mut link,
            &mut ping
        ),
        Ok(None)
    ));
    assert!(matches!(
        brain.control_frame(
            ControlToBrain::GitHub(GitHubOp::SignOut),
            1,
            &mut link,
            &mut ping
        ),
        Err(Ended::Lost)
    ));
    assert_eq!(frames.try_recv().unwrap(), [4, 0, 0, 0, 25, 42, 0, 0]);
    assert!(matches!(
        frames.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(frames);
    assert!(matches!(
        brain.control_frame(
            ControlToBrain::GitHub(GitHubOp::CancelConnect),
            1,
            &mut link,
            &mut ping
        ),
        Err(Ended::Lost)
    ));
    assert_eq!(ping, was_ping);
    brain.close().await;
}

use crate::runtime::account::github_tests::{
    ObservedStore, UserHttp, active_github, owner_accounts, owner_lock, saving_owner, settle_owner,
    start_user_request, user_response, verification_accounts,
};
use crate::runtime::{Full, Selected};
use tokio::time::Instant;

fn brain_with_github_owner(scratch: &Scratch, account: crate::runtime::account::Accounts) -> Brain {
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    let main = store.main_thread().unwrap();
    let high = store.high_water().unwrap();
    let scenes = store.scenes().unwrap();
    Brain::new(
        store,
        main,
        high,
        scenes,
        Selected::Fixture(Box::new(Fixture::parse("").unwrap())),
        scratch.config(),
        account,
    )
    .unwrap()
}

#[tokio::test]
async fn production_off_is_observed_before_account_startup_and_survives_all_sessions() {
    let scratch = Scratch::new("github-owner-production-off");
    let store = ObservedStore::default();
    store.seed(&scratch);
    let attempts = Arc::new(AtomicUsize::new(0));
    let http = Arc::clone(&attempts);
    let mut net = crate::test_support::net();
    net.http = Arc::new(move || {
        http.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::http_client::HttpError::Configuration)
    });
    net.launcher = Arc::new(RejectLaunch(Arc::clone(&attempts)));
    let accounts = owner_accounts(&scratch, store.clone(), net);
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let mut tap = Tap::from_env();
    for _ in 0..2 {
        let ((control, data), mut shell) = duplex();
        let client = async {
            assert!(shell.scenes().await.is_empty());
            for op in operations() {
                shell.send(&ControlToBrain::GitHub(op)).await;
                assert_eq!(
                    shell.control().await,
                    Some(ControlToShell::GitHubError(
                        ErrorKey::GitHubUnavailable.into()
                    ))
                );
            }
            shell.control_out.shutdown().await.unwrap();
            assert_eq!(shell.control().await, None);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        for op in operations() {
            brain.control_offline(ControlToBrain::GitHub(op), 1, &mut tap);
        }
        assert!(brain.account.github.machine().is_loading());
        assert_eq!(brain.account.github.suspended().len(), 0);
        assert!(!brain.account.github.pending());
        assert_eq!(brain.account.github_opened(), Vec::<ControlToShell>::new());
    }
    brain.close().await;
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(active_github(&scratch), 1);
}

#[tokio::test]
async fn eof_and_close_preserve_an_already_connected_credential_and_only_publish_current_state() {
    use crate::connections::github::device::Effect as GitHubEffect;
    use kumochi_protocol::message::GitHubView;
    let scratch = Scratch::new("github-owner-eof");
    let store = ObservedStore::default();
    let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
    accounts.github.enable();
    accounts.github_session(Some(1));
    saving_owner(&mut accounts).await;
    settle_owner(&mut accounts).await;
    assert_eq!(accounts.github.machine().user().unwrap().id, 7);
    let snapshot = accounts.github.machine().view(Instant::now());
    let GitHubView::Connected {
        ref repos,
        truncated,
        default: ref selected,
        ..
    } = snapshot
    else {
        panic!("connected after the real fake-vault save")
    };
    assert_eq!(repos.len(), 0);
    assert!(!truncated);
    assert_eq!(*selected, None);
    let stamp = accounts.github.machine().stamp();
    assert!(
        matches!(accounts.github.suspended(), [(owner, GitHubEffect::RefreshRepos { generation })] if *owner == stamp && *generation == stamp.generation)
    );
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let mut tap = Tap::from_env();
    for _ in 0..2 {
        let ((control, data), mut shell) = duplex();
        let client = async {
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::GitHub(snapshot.clone()))
            );
            assert!(shell.scenes().await.is_empty());
            shell.control_out.shutdown().await.unwrap();
            assert_eq!(shell.control().await, None);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert_eq!(brain.account.github.machine().stamp(), stamp);
        assert_eq!(brain.account.github.machine().user().unwrap().id, 7);
        assert_eq!(active_github(&scratch), 1);
    }
    let fence = brain.account.github.machine().fence();
    brain.close().await;
    assert_eq!(fence.load(Ordering::Acquire), stamp.generation);
    assert_eq!(active_github(&scratch), 1);
    drop(
        crate::connections::vault::Vault::with_service(
            store,
            "github-owner-test",
            &owner_lock(&scratch),
        )
        .unwrap(),
    );
}

#[tokio::test]
async fn eof_queues_compensation_and_close_waits_for_the_held_save_and_worker_release() {
    use kumochi_protocol::message::{Cleanup, GitHubView};
    let scratch = Scratch::new("github-owner-close-held-save");
    let store = ObservedStore::default();
    let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
    accounts.github.enable();
    accounts.github_session(Some(1));
    let (entered, release) = store.hold(true);
    saving_owner(&mut accounts).await;
    entered
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(shell.scenes().await.is_empty());
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert!(brain.account.github.pending());
    assert_eq!(
        brain.account.github.machine().view(Instant::now()),
        GitHubView::SignedOut {
            cleanup: Some(Cleanup::Running)
        }
    );
    let closing = brain.close();
    tokio::pin!(closing);
    tokio::select! {
        biased;
        () = &mut closing => panic!("a held admitted write cannot be discarded"),
        () = tokio::task::yield_now() => {}
    }
    release.send(()).unwrap();
    timeout(PATIENCE, &mut closing).await.unwrap();
    assert_eq!(active_github(&scratch), 0);
    assert!(
        !store
            .store
            .names()
            .iter()
            .any(|name| name.contains("/gen/github/"))
    );
    drop(
        crate::connections::vault::Vault::with_service(
            store,
            "github-owner-test",
            &owner_lock(&scratch),
        )
        .unwrap(),
    );
}

#[tokio::test]
async fn failed_live_output_does_not_stop_a_refused_save_from_queuing_compensation() {
    let scratch = Scratch::new("github-owner-output-lost");
    let store = ObservedStore::default();
    let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
    accounts.github.enable();
    accounts.github_session(Some(1));
    store.store.0.lock().unwrap().refusing = true;
    saving_owner(&mut accounts).await;
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let (out, mut frames) = mpsc::channel(1);
    out.try_send(Vec::new()).unwrap();
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let event = timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            if matches!(
                event,
                crate::runtime::account::Event::GitHub {
                    reply: crate::connections::github::lane::Reply::Saved { saved: false, .. },
                    ..
                }
            ) {
                break event;
            }
            assert!(brain.account_input(event, None).is_ok());
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        brain.account_input(event, Some(&mut link)),
        Err(Full)
    ));
    assert!(brain.account.github.pending());
    assert!(brain.account.github.machine().user().is_none());
    assert_eq!(frames.try_recv().unwrap(), Vec::<u8>::new());
    drop(frames);
    settle_owner(&mut brain.account).await;
    store.store.0.lock().unwrap().refusing = false;
    brain.close().await;
    assert_eq!(active_github(&scratch), 0);
}

#[tokio::test]
async fn a_loaded_user_request_survives_eof_and_settles_through_idle_before_replacement() {
    use crate::connections::http_client::ResponseMeta;
    use kumochi_protocol::message::GitHubView;
    let scratch = Scratch::new("github-user-idle-replacement");
    let http = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":11,"login":"fresh"}"#,
        ResponseMeta::default(),
    )));
    let release = http.hold();
    let (mut accounts, _) = verification_accounts(&scratch, http.factory(), Some(1));
    start_user_request(&mut accounts).await;
    http.entered.notified().await;
    let fence = accounts.github.machine().fence();
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(shell.scenes().await.is_empty());
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(http.active.load(Ordering::SeqCst), 1);
    assert!(brain.account.github.verifying());
    release.send(()).unwrap();
    timeout(PATIENCE, async {
        tokio::select! {
            () = brain.idle() => panic!("idle only ends when dropped"),
            () = async { while fence.load(Ordering::Acquire) == 0 { tokio::task::yield_now().await; } } => {}
        }
    }).await.unwrap();
    assert_eq!(brain.account.github.machine().user().unwrap().id, 11);
    assert!(!brain.account.github.verifying());
    let ((control, data), mut replacement) = duplex();
    let client = async {
        assert_eq!(
            replacement.control().await,
            Some(ControlToShell::GitHub(GitHubView::Connected {
                login: kumochi_protocol::message::GitHubLogin::new("fresh").unwrap(),
                repos: kumochi_scene::text::Bounded::default(),
                truncated: false,
                default: None,
            }))
        );
        assert!(replacement.scenes().await.is_empty());
        replacement.control_out.shutdown().await.unwrap();
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    assert_eq!(http.dropped.load(Ordering::SeqCst), 1);
    brain.close().await;
}

#[tokio::test]
async fn an_old_load_origin_error_is_suppressed_after_the_replacement_is_already_attached() {
    use crate::connections::http_client::ResponseMeta;
    use kumochi_protocol::message::{GitHubLogin, GitHubView};
    let scratch = Scratch::new("github-user-old-origin-error");
    let http = UserHttp::new(Ok(user_response(
        403,
        b"provider-sentinel",
        ResponseMeta::default(),
    )));
    let release = http.hold();
    let (mut accounts, _) = verification_accounts(&scratch, http.factory(), Some(1));
    start_user_request(&mut accounts).await;
    http.entered.notified().await;
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(shell.scenes().await.is_empty());
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
        assert_eq!(
            replacement.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(replacement.scenes().await.is_empty());
        release.send(()).unwrap();
        assert_eq!(
            replacement.control().await,
            Some(ControlToShell::GitHub(GitHubView::Connected {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
                repos: kumochi_scene::text::Bounded::default(),
                truncated: false,
                default: None,
            }))
        );
        replacement.control_out.shutdown().await.unwrap();
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.account.github.machine().user().unwrap().id, 7);
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    brain.close().await;
}
