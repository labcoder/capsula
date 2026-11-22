//! OSC-648: a draft keeps the repository id its first preview bound, so a repository recreated
//! under the reviewed name never receives the issue, whether it is listed before the user's File
//! decision or after a refused attempt.

use super::storage::settle;
use super::*;
use crate::coordinator::Effect;
use kumochi_protocol::message::{FailReason, IssueRefused as Refused};

fn relist(brain: &mut Brain, repo_id: u64) {
    let generation = brain.account.github.machine().stamp().generation;
    drive(
        &mut brain.account,
        GitHubInput::Repos {
            generation,
            result: Ok(Repositories {
                names: Bounded::new(vec![
                    RepoName::new("owner/other").unwrap(),
                    RepoName::new("owner/repo").unwrap(),
                ])
                .unwrap(),
                ids: vec![
                    NonZeroU64::new(8).unwrap(),
                    NonZeroU64::new(repo_id).unwrap(),
                ],
            }),
        },
    );
}

/// A reviewed owner whose listing gives the draft's repository the id 7.
async fn bound_owner(scratch: &Scratch, server: &Arc<FilingServer>) -> (Brain, Action) {
    let (mut brain, action) = filing_owner(scratch, server).await;
    relist(&mut brain, 7);
    let bound = Action {
        repo_id: NonZeroU64::new(7),
        ..action
    };
    let action = rearm_filing(scratch, &mut brain, &bound, 0).await;
    (brain, action)
}

fn file(brain: &mut Brain, action: &Action, request: u64) -> Vec<Effect> {
    brain
        .issue_op(
            brain.issue_session().unwrap(),
            request,
            &IssueOp::Consent {
                draft: action.draft,
                hash: action.hash(),
                approval: action.approval.unwrap(),
                decision: Decision::File,
            },
            Some(brain.epoch),
        )
        .unwrap()
}

async fn file_settled(brain: &mut Brain, action: &Action, request: u64) -> Vec<ControlToShell> {
    let effects = file(brain, action, request);
    brain.apply(effects, None).unwrap();
    settle(brain).await
}

fn refused(action: &Action, request: u64) -> Vec<ControlToShell> {
    let version = StateVersion(action.version.0 + 1);
    let approval = Some(ApprovalRev(action.approval.unwrap().0 + 1));
    vec![
        ControlToShell::ActionState {
            request: Some(request),
            draft: action.draft,
            version,
            state: action.state(),
            approval,
        },
        ControlToShell::IssueRefused {
            request,
            draft: action.draft,
            version,
            approval,
            reason: Refused::RepoNotListed,
        },
    ]
}

fn stored(scratch: &Scratch, action: &Action) -> Action {
    Store::open(&scratch.db())
        .unwrap()
        .action(action.draft)
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_replacement_listed_after_the_preview_consumes_the_offer_without_a_post() {
    let scratch = Scratch::new("github-repo-id-replaced-before-file");
    let server = Arc::new(FilingServer::default());
    let (mut brain, action) = bound_owner(&scratch, &server).await;
    relist(&mut brain, 9);
    assert_eq!(
        file_settled(&mut brain, &action, 1).await,
        refused(&action, 1)
    );
    assert_eq!(brain.filing.review_counts(), (0, 0));
    assert_eq!(
        file(&mut brain, &action, 2),
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 2,
            draft: action.draft,
        })]
    );
    let after = stored(&scratch, &action);
    assert_eq!(
        (after.stage, after.repo_id, after.consumed),
        (Stage::Draft, NonZeroU64::new(7), action.approval)
    );
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}

#[tokio::test]
async fn a_refused_replacement_stays_refused_when_the_same_draft_is_reviewed_again() {
    let scratch = Scratch::new("github-repo-id-replaced-then-reviewed");
    let server = Arc::new(FilingServer {
        replaced: true,
        ..FilingServer::default()
    });
    let (mut brain, action) = bound_owner(&scratch, &server).await;
    file_settled(&mut brain, &action, 1).await;
    let failed = stored(&scratch, &action);
    assert_eq!(
        (&failed.stage, failed.repo_id),
        (
            &Stage::Failed {
                reason: FailReason::NotFound
            },
            NonZeroU64::new(7)
        )
    );
    assert_eq!(server.gets.load(Ordering::SeqCst), 3);
    relist(&mut brain, 9);
    let reviewed = rearm_filing(&scratch, &mut brain, &failed, 2).await;
    assert_eq!(reviewed.repo_id, NonZeroU64::new(7));
    assert_eq!(
        file_settled(&mut brain, &reviewed, 3).await,
        refused(&reviewed, 3)
    );
    assert_eq!(stored(&scratch, &action).repo_id, NonZeroU64::new(7));
    assert_eq!(server.gets.load(Ordering::SeqCst), 3);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    brain.close().await;
}
