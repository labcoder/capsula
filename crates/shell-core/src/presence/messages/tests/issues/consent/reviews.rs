use super::*;

fn next_card(f: &Fixture) -> IssueCard {
    let mut card = f.models[0]
        .issue(DraftId(9))
        .unwrap()
        .card()
        .unwrap()
        .clone();
    card.version = StateVersion(2);
    card.approval = Some(ApprovalRev(2));
    card
}

#[test]
fn an_unrelated_or_duplicate_reply_does_not_retire_the_current_review() {
    let mut f = fixture(ActionState::Draft);
    let valid = next_card(&f);
    let mut malformed = valid.clone();
    malformed.approval = None;
    let review = f.click(0);
    f.controls.try_recv().expect("the explicit rearm");

    assert!(!f.receive(0, review + 1, malformed.clone()));
    assert!(f.pending[0].as_ref().unwrap().awaiting());
    assert!(f.controls.try_recv().is_err());
    f.reply(review, valid);
    assert!(!f.pending[0].as_ref().unwrap().awaiting());

    assert!(!f.receive(0, review, malformed));
    f.decide(0, true, &SHOWN);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
    assert!(f.controls.try_recv().is_err());
}

#[test]
fn a_matching_reply_for_changed_content_retires_and_cannot_be_revived() {
    let mut f = fixture(ActionState::Draft);
    let valid = next_card(&f);
    let mut changed = valid.clone();
    changed.hash = ContentHash([8; 32]);
    let review = f.click(0);
    f.controls.try_recv().expect("the explicit rearm");

    f.reply(review, changed);
    cancelled(&f, 2, review);
    assert!(f.pending[0].is_none());
    f.reply(review, valid);
    f.decide(0, true, &SHOWN);
    assert!(f.pending[0].is_none());
    assert!(f.controls.try_recv().is_err());
}

#[test]
fn a_reply_to_an_old_session_cannot_open_or_cancel_in_the_new_session() {
    let mut f = fixture(ActionState::Draft);
    let valid = next_card(&f);
    let review = f.click(0);
    f.controls.try_recv().expect("the explicit rearm");

    f.session = Session::detached(2, Incarnation(1));
    assert!(
        f.settings
            .github
            .receive(Some(2), 2, connected(), Instant::now())
    );
    assert!(!f.receive(0, review, valid));
    assert!(f.pending[0].is_none());
    assert!(f.route.is_none());
    assert!(f.controls.try_recv().is_err());
}

#[test]
fn a_preview_decision_does_not_override_a_newer_receiver_watermark() {
    use crate::presence::messages::consent::machine::ReviewReply;

    let mut f = fixture(ActionState::Draft);
    let valid = next_card(&f);
    let review = f.click(0);
    f.controls.try_recv().expect("the explicit rearm");
    f.models[0].action_state(
        DraftId(9),
        StateVersion(3),
        ActionState::Draft,
        Some(ApprovalRev(3)),
        None,
    );

    let pending = f.pending[0].as_ref().unwrap();
    assert!(matches!(
        pending.review_reply(review, &valid, &SHOWN, &f.facts(0)),
        ReviewReply::Preview(_)
    ));
    f.reply(review, valid);
    cancelled(&f, 2, review);
    assert!(f.pending[0].is_none());
    let entry = f.models[0].issue(DraftId(9)).unwrap();
    assert!(entry.card().is_none(), "the older card was not hydrated");
    assert_eq!(entry.state(), Some(&ActionState::Draft));
}

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
