use super::*;

#[test]
fn cancelling_before_mint_and_superseding_in_the_peer_view_leave_no_old_authority() {
    let mut f = fixture(ActionState::Draft);
    let mut card = f.models[0]
        .issue(DraftId(9))
        .unwrap()
        .card()
        .unwrap()
        .clone();
    card.version = StateVersion(2);
    card.approval = Some(ApprovalRev(2));
    let rearm = |request| {
        issue(
            request,
            IssueOp::Rearm {
                draft: DraftId(9),
                version: StateVersion(1),
                hash: ContentHash([7; 32]),
            },
        )
    };
    assert_eq!(f.click(0), 1);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    f.escape(0);
    cancelled(&f, 2, 1);
    f.reply(1, card.clone());
    assert!(f.pending[0].is_none());
    assert_eq!(f.click(0), 3);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(3));
    assert_eq!(f.click(1), 5);
    assert_eq!(
        f.controls.try_recv().unwrap(),
        issue(
            4,
            IssueOp::CancelReview {
                draft: DraftId(9),
                review: 3,
            }
        )
    );
    assert_eq!(f.controls.try_recv().unwrap(), rearm(5));
    assert!(f.pending[0].is_none());
    f.reply(3, card.clone());
    f.reply(5, card);
    assert!(f.pending[0].is_none());
    assert!(
        f.pending[1]
            .as_ref()
            .is_some_and(|pending| !pending.awaiting())
    );
    f.decide(1, true, &SHOWN);
    assert_eq!(f.controls.try_recv().unwrap(), payload(6, 2));
    assert!(f.controls.try_recv().is_err());
}
