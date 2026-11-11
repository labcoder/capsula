use super::*;
use gpui_kit::Element as _;

mod consent;

fn issue_frame(request: u32, version: u64, body: &str) -> kumochi_protocol::message::DataToShell {
    use kumochi_protocol::message::{
        ApprovalRev, ContentHash, DataToShell, DraftId, GitHubLogin, IssueBody, IssueCard,
        IssueTitle, RepoName, StateVersion,
    };
    DataToShell::IssueCardRead {
        request,
        card: IssueCard {
            draft: DraftId(9),
            thread: ThreadId(1),
            repo: Some(RepoName::new("o/r").unwrap()),
            account: Some(GitHubLogin::new("octo").unwrap()),
            title: IssueTitle::new("saved title").unwrap(),
            body: IssueBody::new(body).unwrap(),
            hash: ContentHash([7; 32]),
            approval: Some(ApprovalRev(1)),
            version: StateVersion(version),
            state: ActionState::Draft,
        },
    }
}

#[gpui::test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn a_batched_prepend_and_card_shrink_clamp_the_retained_event_anchor(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{DataToBrain, DataToShell, DraftId, PageOf, StateVersion};
    use kumochi_scene::text::Bounded;
    let (model, view, window) = open(cx);
    model.update(cx, |state, cx| {
        state.connected();
        state.show_for(Some(ThreadId(1)));
        assert_eq!(
            state.requests(),
            [
                DataToBrain::PageRequest {
                    request: 1,
                    page: PageOf::Threads {
                        search: None,
                        after: None
                    }
                },
                DataToBrain::PageRequest {
                    request: 2,
                    page: PageOf::Events {
                        thread: ThreadId(1),
                        before: None
                    }
                },
            ]
        );
        let mut events: Vec<_> = (8..=40).rev().map(event).collect();
        events.push(PageEvent {
            id: EventId(7),
            run: None,
            kind: PageEventKind::Issue { draft: DraftId(9) },
            text: "title".into(),
            cut: false,
        });
        state.apply(DataToShell::Page {
            request: 2,
            thread: ThreadId(1),
            events,
            before: Some(EventId(7)),
        });
        assert_eq!(
            state.requests(),
            [DataToBrain::IssueCardsRequest {
                request: 3,
                drafts: Bounded::new(vec![DraftId(9)]).unwrap()
            }]
        );
        state.apply(issue_frame(
            3,
            1,
            &"A long saved issue body that wraps. ".repeat(500),
        ));
        state.older_events();
        assert_eq!(
            state.requests(),
            [DataToBrain::PageRequest {
                request: 4,
                page: PageOf::Events {
                    thread: ThreadId(1),
                    before: Some(EventId(7))
                }
            }]
        );
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });
        cx.notify();
    });
    cx.run_until_parked();
    let long = view.read_with(cx, |view, _| {
        let bounds = view.list.bounds_for_item(0).unwrap();
        assert!(bounds.size.height > view.list.viewport_bounds().size.height * 3.0);
        bounds.size.height
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.focus.focus(window, cx);
                view.list.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: long * 0.6,
                });
                cx.notify();
            });
        })
        .unwrap();
    cx.run_until_parked();
    let before = view.read_with(cx, |view, _| {
        let anchor = view.list.logical_scroll_top();
        assert_eq!(view.ids[anchor.item_ix], ("event", 7));
        assert!((anchor.offset_in_item - long * 0.6).abs() < px(1.0));
        anchor
    });
    model.update(cx, |state, cx| {
        state.apply(DataToShell::Page {
            request: 4,
            thread: ThreadId(1),
            events: vec![event(6)],
            before: None,
        });
        state.action_state(DraftId(9), StateVersion(2), ActionState::Sending, None);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let anchor = view.list.logical_scroll_top();
        assert_eq!(view.ids[anchor.item_ix], ("event", 7));
        assert_eq!(anchor.item_ix, 1);
        let card = view.list.bounds_for_item(1).unwrap();
        assert!(before.offset_in_item > card.size.height);
        assert!((anchor.offset_in_item - card.size.height).abs() < px(1.0));
        let following = view.list.bounds_for_item(2).unwrap();
        assert_eq!(view.ids[2], ("event", 8));
        assert!(following.top() < view.list.viewport_bounds().bottom());
        assert!(following.bottom() > view.list.viewport_bounds().top());
    });
    window
        .update(cx, |_, window, cx| {
            view.read_with(cx, |view, _| assert!(view.focus.is_focused(window)));
            assert!(window.is_action_available(&LaterMessages, cx));
            assert!(window.dispatch_keystroke(gpui_kit::Keystroke::parse("pagedown").unwrap(), cx));
        })
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.list.logical_scroll_top().item_ix > 1);
    });
}

#[gpui::test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn actual_card_batches_notify_both_histories_and_grant_credit_once(cx: &mut TestAppContext) {
    use crate::link::{Batch, LinkEvent, LinkHandle, Session};
    use kumochi_protocol::message::{
        DataToBrain, DataToShell, DraftId, Incarnation, IssueCardsStatus, PageOf, StateVersion,
    };
    use kumochi_scene::text::Bounded;
    let owner = crate::presence::test_fixture::presence(cx);
    let (requests, models) = owner.update(cx, |owner, cx| {
        let (link, requests) = LinkHandle::recording();
        owner.link = link;
        owner.session = Some(Session::detached(1, Incarnation(1)));
        for model in [&owner.threads, &owner.chat] {
            model.update(cx, |state, _| {
                state.connected();
                state.show_for(Some(ThreadId(1)));
            });
        }
        owner.request_pages(cx);
        (requests, [owner.threads.clone(), owner.chat.clone()])
    });
    assert_eq!(
        std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>(),
        [
            DataToBrain::PageRequest {
                request: 1,
                page: PageOf::Threads {
                    search: None,
                    after: None
                }
            },
            DataToBrain::PageRequest {
                request: 2,
                page: PageOf::Events {
                    thread: ThreadId(1),
                    before: None
                }
            },
            DataToBrain::PageRequest {
                request: 3,
                page: PageOf::Threads {
                    search: None,
                    after: None
                }
            },
            DataToBrain::PageRequest {
                request: 4,
                page: PageOf::Events {
                    thread: ThreadId(1),
                    before: None
                }
            },
        ]
    );
    let views: Vec<_> = cx.update(|cx| {
        models
            .iter()
            .map(|model| {
                let mut view = None;
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    let messages =
                        cx.new(|cx| Messages::new(model.clone(), owner.downgrade(), window, cx));
                    view = Some(messages.clone());
                    cx.new(|_| Frame(messages))
                })
                .unwrap();
                view.unwrap()
            })
            .collect()
    });
    owner.update(cx, |owner, cx| {
        let page = |request, bytes| LinkEvent::Data {
            generation: 1,
            bytes,
            message: DataToShell::Page {
                request,
                thread: ThreadId(1),
                before: None,
                events: vec![PageEvent {
                    id: EventId(7),
                    run: None,
                    kind: PageEventKind::Issue { draft: DraftId(9) },
                    text: "title".into(),
                    cut: false,
                }],
            },
        };
        owner
            .link_events(Batch::Plain(vec![page(2, 11), page(4, 13)]), cx)
            .unwrap();
        assert_eq!(owner.session.as_ref().unwrap().granted(), 24);
    });
    assert_eq!(
        requests.try_recv().unwrap(),
        DataToBrain::IssueCardsRequest {
            request: 5,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }
    );
    assert!(requests.try_recv().is_err());
    cx.run_until_parked();
    for view in &views {
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows[0].label.as_deref(), Some("Loading saved issue"));
        });
    }
    owner.update(cx, |owner, cx| {
        assert_eq!(
            owner
                .chat
                .update(cx, |state, _| state.card_request(Bounded::default())),
            Some(DataToBrain::IssueCardsRequest {
                request: 6,
                drafts: Bounded::default()
            })
        );
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Data {
                    generation: 1,
                    bytes: 17,
                    message: issue_frame(5, 1, "shared saved body"),
                }]),
                cx,
            )
            .unwrap();
        assert!(owner.threads.read(cx).owns_request(5));
        assert_eq!(
            owner
                .chat
                .update(cx, |state, _| state.card_request(Bounded::default())),
            None
        );
        assert_eq!(owner.session.as_ref().unwrap().granted(), 41);
    });
    cx.run_until_parked();
    for view in &views {
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows[0].text.as_ref(), "Issue: saved title");
            assert_eq!(view.rows[0].label.as_deref(), Some("Draft"));
            assert_eq!(view.rows[0].details.as_deref(), Some("Repository: o/r\nBound account: octo\n\nshared saved body\n\nThis saved issue is read-only here. Filing and changes aren't available."));
        });
    }
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::Data {
                        generation: 0,
                        bytes: 101,
                        message: issue_frame(5, 99, "stale body"),
                    },
                    LinkEvent::Data {
                        generation: 0,
                        bytes: 103,
                        message: DataToShell::IssueCardsDone {
                            request: 5,
                            status: IssueCardsStatus::Failed,
                        },
                    },
                ]),
                cx,
            )
            .unwrap();
        assert!(owner.threads.read(cx).owns_request(5));
        assert_eq!(owner.session.as_ref().unwrap().granted(), 41);
    });
    cx.run_until_parked();
    for view in &views {
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows[0].label.as_deref(), Some("Draft"));
        });
    }
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::ActionState {
                        generation: 1,
                        draft: DraftId(9),
                        version: StateVersion(2),
                        state: ActionState::Sending,
                        approval: None,
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
        assert!(!owner.threads.read(cx).owns_request(5));
        assert!(owner.threads.read(cx).owns_request(7));
        assert_eq!(owner.session.as_ref().unwrap().granted(), 48);
    });
    assert_eq!(
        requests.try_recv().unwrap(),
        DataToBrain::IssueCardsRequest {
            request: 7,
            drafts: Bounded::new(vec![DraftId(9)]).unwrap()
        }
    );
    assert!(requests.try_recv().is_err());
    cx.run_until_parked();
    for view in &views {
        view.read_with(cx, |view, _| assert_eq!(view.rows[0].details.as_deref(), Some("Loading saved details…\n\nThis saved issue is read-only here. Filing and changes aren't available.")));
    }
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Data {
                    generation: 1,
                    bytes: 13,
                    message: DataToShell::IssueCardsDone {
                        request: 7,
                        status: IssueCardsStatus::Failed,
                    },
                }]),
                cx,
            )
            .unwrap();
        assert!(!owner.threads.read(cx).owns_request(7));
        assert_eq!(owner.session.as_ref().unwrap().granted(), 61);
        assert_eq!(
            owner
                .chat
                .update(cx, |state, _| state.card_request(Bounded::default())),
            Some(DataToBrain::IssueCardsRequest {
                request: 8,
                drafts: Bounded::default()
            })
        );
    });
    cx.run_until_parked();
    for view in &views {
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows[0].label.as_deref(), Some("Preparing or sending"));
            assert_eq!(view.rows[0].details.as_deref(), Some("The saved details aren't available in this view.\n\nThis saved issue is read-only here. Filing and changes aren't available."));
        });
    }
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn saved_issue_rows_hydrate_remeasure_and_keep_unknown_or_unavailable_truthful(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{
        ContentHash, DataToBrain, DataToShell, DraftId, GitHubLogin, IssueBody, IssueCard,
        IssueCardsStatus, IssueTitle, IssueUrl, LookupResult, PageOf, RepoName, StateVersion,
    };
    let (model, view, window) = open(cx);
    model.update(cx, |state, cx| {
        state.connected();
        state.show_for(Some(ThreadId(1)));
        assert_eq!(
            state.requests(),
            [
                DataToBrain::PageRequest {
                    request: 1,
                    page: PageOf::Threads {
                        search: None,
                        after: None
                    }
                },
                DataToBrain::PageRequest {
                    request: 2,
                    page: PageOf::Events {
                        thread: ThreadId(1),
                        before: None
                    }
                },
            ]
        );
        state.apply(DataToShell::Page {
            request: 2,
            thread: ThreadId(1),
            before: None,
            events: vec![PageEvent {
                id: EventId(7),
                run: None,
                kind: PageEventKind::Issue { draft: DraftId(9) },
                text: "old title".into(),
                cut: true,
            }],
        });
        assert_eq!(
            state.requests(),
            [DataToBrain::IssueCardsRequest {
                request: 3,
                drafts: kumochi_scene::text::Bounded::new(vec![DraftId(9)]).unwrap(),
            }]
        );
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].id, ("event", 7));
        assert_eq!(view.rows[0].label.as_deref(), Some("Loading saved issue"));
        assert_eq!(view.rows[0].text.as_ref(), "Issue: old title");
        assert_eq!(view.rows[0].details.as_deref(), Some("Loading saved details…\n\nThis saved issue is read-only here. Filing and changes aren't available."));
        assert_eq!(view.rows[0].warning, None);
        let row = &view.rows[0];
        for (field, value, expected) in [
            ("issue-status", row.label.clone().unwrap(), "Loading saved issue"),
            ("issue-title", row.text.clone(), "Issue: old title"),
            ("issue-details", row.details.clone().unwrap(), "Loading saved details…\n\nThis saved issue is read-only here. Filing and changes aren't available."),
        ] {
            let text = message_text(row, field, value);
            assert_eq!(text.id(), Some(&gpui_kit::ElementId::from(field)));
            assert_eq!(text.a11y_role(), Some(gpui_kit::Role::Label));
            let mut node = gpui_kit::accesskit::Node::new(gpui_kit::Role::Label);
            text.write_a11y_info(&mut node);
            assert_eq!(node.value(), Some(expected));
        }
    });
    let before = {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        frame.debug_bounds("message-event-7").unwrap().size.height
    };
    let body = "private-issue-body-marker with wrapping text. ".repeat(25);
    let url = IssueUrl::new("https://github.com/o/r/issues/7").unwrap();
    model.update(cx, |state, cx| {
        state.apply(DataToShell::IssueCardRead {
            request: 3,
            card: IssueCard {
                draft: DraftId(9),
                thread: ThreadId(1),
                repo: Some(RepoName::new("o/r").unwrap()),
                account: Some(GitHubLogin::new("octo").unwrap()),
                title: IssueTitle::new("saved title").unwrap(),
                body: IssueBody::new(&body).unwrap(),
                hash: ContentHash([7; 32]),
                approval: None,
                version: StateVersion(4),
                state: ActionState::Filed {
                    number: 7,
                    url: url.clone(),
                    confirmed_by_user: true,
                    maybe_duplicate: true,
                },
            },
        });
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(view.rows[0].text.as_ref(), "Issue: saved title");
        assert_eq!(view.rows[0].label.as_deref(), Some("Filed #7\nConfirmed by you.\nhttps://github.com/o/r/issues/7"));
        assert_eq!(view.rows[0].details.as_deref(), Some(format!("Repository: o/r\nBound account: octo\n\n{body}\n\nThis saved issue is read-only here. Filing and changes aren't available.").as_str()));
        assert_eq!(view.rows[0].warning.as_deref(), Some("More than one issue may exist on GitHub."));
        assert_eq!(view.rows[0].cut, None);
        let row = &view.rows[0];
        let details = format!("Repository: o/r\nBound account: octo\n\n{body}\n\nThis saved issue is read-only here. Filing and changes aren't available.");
        for (field, value, expected) in [
            ("issue-status", row.label.clone().unwrap(), "Filed #7\nConfirmed by you.\nhttps://github.com/o/r/issues/7"),
            ("issue-title", row.text.clone(), "Issue: saved title"),
            ("issue-warning", row.warning.clone().unwrap(), "More than one issue may exist on GitHub."),
            ("issue-details", row.details.clone().unwrap(), details.as_str()),
        ] {
            let text = message_text(row, field, value);
            assert_eq!(text.id(), Some(&gpui_kit::ElementId::from(field)));
            assert_eq!(text.a11y_role(), Some(gpui_kit::Role::Label));
            let mut node = gpui_kit::accesskit::Node::new(gpui_kit::Role::Label);
            text.write_a11y_info(&mut node);
            assert_eq!(node.value(), Some(expected));
        }
        let debug = format!("{:?}", view.rows);
        assert!(!debug.contains("private-issue-body-marker"));
        assert!(!debug.contains(&format!("{:?}", b"private-issue-body-marker")));
    });
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        assert!(frame.debug_bounds("message-event-7").unwrap().size.height > before);
    }
    model.update(cx, |state, cx| {
        state.action_state(
            DraftId(9),
            StateVersion(5),
            ActionState::Unknown {
                lookup: LookupResult::Unplaced { number: 7, url },
                maybe_duplicate: false,
            },
            None,
        );
        state.apply(DataToShell::IssueCardsDone {
            request: 3,
            status: IssueCardsStatus::Complete,
        });
        assert_eq!(
            state.requests(),
            [DataToBrain::IssueCardsRequest {
                request: 4,
                drafts: kumochi_scene::text::Bounded::new(vec![DraftId(9)]).unwrap(),
            }]
        );
        state.apply(DataToShell::IssueCardsDone {
            request: 4,
            status: IssueCardsStatus::Failed,
        });
        assert_eq!(state.requests(), []);
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(view.rows[0].label.as_deref(), Some("Outcome unknown. An issue may already exist on GitHub.\nCandidate #7 is not confirmed as this issue.\nhttps://github.com/o/r/issues/7"));
        assert_eq!(view.rows[0].details.as_deref(), Some("The saved details aren't available in this view.\n\nThis saved issue is read-only here. Filing and changes aren't available."));
        assert_eq!(view.rows[0].warning, None);
    });
    model.update(cx, |state, cx| {
        state.disconnected();
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.rows[0].label.as_deref(),
            Some("Saved issue unavailable")
        );
        assert_eq!(view.rows[0].text.as_ref(), "Issue: old title");
        assert!(
            !view.rows[0]
                .details
                .as_ref()
                .unwrap()
                .contains("private-issue-body-marker")
        );
    });
}
