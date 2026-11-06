use gpui_kit::{Entity, Focusable as _, TestAppContext};
use kumochi_protocol::message::{
    AccountOp, AccountView, Action, ControlToBrain, DataToBrain, ErrorKey, Incarnation, Phase,
    RunId, ScreenChoice, ThreadId, UserError,
};

use super::Source;
use crate::link::{Batch, LinkEvent, LinkHandle, Session};
use crate::presence::{Presence, test_fixture};

fn ready(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    async_channel::Receiver<DataToBrain>,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let (data, control) = owner.update(cx, |owner, cx| {
        let (mut link, data) = LinkHandle::recording();
        let control = link.record_controls();
        owner.link = link;
        owner.session = Some(Session::detached(1, Incarnation(1)));
        owner.runs.met(Incarnation(1));
        owner.runs.connected(1);
        owner.runs.settings(ThreadId(1));
        owner.runs.apply_scenes(&[]).expect("empty membership");
        owner.chat.update(cx, |chat, cx| {
            chat.connected();
            cx.notify();
        });
        (data, control)
    });
    (owner, data, control)
}

fn draft(owner: &Entity<Presence>, value: &str, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        let text = owner.text.as_ref().expect("typing fixture");
        text.window
            .update(cx, |_, window, cx| {
                text.input
                    .update(cx, |input, cx| input.set_value(value, window, cx));
            })
            .expect("typing window");
    });
}

fn receive(owner: &Entity<Presence>, events: Vec<LinkEvent>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(events), cx)
            .expect("valid link events");
    });
}

fn retry_failure(thread: u64) -> Vec<LinkEvent> {
    vec![
        LinkEvent::RunState {
            run: RunId(1),
            thread: ThreadId(thread),
            phase: Phase::Answering,
            error: None,
        },
        LinkEvent::RunState {
            run: RunId(1),
            thread: ThreadId(thread),
            phase: Phase::Failed,
            error: Some(
                UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry])
                    .expect("bounded error"),
            ),
        },
    ]
}

fn click_recovery(
    window: gpui_kit::AnyWindowHandle,
    selector: &'static str,
    cx: &mut TestAppContext,
) {
    cx.run_until_parked();
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        let bounds = frame.debug_bounds(selector).expect("recovery control");
        frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    }
    cx.run_until_parked();
}

#[gpui::test]
fn recovery_buttons_follow_live_drafts_and_dispatch_then_dismiss_the_named_failure(
    cx: &mut TestAppContext,
) {
    let (owner, data, control) = ready(cx);
    let window = owner.update(cx, |owner, _| {
        assert!(owner.runs.select_thread(Some(ThreadId(9))));
        owner.text.as_ref().expect("typing fixture").window
    });
    draft(&owner, "button retry", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("original turn"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(9)),
            text: "button retry".into(),
        }
    );
    receive(&owner, retry_failure(9), cx);
    cx.run_until_parked();
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        assert!(frame.debug_bounds("chat-recovery-action-0").is_some());
        assert!(frame.debug_bounds("chat-recovery-action-1").is_none());
    }
    draft(&owner, "newer draft", cx);
    click_recovery(window, "chat-recovery-action-0", cx);
    assert!(data.is_empty());
    owner.read_with(cx, |owner, _| {
        assert!(owner.runs.retry_ready(RunId(1)));
    });
    draft(&owner, "", cx);
    click_recovery(window, "chat-recovery-action-0", cx);
    assert_eq!(
        data.try_recv().expect("button retry"),
        DataToBrain::TextSubmit {
            turn: 2,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(9)),
            text: "button retry".into(),
        }
    );
    receive(
        &owner,
        vec![LinkEvent::RunState {
            run: RunId(2),
            thread: ThreadId(9),
            phase: Phase::Failed,
            error: Some(
                UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry])
                    .expect("bounded error"),
            ),
        }],
        cx,
    );
    draft(&owner, "keep this dismissed draft", cx);
    click_recovery(window, "chat-recovery-dismiss", cx);
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.chat_notice.shown(), None);
        assert_eq!(owner.runs.kept(), Vec::<usize>::new());
        assert_eq!(
            owner
                .text
                .as_ref()
                .expect("typing fixture")
                .input
                .read(cx)
                .value()
                .as_ref(),
            "keep this dismissed draft"
        );
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    assert!(frame.debug_bounds("chat-recovery").is_none());
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn typing_and_caret_notifications_redraw_recovery_only_when_draft_emptiness_changes(
    cx: &mut TestAppContext,
) {
    use std::{cell::Cell, rc::Rc};

    let (owner, data, control) = ready(cx);
    let (window, view, input) = owner.read_with(cx, |owner, _| {
        let text = owner.text.as_ref().expect("typing fixture");
        (text.window, text.view.clone(), text.input.clone())
    });
    window
        .update(cx, |_, window, cx| {
            window.activate_window();
            input.update(cx, |input, cx| input.focus(window, cx));
            window.draw(cx).clear(cx);
        })
        .expect("composer focus");
    cx.run_until_parked();
    let notified = Rc::new(Cell::new(0));
    let _watch = cx.update(|cx| {
        let notified = notified.clone();
        cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
    });
    cx.simulate_input(window, "n");
    cx.run_until_parked();
    assert_eq!(notified.replace(0), 1, "empty to nonempty redraws");
    cx.simulate_input(window, "ext");
    cx.run_until_parked();
    assert_eq!(input.read_with(cx, |input, _| input.value()), "next");
    assert_eq!(notified.replace(0), 0, "unchanged emptiness stays local");
    input.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(
        notified.replace(0),
        0,
        "caret-only notification stays local"
    );
    draft(&owner, "", cx);
    cx.run_until_parked();
    assert_eq!(notified.get(), 1, "nonempty to empty redraws");
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn keyboard_retry_returns_typing_and_escape_to_the_composer(cx: &mut TestAppContext) {
    keyboard_recovery_returns_composer(true, cx);
}

#[gpui::test]
fn keyboard_dismiss_returns_typing_and_escape_to_the_composer(cx: &mut TestAppContext) {
    keyboard_recovery_returns_composer(false, cx);
}

fn keyboard_recovery_returns_composer(retry: bool, cx: &mut TestAppContext) {
    let (owner, data, control) = ready(cx);
    draft(&owner, "keyboard retry", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("original turn"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "keyboard retry".into(),
        }
    );
    receive(&owner, retry_failure(1), cx);
    let (window, input, received) = owner.update(cx, |owner, cx| {
        let text = owner.text.as_ref().expect("typing fixture");
        text.view.update(cx, |view, cx| {
            view.events = owner.events.clone();
            cx.notify();
        });
        (text.window, text.input.clone(), owner.events_in.clone())
    });
    window
        .update(cx, |_, window, cx| {
            window.activate_window();
            window.draw(cx).clear(cx);
            input.update(cx, |input, cx| input.focus(window, cx));
            window.draw(cx).clear(cx);
        })
        .expect("input focus");
    cx.run_until_parked();
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    for key in if retry {
        &["shift-tab", "shift-tab", "shift-tab", "tab", "enter"][..]
    } else {
        &["shift-tab", "shift-tab", "tab", "enter"][..]
    } {
        let keystroke = frame.update(|_, cx| {
            gpui_kit::Keystroke::parse_with_os(key, cx.platform_info().os).expect("test key")
        });
        frame.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        frame.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        frame.update(|window, cx| window.draw(cx).clear(cx));
    }
    cx.run_until_parked();
    let expected = retry.then(|| DataToBrain::TextSubmit {
        turn: 2,
        screen: ScreenChoice::None,
        thread: Some(ThreadId(1)),
        text: "keyboard retry".into(),
    });
    assert_eq!(data.try_recv().ok(), expected);
    window
        .update(cx, |_, window, cx| {
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
        })
        .expect("composer owns focus");
    cx.simulate_input(window, "next");
    input.read_with(cx, |input, _| assert_eq!(input.value().as_ref(), "next"));
    cx.simulate_keystrokes(window, "escape");
    assert!(matches!(
        received.try_recv(),
        Ok(crate::platform::PresenceEvent::Escape)
    ));
    assert!(received.is_empty());
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn retry_dispatches_the_exact_named_turn_once_including_new_adoption(cx: &mut TestAppContext) {
    for selected in [Some(ThreadId(9)), None] {
        let (owner, data, control) = ready(cx);
        owner.update(cx, |owner, _| assert!(owner.runs.select_thread(selected)));
        draft(&owner, "retry café ✓", cx);
        owner.update(cx, |owner, cx| owner.submit(cx).expect("submit"));
        assert_eq!(
            data.try_recv().expect("original turn"),
            DataToBrain::TextSubmit {
                turn: 1,
                screen: ScreenChoice::None,
                thread: selected,
                text: "retry café ✓".into(),
            }
        );
        receive(&owner, retry_failure(9), cx);
        owner.update(cx, |owner, cx| {
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                ""
            );
            owner
                .recover(Source::Run(RunId(1)), Action::Retry, cx)
                .expect("retry");
            owner
                .recover(Source::Run(RunId(1)), Action::Retry, cx)
                .expect("used retry");
        });
        assert_eq!(
            data.try_recv().expect("retried turn"),
            DataToBrain::TextSubmit {
                turn: 2,
                screen: ScreenChoice::None,
                thread: Some(ThreadId(9)),
                text: "retry café ✓".into(),
            }
        );
        assert!(data.is_empty());
        assert!(control.is_empty());
    }
}

#[gpui::test]
fn retry_rechecks_owner_declared_action_and_draft_then_consumes_a_refusal(cx: &mut TestAppContext) {
    for newer in ["newer draft", " "] {
        let (owner, data, control) = ready(cx);
        receive(
            &owner,
            vec![LinkEvent::Account {
                generation: 1,
                view: AccountView::SignedIn {
                    email: None,
                    plan: kumochi_protocol::message::PlanState::Declined,
                    recovery: None,
                },
            }],
            cx,
        );
        draft(&owner, "original", cx);
        owner.update(cx, |owner, cx| owner.submit(cx).expect("submit"));
        assert_eq!(
            data.try_recv().expect("original turn"),
            DataToBrain::TextSubmit {
                turn: 1,
                screen: ScreenChoice::None,
                thread: Some(ThreadId(1)),
                text: "original".into(),
            }
        );
        receive(&owner, retry_failure(1), cx);
        owner.update(cx, |owner, cx| {
            owner
                .recover(Source::Run(RunId(2)), Action::Retry, cx)
                .expect("stale owner ignored");
            owner
                .recover(Source::Run(RunId(1)), Action::Enable, cx)
                .expect("undeclared action ignored");
            assert!(owner.runs.retry_ready(RunId(1)));
        });
        draft(&owner, newer, cx);
        owner.update(cx, |owner, cx| {
            owner
                .recover(Source::Run(RunId(1)), Action::Retry, cx)
                .expect("refused retry");
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                newer
            );
            assert!(!owner.runs.retry_ready(RunId(1)));
            assert_eq!(owner.runs.kept(), Vec::<usize>::new());
        });
        draft(&owner, "", cx);
        owner.update(cx, |owner, cx| {
            owner
                .recover(Source::Run(RunId(1)), Action::Retry, cx)
                .expect("refusal remains consumed");
        });
        assert!(data.is_empty());
        assert!(control.is_empty());
    }
}

#[gpui::test]
fn account_actions_use_the_shared_admission_and_never_resubmit_a_turn(cx: &mut TestAppContext) {
    let (owner, data, control) = ready(cx);
    let window = owner.read_with(cx, |owner, _| {
        owner.text.as_ref().expect("typing fixture").window
    });
    receive(
        &owner,
        vec![
            LinkEvent::Account {
                generation: 1,
                view: AccountView::Busy,
            },
            LinkEvent::Error {
                generation: 1,
                error: UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn])
                    .expect("bounded error"),
            },
        ],
        cx,
    );
    click_recovery(window, "chat-recovery-action-0", cx);
    assert!(control.is_empty());
    assert!(data.is_empty());
    receive(
        &owner,
        vec![
            LinkEvent::Account {
                generation: 1,
                view: AccountView::SignedOut {
                    revocation_unconfirmed: false,
                },
            },
            LinkEvent::Error {
                generation: 1,
                error: UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn])
                    .expect("bounded error"),
            },
        ],
        cx,
    );
    click_recovery(window, "chat-recovery-action-0", cx);
    click_recovery(window, "chat-recovery-action-0", cx);
    assert_eq!(
        control.try_recv().expect("sign-in intent"),
        ControlToBrain::AccountOp(AccountOp::SignIn)
    );
    assert!(control.is_empty());
    assert!(data.is_empty());
    receive(
        &owner,
        vec![
            LinkEvent::Account {
                generation: 1,
                view: AccountView::Busy,
            },
            LinkEvent::Error {
                generation: 1,
                error: UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry])
                    .expect("bounded error"),
            },
        ],
        cx,
    );
    click_recovery(window, "chat-recovery-action-0", cx);
    assert_eq!(
        control.try_recv().expect("account retry intent"),
        ControlToBrain::AccountOp(AccountOp::Retry)
    );
    assert!(control.is_empty());
    assert!(data.is_empty());
}

fn renewal_general() -> LinkEvent {
    LinkEvent::Error {
        generation: 1,
        error: UserError::new(
            ErrorKey::RenewalUnconfirmed,
            Vec::new(),
            vec![Action::SignIn],
        )
        .expect("bounded error"),
    }
}

#[gpui::test]
fn settings_retry_rechecks_its_captured_field_and_waits_for_the_matching_answer(
    cx: &mut TestAppContext,
) {
    use crate::settings::{Field, Update};
    use kumochi_protocol::message::{
        AssistantName, DockPosition, MaxFrameRate, SettingsOp, SettingsView,
    };
    let (owner, data, control) = ready(cx);
    let saved = SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid name"),
        user_name: None,
        onboarded: true,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
    };
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, _| {
            assert_eq!(state.set_view(saved.clone(), Update::Unsolicited), None);
            state.change(Field::Assistant, "Nimbus");
        });
        assert!(owner.save_setting(Field::Assistant, cx));
    });
    assert_eq!(
        control.try_recv().expect("original save"),
        ControlToBrain::Settings(SettingsOp::SetAssistantName(
            AssistantName::new("Nimbus").expect("valid name")
        ))
    );
    receive(
        &owner,
        vec![LinkEvent::Error {
            generation: 1,
            error: UserError::new(ErrorKey::SettingsRefused, Vec::new(), vec![Action::Retry])
                .expect("bounded error"),
        }],
        cx,
    );
    let source = Source::Settings(Some(Field::Assistant));
    let window = owner.read_with(cx, |owner, _| {
        owner.text.as_ref().expect("typing fixture").window
    });
    owner.update(cx, |owner, cx| {
        assert!(matches!(
            owner.chat_notice.shown().map(|notice| notice.source),
            Some(Source::Settings(Some(Field::Assistant)))
        ));
        owner
            .recover(Source::Settings(Some(Field::User)), Action::Retry, cx)
            .expect("wrong field ignored");
        owner
            .recover(source, Action::Retry, cx)
            .expect("saving field waits");
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    assert!(control.is_empty());
    draft(&owner, "keep this chat draft", cx);
    cx.run_until_parked();
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, cx| {
            let completion = state
                .set_view(saved, Update::Answer)
                .expect("matching answer");
            assert!(completion.failed);
            assert_eq!(completion.field, Field::Assistant);
            state.change(Field::Assistant, "Cirrus");
            state.change(Field::User, "keep this settings draft");
            cx.notify();
        });
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    click_recovery(window, "chat-recovery-action-0", cx);
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.settings.read(cx).value(Field::User),
            "keep this settings draft"
        );
        assert_eq!(
            owner
                .text
                .as_ref()
                .expect("typing fixture")
                .input
                .read(cx)
                .value()
                .as_ref(),
            "keep this chat draft"
        );
    });
    assert_eq!(
        control.try_recv().expect("captured field retry"),
        ControlToBrain::Settings(SettingsOp::SetAssistantName(
            AssistantName::new("Cirrus").expect("valid name")
        ))
    );
    assert!(control.is_empty());
    assert!(data.is_empty());
}

fn history_action(
    selector: &'static str,
    reply: kumochi_protocol::message::DataToShell,
    expected: &[DataToBrain],
    cx: &mut TestAppContext,
) {
    use gpui_kit::VisualTestContext;
    use kumochi_protocol::message::{DataToShell, PageOf};
    let (owner, data, control) = ready(cx);
    let window = owner.update(cx, |owner, cx| {
        assert!(owner.runs.select_thread(Some(ThreadId(3))));
        owner.chat.update(cx, |chat, cx| {
            chat.show_for(Some(ThreadId(3)));
            cx.notify();
        });
        let text = owner.text.as_mut().expect("typing fixture");
        text.typing.open();
        let window = text.window;
        owner.runs_changed(cx).expect("focused history");
        window
    });
    assert_eq!(
        std::iter::from_fn(|| data.try_recv().ok()).collect::<Vec<_>>(),
        vec![
            DataToBrain::PageRequest {
                request: 1,
                page: PageOf::Threads {
                    search: None,
                    after: None
                }
            },
            DataToBrain::PageRequest {
                request: 3,
                page: PageOf::Events {
                    thread: ThreadId(3),
                    before: None
                }
            },
        ]
    );
    receive(
        &owner,
        vec![
            LinkEvent::Data {
                generation: 1,
                message: DataToShell::ThreadList {
                    request: 1,
                    threads: Vec::new(),
                    next: None,
                },
                bytes: 17,
            },
            LinkEvent::Data {
                generation: 1,
                message: reply,
                bytes: 19,
            },
        ],
        cx,
    );
    draft(&owner, "keep this history draft", cx);
    cx.run_until_parked();
    {
        let mut frame = VisualTestContext::from_window(window, cx);
        let bounds = frame
            .debug_bounds(selector)
            .expect("enabled history control");
        frame.simulate_click(
            gpui_kit::point(bounds.left() + gpui_kit::px(20.0), bounds.center().y),
            gpui_kit::Modifiers::default(),
        );
    }
    cx.run_until_parked();
    assert_eq!(
        std::iter::from_fn(|| data.try_recv().ok())
            .collect::<Vec<_>>()
            .as_slice(),
        expected
    );
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner
                .text
                .as_ref()
                .expect("typing fixture")
                .input
                .read(cx)
                .value()
                .as_ref(),
            "keep this history draft"
        );
    });
    assert!(control.is_empty());
}

#[gpui::test]
fn actual_history_retry_sends_exact_focused_requests_and_keeps_the_draft(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{DataToShell, PageOf};
    history_action(
        "chat-retry-control",
        DataToShell::PageFailed { request: 3 },
        &[
            DataToBrain::PageRequest {
                request: 5,
                page: PageOf::Threads {
                    search: None,
                    after: None,
                },
            },
            DataToBrain::PageRequest {
                request: 7,
                page: PageOf::Events {
                    thread: ThreadId(3),
                    before: None,
                },
            },
        ],
        cx,
    );
}

#[gpui::test]
fn actual_history_older_sends_the_exact_cursor_and_keeps_the_draft(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{DataToShell, EventId, PageEvent, PageEventKind, PageOf};
    history_action(
        "chat-older-control",
        DataToShell::Page {
            request: 3,
            thread: ThreadId(3),
            events: (1..=50)
                .rev()
                .map(|id| PageEvent {
                    id: EventId(id),
                    run: Some(RunId(1)),
                    kind: PageEventKind::AssistantText,
                    text: format!("message {id}"),
                    cut: false,
                })
                .collect(),
            before: Some(EventId(1)),
        },
        &[DataToBrain::PageRequest {
            request: 5,
            page: PageOf::Events {
                thread: ThreadId(3),
                before: Some(EventId(1)),
            },
        }],
        cx,
    );
}

fn renewal_run() -> LinkEvent {
    LinkEvent::RunState {
        run: RunId(1),
        thread: ThreadId(1),
        phase: Phase::Failed,
        error: Some(
            UserError::new(
                ErrorKey::RenewalUnconfirmed,
                Vec::new(),
                vec![Action::SignIn, Action::Retry],
            )
            .expect("bounded error"),
        ),
    }
}

#[gpui::test]
fn an_accepted_submit_starts_the_next_renewal_incident(cx: &mut TestAppContext) {
    let (owner, data, control) = ready(cx);
    receive(&owner, vec![renewal_general()], cx);
    owner.update(cx, |owner, cx| owner.dismiss_recovery(Source::Account, cx));
    receive(&owner, vec![renewal_general()], cx);
    owner.read_with(cx, |owner, _| assert_eq!(owner.chat_notice.shown(), None));
    draft(&owner, "a deliberate new turn", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("accepted new turn"));
    assert_eq!(
        data.try_recv().expect("new turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "a deliberate new turn".into(),
        }
    );
    receive(&owner, vec![renewal_general()], cx);
    owner.read_with(cx, |owner, _| {
        assert!(matches!(
            owner.chat_notice.shown().map(|notice| notice.source),
            Some(Source::Account)
        ));
    });
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn an_accepted_run_retry_starts_the_next_renewal_incident(cx: &mut TestAppContext) {
    let (owner, data, control) = ready(cx);
    draft(&owner, "retry the named message", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("original turn"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "retry the named message".into(),
        }
    );
    receive(
        &owner,
        vec![
            LinkEvent::RunState {
                run: RunId(1),
                thread: ThreadId(1),
                phase: Phase::Answering,
                error: None,
            },
            renewal_run(),
        ],
        cx,
    );
    owner.update(cx, |owner, cx| {
        owner
            .recover(Source::Run(RunId(1)), Action::Retry, cx)
            .expect("explicit retry");
    });
    assert_eq!(
        data.try_recv().expect("retried turn"),
        DataToBrain::TextSubmit {
            turn: 2,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "retry the named message".into(),
        }
    );
    receive(&owner, vec![renewal_general()], cx);
    owner.read_with(cx, |owner, _| {
        assert!(matches!(
            owner.chat_notice.shown().map(|notice| notice.source),
            Some(Source::Account)
        ));
    });
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn authoritative_account_recovery_clears_only_its_renewal_and_starts_the_next_incident(
    cx: &mut TestAppContext,
) {
    let (owner, data, control) = ready(cx);
    receive(
        &owner,
        vec![
            LinkEvent::Account {
                generation: 1,
                view: AccountView::SignedOut {
                    revocation_unconfirmed: false,
                },
            },
            renewal_general(),
        ],
        cx,
    );
    owner.update(cx, |owner, cx| {
        owner
            .recover(Source::Account, Action::SignIn, cx)
            .expect("sign-in intent");
    });
    assert_eq!(
        control.try_recv().expect("sign-in intent"),
        ControlToBrain::AccountOp(AccountOp::SignIn)
    );
    receive(
        &owner,
        vec![LinkEvent::Account {
            generation: 1,
            view: AccountView::SignedIn {
                email: None,
                plan: kumochi_protocol::message::PlanState::Enabled,
                recovery: None,
            },
        }],
        cx,
    );
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.chat_notice.shown(), None);
        let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
        assert_eq!(view.recovery, None);
        assert_eq!(view.notice, None);
    });
    receive(&owner, vec![renewal_general()], cx);
    owner.read_with(cx, |owner, _| {
        assert!(matches!(
            owner.chat_notice.shown().map(|notice| notice.source),
            Some(Source::Account)
        ));
    });
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn a_run_retry_waits_for_account_admission_and_uses_up_a_refused_attempt(cx: &mut TestAppContext) {
    let (owner, data, control) = ready(cx);
    receive(
        &owner,
        vec![LinkEvent::Account {
            generation: 1,
            view: AccountView::SignedOut {
                revocation_unconfirmed: false,
            },
        }],
        cx,
    );
    draft(&owner, "saved failed turn", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("original turn"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "saved failed turn".into(),
        }
    );
    receive(&owner, retry_failure(1), cx);
    owner.update(cx, |owner, cx| {
        owner.account_op(AccountOp::SignIn, cx);
        owner
            .recover(Source::Run(RunId(1)), Action::Retry, cx)
            .expect("admission refusal");
        assert!(!owner.runs.retry_ready(RunId(1)));
        assert_eq!(owner.runs.kept(), Vec::<usize>::new());
    });
    assert_eq!(
        control.try_recv().expect("pending account operation"),
        ControlToBrain::AccountOp(AccountOp::SignIn)
    );
    receive(
        &owner,
        vec![LinkEvent::Account {
            generation: 1,
            view: AccountView::SignedOut {
                revocation_unconfirmed: false,
            },
        }],
        cx,
    );
    owner.update(cx, |owner, cx| {
        owner
            .recover(Source::Run(RunId(1)), Action::Retry, cx)
            .expect("refusal stays consumed");
    });
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn actual_escape_stops_then_clears_then_dismisses_without_changing_the_draft(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{SceneDescriptor, SceneId};
    let (owner, data, control) = ready(cx);
    owner.update(cx, |owner, _| {
        owner
            .runs
            .apply_scenes(&[SceneDescriptor {
                scene: SceneId(3),
                revision: 2,
                committed: 2,
            }])
            .expect("recoverable drawing");
        owner.text.as_mut().expect("typing fixture").typing.open();
    });
    draft(&owner, "sent message", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("submit"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "sent message".into(),
        }
    );
    receive(
        &owner,
        vec![LinkEvent::RunState {
            run: RunId(1),
            thread: ThreadId(1),
            phase: Phase::Answering,
            error: None,
        }],
        cx,
    );
    draft(&owner, "keep this draft", cx);
    for expected in [
        Some(ControlToBrain::Stop {
            run: RunId(1),
            revealed: Vec::new(),
        }),
        Some(ControlToBrain::ClearDrawing {
            scene_ids: vec![SceneId(3)],
        }),
        None,
    ] {
        owner.update(cx, |owner, cx| {
            owner.escape(cx).expect("headless Esc consumer");
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                "keep this draft"
            );
        });
        assert_eq!(control.try_recv().ok(), expected);
    }
    owner.read_with(cx, |owner, _| assert!(!owner.text_open()));
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn renewal_uses_one_run_owned_surface_and_a_delayed_frame_cannot_restore_the_banner(
    cx: &mut TestAppContext,
) {
    for arrivals in [
        vec![renewal_general(), renewal_run()],
        vec![renewal_run(), renewal_general()],
    ] {
        let (owner, data, control) = ready(cx);
        draft(&owner, "failed message", cx);
        owner.update(cx, |owner, cx| owner.submit(cx).expect("submit"));
        assert_eq!(
            data.try_recv().expect("original turn"),
            DataToBrain::TextSubmit {
                turn: 1,
                screen: ScreenChoice::None,
                thread: Some(ThreadId(1)),
                text: "failed message".into(),
            }
        );
        receive(
            &owner,
            vec![LinkEvent::RunState {
                run: RunId(1),
                thread: ThreadId(1),
                phase: Phase::Answering,
                error: None,
            }],
            cx,
        );
        receive(&owner, arrivals, cx);
        owner.update(cx, |owner, cx| {
            assert!(matches!(
                owner.chat_notice.shown().map(|notice| notice.source),
                Some(Source::Run(RunId(1)))
            ));
            let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
            assert!(matches!(
                view.recovery.as_ref().map(|notice| notice.source),
                Some(Source::Run(RunId(1)))
            ));
            assert!(view.retry);
            owner.dismiss_recovery(Source::Run(RunId(1)), cx);
        });
        receive(&owner, vec![renewal_general(), renewal_run()], cx);
        owner.update(cx, |owner, cx| {
            assert_eq!(owner.chat_notice.shown(), None);
            let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
            assert_eq!(view.recovery, None);
            assert_eq!(view.notice, None);
            assert!(!view.retry);
            assert_eq!(owner.runs.kept(), Vec::<usize>::new());
        });
        assert!(data.is_empty());
        assert!(control.is_empty());
    }
}
