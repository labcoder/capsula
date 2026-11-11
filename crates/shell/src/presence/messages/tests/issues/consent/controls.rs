use super::*;
use kumochi_protocol::message::{IssueRefused, IssueUrl};

fn click(f: &Fixture, at: usize, selector: &'static str, cx: &mut TestAppContext) {
    f.views[at]
        .1
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[at].1, cx);
    let point = frame.debug_bounds(selector).unwrap().center();
    frame.simulate_click(point, gpui_kit::Modifiers::default());
    frame.run_until_parked();
}

fn deliver(f: &Fixture, events: Vec<LinkEvent>, cx: &mut TestAppContext) {
    f.owner.update(cx, |owner, cx| {
        owner.link_events(Batch::Plain(events), cx).unwrap();
    });
    cx.run_until_parked();
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one rendered File failure scenario covers correlation, no replay and fresh manual confirmation"
)]
fn storage_error_preserves_cards_and_requires_fresh_manual_file_confirmation(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "Literal saved body\n{ $repo }");
    let expected = f.models[0].read_with(cx, |model, _| {
        model.issue(DraftId(9)).unwrap().card().unwrap().clone()
    });
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(1, 1));
    for (generation, request, draft) in
        [(2, 1, DraftId(9)), (1, 2, DraftId(9)), (1, 1, DraftId(10))]
    {
        deliver(
            &f,
            vec![LinkEvent::IssueUnavailable {
                generation,
                request,
                draft,
            }],
            cx,
        );
        for model in &f.models {
            model.read_with(cx, |model, _| {
                let entry = model.issue(DraftId(9)).unwrap();
                assert!(entry.pending());
                assert_eq!(entry.feedback, None);
                assert_eq!(entry.card(), Some(&expected));
            });
        }
        assert!(f.controls.try_recv().is_err());
        assert!(f.requests.try_recv().is_err());
    }
    deliver(
        &f,
        vec![LinkEvent::IssueUnavailable {
            generation: 1,
            request: 1,
            draft: DraftId(9),
        }],
        cx,
    );
    for model in &f.models {
        model.read_with(cx, |model, _| {
            let entry = model.issue(DraftId(9)).unwrap();
            assert!(!entry.pending());
            assert!(!entry.loading());
            assert_eq!(
                entry.feedback,
                Some(crate::threads::IssueFeedback::Unavailable)
            );
            assert_eq!(entry.card(), Some(&expected));
        });
    }
    for (view, window) in &f.views {
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows[0].feedback.as_deref(), Some("This action couldn't be completed. Review the saved details before trying again."));
            assert!(view.pending_issue.is_none());
        });
        let mut frame = gpui_kit::VisualTestContext::from_window(*window, cx);
        assert!(frame.debug_bounds("issue-file-9").is_some());
        assert!(!frame.update(WindowExt::has_active_dialog));
    }
    deliver(
        &f,
        vec![
            LinkEvent::IssueUnavailable {
                generation: 1,
                request: 1,
                draft: DraftId(9),
            },
            LinkEvent::GitHub {
                generation: 1,
                view: connected(),
            },
        ],
        cx,
    );
    assert!(f.controls.try_recv().is_err());
    assert!(f.requests.try_recv().is_err());
    for (_, window) in &f.views {
        let mut frame = gpui_kit::VisualTestContext::from_window(*window, cx);
        assert!(!frame.update(WindowExt::has_active_dialog));
    }
    open(&f, 1, cx);
    assert!(f.controls.try_recv().is_err());
    cx.simulate_keystrokes(f.views[1].1, "escape");
    cx.run_until_parked();
    assert!(f.controls.try_recv().is_err());
    open(&f, 1, cx);
    confirm(&f, 1, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 1));
    deliver(
        &f,
        vec![LinkEvent::IssueUnavailable {
            generation: 1,
            request: 1,
            draft: DraftId(9),
        }],
        cx,
    );
    for model in &f.models {
        model.read_with(cx, |model, _| {
            let entry = model.issue(DraftId(9)).unwrap();
            assert!(entry.pending());
            assert_eq!(entry.feedback, None);
            assert_eq!(entry.card(), Some(&expected));
        });
    }
    assert!(f.controls.try_recv().is_err());
    assert!(f.requests.try_recv().is_err());
}

#[gpui::test]
fn storage_error_allows_only_manual_check_or_dismiss_and_never_schedules_a_request(
    cx: &mut TestAppContext,
) {
    for (selector, op) in [
        ("issue-check-9", IssueOp::CheckAgain { draft: DraftId(9) }),
        ("issue-dismiss-9", IssueOp::Discard { draft: DraftId(9) }),
    ] {
        let f = fixture(
            cx,
            ActionState::Unknown {
                lookup: LookupResult::None,
                maybe_duplicate: true,
            },
            "saved body",
        );
        let expected = f.models[0].read_with(cx, |model, _| {
            model.issue(DraftId(9)).unwrap().card().unwrap().clone()
        });
        click(&f, 0, selector, cx);
        assert_eq!(
            f.controls.try_recv().unwrap(),
            ControlToBrain::Issue {
                request: 1,
                op: op.clone()
            }
        );
        deliver(
            &f,
            vec![LinkEvent::IssueUnavailable {
                generation: 1,
                request: 1,
                draft: DraftId(9),
            }],
            cx,
        );
        for model in &f.models {
            model.read_with(cx, |model, _| {
                let entry = model.issue(DraftId(9)).unwrap();
                assert!(!entry.pending());
                assert_eq!(entry.card(), Some(&expected));
            });
        }
        assert!(f.controls.try_recv().is_err());
        assert!(f.requests.try_recv().is_err());
        click(&f, 1, selector, cx);
        assert_eq!(
            f.controls.try_recv().unwrap(),
            ControlToBrain::Issue { request: 2, op }
        );
        assert!(f.controls.try_recv().is_err());
        assert!(f.requests.try_recv().is_err());
    }
}

#[gpui::test]
fn late_storage_error_cannot_overwrite_completed_attempt_feedback(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(1, 1));
    deliver(
        &f,
        vec![
            LinkEvent::ActionState {
                generation: 1,
                request: Some(1),
                draft: DraftId(9),
                version: StateVersion(1),
                state: ActionState::Draft,
                approval: Some(ApprovalRev(1)),
            },
            LinkEvent::IssueUnavailable {
                generation: 1,
                request: 1,
                draft: DraftId(9),
            },
        ],
        cx,
    );
    for model in &f.models {
        model.read_with(cx, |model, _| {
            let entry = model.issue(DraftId(9)).unwrap();
            assert!(!entry.pending());
            assert_eq!(entry.feedback, None);
        });
    }
    deliver(
        &f,
        vec![
            LinkEvent::IssueRefused {
                generation: 1,
                request: 1,
                draft: DraftId(9),
                version: StateVersion(1),
                approval: Some(ApprovalRev(1)),
                reason: IssueRefused::NotAllowedNow,
            },
            LinkEvent::IssueUnavailable {
                generation: 1,
                request: 1,
                draft: DraftId(9),
            },
        ],
        cx,
    );
    for model in &f.models {
        model.read_with(cx, |model, _| {
            assert_eq!(
                model.issue(DraftId(9)).unwrap().feedback,
                Some(crate::threads::IssueFeedback::Refused(
                    IssueRefused::NotAllowedNow
                ))
            );
        });
    }
    assert!(f.controls.try_recv().is_err());
    assert!(f.requests.try_recv().is_err());
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one headless refusal and fresh-consent scenario for both retryable stages"
)]
fn unchanged_stage_refusal_keeps_correlation_and_a_fresh_attempt_uses_the_new_approval(
    cx: &mut TestAppContext,
) {
    for state in [ActionState::Draft, ActionState::Failed(FailReason::Network)] {
        let f = fixture(cx, state.clone(), "Literal { $repo }\n**not markup**");
        open(&f, 0, cx);
        confirm(&f, 0, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(1, 1));
        for model in &f.models {
            model.read_with(cx, |model, _| {
                assert!(model.issue(DraftId(9)).unwrap().pending());
            });
        }
        deliver(
            &f,
            vec![LinkEvent::ActionState {
                generation: 1,
                request: None,
                draft: DraftId(9),
                version: StateVersion(1),
                state: state.clone(),
                approval: Some(ApprovalRev(1)),
            }],
            cx,
        );
        assert!(f.models[1].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().pending()));
        deliver(
            &f,
            vec![
                LinkEvent::ActionState {
                    generation: 1,
                    request: Some(1),
                    draft: DraftId(9),
                    version: StateVersion(2),
                    state: state.clone(),
                    approval: Some(ApprovalRev(2)),
                },
                LinkEvent::IssueRefused {
                    generation: 1,
                    request: 1,
                    draft: DraftId(9),
                    version: StateVersion(2),
                    approval: Some(ApprovalRev(2)),
                    reason: IssueRefused::StaleApproval,
                },
            ],
            cx,
        );
        for view in &f.views {
            view.0.read_with(cx, |view, _| assert_eq!(view.rows[0].feedback.as_deref(), Some("The approval changed. Review the current saved details before trying again.")));
        }
        assert_eq!(
            f.requests.try_recv().unwrap(),
            DataToBrain::IssueCardsRequest {
                request: 6,
                drafts: Bounded::new(vec![DraftId(9)]).unwrap(),
            }
        );
        let mut card = issue_frame(6, 2, "Literal { $repo }\n**not markup**");
        if let DataToShell::IssueCardRead { card, .. } = &mut card {
            card.state = state.clone();
            card.approval = Some(ApprovalRev(2));
        }
        deliver(
            &f,
            vec![
                LinkEvent::Data {
                    generation: 1,
                    message: card,
                    bytes: 17,
                },
                LinkEvent::Data {
                    generation: 1,
                    message: DataToShell::IssueCardsDone {
                        request: 6,
                        status: IssueCardsStatus::Complete,
                    },
                    bytes: 7,
                },
            ],
            cx,
        );
        open(&f, 1, cx);
        confirm(&f, 1, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
        f.views[0].0.read_with(cx, |view, _| {
            assert_eq!(
                view.rows[0].feedback.as_deref(),
                Some("Waiting for this action's result…")
            );
        });
        let mut stale = Vec::new();
        for (generation, request, draft, version, approval) in [
            (1, 1, DraftId(9), StateVersion(2), Some(ApprovalRev(2))),
            (2, 2, DraftId(9), StateVersion(2), Some(ApprovalRev(2))),
            (1, 2, DraftId(10), StateVersion(2), Some(ApprovalRev(2))),
            (1, 2, DraftId(9), StateVersion(1), Some(ApprovalRev(1))),
            (1, 2, DraftId(9), StateVersion(2), Some(ApprovalRev(1))),
        ] {
            stale.push(LinkEvent::IssueRefused {
                generation,
                request,
                draft,
                version,
                approval,
                reason: IssueRefused::OtherAccount,
            });
        }
        stale.push(LinkEvent::ActionState {
            generation: 1,
            request: Some(1),
            draft: DraftId(9),
            version: StateVersion(2),
            state,
            approval: Some(ApprovalRev(2)),
        });
        deliver(&f, stale, cx);
        for model in &f.models {
            model.read_with(cx, |model, _| {
                let entry = model.issue(DraftId(9)).unwrap();
                assert!(entry.pending());
                assert_eq!(entry.feedback, None);
            });
        }
        deliver(
            &f,
            vec![LinkEvent::ActionState {
                generation: 1,
                request: Some(2),
                draft: DraftId(9),
                version: StateVersion(3),
                state: ActionState::Filed {
                    number: 7,
                    url: IssueUrl::new("https://github.com/o/r/issues/7").unwrap(),
                    confirmed_by_user: false,
                    maybe_duplicate: false,
                },
                approval: None,
            }],
            cx,
        );
        assert!(!f.models[0].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().pending()));
        assert!(f.controls.try_recv().is_err());
    }
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one headless GET-only scenario with unchanged busy and offline acknowledgements"
)]
fn unknown_check_again_is_get_only_and_final_unchanged_busy_or_offline_replies_release_it(
    cx: &mut TestAppContext,
) {
    for (offline, reason, text) in [
        (
            false,
            IssueRefused::NotAllowedNow,
            "This action isn't available right now. Try again after the current work finishes.",
        ),
        (
            true,
            IssueRefused::NotConnected,
            "GitHub isn't connected. Reconnect before trying again.",
        ),
    ] {
        let state = ActionState::Unknown {
            lookup: LookupResult::None,
            maybe_duplicate: true,
        };
        let f = fixture(cx, state.clone(), "saved body");
        if offline {
            deliver(
                &f,
                vec![LinkEvent::GitHub {
                    generation: 1,
                    view: GitHubView::SignedOut { cleanup: None },
                }],
                cx,
            );
        }
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
            assert!(frame.debug_bounds("issue-file-9").is_none());
            assert!(frame.debug_bounds("issue-check-9").is_some());
            assert!(frame.debug_bounds("issue-dismiss-9").is_some());
        }
        click(&f, 0, "issue-check-9", cx);
        assert_eq!(
            f.controls.try_recv().unwrap(),
            ControlToBrain::Issue {
                request: 1,
                op: IssueOp::CheckAgain { draft: DraftId(9) }
            }
        );
        deliver(
            &f,
            vec![LinkEvent::ActionState {
                generation: 1,
                request: None,
                draft: DraftId(9),
                version: StateVersion(1),
                state: state.clone(),
                approval: Some(ApprovalRev(1)),
            }],
            cx,
        );
        click(&f, 1, "issue-check-9", cx);
        assert!(f.controls.try_recv().is_err());
        assert!(f.models[0].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().pending()));
        deliver(
            &f,
            vec![LinkEvent::ActionState {
                generation: 1,
                request: Some(1),
                draft: DraftId(9),
                version: StateVersion(1),
                state,
                approval: Some(ApprovalRev(1)),
            }],
            cx,
        );
        for model in &f.models {
            model.read_with(cx, |model, _| {
                let entry = model.issue(DraftId(9)).unwrap();
                assert!(!entry.pending());
                assert_eq!(entry.feedback, None);
            });
        }
        deliver(
            &f,
            vec![LinkEvent::IssueRefused {
                generation: 1,
                request: 1,
                draft: DraftId(9),
                version: StateVersion(1),
                approval: Some(ApprovalRev(1)),
                reason,
            }],
            cx,
        );
        f.views[0].0.read_with(cx, |view, _| {
            assert_eq!(
                view.rows[0].warning.as_deref(),
                Some("More than one issue may exist on GitHub.")
            );
            assert_eq!(view.rows[0].feedback.as_deref(), Some(text));
        });
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        assert!(frame.debug_bounds("issue-file-9").is_none());
        assert!(!frame.update(WindowExt::has_active_dialog));
        click(&f, 1, "issue-check-9", cx);
        assert_eq!(
            f.controls.try_recv().unwrap(),
            ControlToBrain::Issue {
                request: 2,
                op: IssueOp::CheckAgain { draft: DraftId(9) }
            }
        );
    }
}

#[gpui::test]
fn unusable_bound_drafts_offer_replacement_guidance_and_dismiss_without_retargeting(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let mut view = connected();
    if let GitHubView::Connected { login, .. } = &mut view {
        *login = GitHubLogin::new("another").unwrap();
    }
    deliver(
        &f,
        vec![LinkEvent::GitHub {
            generation: 1,
            view,
        }],
        cx,
    );
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        assert!(frame.debug_bounds("issue-file-9").is_none());
        assert!(frame.debug_bounds("issue-check-9").is_none());
        assert!(frame.debug_bounds("issue-dismiss-9").is_some());
        assert!(frame.debug_bounds("issue-replacement").is_some());
    }
    f.models[0].read_with(cx, |model, _| {
        let entry = model.issue(DraftId(9)).unwrap();
        assert_eq!(
            entry.card().unwrap().account.as_ref().unwrap().as_str(),
            "octo"
        );
        assert_eq!(entry.card().unwrap().repo.as_ref().unwrap().as_str(), "o/r");
    });
    click(&f, 0, "issue-dismiss-9", cx);
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            request: 1,
            op: IssueOp::Discard { draft: DraftId(9) }
        }
    );
    assert!(f.models[1].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().pending()));
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn navigating_away_and_back_never_reuses_an_operation_request(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(1, 1));
    deliver(
        &f,
        vec![LinkEvent::ActionState {
            generation: 1,
            request: Some(1),
            draft: DraftId(9),
            version: StateVersion(1),
            state: ActionState::Draft,
            approval: Some(ApprovalRev(1)),
        }],
        cx,
    );
    f.models[0].update(cx, |model, cx| {
        model.hide();
        model.show_for(Some(ThreadId(1)));
        cx.notify();
    });
    f.owner.update(cx, Presence::request_pages);
    assert_eq!(
        f.requests.try_recv().unwrap(),
        DataToBrain::PageRequest {
            request: 6,
            page: PageOf::Events {
                thread: ThreadId(1),
                before: None
            },
        }
    );
    deliver(
        &f,
        vec![LinkEvent::Data {
            generation: 1,
            message: page(6),
            bytes: 11,
        }],
        cx,
    );
    assert!(f.requests.try_recv().is_err());
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 1));
    deliver(
        &f,
        vec![LinkEvent::IssueRefused {
            generation: 1,
            request: 1,
            draft: DraftId(9),
            version: StateVersion(1),
            approval: Some(ApprovalRev(1)),
            reason: IssueRefused::OtherAccount,
        }],
        cx,
    );
    f.models[0].read_with(cx, |model, _| {
        let entry = model.issue(DraftId(9)).unwrap();
        assert!(entry.pending());
        assert_eq!(entry.feedback, None);
    });
}

#[gpui::test]
fn only_unknown_offers_check_again_and_final_states_offer_no_filing_controls(
    cx: &mut TestAppContext,
) {
    for (state, file, dismiss, check) in [
        (ActionState::Draft, true, true, false),
        (ActionState::Failed(FailReason::Network), true, true, false),
        (
            ActionState::Unknown {
                lookup: LookupResult::Pending,
                maybe_duplicate: false,
            },
            false,
            true,
            true,
        ),
        (ActionState::Sending, false, false, false),
        (
            ActionState::Filed {
                number: 7,
                url: IssueUrl::new("https://github.com/o/r/issues/7").unwrap(),
                confirmed_by_user: false,
                maybe_duplicate: false,
            },
            false,
            false,
            false,
        ),
        (
            ActionState::Discarded { maybe_filed: true },
            false,
            false,
            false,
        ),
    ] {
        let f = fixture(cx, state, "saved body");
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        assert_eq!(frame.debug_bounds("issue-file-9").is_some(), file);
        assert_eq!(frame.debug_bounds("issue-dismiss-9").is_some(), dismiss);
        assert_eq!(frame.debug_bounds("issue-check-9").is_some(), check);
        assert!(!frame.update(WindowExt::has_active_dialog));
        assert!(f.controls.try_recv().is_err());
    }
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one headless reconnect scenario checks discarded feedback and replacement-session correlation"
)]
fn reconnect_drops_feedback_and_old_session_refusals_cannot_finish_a_new_attempt(
    cx: &mut TestAppContext,
) {
    let f = fixture(
        cx,
        ActionState::Unknown {
            lookup: LookupResult::None,
            maybe_duplicate: false,
        },
        "saved body",
    );
    click(&f, 0, "issue-check-9", cx);
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            request: 1,
            op: IssueOp::CheckAgain { draft: DraftId(9) }
        }
    );
    deliver(
        &f,
        vec![LinkEvent::IssueRefused {
            generation: 1,
            request: 1,
            draft: DraftId(9),
            version: StateVersion(1),
            approval: Some(ApprovalRev(1)),
            reason: IssueRefused::NotConnected,
        }],
        cx,
    );
    assert_eq!(
        f.models[0].read_with(cx, |model, _| model.issue(DraftId(9)).unwrap().feedback),
        Some(crate::threads::IssueFeedback::Refused(
            IssueRefused::NotConnected
        ))
    );
    deliver(
        &f,
        vec![
            LinkEvent::State(crate::link::LinkState::Reconnecting),
            LinkEvent::Connected(Session::detached(
                2,
                kumochi_protocol::message::Incarnation(1),
            )),
        ],
        cx,
    );
    assert!(f.models[0].read_with(cx, |model, _| model.issue(DraftId(9)).is_none()));
    assert_eq!(std::iter::from_fn(|| f.requests.try_recv().ok()).count(), 4);
    deliver(
        &f,
        vec![
            LinkEvent::GitHub {
                generation: 2,
                view: connected(),
            },
            LinkEvent::Data {
                generation: 2,
                message: page(7),
                bytes: 11,
            },
            LinkEvent::Data {
                generation: 2,
                message: page(9),
                bytes: 11,
            },
        ],
        cx,
    );
    assert_eq!(
        f.requests.try_recv().unwrap(),
        DataToBrain::IssueCardsRequest {
            request: 10,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }
    );
    deliver(
        &f,
        vec![
            LinkEvent::Data {
                generation: 2,
                message: issue_frame(10, 1, "saved body"),
                bytes: 17,
            },
            LinkEvent::Data {
                generation: 2,
                message: DataToShell::IssueCardsDone {
                    request: 10,
                    status: IssueCardsStatus::Complete,
                },
                bytes: 7,
            },
        ],
        cx,
    );
    open(&f, 1, cx);
    confirm(&f, 1, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(2, 1));
    deliver(
        &f,
        vec![LinkEvent::IssueRefused {
            generation: 1,
            request: 2,
            draft: DraftId(9),
            version: StateVersion(1),
            approval: Some(ApprovalRev(1)),
            reason: IssueRefused::OtherAccount,
        }],
        cx,
    );
    f.models[0].read_with(cx, |model, _| {
        let entry = model.issue(DraftId(9)).unwrap();
        assert!(entry.pending());
        assert_eq!(entry.feedback, None);
    });
}
