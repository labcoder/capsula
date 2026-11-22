use super::*;
use crate::link::{Batch, LinkEvent, LinkHandle, Session};
use gpui_kit::component::WindowExt;
use kumochi_protocol::message::{
    ApprovalRev, ContentHash, ControlToBrain, DataToBrain, DataToShell, Decision, Deleted, DraftId,
    FailReason, GitHubLogin, GitHubView, Incarnation, IssueCardsStatus, IssueOp, IssueSession,
    IssueUnavailableKind, LookupResult, PageOf, RepoName, StateVersion,
};
use kumochi_scene::text::Bounded;

mod controls;
mod reviews;

struct Fixture {
    owner: Entity<Presence>,
    models: [Entity<Threads>; 2],
    views: Vec<(Entity<Messages>, gpui_kit::AnyWindowHandle)>,
    controls: async_channel::Receiver<ControlToBrain>,
    requests: async_channel::Receiver<DataToBrain>,
}

struct ConsentFrame(Entity<Messages>);

impl Render for ConsentFrame {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(self.0.clone())
    }
}

fn connected() -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new("octo").unwrap(),
        repos: Bounded::new(vec![
            RepoName::new("o/r").unwrap(),
            RepoName::new("o/other").unwrap(),
        ])
        .unwrap(),
        default: Some(RepoName::new("o/other").unwrap()),
        truncated: false,
    }
}

fn page(request: u32) -> DataToShell {
    DataToShell::Page {
        request,
        thread: ThreadId(1),
        next: None,
        events: vec![PageEvent {
            id: EventId(7),
            created_ms: 0,
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(9) },
            text: "old title".into(),
            cut: false,
        }],
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixture initializes the owner and both histories"
)]
fn fixture(cx: &mut TestAppContext, state: ActionState, body: &str) -> Fixture {
    let owner = crate::presence::test_fixture::presence(cx);
    let (models, controls, requests) = owner.update(cx, |owner, cx| {
        let (mut link, requests) = LinkHandle::recording();
        let controls = link.record_controls();
        owner.link = link;
        owner.session = Some(Session::detached(1, Incarnation(1)));
        for model in [&owner.threads, &owner.chat] {
            model.update(cx, |state, _| {
                state.connected();
                state.show_for(Some(ThreadId(1)));
            });
        }
        owner.request_pages(cx);
        (
            [owner.threads.clone(), owner.chat.clone()],
            controls,
            requests,
        )
    });
    let views = cx.update(|cx| {
        crate::theme::init(cx);
        cx.set_reduce_motion(true);
        models
            .iter()
            .map(|model| {
                let mut view = None;
                let (window, _) =
                    gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                        let messages = cx
                            .new(|cx| Messages::new(model.clone(), owner.downgrade(), window, cx));
                        view = Some(messages.clone());
                        cx.new(|_| ConsentFrame(messages))
                    })
                    .unwrap();
                (view.unwrap(), window)
            })
            .collect()
    });
    assert_eq!(std::iter::from_fn(|| requests.try_recv().ok()).count(), 4);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 1,
                        view: connected(),
                    },
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 11,
                        message: page(2),
                    },
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 13,
                        message: page(4),
                    },
                ]),
                cx,
            )
            .unwrap();
    });
    assert_eq!(
        requests.try_recv().unwrap(),
        DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap(),
        }
    );
    assert!(requests.try_recv().is_err());
    let mut card = issue_frame(5, 1, body);
    if let DataToShell::IssueCardRead { card, .. } = &mut card {
        card.state = state;
    }
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 17,
                        message: card,
                    },
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 7,
                        message: DataToShell::IssueCardsDone {
                            request: 5,
                            status: IssueCardsStatus::Complete,
                        },
                    },
                ]),
                cx,
            )
            .unwrap();
        assert_eq!(owner.session.as_ref().unwrap().granted(), 48);
    });
    cx.run_until_parked();
    Fixture {
        owner,
        models,
        views,
        controls,
        requests,
    }
}

fn open(f: &Fixture, at: usize, cx: &mut TestAppContext) {
    let (view, window) = &f.views[at];
    let mut card = f.models[at].read_with(cx, |model, _| {
        model.issue(DraftId(9)).unwrap().card().unwrap().clone()
    });
    let (request, session, generation) = f.owner.read_with(cx, |owner, _| {
        let session = owner.session.as_ref().unwrap();
        (
            owner.issue_request + 1,
            session.issue_session(),
            session.generation(),
        )
    });
    window
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(*window, cx);
        let point = frame.debug_bounds("issue-file-9").unwrap().center();
        frame.simulate_click(point, gpui_kit::Modifiers::default());
    }
    cx.run_until_parked();
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            session,
            request,
            op: Box::new(IssueOp::Rearm {
                draft: DraftId(9),
                version: card.version,
                hash: card.hash
            }),
        }
    );
    assert!(
        !gpui_kit::VisualTestContext::from_window(*window, cx).update(WindowExt::has_active_dialog)
    );
    card.version.0 += 1;
    card.approval = Some(ApprovalRev(
        card.approval.map_or(1, |approval| approval.0 + 1),
    ));
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Data {
                    generation,
                    bytes: 23,
                    message: DataToShell::IssueReview { request, card },
                }]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.pending_issue.is_some()));
    assert!(
        gpui_kit::VisualTestContext::from_window(*window, cx).update(WindowExt::has_active_dialog)
    );
}

fn cancelled(f: &Fixture, request: u64, review: u64) {
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 1
            },
            request,
            op: Box::new(IssueOp::CancelReview {
                draft: DraftId(9),
                review
            }),
        }
    );
    assert!(f.controls.try_recv().is_err());
}

fn payload(request: u64, approval: u64) -> ControlToBrain {
    ControlToBrain::Issue {
        session: IssueSession {
            incarnation: Incarnation(1),
            epoch: 1,
        },
        request,
        op: Box::new(IssueOp::Consent {
            draft: DraftId(9),
            hash: ContentHash([7; 32]),
            approval: ApprovalRev(approval),
            decision: Decision::File,
        }),
    }
}

fn confirm(f: &Fixture, at: usize, cx: &mut TestAppContext) {
    cx.simulate_keystrokes(f.views[at].1, "tab enter");
    cx.run_until_parked();
}

#[gpui::test]
fn draft_and_definite_failure_emit_only_the_exact_decision_and_never_a_remote_outcome(
    cx: &mut TestAppContext,
) {
    for state in [ActionState::Draft, ActionState::Failed(FailReason::Network)] {
        let f = fixture(cx, state.clone(), "saved body");
        open(&f, 0, cx);
        confirm(&f, 0, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(2, 2));
        cx.simulate_keystrokes(f.views[0].1, "enter space enter");
        assert!(f.controls.try_recv().is_err());
        for model in &f.models {
            model.read_with(cx, |model, _| {
                assert_eq!(model.issue(DraftId(9)).unwrap().state(), Some(&state));
            });
        }
        f.owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::ActionState {
                        generation: 1,
                        request: Some(2),
                        draft: DraftId(9),
                        version: StateVersion(2),
                        state: state.clone(),
                        approval: Some(ApprovalRev(2)),
                    }]),
                    cx,
                )
                .unwrap();
        });
        open(&f, 1, cx);
        confirm(&f, 1, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
        assert!(f.controls.try_recv().is_err());
    }
}

#[gpui::test]
fn cancel_escape_immediate_and_held_enter_retire_reviews_without_filing(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    for (key, request, review) in [("enter", 2, 1), ("escape", 4, 3)] {
        open(&f, 0, cx);
        cx.simulate_keystrokes(f.views[0].1, key);
        cx.run_until_parked();
        cancelled(&f, request, review);
        assert!(
            f.views[0]
                .0
                .read_with(cx, |view, _| view.pending_issue.is_none())
        );
    }
    open(&f, 0, cx);
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        for held in [false, true, true] {
            frame.simulate_event(gpui_kit::KeyDownEvent {
                keystroke: gpui_kit::Keystroke::parse("enter").unwrap(),
                is_held: held,
                prefer_character_input: false,
            });
        }
        frame.run_until_parked();
        cancelled(&f, 6, 5);
    }
    open(&f, 0, cx);
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        let cancel = frame.debug_bounds("confirmation-cancel").unwrap().center();
        frame.simulate_click(cancel, gpui_kit::Modifiers::default());
        frame.run_until_parked();
        cancelled(&f, 8, 7);
    }
    open(&f, 0, cx);
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
    let button = frame.debug_bounds("confirmation-danger").unwrap().center();
    frame.simulate_click(button, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    assert_eq!(f.controls.try_recv().unwrap(), payload(10, 6));
    frame.simulate_click(button, gpui_kit::Modifiers::default());
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn a_batched_scope_loss_and_same_return_rejects_focused_confirm_before_observers(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    cx.simulate_keystrokes(f.views[0].1, "tab");
    cx.update(|cx| {
        f.owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![
                        LinkEvent::GitHub {
                            generation: 1,
                            view: GitHubView::SignedOut { cleanup: None },
                        },
                        LinkEvent::GitHub {
                            generation: 1,
                            view: connected(),
                        },
                    ]),
                    cx,
                )
                .unwrap();
        });
        assert!(f.views[0].0.read(cx).pending_issue.is_some());
        f.views[0]
            .1
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
            })
            .unwrap();
        cancelled(&f, 2, 1);
        assert!(f.views[0].0.read(cx).pending_issue.is_none());
    });
    cx.run_until_parked();
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn an_unverified_saved_login_rejects_focused_consent_and_cannot_open_a_new_confirmation(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    cx.simulate_keystrokes(f.views[0].1, "tab");
    cx.update(|cx| {
        f.owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::GitHub {
                        generation: 1,
                        view: GitHubView::NotVerified {
                            login: GitHubLogin::new("octo").unwrap(),
                        },
                    }]),
                    cx,
                )
                .unwrap();
        });
        f.views[0]
            .1
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
            })
            .unwrap();
        cancelled(&f, 2, 1);
        assert!(f.views[0].0.read(cx).pending_issue.is_none());
    });
    cx.run_until_parked();
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
    frame.update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
        window.draw(cx).clear(cx);
    });
    assert!(frame.debug_bounds("issue-file-9").is_none());
    frame.run_until_parked();
    frame.update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
        assert!(f.views[0].0.read(cx).pending_issue.is_none());
    });
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn hide_and_selection_loss_return_cannot_borrow_the_peers_retained_full_card(
    cx: &mut TestAppContext,
) {
    for hide in [true, false] {
        let f = fixture(cx, ActionState::Draft, "saved body");
        open(&f, 0, cx);
        cx.simulate_keystrokes(f.views[0].1, "tab");
        cx.update(|cx| {
            f.models[0].update(cx, |model, cx| {
                if hide {
                    model.hide();
                } else {
                    model.select(Some(ThreadId(2)));
                }
                model.show_for(Some(ThreadId(1)));
                let requests = model.requests();
                let request = requests
                    .iter()
                    .find_map(|request| match request {
                        DataToBrain::PageRequest {
                            request,
                            page:
                                PageOf::Events {
                                    thread: ThreadId(1),
                                    ..
                                },
                        } => Some(*request),
                        _ => None,
                    })
                    .unwrap();
                model.apply_for_tests(page(request));
                assert_eq!(
                    model.issue(DraftId(9)).unwrap().card().unwrap().version,
                    StateVersion(2)
                );
                cx.notify();
            });
            assert!(f.views[0].0.read(cx).pending_issue.is_some());
            f.views[0]
                .1
                .update(cx, |_, window, cx| {
                    window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
                })
                .unwrap();
            cancelled(&f, 2, 1);
            assert!(f.views[0].0.read(cx).pending_issue.is_none());
        });
        cx.run_until_parked();
        open(&f, 0, cx);
        confirm(&f, 0, cx);
        assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
    }
}

#[gpui::test]
fn a_peer_state_update_invalidates_stale_consent_without_duplicate_hydration_or_credit(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 1, cx);
    cx.simulate_keystrokes(f.views[1].1, "tab");
    cx.update(|cx| {
        f.owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::ActionState {
                        request: None,
                        generation: 1,
                        draft: DraftId(9),
                        version: StateVersion(3),
                        state: ActionState::Failed(FailReason::Rejected),
                        approval: Some(ApprovalRev(3)),
                    }]),
                    cx,
                )
                .unwrap();
        });
        assert!(f.views[1].0.read(cx).pending_issue.is_some());
        f.views[1]
            .1
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
            })
            .unwrap();
        cancelled(&f, 2, 1);
    });
    assert_eq!(
        f.requests.try_recv().unwrap(),
        DataToBrain::IssueCardsRequest {
            request: 6,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }
    );
    assert!(f.requests.try_recv().is_err());
    f.owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.session.as_ref().unwrap().granted(), 71);
        for model in &f.models {
            assert_eq!(
                model.read(cx).issue(DraftId(9)).unwrap().state(),
                Some(&ActionState::Failed(FailReason::Rejected))
            );
        }
    });
    let mut card = issue_frame(6, 3, "edited body");
    if let DataToShell::IssueCardRead { card, .. } = &mut card {
        card.state = ActionState::Failed(FailReason::Rejected);
        card.approval = Some(ApprovalRev(3));
    }
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 19,
                        message: card,
                    },
                    LinkEvent::Data {
                        generation: 1,
                        bytes: 5,
                        message: DataToShell::IssueCardsDone {
                            request: 6,
                            status: IssueCardsStatus::Complete,
                        },
                    },
                ]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    open(&f, 1, cx);
    confirm(&f, 1, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(4, 4));
    f.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.session.as_ref().unwrap().granted(), 118);
    });
}

#[gpui::test]
fn deletion_refusal_and_session_replacement_each_end_the_old_attempt(cx: &mut TestAppContext) {
    for deletion in [true, false] {
        let f = fixture(cx, ActionState::Draft, "saved body");
        open(&f, 0, cx);
        cx.simulate_keystrokes(f.views[0].1, "tab");
        cx.update(|cx| {
            f.owner.update(cx, |owner, cx| {
                if deletion {
                    assert!(owner.runs.confirm_delete(ThreadId(1)));
                    owner.sync_deletes(cx);
                    owner.thread_deleted(ThreadId(1), Deleted::NotHidden, None, cx);
                } else {
                    owner.session = Some(Session::detached(2, Incarnation(1)));
                }
            });
            f.views[0]
                .1
                .update(cx, |_, window, cx| {
                    window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
                })
                .unwrap();
            if deletion {
                cancelled(&f, 2, 1);
            } else {
                assert!(f.controls.try_recv().is_err());
            }
            assert!(f.views[0].0.read(cx).pending_issue.is_none());
        });
    }
}

#[gpui::test]
fn unavailable_unknown_loading_and_wrong_scope_never_open_a_confirmation(cx: &mut TestAppContext) {
    let f = fixture(
        cx,
        ActionState::Unknown {
            lookup: LookupResult::None,
            maybe_duplicate: false,
        },
        "saved body",
    );
    f.views[0]
        .1
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    f.views[0]
        .1
        .update(cx, |_, window, cx| {
            f.views[0].0.update(cx, |view, cx| {
                assert!(!view.open_issue_confirmation(DraftId(9), window, cx));
                assert!(view.pending_issue.is_none());
            });
        })
        .unwrap();
    assert!(f.controls.try_recv().is_err());
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    f.views[0]
        .1
        .update(cx, |_, window, cx| window.close_dialog(cx))
        .unwrap();
    cx.run_until_parked();
    let mut wrong = connected();
    if let GitHubView::Connected {
        repos, truncated, ..
    } = &mut wrong
    {
        *repos = Bounded::default();
        *truncated = true;
    }
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 1,
                    view: wrong,
                }]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    f.views[0]
        .1
        .update(cx, |_, window, cx| {
            f.views[0].0.update(cx, |view, cx| {
                assert!(!view.open_issue_confirmation(DraftId(9), window, cx));
                assert!(view.pending_issue.is_none());
                assert!(!window.has_active_dialog(cx));
            });
        })
        .unwrap();
    cancelled(&f, 2, 1);
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one refusal, reconnect and fresh confirmation scenario, read top to bottom"
)]
fn queue_refusal_is_not_replayed_by_later_valid_input(cx: &mut TestAppContext) {
    let mut f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    f.controls.close();
    confirm(&f, 0, cx);
    for model in &f.models {
        model.read_with(cx, |model, _| {
            let entry = model.issue(DraftId(9)).unwrap();
            assert!(!entry.pending());
            assert_eq!(
                entry.feedback,
                Some(crate::threads::IssueFeedback::QueueFailed)
            );
        });
    }
    f.views[0].0.read_with(cx, |view, _| {
        assert_eq!(
            view.rows[0].feedback.as_deref(),
            Some("The action wasn't queued. Try again when connected.")
        );
    });
    f.controls = f.owner.update(cx, |owner, _| owner.link.record_controls());
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 1,
                    view: connected(),
                }]),
                cx,
            )
            .unwrap();
    });
    cx.simulate_keystrokes(f.views[0].1, "enter space");
    assert!(f.controls.try_recv().is_err());
    let alive = f
        .owner
        .read_with(cx, |owner, _| owner.session.as_ref().unwrap().alive());
    assert!(!alive.load(std::sync::atomic::Ordering::Acquire));
    controls::deliver(
        &f,
        vec![
            LinkEvent::State(crate::link::LinkState::Reconnecting),
            LinkEvent::Connected(Session::detached(2, Incarnation(1))),
        ],
        cx,
    );
    assert_eq!(std::iter::from_fn(|| f.requests.try_recv().ok()).count(), 4);
    controls::deliver(
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
            drafts: Bounded::new(vec![DraftId(9)]).unwrap(),
        }
    );
    controls::deliver(
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
    assert!(f.controls.try_recv().is_err());
    cx.simulate_keystrokes(f.views[0].1, "enter space");
    assert!(f.controls.try_recv().is_err());
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 2,
            },
            request: 5,
            op: Box::new(IssueOp::Consent {
                draft: DraftId(9),
                hash: ContentHash([7; 32]),
                approval: ApprovalRev(2),
                decision: Decision::File,
            }),
        }
    );
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn hydrated_maximum_preview_keeps_exact_semantics_keyboard_scroll_and_private_debug(
    cx: &mut TestAppContext,
) {
    let body = "PRIVATE\n".repeat(4096);
    assert_eq!(body.len(), kumochi_protocol::message::MAX_ISSUE_BODY_BYTES);
    let f = fixture(cx, ActionState::Draft, &body);
    f.views[0].0.read_with(cx, |view, cx| {
        let entry = view.model.read(cx).issue(DraftId(9)).unwrap();
        let preview = crate::presence::messages::consent::Preview::new(entry.card().unwrap(), cx);
        let expected = ["Issue: saved title".to_owned(), format!("Repository: o/r\nBound account: octo\n\n{body}"),
            "Confirming asks to file this exact saved issue using the bound GitHub account and repository. GitHub will interpret mentions, links and Markdown in this text. A queued decision does not mean an issue was filed.".to_owned()];
        for ((text, expected), id) in preview.texts().into_iter().zip(&expected).zip(["issue-preview-title", "issue-preview-details", "issue-preview-consequence"]) {
            assert_eq!(text.id(), Some(&gpui_kit::ElementId::from(id)));
            assert_eq!(text.a11y_role(), Some(gpui_kit::Role::Label));
            let mut node = gpui_kit::accesskit::Node::new(gpui_kit::Role::Label);
            text.write_a11y_info(&mut node);
            assert_eq!(node.value(), Some(expected.as_str()));
        }
        let debug = format!("{preview:?}");
        for marker in ["PRIVATE", "o/r", "octo", "saved title"] {
            assert!(!debug.contains(marker));
            assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
        }
    });
    open(&f, 0, cx);
    f.views[0].0.read_with(cx, |view, _| {
        let debug = format!("{:?}", view.pending_issue);
        assert!(!debug.contains("PRIVATE"));
        assert!(!debug.contains(&format!("{:?}", b"PRIVATE")));
    });
    let before = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx)
        .debug_bounds("issue-preview-fields")
        .unwrap()
        .top();
    cx.simulate_keystrokes(f.views[0].1, "shift-tab pagedown");
    cx.run_until_parked();
    let after = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx)
        .debug_bounds("issue-preview-fields")
        .unwrap()
        .top();
    assert!(after < before);
    cx.simulate_keystrokes(f.views[0].1, "escape");
    cx.run_until_parked();
    cancelled(&f, 2, 1);
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
    assert!(frame.debug_bounds("confirmation-danger").is_none());
    assert!(frame.debug_bounds("issue-file-9").is_some());
}

#[gpui::test]
fn loading_missing_broken_session_and_wrong_login_fail_closed(cx: &mut TestAppContext) {
    for failure in ["loading", "unavailable", "broken", "session", "login"] {
        let f = fixture(cx, ActionState::Draft, "saved body");
        match failure {
            "session" => f.owner.update(cx, |owner, _| owner.session = None),
            "login" => f.owner.update(cx, |owner, cx| {
                let mut view = connected();
                if let GitHubView::Connected { login, .. } = &mut view {
                    *login = GitHubLogin::new("someone-else").unwrap();
                }
                owner
                    .link_events(
                        Batch::Plain(vec![LinkEvent::GitHub {
                            generation: 1,
                            view,
                        }]),
                        cx,
                    )
                    .unwrap();
            }),
            _ => f.models[0].update(cx, |model, cx| {
                if failure == "unavailable" {
                    model.action_state(
                        DraftId(9),
                        StateVersion(2),
                        ActionState::Draft,
                        Some(ApprovalRev(2)),
                        None,
                    );
                }
                assert_eq!(
                    model.card_request_for_tests(Bounded::new(vec![DraftId(9)]).unwrap()),
                    Some(DataToBrain::IssueCardsRequest {
                        request: 6,
                        drafts: Bounded::new(vec![DraftId(9)]).unwrap()
                    })
                );
                if failure == "unavailable" {
                    model.apply_for_tests(DataToShell::IssueCardsDone {
                        request: 6,
                        status: IssueCardsStatus::Failed,
                    });
                } else if failure == "broken" {
                    let mut conflict = issue_frame(6, 1, "saved body");
                    let DataToShell::IssueCardRead { card, .. } = &mut conflict else {
                        panic!("the fixture supplies a full card");
                    };
                    card.state = ActionState::Sending;
                    card.approval = None;
                    model.apply_for_tests(conflict);
                    assert!(model.cards_broken());
                    model.apply_for_tests(DataToShell::IssueCardsDone {
                        request: 6,
                        status: IssueCardsStatus::Complete,
                    });
                    assert!(!model.owns_request(6));
                    assert!(model.cards_broken());
                }
                cx.notify();
            }),
        }
        f.views[0]
            .1
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        f.views[0]
            .1
            .update(cx, |_, window, cx| {
                f.views[0].0.update(cx, |view, cx| {
                    assert!(
                        !view.open_issue_confirmation(DraftId(9), window, cx),
                        "{failure}"
                    );
                    assert!(view.pending_issue.is_none());
                    assert!(!window.has_active_dialog(cx));
                });
            })
            .unwrap();
        assert!(f.controls.try_recv().is_err());
    }
}

#[gpui::test]
fn a_rejected_open_leaves_no_pending_intent_and_does_not_close_a_different_dialog(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    f.views[0]
        .1
        .update(cx, |_, window, cx| {
            window.close_dialog(cx);
            window.open_alert_dialog(cx, |alert, _, _| alert.title("Other"));
        })
        .unwrap();
    cx.run_until_parked();
    f.views[0]
        .1
        .update(cx, |_, window, cx| {
            let focused = window.focused(cx);
            f.views[0].0.update(cx, |view, cx| {
                assert!(!view.open_issue_confirmation(DraftId(9), window, cx));
                assert!(view.pending_issue.is_none());
            });
            assert!(window.has_active_dialog(cx));
            assert_eq!(window.focused(cx), focused);
        })
        .unwrap();
    cancelled(&f, 2, 1);
}

#[gpui::test]
fn the_ordinary_history_rearms_before_opening_the_real_confirmation(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    let window = cx.update(|cx| {
        let (window, _) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            let messages =
                cx.new(|cx| Messages::new(f.models[0].clone(), f.owner.downgrade(), window, cx));
            cx.new(|_| Frame(messages))
        })
        .unwrap();
        window
    });
    cx.run_until_parked();
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        let opener = frame.debug_bounds("issue-file-9").unwrap().center();
        assert!(frame.debug_bounds("confirmation-danger").is_none());
        assert!(!frame.update(WindowExt::has_active_dialog));
        frame.update(|window, _| window.activate_window());
        frame.run_until_parked();
        frame.simulate_click(opener, gpui_kit::Modifiers::default());
        frame.run_until_parked();
        assert!(frame.debug_bounds("confirmation-danger").is_none());
        assert!(!frame.update(WindowExt::has_active_dialog));
    }
    assert_eq!(
        f.controls.try_recv().unwrap(),
        ControlToBrain::Issue {
            session: IssueSession {
                incarnation: Incarnation(1),
                epoch: 1
            },
            request: 1,
            op: Box::new(IssueOp::Rearm {
                draft: DraftId(9),
                version: StateVersion(1),
                hash: ContentHash([7; 32])
            }),
        }
    );
    let DataToShell::IssueCardRead { mut card, .. } = issue_frame(5, 2, "saved body") else {
        panic!("card fixture");
    };
    card.approval = Some(ApprovalRev(2));
    f.owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Data {
                    generation: 1,
                    bytes: 23,
                    message: DataToShell::IssueReview { request: 1, card },
                }]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    assert!(frame.debug_bounds("confirmation-danger").is_some());
    assert!(frame.update(WindowExt::has_active_dialog));
    assert!(f.controls.try_recv().is_err());
}

#[gpui::test]
fn a_coalesced_peer_hydration_cannot_revive_an_old_confirmation(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    cx.simulate_keystrokes(f.views[0].1, "tab");
    cx.update(|cx| {
        f.models[1].update(cx, |model, _| {
            assert_eq!(
                model.card_request_for_tests(Bounded::new(vec![DraftId(9)]).unwrap()),
                Some(DataToBrain::IssueCardsRequest {
                    request: 6,
                    drafts: Bounded::new(vec![DraftId(9)]).unwrap()
                })
            );
        });
        f.owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![
                        LinkEvent::Data {
                            generation: 1,
                            bytes: 17,
                            message: issue_frame(6, 2, "saved body"),
                        },
                        LinkEvent::Data {
                            generation: 1,
                            bytes: 7,
                            message: DataToShell::IssueCardsDone {
                                request: 6,
                                status: IssueCardsStatus::Complete,
                            },
                        },
                    ]),
                    cx,
                )
                .unwrap();
        });
        f.models[0].read_with(cx, |model, _| {
            assert!(!model.issue(DraftId(9)).unwrap().loading());
            assert_eq!(
                model.issue(DraftId(9)).unwrap().card().unwrap().version,
                StateVersion(2)
            );
        });
        f.views[0]
            .1
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(gpui_kit::Keystroke::parse("enter").unwrap(), cx);
            })
            .unwrap();
        cancelled(&f, 2, 1);
        assert!(f.views[0].0.read(cx).pending_issue.is_none());
    });
    cx.run_until_parked();
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
    f.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.session.as_ref().unwrap().granted(), 118);
    });
}

#[gpui::test]
fn losing_the_window_retires_consent_instead_of_reviving_on_activation(cx: &mut TestAppContext) {
    let f = fixture(cx, ActionState::Draft, "saved body");
    open(&f, 0, cx);
    cx.simulate_keystrokes(f.views[0].1, "tab");
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        frame.deactivate_window();
        frame.run_until_parked();
        assert!(!frame.update(WindowExt::has_active_dialog));
    }
    f.views[0]
        .1
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    cancelled(&f, 2, 1);
    cx.simulate_keystrokes(f.views[0].1, "enter");
    assert!(f.controls.try_recv().is_err());
    assert!(
        f.views[0]
            .0
            .read_with(cx, |view, _| view.pending_issue.is_none())
    );
    open(&f, 0, cx);
    confirm(&f, 0, cx);
    assert_eq!(f.controls.try_recv().unwrap(), payload(4, 3));
}

#[gpui::test]
fn a_small_large_text_modal_scrolls_the_maximum_body_without_losing_its_actions(
    cx: &mut TestAppContext,
) {
    let f = fixture(cx, ActionState::Draft, &"PRIVATE\n".repeat(4096));
    cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(24.0)));
    {
        let frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        frame.simulate_resize(size(px(400.0), px(300.0)));
    }
    cx.run_until_parked();
    open(&f, 0, cx);
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
        let viewport = frame.update(|window, _| {
            assert_eq!(window.rem_size(), px(24.0));
            window.viewport_size()
        });
        assert!(
            frame
                .debug_bounds("issue-preview-scroll")
                .unwrap()
                .size
                .height
                > px(0.0)
        );
        for selector in ["confirmation-cancel", "confirmation-danger"] {
            let bounds = frame.debug_bounds(selector).unwrap();
            assert!(bounds.top() >= px(0.0));
            assert!(bounds.bottom() <= viewport.height);
            assert!(bounds.left() >= px(0.0));
            assert!(bounds.right() <= viewport.width);
        }
    }
    cx.simulate_keystrokes(f.views[0].1, "shift-tab end");
    cx.run_until_parked();
    let mut frame = gpui_kit::VisualTestContext::from_window(f.views[0].1, cx);
    let fields = frame.debug_bounds("issue-preview-fields").unwrap();
    let viewport = frame.debug_bounds("issue-preview-scroll").unwrap();
    assert!(
        (fields.bottom() - viewport.bottom()).abs() < px(1.0),
        "fields={fields:?} viewport={viewport:?}"
    );
    assert!(f.controls.try_recv().is_err());
}
