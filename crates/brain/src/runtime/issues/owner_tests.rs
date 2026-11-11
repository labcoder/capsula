use super::*;
use kumochi_protocol::message::{
    ApprovalRev, GitHubLogin, IssueBody, IssueTitle, RepoName, StateVersion,
};
use kumochi_scene::text::Bounded;
use std::num::NonZeroU64;

#[tokio::test]
async fn an_older_lookup_cannot_complete_a_new_job_in_the_same_uncertain_episode() {
    let scratch = crate::test_support::Scratch::new("github-filing-stale-lookup-owner");
    let mut action = {
        let mut store = crate::store::Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let thread = store.main_thread().unwrap();
        store
            .create_draft(
                thread,
                &IssueTitle::new("local issue").unwrap(),
                &IssueBody::new("saved body").unwrap(),
                Some(&RepoName::new("owner/repo").unwrap()),
                Some(&crate::connections::github::api::GitHubUser {
                    id: 7,
                    login: GitHubLogin::new("owner").unwrap(),
                }),
            )
            .unwrap()
    };
    action.version = StateVersion(4);
    action.approval = Some(ApprovalRev(2));
    action.consumed = Some(ApprovalRev(1));
    action.uncertain = 1;
    action.stage = Stage::Unknown {
        mark: crate::issues::Mark {
            at_s: NonZeroU64::new(120).unwrap(),
            presend: Bounded::default(),
        },
        lookup: kumochi_protocol::message::LookupResult::Pending,
    };
    let mut brain = crate::test_support::brain(&scratch, "");
    brain.filing.job = Some(Job {
        id: 20,
        epoch: brain.epoch,
        request: 202,
        stamp: brain.account.github.machine().stamp(),
        check: true,
        control: Control::new(),
        admission: None,
        action: Some(action.clone()),
        token: None,
        prepared: None,
        phase: Phase::Store {
            serial: 21,
            after: AfterWrite::Read,
        },
    });
    assert_eq!(
        brain.issue_event(Event {
            id: 19,
            outcome: Outcome::Checked(Ok(Lookup::None))
        }),
        Vec::new()
    );
    assert_eq!(
        brain.issue_written(18, Ok(Answer::Unchanged(action.clone()))),
        Vec::new()
    );
    let retained = brain.filing.job.take().unwrap();
    assert_eq!(retained.id, 20);
    assert_eq!(retained.request, 202);
    assert_eq!(retained.action, Some(action));
    assert!(matches!(
        retained.phase,
        Phase::Store {
            serial: 21,
            after: AfterWrite::Read
        }
    ));
    assert!(!retained.control.cancelled());
    brain.close().await;
}
