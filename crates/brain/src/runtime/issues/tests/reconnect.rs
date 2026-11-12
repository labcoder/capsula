use super::*;
use kumochi_protocol::message::{IssueCard, IssueCardsStatus};
use std::sync::Mutex;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "two real framed sessions order replacement hydration before the old owner's settlement"
)]
async fn a_replacement_hydrated_while_construction_waits_receives_the_old_settlement() {
    let scratch = Scratch::new("github-filing-replacement-hydrated-sending");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = reviewed_filing_owner(&scratch, &server).await;
    let session = brain.issue_session().unwrap();
    let entered = Arc::new(Notify::new());
    let started = Arc::clone(&entered);
    let http = Arc::clone(&server);
    let (release, resume) = std::sync::mpsc::channel();
    let resume = Mutex::new(resume);
    brain.account.net.http = Arc::new(move || {
        http.factories.fetch_add(1, Ordering::SeqCst);
        started.notify_one();
        resume.lock().unwrap().recv_timeout(PATIENCE).unwrap();
        Ok(SharedHttp::new(Arc::clone(&http)))
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        open_filing(&mut shell).await;
        shell
            .send(&ControlToBrain::Issue {
                session,
                request: 1001,
                op: Box::new(IssueOp::Consent {
                    draft: action.draft,
                    hash: action.hash(),
                    approval: ApprovalRev(2),
                    decision: Decision::File,
                }),
            })
            .await;
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
        timeout(PATIENCE, entered.notified()).await.unwrap();
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert!(brain.filing.pending());
    let original_epoch = brain.epoch;
    brain.prepare_issue_session(Incarnation(1));
    let ((control, data), mut replacement) = duplex();
    let client = async {
        open_filing(&mut replacement).await;
        replacement.credit(64 * 1024).await;
        replacement
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
            replacement.data().await,
            DataToShell::IssueCardRead {
                request: 87,
                card: IssueCard {
                    draft: action.draft,
                    thread: action.thread,
                    repo: Some(RepoName::new("owner/repo").unwrap()),
                    account: Some(GitHubLogin::new("owner-sentinel").unwrap()),
                    title: IssueTitle::new("  Local café  ").unwrap(),
                    body: IssueBody::new("first\n\tcafé\n").unwrap(),
                    hash: action.hash(),
                    approval: None,
                    version: StateVersion(3),
                    state: ActionState::Sending,
                },
            }
        );
        assert_eq!(
            replacement.data().await,
            DataToShell::IssueCardsDone {
                request: 87,
                status: IssueCardsStatus::Complete,
            }
        );
        release.send(()).unwrap();
        assert_eq!(
            replacement.control().await,
            Some(ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(4),
                state: ActionState::Failed(kumochi_protocol::message::FailReason::Interrupted),
                approval: Some(ApprovalRev(3)),
            }),
            "the old request must not accompany its authoritative settled state"
        );
        replacement.control_out.shutdown().await.unwrap();
        assert_eq!(replacement.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("a replacement that hydrated Sending must receive the late settlement");
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.epoch, original_epoch + 1);
    assert!(!brain.filing.pending());
    brain.close().await;
    assert_eq!(
        Store::open(&scratch.db())
            .unwrap()
            .action(action.draft)
            .unwrap(),
        Some(Action {
            version: StateVersion(4),
            stage: Stage::Failed {
                reason: kumochi_protocol::message::FailReason::Interrupted,
            },
            approval: Some(ApprovalRev(3)),
            consumed: Some(ApprovalRev(2)),
            ..action
        })
    );
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    assert_eq!(server.receipt.load(Ordering::Acquire), 0);
}
