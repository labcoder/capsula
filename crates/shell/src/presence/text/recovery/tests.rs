use gpui_kit::{Entity, Focusable as _, TestAppContext};
use kumochi_protocol::message::{
    AccountOp, AccountView, Action, ControlToBrain, DataToBrain, ErrorKey, Incarnation, Phase,
    RunId, RunOrigin, ScreenChoice, ThreadId, UserError,
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

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one reconnect flow checks every stale-key side effect before current-session recovery"
)]
fn old_connection_keys_cannot_touch_reused_turns_after_either_reconnect(cx: &mut TestAppContext) {
    use crate::escape::Activity;
    use crate::link::LinkState;
    use crate::runs::Target;
    use kumochi_protocol::message::{DataToShell, Deleted, PageOf};

    for (incarnation, run) in [(Incarnation(1), RunId(2)), (Incarnation(2), RunId(1))] {
        let (owner, data, control) = ready(cx);
        owner.update(cx, |owner, _| {
            owner
                .runs
                .submit("old connection", |_| true)
                .expect("old turn");
        });
        receive(
            &owner,
            vec![LinkEvent::RunState {
                generation: 1,
                run: RunId(1),
                thread: ThreadId(1),
                phase: Phase::Thinking,
                error: None,
                origin: Some(RunOrigin::Typed { turn: 1 }),
            }],
            cx,
        );
        receive(
            &owner,
            vec![
                LinkEvent::State(LinkState::Reconnecting),
                LinkEvent::Connected(Session::detached(2, incarnation)),
            ],
            cx,
        );
        owner.update(cx, |owner, _| {
            owner.runs.settings(ThreadId(1));
            owner
                .runs
                .apply_scenes(&[])
                .expect("new connection membership");
            assert!(owner.runs.select_thread(None));
            owner
                .runs
                .submit("new session text", |_| true)
                .expect("reused turn 1");
            assert!(owner.runs.confirm_delete(ThreadId(9)));
            assert_eq!(owner.runs.deleted(ThreadId(9), Deleted::Removed, None), 0);
            assert_eq!(owner.runs.apply_data(DataToShell::RunEnd { run }, 17), 0);
        });
        draft(&owner, "", cx);
        while data.try_recv().is_ok() {}
        while control.try_recv().is_ok() {}
        owner.update(cx, |owner, cx| {
            owner.text.as_mut().expect("typing fixture").typing.open();
            owner.chat.update(cx, |chat, cx| {
                chat.show_for(None);
                cx.notify();
            });
            assert!(owner.text_open());
            let chat = owner.chat.read(cx);
            assert_eq!(chat.open, None);
            assert!(chat.listing(), "a shown normal history read is waiting");
            assert!(!chat.reading());
            assert_eq!(owner.activity, Activity::default());
            let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
            assert_eq!(
                (view.target, view.stop, view.retry),
                (Target::Kumo, false, false)
            );
            assert_eq!(view.recovery, None);
            assert_eq!(view.notice, None);
        });
        receive(
            &owner,
            vec![
                LinkEvent::TurnRefused {
                    generation: 1,
                    turn: 1,
                    thread: None,
                },
                LinkEvent::RunState {
                    generation: 1,
                    run,
                    thread: ThreadId(7),
                    phase: Phase::Failed,
                    error: Some(ErrorKey::SessionEnded.into()),
                    origin: Some(RunOrigin::Typed { turn: 1 }),
                },
            ],
            cx,
        );
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.runs.kept(), [16]);
            assert_eq!(owner.runs.target(), Target::New);
            assert!(owner.runs.holds(ThreadId(9)));
            assert_eq!(owner.chat_notice.shown(), None);
            assert_eq!(owner.session.as_ref().map(Session::granted), Some(0));
            assert!(owner.text_open());
            assert_eq!(owner.activity, Activity::default());
            assert_eq!(owner.chat.read(cx).open, None);
            assert!(owner.chat.read(cx).listing());
            assert!(!owner.chat.read(cx).reading());
            let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
            assert_eq!(
                (view.target, view.stop, view.retry),
                (Target::Kumo, false, false)
            );
            assert_eq!(view.recovery, None);
            assert_eq!(view.notice, None);
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
        });
        assert!(
            data.is_empty(),
            "no history/delete work for old-generation events"
        );
        assert!(control.is_empty(), "no Stop for old-generation events");

        receive(
            &owner,
            vec![LinkEvent::TurnRefused {
                generation: 2,
                turn: 99,
                thread: None,
            }],
            cx,
        );
        assert_eq!(
            data.try_recv()
                .expect("current event permits the waiting history read"),
            DataToBrain::PageRequest {
                request: 1,
                page: PageOf::Threads {
                    search: None,
                    after: None
                },
            }
        );
        assert!(data.is_empty(), "exactly one waiting history request");
        assert!(control.is_empty());
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.runs.kept(), [16]);
            assert_eq!(owner.runs.target(), Target::New);
            assert!(owner.runs.holds(ThreadId(9)));
            assert_eq!(owner.session.as_ref().map(Session::granted), Some(0));
            assert_eq!(
                owner.activity,
                Activity {
                    working: true,
                    ..Activity::default()
                }
            );
            let view = owner.text.as_ref().expect("typing fixture").view.read(cx);
            assert_eq!(
                (view.target, view.stop, view.retry),
                (Target::New, false, false)
            );
            assert_eq!(view.recovery, None);
            assert_eq!(view.notice, None);
        });

        receive(
            &owner,
            vec![LinkEvent::RunState {
                generation: 2,
                run,
                thread: ThreadId(7),
                phase: Phase::Failed,
                error: Some(ErrorKey::SessionEnded.into()),
                origin: Some(RunOrigin::Typed { turn: 1 }),
            }],
            cx,
        );
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.runs.kept(), Vec::<usize>::new());
            assert_eq!(owner.runs.target(), Target::Task(ThreadId(7)));
            assert!(!owner.runs.holds(ThreadId(9)));
            assert!(
                !owner.runs.working(),
                "both state and held RunEnd have completed"
            );
            assert_eq!(owner.session.as_ref().map(Session::granted), Some(17));
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                "new session text"
            );
        });
        draft(&owner, "leave this newer draft", cx);
        receive(
            &owner,
            vec![LinkEvent::RunState {
                generation: 2,
                run,
                thread: ThreadId(7),
                phase: Phase::Failed,
                error: Some(ErrorKey::SessionEnded.into()),
                origin: None,
            }],
            cx,
        );
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.session.as_ref().map(Session::granted), Some(17));
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                "leave this newer draft"
            );
        });
    }
}

fn retry_failure(thread: u64, turn: u32) -> Vec<LinkEvent> {
    vec![
        LinkEvent::RunState {
            generation: 1,
            origin: Some(RunOrigin::Typed { turn }),
            run: RunId(1),
            thread: ThreadId(thread),
            phase: Phase::Answering,
            error: None,
        },
        LinkEvent::RunState {
            generation: 1,
            origin: None,
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
    receive(&owner, retry_failure(9, 1), cx);
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
            generation: 1,
            origin: Some(RunOrigin::Typed { turn: 2 }),
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
fn settings_only_waiting_changes_disable_and_reenable_the_retained_run_retry(
    cx: &mut TestAppContext,
) {
    let (owner, data, control) = ready(cx);
    draft(&owner, "settings-only retry", cx);
    owner.update(cx, |owner, cx| owner.submit(cx).expect("original turn"));
    assert_eq!(
        data.try_recv().expect("original turn"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "settings-only retry".into(),
        }
    );
    receive(&owner, retry_failure(1, 1), cx);
    cx.run_until_parked();
    let (window, settings) = owner.read_with(cx, |owner, _| {
        (
            owner.text.as_ref().expect("typing fixture").window,
            owner.settings.clone(),
        )
    });
    settings.update(cx, |settings, cx| {
        settings.accounts.waiting = Some(AccountOp::SignIn);
        cx.notify();
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    assert!(data.is_empty());
    assert!(control.is_empty());
    owner.read_with(cx, |owner, _| assert!(owner.runs.retry_ready(RunId(1))));
    settings.update(cx, |settings, cx| {
        settings.accounts.waiting = None;
        cx.notify();
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    assert_eq!(
        data.try_recv().expect("reenabled retry"),
        DataToBrain::TextSubmit {
            turn: 2,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "settings-only retry".into(),
        }
    );
    assert!(data.is_empty());
    assert!(control.is_empty());
}

#[gpui::test]
fn settings_only_waiting_clear_reenables_sign_in_without_a_conversation_update(
    cx: &mut TestAppContext,
) {
    let (owner, data, control) = ready(cx);
    let (window, settings) = owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |settings, cx| {
            settings.accounts.view = Some(AccountView::SignedOut {
                revocation_unconfirmed: false,
            });
            settings.accounts.waiting = Some(AccountOp::SignIn);
            cx.notify();
        });
        owner.chat_notice.present(
            UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn])
                .expect("bounded sign-in recovery"),
            Source::Account,
        );
        owner.refresh_conversation(cx);
        (
            owner.text.as_ref().expect("typing fixture").window,
            owner.settings.clone(),
        )
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    assert!(control.is_empty());
    settings.update(cx, |settings, cx| {
        settings.accounts.waiting = None;
        cx.notify();
    });
    click_recovery(window, "chat-recovery-action-0", cx);
    assert_eq!(
        control.try_recv().expect("reenabled sign-in"),
        ControlToBrain::AccountOp(AccountOp::SignIn)
    );
    assert!(control.is_empty());
    assert!(data.is_empty());
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
        let notified = Rc::clone(&notified);
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
    keyboard_recovery_returns_composer(RecoveryControl::Retry, cx);
}

#[gpui::test]
fn keyboard_dismiss_returns_typing_and_escape_to_the_composer(cx: &mut TestAppContext) {
    keyboard_recovery_returns_composer(RecoveryControl::Dismiss, cx);
}

#[derive(Clone, Copy)]
enum RecoveryControl {
    Retry,
    Dismiss,
}

fn keyboard_recovery_returns_composer(control: RecoveryControl, cx: &mut TestAppContext) {
    let retry = matches!(control, RecoveryControl::Retry);
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
    receive(&owner, retry_failure(1, 1), cx);
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
        receive(&owner, retry_failure(9, 1), cx);
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
        receive(&owner, retry_failure(1, 1), cx);
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
                request: 2,
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
        DataToShell::PageFailed { request: 2 },
        &[
            DataToBrain::PageRequest {
                request: 3,
                page: PageOf::Threads {
                    search: None,
                    after: None,
                },
            },
            DataToBrain::PageRequest {
                request: 4,
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
            request: 2,
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
            request: 3,
            page: PageOf::Events {
                thread: ThreadId(3),
                before: Some(EventId(1)),
            },
        }],
        cx,
    );
}

fn opened_history(
    first: kumochi_protocol::message::DataToShell,
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    gpui_kit::AnyWindowHandle,
    async_channel::Receiver<DataToBrain>,
) {
    use kumochi_protocol::message::DataToShell;
    let (owner, data, _) = ready(cx);
    let window = owner.update(cx, |owner, cx| {
        assert!(owner.runs.select_thread(Some(ThreadId(3))));
        owner.chat.update(cx, |chat, cx| {
            chat.show_for(Some(ThreadId(3)));
            cx.notify();
        });
        owner.text.as_mut().expect("typing fixture").typing.open();
        owner.runs_changed(cx).expect("focused history");
        owner.text.as_ref().expect("typing fixture").window
    });
    assert_eq!(data.len(), 2);
    while data.try_recv().is_ok() {}
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
                message: first,
                bytes: 19,
            },
        ],
        cx,
    );
    cx.run_until_parked();
    (owner, window, data)
}

#[gpui::test]
fn older_button_reveals_each_page_of_drawing_dense_history(cx: &mut TestAppContext) {
    use gpui_kit::VisualTestContext;
    use kumochi_protocol::message::{
        DataToShell, EventId, PageEvent, PageEventKind, PageOf, SceneId,
    };

    let page = |request, first, last, before: Option<u64>| DataToShell::Page {
        request,
        thread: ThreadId(3),
        events: (first..=last)
            .rev()
            .map(|id| PageEvent {
                id: EventId(id),
                run: Some(RunId(id / 10 + 1)),
                kind: match id % 5 {
                    0 => PageEventKind::User,
                    1 => PageEventKind::AssistantText,
                    _ => PageEventKind::NoVisual {
                        scene: SceneId(id / 10 + 1),
                    },
                },
                text: if id % 5 <= 1 {
                    "fixture message".into()
                } else {
                    String::new()
                },
                cut: false,
            })
            .collect(),
        before: before.map(EventId),
    };
    let (owner, window, data) = opened_history(page(2, 151, 200, Some(151)), cx);
    draft(&owner, "keep this history draft", cx);
    cx.run_until_parked();
    for (request, first, last, before) in [
        (3, 101, 150, Some(101)),
        (4, 51, 100, Some(51)),
        (5, 1, 50, None),
    ] {
        click_recovery(window, "chat-older-control", cx);
        assert_eq!(
            data.try_recv().expect("older request"),
            DataToBrain::PageRequest {
                request,
                page: PageOf::Events {
                    thread: ThreadId(3),
                    before: Some(EventId(last + 1))
                },
            }
        );
        assert!(data.is_empty());
        receive(
            &owner,
            vec![LinkEvent::Data {
                generation: 1,
                message: page(request, first, last, before),
                bytes: 19,
            }],
            cx,
        );
        cx.run_until_parked();
        let mut frame = VisualTestContext::from_window(window, cx);
        let region = frame.debug_bounds("chat-message-region").expect("messages");
        let selector = match first {
            101 => "message-event-101",
            51 => "message-event-51",
            1 => "message-event-1",
            _ => unreachable!("three fixture pages"),
        };
        let oldest = frame
            .debug_bounds(selector)
            .expect("new older message is rendered");
        assert!(
            oldest.intersects(&region),
            "the new page must be visible, not above the preserved anchor"
        );
        assert_eq!(
            frame.debug_bounds("chat-older-control").is_some(),
            before.is_some()
        );
    }
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.chat.read(cx).events.len(), 200);
        assert_eq!(owner.chat.read(cx).before, None);
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
}

#[gpui::test]
fn older_button_continues_pages_that_only_extend_a_drawing_row(cx: &mut TestAppContext) {
    use gpui_kit::VisualTestContext;
    use kumochi_protocol::message::{
        DataToShell, EventId, PageEvent, PageEventKind, PageOf, SceneId,
    };

    let page = |request, first, last, before: Option<u64>| DataToShell::Page {
        request,
        thread: ThreadId(3),
        events: (first..=last)
            .rev()
            .map(|id| PageEvent {
                id: EventId(id),
                run: Some(RunId(1)),
                kind: if id == 1 {
                    PageEventKind::User
                } else {
                    PageEventKind::NoVisual { scene: SceneId(2) }
                },
                text: String::new(),
                cut: false,
            })
            .collect(),
        before: before.map(EventId),
    };
    let (owner, window, data) = opened_history(page(2, 102, 151, Some(102)), cx);
    click_recovery(window, "chat-older-control", cx);
    for (request, first, last, before) in [(3, 52, 101, Some(52)), (4, 1, 51, None)] {
        assert_eq!(
            data.try_recv().expect("older request"),
            DataToBrain::PageRequest {
                request,
                page: PageOf::Events {
                    thread: ThreadId(3),
                    before: Some(EventId(last + 1))
                },
            }
        );
        assert!(data.is_empty());
        receive(
            &owner,
            vec![LinkEvent::Data {
                generation: 1,
                message: page(request, first, last, before),
                bytes: 19,
            }],
            cx,
        );
        cx.run_until_parked();
    }
    assert!(data.is_empty());
    let mut frame = VisualTestContext::from_window(window, cx);
    let region = frame.debug_bounds("chat-message-region").expect("messages");
    assert!(
        frame
            .debug_bounds("message-event-1")
            .expect("oldest user row")
            .intersects(&region)
    );
    assert!(frame.debug_bounds("chat-older-control").is_none());
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.chat.read(cx).events.len(), 151);
        assert_eq!(owner.chat.read(cx).before, None);
    });
}

fn renewal_run() -> LinkEvent {
    LinkEvent::RunState {
        generation: 1,
        origin: None,
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
                generation: 1,
                origin: Some(RunOrigin::Typed { turn: 1 }),
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
    receive(&owner, retry_failure(1, 1), cx);
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
            generation: 1,
            origin: Some(RunOrigin::Typed { turn: 1 }),
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
                generation: 1,
                origin: Some(RunOrigin::Typed { turn: 1 }),
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
