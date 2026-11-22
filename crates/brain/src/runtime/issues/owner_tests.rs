use super::*;
use crate::config::tuning::GitHubTuning;
use kumochi_protocol::message::{
    ApprovalRev, GitHubLogin, IssueBody, IssueTitle, RepoName, StateVersion,
};
use kumochi_scene::text::Bounded;
use std::num::NonZeroU64;

#[tokio::test]
async fn failed_request_identities_are_fenced_at_zero_and_max_until_the_session_changes() {
    let scratch = crate::test_support::Scratch::new("github-filing-request-watermark");
    let mut brain = crate::test_support::brain(&scratch, "");
    let session = brain.prepare_issue_session(Incarnation(1));
    for request in [0, u64::MAX] {
        let effects = brain
            .issue_op(
                session,
                request,
                &IssueOp::Discard { draft: DraftId(1) },
                Some(brain.epoch),
            )
            .unwrap();
        let [Effect::Store(StoreOp::Issue { serial, .. })] = effects.as_slice() else {
            panic!("a new request reaches the ordered store exactly once");
        };
        assert_eq!(
            brain.issue_written(*serial, Err(())),
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request,
                draft: DraftId(1),
            })]
        );
        assert_eq!(
            brain.issue_op(
                session,
                request,
                &IssueOp::CheckAgain { draft: DraftId(2) },
                Some(brain.epoch)
            ),
            Some(vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request,
                draft: DraftId(2),
            })])
        );
        if request == u64::MAX {
            assert_eq!(
                brain.issue_op(
                    session,
                    request - 1,
                    &IssueOp::Discard { draft: DraftId(3) },
                    Some(brain.epoch)
                ),
                Some(vec![Effect::Control(ControlToShell::IssueUnavailable {
                    kind: IssueUnavailableKind::Action,
                    request: u64::MAX - 1,
                    draft: DraftId(3),
                })])
            );
        }
        assert_eq!(brain.filing.deadline_for_test(), None);
        assert!(!brain.filing.pending());
    }
    let replacement = brain.prepare_issue_session(Incarnation(1));
    assert_eq!(replacement.epoch, session.epoch + 1);
    for stale in [
        session,
        IssueSession {
            incarnation: Incarnation(2),
            ..replacement
        },
    ] {
        assert_eq!(
            brain.issue_op(
                stale,
                u64::MAX,
                &IssueOp::Discard { draft: DraftId(3) },
                Some(brain.epoch)
            ),
            None
        );
    }
    let effects = brain
        .issue_op(
            replacement,
            0,
            &IssueOp::Discard { draft: DraftId(1) },
            Some(brain.epoch),
        )
        .unwrap();
    let [Effect::Store(StoreOp::Issue { serial, .. })] = effects.as_slice() else {
        panic!("only fresh provenance allows the replacement's restarted request counter");
    };
    assert_eq!(
        brain.issue_written(*serial, Err(())),
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 0,
            draft: DraftId(1)
        })]
    );
    assert_eq!(brain.filing.deadline_for_test(), None);
    assert!(!brain.filing.pending());
    brain.close().await;
}

#[tokio::test]
async fn an_old_sessions_authoritative_action_survives_without_its_request_or_refusal() {
    let scratch = crate::test_support::Scratch::new("github-filing-old-session-notices");
    let action = {
        let mut store = crate::store::Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let thread = store.main_thread().unwrap();
        store
            .create_draft(
                thread,
                &IssueTitle::new("local issue").unwrap(),
                &IssueBody::new("saved body").unwrap(),
                None,
                None,
            )
            .unwrap()
    };
    let mut brain = crate::test_support::brain(&scratch, "");
    let old = brain.epoch;
    brain.epoch += 1;
    for answer in [
        Answer::Changed(action.clone()),
        Answer::Unchanged(action.clone()),
        Answer::Refused(IssueRefused::NotAllowedNow, action.clone()),
    ] {
        assert_eq!(
            brain.issue_notice(
                old,
                Some(19),
                action.draft,
                &answer,
                Some(IssueRefused::NotConnected)
            ),
            vec![Effect::Control(ControlToShell::ActionState {
                request: None,
                draft: action.draft,
                version: StateVersion(1),
                state: kumochi_protocol::message::ActionState::Draft,
                approval: Some(ApprovalRev(1)),
            })]
        );
    }
    assert_eq!(
        brain.issue_notice(
            old,
            Some(19),
            action.draft,
            &Answer::Gone,
            Some(IssueRefused::NotConnected)
        ),
        Vec::new()
    );
    assert_eq!(brain.issue_unavailable(old, 19, action.draft), Vec::new());
    brain.close().await;
}

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
        draft: action.draft,
        stamp: brain.account.github.machine().stamp(),
        check: true,
        control: Control::new(GitHubTuning::default()),
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
    let draft = action.draft;
    let state = kumochi_protocol::message::ActionState::Unknown {
        lookup: kumochi_protocol::message::LookupResult::Pending,
        maybe_duplicate: false,
    };
    assert_eq!(
        brain.issue_written(21, Ok(Answer::Unchanged(action))),
        vec![
            Effect::Control(ControlToShell::ActionState {
                request: None,
                draft,
                version: StateVersion(4),
                state: state.clone(),
                approval: Some(ApprovalRev(2)),
            }),
            Effect::Control(ControlToShell::ActionState {
                request: Some(202),
                draft,
                version: StateVersion(4),
                state,
                approval: Some(ApprovalRev(2)),
            }),
            Effect::Control(ControlToShell::IssueRefused {
                request: 202,
                draft,
                version: StateVersion(4),
                approval: Some(ApprovalRev(2)),
                reason: IssueRefused::NotConnected,
            }),
        ]
    );
    assert_eq!(brain.issue_written(21, Err(())), Vec::new());
    brain.close().await;
}
