use super::controls::{click, deliver};
use super::*;
use kumochi_protocol::message::IssueCard;

fn rearm(request: u64) -> ControlToBrain {
    ControlToBrain::Issue {
        session: IssueSession {
            incarnation: Incarnation(1),
            epoch: 1,
        },
        request,
        op: Box::new(IssueOp::Rearm {
            draft: DraftId(9),
            version: StateVersion(1),
            hash: ContentHash([7; 32]),
        }),
    }
}

fn offer(f: &Fixture, cx: &TestAppContext) -> IssueCard {
    let mut card = f.models[0].read_with(cx, |model, _| {
        model.issue(DraftId(9)).unwrap().card().unwrap().clone()
    });
    card.version = StateVersion(2);
    card.approval = Some(ApprovalRev(2));
    card
}

fn reply(request: u64, card: IssueCard) -> LinkEvent {
    LinkEvent::Data {
        generation: 1,
        bytes: 23,
        message: DataToShell::IssueReview { request, card },
    }
}

fn modal(f: &Fixture, at: usize, cx: &mut TestAppContext) -> bool {
    gpui_kit::VisualTestContext::from_window(f.views[at].1, cx).update(WindowExt::has_active_dialog)
}

#[gpui::test]
fn only_a_matching_live_reply_opens_once_and_returns_each_frames_credit(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "Literal saved details");
    let card = offer(&f, cx);
    deliver(&f, vec![reply(1, card.clone())], cx);
    assert!(!modal(&f, 0, cx));
    assert_eq!(
        f.models[0].read_with(cx, |model, _| model
            .issue(DraftId(9))
            .unwrap()
            .card()
            .unwrap()
            .version),
        StateVersion(1)
    );
    assert!(f.controls.try_recv().is_err());
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    assert!(!modal(&f, 0, cx));
    assert_eq!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.rows[0].feedback.clone())
            .as_deref(),
        Some("Saving a fresh review…")
    );
    let mut wrong_draft = card.clone();
    wrong_draft.draft = DraftId(10);
    deliver(&f, vec![reply(2, card.clone()), reply(1, wrong_draft)], cx);
    assert!(!modal(&f, 0, cx));
    assert!(f.controls.try_recv().is_err());
    deliver(&f, vec![reply(1, card.clone()), reply(1, card)], cx);
    assert!(modal(&f, 0, cx));
    assert_eq!(
        f.owner
            .read_with(cx, |owner, _| owner.session.as_ref().unwrap().granted()),
        163
    );
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
    assert!(f.controls.try_recv().is_err());
    assert!(f.requests.try_recv().is_err());
}

#[gpui::test]
fn malformed_matching_reviews_retire_and_later_valid_data_cannot_revive_them(
    cx: &mut TestAppContext,
) {
    for invalid in 0..8 {
        let f = fixture(cx, ActionState::Draft, "saved body");
        let card = offer(&f, cx);
        click(&f, 0, "issue-file-9", cx);
        assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
        let mut bad = card.clone();
        match invalid {
            0 => bad.thread = ThreadId(2),
            1 => bad.hash = ContentHash([8; 32]),
            2 => bad.version = StateVersion(1),
            3 => bad.approval = Some(ApprovalRev(1)),
            4 => bad.approval = None,
            5 => bad.account = Some(GitHubLogin::new("another").unwrap()),
            6 => bad.repo = Some(RepoName::new("o/not-listed").unwrap()),
            7 => bad.state = ActionState::Sending,
            _ => unreachable!(),
        }
        deliver(&f, vec![reply(1, bad)], cx);
        cancelled(&f, 2, 1);
        deliver(&f, vec![reply(1, card)], cx);
        assert!(!modal(&f, 0, cx));
        assert!(
            f.views[0]
                .0
                .read_with(cx, |view, _| view.pending_issue.is_none())
        );
        assert_eq!(
            f.models[0].read_with(cx, |model, _| model
                .issue(DraftId(9))
                .unwrap()
                .card()
                .unwrap()
                .version),
            StateVersion(1)
        );
        assert_eq!(
            f.owner
                .read_with(cx, |owner, _| owner.session.as_ref().unwrap().granted()),
            94
        );
        assert!(f.controls.try_recv().is_err());
        open(&f, 0, cx);
        confirm(&f, 0, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(4, 2));
    }
}

#[gpui::test]
fn cancelling_before_mint_and_superseding_in_the_peer_view_leave_no_old_authority(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let card = offer(&f, cx);
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    cx.simulate_keystrokes(f.views[0].1, "escape");
    cx.run_until_parked();
    cancelled(&f, 2, 1);
    deliver(&f, vec![reply(1, card.clone())], cx);
    assert!(!modal(&f, 0, cx));
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(3));
    click(&f, 1, "issue-file-9", cx);
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 1
            },
            request: 4,
            op: Box::new(IssueOp::CancelReview {
                draft: DraftId(9),
                review: 3
            }),
        }
    );
    assert_eq!(f.controls.try_recv().unwrap(), rearm(5));
    deliver(&f, vec![reply(3, card.clone()), reply(5, card)], cx);
    assert!(!modal(&f, 0, cx));
    assert!(modal(&f, 1, cx));
    confirm(&f, 1, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(6, 2));
    assert!(f.controls.try_recv().is_err());
    assert_eq!(
        f.owner
            .read_with(cx, |owner, _| owner.session.as_ref().unwrap().granted()),
        117
    );
}

#[gpui::test]
fn awaiting_scope_loss_is_final_even_when_the_same_thread_returns_before_observers(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let card = offer(&f, cx);
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    f.models[0].update(cx, |model, _| {
        model.show_for(Some(ThreadId(2)));
        model.show_for(Some(ThreadId(1)));
    });
    deliver(&f, vec![reply(1, card)], cx);
    cancelled(&f, 2, 1);
    assert!(!modal(&f, 0, cx));
    assert!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.pending_issue.is_none())
    );
    assert_eq!(
        f.owner
            .read_with(cx, |owner, _| owner.session.as_ref().unwrap().granted()),
        71
    );
}

#[gpui::test]
fn a_dialog_that_appears_while_saving_is_not_replaced_by_a_review_reply(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let card = offer(&f, cx);
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    f.views[0]
        .1
        .update(cx, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Other dialog"));
        })
        .unwrap();
    deliver(&f, vec![reply(1, card)], cx);
    cancelled(&f, 2, 1);
    assert!(modal(&f, 0, cx));
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
    assert!(frame.debug_bounds("confirmation-danger").is_none());
    assert!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.pending_issue.is_none())
    );
}

#[gpui::test]
fn deferred_review_credit_stays_with_the_original_session_after_reconnect(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let card = offer(&f, cx);
    let original = f
        .owner
        .read_with(cx, |owner, _| owner.session.clone().unwrap());
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    reply(1, card),
                    LinkEvent::State(crate::link::LinkState::Reconnecting),
                    LinkEvent::Connected(Session::detached(2, Incarnation(1))),
                ]),
                cx,
            )
            .unwrap();
        assert_eq!(original.granted(), 48);
    });
    cx.run_until_parked();
    cancelled(&f, 2, 1);
    assert_eq!(original.granted(), 71);
    assert_eq!(
        f.owner
            .read_with(cx, |owner, _| owner.session.as_ref().unwrap().granted()),
        0
    );
    assert!(!modal(&f, 0, cx));
    assert!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.pending_issue.is_none())
    );
}

#[gpui::test]
fn a_failed_cancel_enqueue_closes_the_original_session_queue(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let controls = f
        .owner
        .update(cx, |owner, _| owner.link.record_controls_bounded());
    click(&f, 0, "issue-file-9", cx);
    assert_eq!(controls.try_recv().unwrap(), rearm(1));
    f.owner.update(cx, |owner, _| {
        let session = owner.session.as_ref().unwrap();
        for _ in 0..controls.capacity().unwrap() {
            assert!(owner.link.send_control(session, ControlToBrain::Pong));
        }
    });
    cx.simulate_keystrokes(f.views[0].1, "escape");
    cx.run_until_parked();
    assert!(controls.is_closed());
    assert_eq!(
        std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>(),
        vec![ControlToBrain::Pong; controls.capacity().unwrap()]
    );
    assert!(!modal(&f, 0, cx));
    assert!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.pending_issue.is_none())
    );
    assert!(!f.models[0].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().pending()));
}

#[gpui::test]
fn review_terminals_retire_pending_and_deferred_permission_without_filing(cx: &mut TestAppContext) {
    for (kind, text) in [
        (
            IssueUnavailableKind::ReviewNotSaved,
            "The review couldn't be saved. Nothing was sent to GitHub. Restore storage, then explicitly review again.",
        ),
        (
            IssueUnavailableKind::ReviewCapacity,
            "Other reviews are open or finishing. Finish or cancel one, then review again.",
        ),
    ] {
        let f = fixture(cx, ActionState::Draft, "saved body");
        let card = offer(&f, cx);
        click(&f, 0, "issue-file-9", cx);
        assert_eq!(f.controls.try_recv().unwrap(), rearm(1));
        deliver(
            &f,
            vec![
                reply(1, card.clone()),
                LinkEvent::IssueUnavailable {
                    generation: 1,
                    request: 1,
                    draft: DraftId(9),
                    kind,
                },
            ],
            cx,
        );
        cancelled(&f, 2, 1);
        assert!(!modal(&f, 0, cx));
        assert_eq!(
            f.views[0]
                .0
                .read_with(cx, |view, _| view.rows[0].feedback.clone())
                .as_deref(),
            Some(text)
        );
        deliver(&f, vec![reply(1, card)], cx);
        assert!(!modal(&f, 0, cx));
        assert!(f.controls.try_recv().is_err());
        open(&f, 0, cx);
        confirm(&f, 0, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(4, 2));
    }
}

#[gpui::test]
fn possible_post_handoff_storage_failure_never_invites_another_file(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
    deliver(
        &f,
        vec![LinkEvent::IssueUnavailable {
            generation: 1,
            request: 2,
            draft: DraftId(9),
            kind: IssueUnavailableKind::AttemptNotSaved,
        }],
        cx,
    );
    for (view, window) in &f.views {
        assert_eq!(
            view.read_with(cx, |view, _| view.rows[0].feedback.clone())
                .as_deref(),
            Some(
                "The confirmed outcome couldn't be saved. GitHub may have received this issue. Do not file it again."
            )
        );
        let mut frame = gpui_kit::VisualTestContext::from_window(*window, cx);
        assert!(frame.debug_bounds("issue-file-9").is_none());
        assert!(frame.debug_bounds("issue-replacement").is_none());
        assert!(frame.debug_bounds("issue-dismiss-9").is_some());
    }
    let card = offer(&f, cx);
    deliver(
        &f,
        vec![
            reply(1, card),
            LinkEvent::GitHub {
                generation: 1,
                view: connected(),
            },
        ],
        cx,
    );
    assert!(!modal(&f, 0, cx));
    assert!(f.controls.try_recv().is_err());
}
