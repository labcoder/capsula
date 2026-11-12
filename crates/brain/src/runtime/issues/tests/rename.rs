use super::*;
use crate::coordinator::{Effect, Input};
use crate::issues::Answer as IssueAnswer;
use kumochi_protocol::message::IssueRefused;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real lost POST must block a case-renamed duplicate before and after dismissal"
)]
async fn a_case_only_repository_change_cannot_send_an_unknown_or_dismissed_issue_again() {
    for dismissed in [false, true] {
        let scratch = Scratch::new("github-filing-case-renamed-duplicate");
        let server = Arc::new(FilingServer {
            lose_response: true,
            ..FilingServer::default()
        });
        let (mut brain, original) = reviewed_filing_owner(&scratch, &server).await;
        let (posting, _) = super::authority::posting_acknowledgement(&mut brain, &original).await;
        let effects = brain.handle(posting).unwrap();
        brain.apply(effects, None).unwrap();
        timeout(PATIENCE, server.posted.notified()).await.unwrap();
        assert!(
            timeout(PATIENCE, brain.finish_filing(Instant::now() + PATIENCE))
                .await
                .unwrap()
        );
        let unknown = Action {
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
            ..original
        };
        assert_eq!(
            Store::open(&scratch.db())
                .unwrap()
                .action(unknown.draft)
                .unwrap(),
            Some(unknown.clone())
        );
        let retained = if dismissed {
            let effects = brain
                .issue_op(
                    brain.issue_session().unwrap(),
                    1001,
                    &IssueOp::Discard {
                        draft: unknown.draft,
                    },
                    Some(brain.epoch),
                )
                .unwrap();
            brain.apply(effects, None).unwrap();
            let saved = timeout(PATIENCE, brain.inputs.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                brain.handle(saved).unwrap(),
                vec![Effect::Control(ControlToShell::ActionState {
                    request: Some(1001),
                    draft: unknown.draft,
                    version: StateVersion(6),
                    state: ActionState::Discarded { maybe_filed: true },
                    approval: None,
                })]
            );
            Action {
                version: StateVersion(6),
                stage: Stage::Discarded,
                approval: None,
                ..unknown
            }
        } else {
            unknown
        };
        let current = RepoName::new("Owner/Repo").unwrap();
        let generation = brain.account.github.machine().stamp().generation;
        drive(
            &mut brain.account,
            GitHubInput::Repos {
                generation,
                result: Ok(Repositories {
                    names: Bounded::new(vec![current.clone()]).unwrap(),
                }),
            },
        );
        assert_eq!(
            brain.account.github.issue_snapshot().repos,
            vec![current.clone()]
        );
        let second = {
            let mut store = Store::open(&scratch.db()).unwrap();
            store
                .create_draft(
                    retained.thread,
                    &retained.title,
                    &retained.body,
                    Some(&current),
                    retained.account.as_ref(),
                )
                .unwrap()
        };
        assert_eq!(retained.repo.as_ref().unwrap().as_str(), "owner/repo");
        assert_eq!(second.repo.as_ref().unwrap().as_str(), "Owner/Repo");
        assert_eq!(second.account.as_ref().unwrap().id, 7);
        assert_eq!(second.account, retained.account);
        assert_ne!(retained.hash(), second.hash());
        assert_eq!(second.approval, Some(ApprovalRev(4)));
        let second = rearm_filing(&scratch, &mut brain, &second, 1002).await;
        assert_eq!(second.version, StateVersion(2));
        assert_eq!(second.approval, Some(ApprovalRev(5)));
        let consent = IssueOp::Consent {
            draft: second.draft,
            hash: second.hash(),
            approval: ApprovalRev(5),
            decision: Decision::File,
        };
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                1003,
                &consent,
                Some(brain.epoch),
            )
            .unwrap();
        brain.apply(effects, None).unwrap();
        let saved = timeout(PATIENCE, brain.inputs.recv())
            .await
            .unwrap()
            .unwrap();
        let refused = Action {
            version: StateVersion(3),
            approval: Some(ApprovalRev(6)),
            consumed: Some(ApprovalRev(5)),
            ..second.clone()
        };
        assert!(matches!(
            &saved,
            Input::IssueWritten {
                reply: Ok(IssueAnswer::Refused(IssueRefused::UnresolvedDraft { draft }, action)),
                ..
            } if *draft == retained.draft && action == &refused
        ));
        let terminal = ControlToShell::ActionState {
            request: Some(1003),
            draft: second.draft,
            version: StateVersion(3),
            state: ActionState::Draft,
            approval: Some(ApprovalRev(6)),
        };
        assert_eq!(
            brain.handle(saved).unwrap(),
            vec![
                Effect::Control(terminal),
                Effect::Control(ControlToShell::IssueRefused {
                    request: 1003,
                    draft: second.draft,
                    version: StateVersion(3),
                    approval: Some(ApprovalRev(6)),
                    reason: IssueRefused::UnresolvedDraft {
                        draft: retained.draft
                    },
                }),
            ]
        );
        assert!(!brain.filing.pending());
        let effects = brain
            .issue_op(
                brain.issue_session().unwrap(),
                1003,
                &consent,
                Some(brain.epoch),
            )
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::Control(ControlToShell::IssueUnavailable {
                kind: IssueUnavailableKind::Action,
                request: 1003,
                draft: second.draft,
            })]
        );
        brain.apply(effects, None).unwrap();
        assert!(!brain.filing.pending());
        assert_eq!(server.factories.load(Ordering::SeqCst), 1);
        assert_eq!(server.gets.load(Ordering::SeqCst), 4);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_eq!(server.receipt.load(Ordering::Acquire), 17);
        brain.close().await;
        let store = Store::open(&scratch.db()).unwrap();
        assert_eq!(store.action(retained.draft).unwrap(), Some(retained));
        assert_eq!(store.action(second.draft).unwrap(), Some(refused));
    }
}
