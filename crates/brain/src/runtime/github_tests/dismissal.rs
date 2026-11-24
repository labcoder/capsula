use super::*;
use crate::connections::github::{api::GitHubUser, device::Input as GitHubInput};
use crate::runtime::account::github_tests::{drive, verifying_owner};
use kumochi_protocol::message::{Cleanup, GitHubLogin, GitHubView};

struct Release(std::sync::mpsc::Sender<()>);

impl Drop for Release {
    fn drop(&mut self) {
        let _released = self.0.send(());
    }
}

#[tokio::test]
async fn wire_cancel_retires_interactive_verification_and_rejects_its_late_identity() {
    let scratch = Scratch::new("github-dismiss-verifying");
    let store = ObservedStore::default();
    let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
    accounts.github.enable();
    accounts.github_session(Some(1));
    let stamp = verifying_owner(&mut accounts).await;
    let fence = accounts.github.machine().fence();
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(shell.scenes().await.is_empty());
        shell
            .send(&ControlToBrain::GitHub(GitHubOp::CancelConnect))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: None,
            }))
        );
        assert_eq!(fence.load(Ordering::Acquire), stamp.generation + 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.account.github.machine().user(), None);
    assert_eq!(
        drive(
            &mut brain.account,
            GitHubInput::User {
                stamp,
                result: Ok(GitHubUser {
                    id: 7,
                    login: GitHubLogin::new("owner-sentinel").unwrap()
                }),
            }
        ),
        Vec::<ControlToShell>::new()
    );
    assert_eq!(
        settle_owner(&mut brain.account).await,
        Vec::<ControlToShell>::new()
    );
    assert_eq!(
        brain.account.github.machine().view(Instant::now()),
        GitHubView::SignedOut { cleanup: None }
    );
    assert_eq!(brain.account.github.machine().user(), None);
    assert_eq!(fence.load(Ordering::Acquire), stamp.generation + 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    brain.close().await;
    assert_eq!(active_github(&scratch), 0);
    assert!(
        !store
            .store
            .names()
            .iter()
            .any(|name| name.contains("/gen/github/"))
    );
}

#[tokio::test]
async fn wire_cancel_compensates_a_held_save_while_the_shell_remains_connected() {
    let scratch = Scratch::new("github-dismiss-saving");
    let store = ObservedStore::default();
    let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
    accounts.github.enable();
    accounts.github_session(Some(1));
    let (entered, release) = store.hold(true);
    let release = Release(release);
    let stamp = saving_owner(&mut accounts).await;
    entered
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let fence = accounts.github.machine().fence();
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::Requesting))
        );
        assert!(shell.scenes().await.is_empty());
        shell
            .send(&ControlToBrain::GitHub(GitHubOp::CancelConnect))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running),
            }))
        );
        assert_eq!(fence.load(Ordering::Acquire), stamp.generation + 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        drop(release);
        loop {
            match shell.control().await {
                Some(ControlToShell::GitHub(GitHubView::SignedOut {
                    cleanup: Some(Cleanup::Running),
                })) => {}
                Some(ControlToShell::GitHub(GitHubView::SignedOut { cleanup: None })) => break,
                other => panic!("cleanup must never publish Connected: {other:?}"),
            }
        }
        assert_eq!(active_github(&scratch), 0);
        assert!(
            !store
                .store
                .names()
                .iter()
                .any(|name| name.contains("/gen/github/"))
        );
        assert_eq!(fence.load(Ordering::Acquire), stamp.generation + 1);
        shell.control_out.shutdown().await.unwrap();
        while let Some(frame) = shell.control().await {
            assert_eq!(
                frame,
                ControlToShell::GitHub(GitHubView::SignedOut { cleanup: None })
            );
        }
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert!(!brain.account.github.pending());
    assert_eq!(brain.account.github.machine().user(), None);
    assert_eq!(
        brain.account.github.machine().view(Instant::now()),
        GitHubView::SignedOut { cleanup: None }
    );
    assert_eq!(fence.load(Ordering::Acquire), stamp.generation + 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 1);
    brain.close().await;
    assert_eq!(active_github(&scratch), 0);
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
async fn an_unobserved_connected_save_is_terminal_for_both_wire_cancel_and_eof() {
    for eof in [false, true] {
        let scratch = Scratch::new("github-dismiss-completed");
        let store = ObservedStore::default();
        let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
        accounts.github.enable();
        accounts.github_session(Some(1));
        saving_owner(&mut accounts).await;
        settle_owner(&mut accounts).await;
        let stamp = accounts.github.machine().stamp();
        let fence = accounts.github.machine().fence();
        let snapshot = GitHubView::ConnectedUnlisted {
            login: GitHubLogin::new("owner-sentinel").unwrap(),
            listing: true,
        };
        assert_eq!(accounts.github.machine().view(Instant::now()), snapshot);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        assert_eq!(active_github(&scratch), 1);
        let names = store.store.names();
        let mut brain = brain_with_github_owner(&scratch, accounts);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            if eof {
                shell.control_out.shutdown().await.unwrap();
                drop(shell);
            } else {
                shell
                    .send(&ControlToBrain::GitHub(GitHubOp::CancelConnect))
                    .await;
                assert_eq!(
                    shell.control().await,
                    Some(ControlToShell::GitHub(snapshot.clone()))
                );
                assert!(shell.scenes().await.is_empty());
                assert_eq!(
                    shell.control().await,
                    Some(ControlToShell::GitHub(snapshot.clone()))
                );
                assert_eq!(active_github(&scratch), 1);
                shell.control_out.shutdown().await.unwrap();
                assert_eq!(shell.control().await, None);
            }
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert_eq!(brain.account.github.machine().stamp(), stamp);
        let user = brain.account.github.machine().user().unwrap();
        assert_eq!(user.id, 7);
        assert_eq!(user.login.as_str(), "owner-sentinel");
        assert_eq!(active_github(&scratch), 1);
        assert_eq!(store.store.names(), names);
        let ((control, data), mut replacement) = duplex();
        let client = async {
            assert_eq!(
                replacement.control().await,
                Some(ControlToShell::GitHub(snapshot.clone()))
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
        assert_eq!(brain.account.github.machine().stamp(), stamp);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        brain.close().await;
        assert_eq!(fence.load(Ordering::Acquire), stamp.generation);
        assert_eq!(active_github(&scratch), 1);
        assert_eq!(store.store.names(), names);
    }
}
