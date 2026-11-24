use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};

use super::{SettingsPanel, SettingsTab, SettingsView};
use crate::presence::test_fixture;

use crate::link::{Batch, LinkEvent, LinkState, Session};
use crate::presence::Presence;
use crate::sign_in_mark::SignInMark;
use gpui_kit::{AnyWindowHandle, Entity, VisualTestContext, px, size};
use kumochi_protocol::message::{
    AccountOp, AccountView, ClaudeCodeView, ClaudeVersion, ConnectionOp, ConnectionsView,
    ControlToBrain, GitHubOp, GitHubView, Incarnation, KeyView, PlanState, UserCode,
};
use kumochi_scene::text::Bounded;

fn detached_settings(
    owner: &mut Presence,
    cx: &mut gpui_kit::Context<'_, Presence>,
) -> (AnyWindowHandle, Entity<SettingsView>) {
    let model = owner.settings.clone();
    let weak = cx.weak_entity();
    let events = owner.events.clone();
    let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
        cx.new(|cx| SettingsView::new(model, weak, events, window, cx))
    })
    .expect("headless settings window");
    owner.settings_panel = Some(SettingsPanel {
        window,
        view: view.clone(),
        visible: false,
        focused: false,
        bounds: [0; 4],
        previous: None,
        activation: None,
    });
    (window, view)
}

#[gpui::test]
fn a_failed_settings_hide_preserves_the_edit_and_defers_close_effects(cx: &mut TestAppContext) {
    use crate::presence::OpenError;
    use crate::settings::{Field, Update};
    use crate::strings::Text;
    use kumochi_protocol::message::{AssistantName, SettingsOp};

    test_fixture::without_desktop(cx);
    let owner = test_fixture::presence(cx);
    let (window, view, controls) = owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(1, Incarnation(1)));
        owner.settings_chatgpt.sent(1);
        owner.settings.update(cx, |state, _| {
            assert_eq!(
                state.set_view(super::super::general::tests::saved(), Update::Unsolicited),
                None
            );
            state.change(Field::Assistant, "Cirrus");
        });
        let (window, view) = detached_settings(owner, cx);
        let panel = owner.settings_panel.as_mut().expect("detached Settings");
        panel.visible = true;
        panel.focused = true;
        (window, view, owner.link.record_controls())
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.editing = Some(Field::Assistant);
                view.hold_notice = Some(Text::SettingsUnavailable);
                view.inputs[0].update(cx, |input, cx| {
                    input.set_value("Cirrus", window, cx);
                    input.focus(window, cx);
                });
            });
            let focus = window.focused(cx).expect("focused editor");
            assert!(matches!(
                owner.update(cx, Presence::dismiss_settings),
                Err(OpenError::Gpui(_))
            ));
            assert_eq!(window.focused(cx), Some(focus));
        })
        .expect("the Settings window is still open");
    owner.read_with(cx, |owner, cx| {
        assert!(owner.settings_focused());
        assert_eq!(
            owner.settings_chatgpt,
            SignInMark::Open {
                generation: 1,
                shown: false
            }
        );
        let state = owner.settings.read(cx);
        assert_eq!(state.value(Field::Assistant), "Cirrus");
        assert!(!state.field(Field::Assistant).saving);
        let view = view.read(cx);
        assert_eq!(view.editing(), Some(Field::Assistant));
        assert_eq!(view.hold_notice, Some(Text::SettingsUnavailable));
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));

    owner.update(cx, |owner, cx| {
        owner.dismiss_settings(cx).expect("retry close")
    });
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    view.read_with(cx, |view, _| {
        assert_eq!(view.editing(), None);
        assert_eq!(view.hold_notice, None);
    });
    assert_eq!(
        drain(&controls),
        [
            ControlToBrain::AccountOp(AccountOp::CancelSignIn),
            ControlToBrain::Settings(SettingsOp::SetAssistantName(
                AssistantName::new("Cirrus").expect("valid name")
            )),
        ]
    );
    window
        .update(cx, |_, window, _| window.remove_window())
        .expect("remove the detached Settings window");
    drop(view);
    drop(owner);
    cx.run_until_parked();
}

fn settings_over_hold(
    owner: &Entity<Presence>,
    tab: SettingsTab,
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<SettingsView>) {
    let (window, view) = owner.update(cx, |owner, cx| {
        assert!(owner.activity.working && owner.voice_working());
        let (window, view) = detached_settings(owner, cx);
        let panel = owner.settings_panel.as_mut().unwrap();
        panel.visible = true;
        panel.focused = true;
        owner.panel_visibility = crate::presence::PanelVisibility::Hidden;
        view.update(cx, |view, _| {
            view.tab = tab;
            view.stop = owner.escape_hold.shows_stop();
        });
        (window, view)
    });
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.simulate_resize(size(px(768.), px(1600.)));
    frame.update(|window, cx| {
        assert!(view.read(cx).stop);
        view.update(cx, |view, cx| view.focus(window, cx));
    });
    (window, view)
}

#[gpui::test]
fn cancelling_a_loading_speech_model_ends_the_waiting_hold_and_keeps_settings_focused(
    cx: &mut TestAppContext,
) {
    use crate::presence::voice_wiring_tests::{hold, ready};
    use crate::strings::Text;
    use kumochi_protocol::message::{GestureId, PartState, SettingsOp, VoiceView};
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    test_fixture::without_desktop(cx);
    let (owner, data, controls) = ready(cx);
    account_views(
        &owner,
        vec![LinkEvent::Voice {
            generation: 1,
            view: VoiceView {
                speech_input: PartState::Loading,
                spoken_replies: PartState::Ready,
            },
        }],
        cx,
    );
    owner.update(cx, |owner, _| {
        owner.panel_visibility = crate::presence::PanelVisibility::Visible;
    });
    let (token, _) = hold(&owner, 1, cx);
    let gesture = GestureId(1);
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized { gesture: id, .. } if id == gesture
    ));
    let (window, view) = settings_over_hold(&owner, SettingsTab::Voice, cx);
    VisualTestContext::from_window(window, cx).update(|window, cx| {
        window.draw(cx).clear(cx);
        window.focus_next(cx);
        assert!(!view.read(cx).focus.is_focused(window));
    });
    click_github(window, "speech-model-action", cx);
    assert!(!view.read_with(cx, |view, _| view.stop));
    owner.read_with(cx, |owner, cx| {
        assert!(!owner.activity.working);
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
        assert!(owner.settings.read(cx).voice_setup_pending(SpeechInput));
    });
    let messages = drain(&controls);
    assert_eq!(
        messages
            .into_iter()
            .filter(|message| message != &ControlToBrain::GestureCancel { gesture })
            .collect::<Vec<_>>(),
        [
            ControlToBrain::Settings(SettingsOp::CancelVoiceInstall),
            ControlToBrain::VoiceHoldStopped { gesture },
        ]
    );
    assert!(token.retired());
    assert!(data.is_empty());
    VisualTestContext::from_window(window, cx)
        .update(|window, cx| assert!(view.read(cx).focus.is_focused(window)));
}

fn github_panel(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<Presence>,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let (window, view) = super::super::general::tests::open(cx);
    let settings_view = view.clone();
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        view.tab = SettingsTab::Accounts;
        owner.update(cx, |owner, _| {
            view.events = owner.events.clone();
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.settings_panel = Some(SettingsPanel {
                window,
                view: settings_view.clone(),
                visible: true,
                focused: true,
                bounds: [0; 4],
                previous: None,
                activation: None,
            });
            owner.link.record_controls()
        })
    });
    {
        let mut frame = VisualTestContext::from_window(window, cx);
        frame.simulate_resize(size(px(768.), px(1600.)));
        frame.update(|window, cx| view.update(cx, |view, cx| view.focus(window, cx)));
    }
    receive_github(&owner, GitHubView::SignedOut { cleanup: None }, cx);
    (window, owner, controls)
}

fn receive_github(owner: &Entity<Presence>, view: GitHubView, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view,
                }]),
                cx,
            )
            .expect("current GitHub snapshot");
    });
    cx.run_until_parked();
}

fn click_github(window: AnyWindowHandle, selector: &'static str, cx: &mut TestAppContext) {
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = frame
        .debug_bounds(selector)
        .expect("production Settings control");
    frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    frame.run_until_parked();
}

fn start_github(
    window: AnyWindowHandle,
    controls: &async_channel::Receiver<ControlToBrain>,
    cx: &mut TestAppContext,
) {
    click_github(window, "github-connect", cx);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::Connect))
    );
}

fn challenge() -> GitHubView {
    GitHubView::AwaitingUser {
        user_code: UserCode::new("ABCD-EFGH").unwrap(),
        expires_in_s: 900,
    }
}

#[derive(Clone, Copy)]
enum Dismissal {
    Escape,
    Close,
}

fn dismiss_challenge(dismissal: Dismissal, cx: &mut TestAppContext) {
    let (window, owner, controls) = github_panel(cx);
    start_github(window, &controls, cx);
    receive_github(&owner, challenge(), cx);
    if matches!(dismissal, Dismissal::Close) {
        click_github(window, "close-settings", cx);
    } else {
        {
            let mut frame = VisualTestContext::from_window(window, cx);
            frame.simulate_keystrokes("escape");
        }
        owner.update(cx, |owner, cx| {
            let event = owner
                .events_in
                .try_recv()
                .expect("the production Escape action");
            assert!(matches!(event, crate::presence::PresenceEvent::Escape));
            owner.on_event(event, cx).expect("Escape dismissal");
        });
        cx.run_until_parked();
    }
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::CancelConnect))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn escape_dismissal_cancels_the_displayed_interactive_challenge(cx: &mut TestAppContext) {
    dismiss_challenge(Dismissal::Escape, cx);
}

#[gpui::test]
fn close_dismissal_cancels_the_displayed_interactive_challenge(cx: &mut TestAppContext) {
    dismiss_challenge(Dismissal::Close, cx);
}

#[gpui::test]
fn dismissal_cancels_connect_before_the_first_view(cx: &mut TestAppContext) {
    let (window, owner, controls) = github_panel(cx);
    start_github(window, &controls, cx);
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::CancelConnect))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn dismissal_never_cancels_background_verification_or_renewal(cx: &mut TestAppContext) {
    for renewing in [false, true] {
        let (window, owner, controls) = github_panel(cx);
        if renewing {
            receive_github(
                &owner,
                GitHubView::Connected {
                    login: kumochi_protocol::message::GitHubLogin::new("saved-owner").unwrap(),
                    repos: Bounded::default(),
                    default: None,
                    truncated: false,
                },
                cx,
            );
        }
        receive_github(&owner, GitHubView::Requesting, cx);
        click_github(window, "close-settings", cx);
        assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    }
}

#[gpui::test]
fn only_a_still_visible_accounts_visit_lists_on_the_first_handshake(cx: &mut TestAppContext) {
    #[derive(Debug)]
    enum Visit {
        Never,
        Open,
        LeftTab,
        ClosedSettings,
    }

    let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
    let _cursor = crate::platform::cursor_fixture::scoped([100, 100]);
    for (visit, expected) in [
        (Visit::Never, None),
        (Visit::Open, Some(GitHubOp::ListReposIfUnlisted)),
        (Visit::LeftTab, None),
        (Visit::ClosedSettings, None),
    ] {
        let owner = test_fixture::presence(cx);
        let (window, controls) = owner.update(cx, |owner, cx| {
            let (window, view) = detached_settings(owner, cx);
            view.update(cx, |view, _| view.github_configured = Some(true));
            owner.settings_panel.as_mut().unwrap().visible = true;
            let controls = owner.link.record_controls();
            if !matches!(visit, Visit::Never) {
                owner.open_accounts(cx).unwrap();
            }
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::State(LinkState::Connecting)]),
                    cx,
                )
                .unwrap();
            match visit {
                Visit::LeftTab => owner.open_general(cx).unwrap(),
                Visit::ClosedSettings => owner.dismiss_settings(cx).unwrap(),
                Visit::Never | Visit::Open => {}
            }
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Connected(Session::detached(
                        7,
                        Incarnation(1),
                    ))]),
                    cx,
                )
                .unwrap();
            (window, controls)
        });
        let github_ops = || {
            std::iter::from_fn(|| controls.try_recv().ok())
                .take(controls.len())
                .filter_map(|control| match control {
                    ControlToBrain::GitHub(op) => Some(op),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        receive_github(&owner, GitHubView::Requesting, cx);
        assert!(github_ops().is_empty(), "{visit:?}");
        receive_github(
            &owner,
            GitHubView::ConnectedUnlisted {
                login: kumochi_protocol::message::GitHubLogin::new("saved-owner").unwrap(),
                listing: false,
            },
            cx,
        );
        assert_eq!(github_ops(), expected.as_slice(), "{visit:?}");
        drop(owner);
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}

#[gpui::test]
fn repository_refresh_follows_accounts_entry_not_snapshots_or_ordinary_renewal(
    cx: &mut TestAppContext,
) {
    let (_, owner, controls) = github_panel(cx);
    let login = kumochi_protocol::message::GitHubLogin::new("saved-owner").unwrap();
    let unlisted = || GitHubView::ConnectedUnlisted {
        login: login.clone(),
        listing: false,
    };
    owner.update(cx, Presence::open_general).unwrap();
    receive_github(&owner, unlisted(), cx);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));

    receive_github(&owner, GitHubView::Requesting, cx);
    owner.update(cx, Presence::open_accounts).unwrap();
    cx.run_until_parked();
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    receive_github(&owner, unlisted(), cx);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::ListReposIfUnlisted))
    );
    receive_github(
        &owner,
        GitHubView::ConnectedUnlisted {
            login: login.clone(),
            listing: true,
        },
        cx,
    );
    receive_github(
        &owner,
        GitHubView::Connected {
            login: login.clone(),
            repos: Bounded::default(),
            default: None,
            truncated: false,
        },
        cx,
    );
    owner.update(cx, Presence::open_accounts).unwrap();
    receive_github(&owner, GitHubView::Requesting, cx);
    receive_github(&owner, unlisted(), cx);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));

    owner.update(cx, Presence::open_general).unwrap();
    receive_github(&owner, GitHubView::Requesting, cx);
    owner.update(cx, Presence::open_accounts).unwrap();
    owner.update(cx, Presence::open_general).unwrap();
    receive_github(&owner, unlisted(), cx);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(cx, Presence::open_accounts).unwrap();
    cx.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::ListReposIfUnlisted))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn a_renewal_before_repository_listing_acknowledgment_still_serves_the_open_visit(
    cx: &mut TestAppContext,
) {
    let (window, owner, controls) = github_panel(cx);
    let unlisted = |listing| GitHubView::ConnectedUnlisted {
        login: kumochi_protocol::message::GitHubLogin::new("saved-owner").unwrap(),
        listing,
    };
    let list = Ok(ControlToBrain::GitHub(GitHubOp::ListReposIfUnlisted));
    owner.update(cx, Presence::open_general).unwrap();
    receive_github(&owner, unlisted(false), cx);
    owner.update(cx, Presence::open_accounts).unwrap();
    cx.run_until_parked();
    assert_eq!(controls.try_recv(), list);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 7,
                        view: GitHubView::Requesting,
                    },
                    LinkEvent::GitHub {
                        generation: 7,
                        view: unlisted(false),
                    },
                ]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    assert_eq!(controls.try_recv(), list);
    receive_github(&owner, unlisted(true), cx);
    click_github(window, "github-sign-out", cx);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::SignOut))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn dismissal_preserves_an_interactive_sign_in_that_has_completed(cx: &mut TestAppContext) {
    let (window, owner, controls) = github_panel(cx);
    start_github(window, &controls, cx);
    receive_github(
        &owner,
        GitHubView::Connected {
            login: kumochi_protocol::message::GitHubLogin::new("saved-owner").unwrap(),
            repos: Bounded::default(),
            default: None,
            truncated: false,
        },
        cx,
    );
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    assert!(!controls.is_closed());
}

#[gpui::test]
fn terminal_batches_retire_interactive_provenance_before_background_work(cx: &mut TestAppContext) {
    use crate::link::LinkState;
    use kumochi_protocol::message::{ErrorKey, GitHubLogin};

    let login = GitHubLogin::new("saved-owner").unwrap();
    let terminals = vec![
        LinkEvent::GitHub {
            generation: 7,
            view: GitHubView::Connected {
                login: login.clone(),
                repos: Bounded::default(),
                default: None,
                truncated: false,
            },
        },
        LinkEvent::GitHub {
            generation: 7,
            view: GitHubView::SignedOut { cleanup: None },
        },
        LinkEvent::GitHub {
            generation: 7,
            view: GitHubView::NeedsSignIn {
                login: login.clone(),
            },
        },
        LinkEvent::GitHub {
            generation: 7,
            view: GitHubView::NotInstalled {
                login: login.clone(),
            },
        },
        LinkEvent::GitHub {
            generation: 7,
            view: GitHubView::NotVerified { login },
        },
        LinkEvent::GitHubError {
            generation: 7,
            error: ErrorKey::SignInFailed.into(),
        },
        LinkEvent::Connected(Session::detached(8, Incarnation(1))),
        LinkEvent::State(LinkState::Reconnecting),
    ];
    for terminal in terminals {
        let (window, owner, controls) = github_panel(cx);
        start_github(window, &controls, cx);
        receive_github(&owner, challenge(), cx);
        let interactive = owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![
                        terminal,
                        LinkEvent::GitHub {
                            generation: 7,
                            view: GitHubView::Requesting,
                        },
                    ]),
                    cx,
                )
                .unwrap();
            owner
                .settings_panel
                .as_ref()
                .unwrap()
                .view
                .read(cx)
                .github_interactive
        });
        click_github(window, "close-settings", cx);
        assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        assert!(!controls.is_closed());
        assert_eq!(interactive, None, "ownership retires before observers run");
    }
}

#[gpui::test]
fn a_fresh_connect_keeps_interactive_provenance_after_terminal_delivery(cx: &mut TestAppContext) {
    use kumochi_protocol::message::GitHubLogin;

    for terminal in [
        GitHubView::SignedOut { cleanup: None },
        GitHubView::NeedsSignIn {
            login: GitHubLogin::new("saved-owner").unwrap(),
        },
    ] {
        let (window, owner, controls) = github_panel(cx);
        start_github(window, &controls, cx);
        receive_github(&owner, challenge(), cx);
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::GitHub {
                        generation: 7,
                        view: terminal.clone(),
                    }]),
                    cx,
                )
                .unwrap();
            assert!(owner.github_op(GitHubOp::Connect, cx));
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::GitHub {
                        generation: 7,
                        view: terminal,
                    }]),
                    cx,
                )
                .unwrap();
        });
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GitHub(GitHubOp::Connect))
        );
        cx.run_until_parked();
        assert_eq!(
            owner.read_with(cx, |owner, cx| {
                owner
                    .settings_panel
                    .as_ref()
                    .unwrap()
                    .view
                    .read(cx)
                    .github_interactive
            }),
            Some(7)
        );
        click_github(window, "close-settings", cx);
        assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GitHub(GitHubOp::CancelConnect))
        );
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        assert!(!controls.is_closed());
    }
}

#[gpui::test]
fn old_session_terminals_cannot_clear_current_interactive_provenance(cx: &mut TestAppContext) {
    use kumochi_protocol::message::ErrorKey;

    let (window, owner, controls) = github_panel(cx);
    start_github(window, &controls, cx);
    receive_github(&owner, challenge(), cx);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 6,
                        view: GitHubView::SignedOut { cleanup: None },
                    },
                    LinkEvent::GitHubError {
                        generation: 6,
                        error: ErrorKey::SignInFailed.into(),
                    },
                ]),
                cx,
            )
            .unwrap();
    });
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::GitHub(GitHubOp::CancelConnect))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    assert!(!controls.is_closed());
}

#[gpui::test]
fn dismissal_with_a_full_control_queue_keeps_the_error_and_ends_the_link(cx: &mut TestAppContext) {
    let (window, owner, controls) = github_panel(cx);
    start_github(window, &controls, cx);
    receive_github(&owner, challenge(), cx);
    let controls = owner.update(cx, |owner, _| {
        let controls = owner.link.record_controls_bounded();
        let session = owner.session.as_ref().unwrap();
        assert_eq!(controls.capacity(), Some(64));
        for _ in 0..64 {
            assert!(owner.link.send_control(session, ControlToBrain::Pong));
        }
        assert!(controls.is_full());
        controls
    });
    click_github(window, "github-cancel", cx);
    assert!(owner.read_with(cx, |owner, _| owner.settings_open()));
    assert!(
        !controls.is_closed(),
        "the visible Cancel button remains retryable"
    );
    assert!(owner.read_with(cx, |owner, cx| owner.settings.read(cx).github.send_failed));
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert!(
        controls.is_closed(),
        "the visibility fallback ends the stalled connection"
    );
    assert!(owner.read_with(cx, |owner, cx| owner.settings.read(cx).github.send_failed));
}

#[gpui::test]
fn rejected_speech_input_keeps_the_settings_panel_open(cx: &mut TestAppContext) {
    use crate::input::TalkHeld;
    use crate::input::hold::Intent;
    use crate::presence::voice::Stamp;
    use crate::strings::Text;
    use kumochi_protocol::message::{GestureId, PartState};

    let (owner, _data, _controls) = crate::presence::voice_wiring_tests::ready(cx);
    owner.update(cx, |owner, cx| {
        let model = owner.settings.clone();
        let weak = cx.weak_entity();
        let events = owner.events.clone();
        let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(model, weak, events, window, cx))
        })
        .unwrap();
        owner.settings_panel = Some(SettingsPanel {
            window,
            view,
            visible: true,
            focused: true,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner.settings.update(cx, |settings, _| {
            settings.receive_voice(
                Some(1),
                1,
                kumochi_protocol::message::VoiceView {
                    speech_input: PartState::NotInstalled,
                    spoken_replies: PartState::Ready,
                },
            );
        });
        assert!(owner.settings_open());
        crate::presence::voice_wiring_tests::gate(owner, 1, cx);
        let token = owner.link.microphone_gate().arm(GestureId(1)).unwrap();
        owner.link.track_input(GestureId(1));
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture: GestureId(1),
                generation: Ok(token),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert!(owner.settings_open());
        assert!(owner.settings_focused());
        assert_eq!(owner.voice.status, Some(Text::VoiceNotInstalled));
        assert!(owner.voice.pending.is_none());
    });
}

#[gpui::test]
fn a_stale_or_output_only_setup_cancel_does_not_revoke_the_current_hold(cx: &mut TestAppContext) {
    use crate::presence::voice_wiring_tests::{hold, ready};
    use kumochi_protocol::message::{GestureId, PartState, SettingsOp, VoiceView};
    use kumochi_protocol::voice::VoicePart::SpokenReplies;

    test_fixture::without_desktop(cx);
    let (owner, _data, controls) = ready(cx);
    hold(&owner, 1, cx);
    while controls.try_recv().is_ok() {}
    owner.update(cx, |owner, cx| {
        let view = |spoken_replies| VoiceView {
            speech_input: PartState::Ready,
            spoken_replies,
        };
        let downloading = view(PartState::Downloading { done: 1, total: 2 });
        let cancel = owner.settings.update(cx, |settings, _| {
            assert!(settings.receive_voice(Some(1), 1, downloading));
            let cancel = settings.voice_setup_intent(SpokenReplies).expect("cancel");
            assert!(settings.receive_voice(Some(1), 1, view(PartState::Ready)));
            cancel
        });
        owner.voice_setup_action(cancel, cx);
        assert!(controls.is_empty());
        assert!(owner.link.input_is(GestureId(1)));
        assert!(owner.voice.pending.is_some());

        let cancel = owner.settings.update(cx, |settings, _| {
            assert!(settings.receive_voice(Some(1), 1, downloading));
            settings
                .voice_setup_intent(SpokenReplies)
                .expect("current cancel")
        });
        owner.voice_setup_action(cancel, cx);
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::Settings(SettingsOp::CancelVoiceInstall))
        );
        assert!(controls.is_empty());
        assert!(owner.link.input_is(GestureId(1)));
        assert!(owner.voice.pending.is_some());
    });
}

#[gpui::test]
fn recovery_settings_clicks_open_the_exact_tab_through_the_hidden_panel_path(
    cx: &mut TestAppContext,
) {
    use crate::chat_notice::Source;
    use kumochi_protocol::message::{Action, ErrorKey, UserError};

    for (key, source, expected) in [
        (
            ErrorKey::AnthropicKeyRejected,
            Source::Account,
            SettingsTab::Accounts,
        ),
        (
            ErrorKey::SettingsRefused,
            Source::Settings(None),
            SettingsTab::General,
        ),
    ] {
        let owner = test_fixture::presence(cx);
        let (window, view) = owner.update(cx, |owner, cx| {
            let (_, view) = detached_settings(owner, cx);
            owner.chat_notice.present(
                UserError::new(key, Vec::new(), vec![Action::OpenSettings])
                    .expect("bounded settings recovery"),
                source,
            );
            owner.refresh_conversation(cx);
            assert!(!owner.settings_open());
            (owner.text.as_ref().expect("typing fixture").window, view)
        });
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            let bounds = frame
                .debug_bounds("chat-recovery-action-0")
                .expect("Settings recovery control");
            frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        }
        cx.run_until_parked();
        owner.read_with(cx, |owner, cx| {
            assert!(owner.settings_open());
            assert_eq!(view.read(cx).tab, expected);
        });
    }
}

#[gpui::test]
fn recovery_usage_click_opens_the_public_usage_destination(cx: &mut TestAppContext) {
    use gpui_kit::{MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput};
    use kumochi_protocol::message::{Action, ErrorKey, MANAGE_USAGE_URL, UserError};

    let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
    let (owner, _data, controls) = crate::presence::voice_wiring_tests::ready(cx);
    let account = || LinkEvent::Account {
        generation: 1,
        view: AccountView::SignedOut {
            revocation_unconfirmed: false,
        },
    };
    let window = owner.update(cx, |owner, cx| {
        owner.open_text(cx).expect("open chat");
        owner
            .link_events(
                Batch::Plain(vec![
                    account(),
                    LinkEvent::Error {
                        generation: 1,
                        error: UserError::new(
                            ErrorKey::UsageLimit,
                            Vec::new(),
                            vec![Action::ManageUsage],
                        )
                        .expect("bounded usage recovery"),
                    },
                ]),
                cx,
            )
            .expect("current account and usage notice");
        owner.text.as_ref().expect("typing fixture").window
    });
    cx.run_until_parked();
    let position = VisualTestContext::from_window(window, cx)
        .debug_bounds("chat-recovery-action-0")
        .expect("painted Manage usage link")
        .center();
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            assert!(owner.account_op(AccountOp::SignIn, cx));
        });
        window
            .update(cx, |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        position,
                        button: MouseButton::Left,
                        click_count: 1,
                        ..MouseDownEvent::default()
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        position,
                        button: MouseButton::Left,
                        click_count: 1,
                        ..MouseUpEvent::default()
                    }),
                    cx,
                );
            })
            .expect("click the last painted link before repaint");
    });
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::AccountOp(AccountOp::SignIn))
    );
    assert_eq!(
        cx.opened_url(),
        None,
        "the stale link cannot open while waiting"
    );

    click_github(window, "chat-recovery-action-0", cx);
    assert_eq!(cx.opened_url(), None, "the repainted link stays disabled");
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(vec![account()]), cx)
            .expect("authoritative account answer");
    });
    click_github(window, "chat-recovery-action-0", cx);
    assert_eq!(cx.opened_url().as_deref(), Some(MANAGE_USAGE_URL));
    assert!(controls.is_empty());
}

#[gpui::test]
fn recovery_openers_select_the_exact_tab_without_toggling_a_visible_window_closed(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        let (_, view) = detached_settings(owner, cx);
        owner.settings_panel.as_mut().unwrap().visible = true;
        owner.open_accounts(cx).expect("accounts tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, SettingsTab::Accounts);
        owner.open_accounts(cx).expect("accounts stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, SettingsTab::Accounts);
        owner.open_general(cx).expect("general tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, SettingsTab::General);
        owner.open_general(cx).expect("general stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, SettingsTab::General);
    });
}

#[gpui::test]
fn open_settings_in_the_chat_recovery_swaps_the_chat_for_settings(cx: &mut TestAppContext) {
    use gpui_kit::{MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput};
    use kumochi_protocol::message::{Action, ErrorKey, UserError};
    let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (window, view) = super::super::general::tests::open(cx);
    let notice = |key, action| {
        Batch::Plain(vec![LinkEvent::Error {
            generation: 7,
            error: UserError::new(key, Vec::new(), vec![action]).expect("bounded error"),
        }])
    };
    let opener = || notice(ErrorKey::ConnectionNeeded, Action::OpenSettings);
    let chat = owner.update(cx, |owner, cx| {
        owner.settings_panel = Some(SettingsPanel {
            window,
            view: view.clone(),
            visible: false,
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner.open_text(cx).expect("open chat");
        owner.link_events(opener(), cx).expect("current opener");
        assert!(owner.text_open());
        assert!(!owner.settings_open());
        owner.text.as_ref().expect("typing fixture").window
    });
    cx.run_until_parked();
    let position = VisualTestContext::from_window(chat, cx)
        .debug_bounds("chat-recovery-action-0")
        .expect("painted Accounts opener")
        .center();
    cx.update(|cx| {
        owner.update(cx, |owner, cx| {
            owner
                .link_events(notice(ErrorKey::TrustStoreUnavailable, Action::SignIn), cx)
                .expect("replace the opener before the next paint");
        });
        chat.update(cx, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 1,
                    ..MouseDownEvent::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 1,
                    ..MouseUpEvent::default()
                }),
                cx,
            );
        })
        .expect("click the opener on the last painted frame");
    });
    owner.update(cx, |owner, cx| {
        assert!(
            !owner.settings_open(),
            "the stale click must not open Settings"
        );
        assert!(owner.text_open());
        owner
            .link_events(opener(), cx)
            .expect("fresh Accounts opener");
    });
    cx.run_until_parked();
    {
        let mut frame = VisualTestContext::from_window(chat, cx);
        let bounds = frame
            .debug_bounds("chat-recovery-action-0")
            .expect("Open settings in the chat");
        frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    }
    cx.run_until_parked();
    owner.read_with(cx, |owner, cx| {
        assert!(owner.settings_open(), "Settings is shown");
        assert!(!owner.text_open(), "the chat is hidden");
        assert_eq!(view.read(cx).tab, SettingsTab::Accounts);
    });
}

#[gpui::test]
fn opening_settings_from_chat_keeps_the_name_save_until_its_answer(cx: &mut TestAppContext) {
    use crate::settings::{Error, Field};
    use kumochi_protocol::message::{Action, AssistantName, ErrorKey, SettingsOp, UserError};

    let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
    let settings = |view, answer| LinkEvent::Settings {
        generation: 7,
        view,
        answer,
    };
    let open_settings = |key| UserError::new(key, vec![], vec![Action::OpenSettings]).unwrap();
    for refused in [false, true] {
        let owner = test_fixture::presence(cx);
        let mut saved = super::super::general::tests::saved();
        let (window, view, controls) = owner.update(cx, |owner, cx| {
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner
                .link_events(Batch::Plain(vec![settings(saved.clone(), false)]), cx)
                .unwrap();
            let (window, view) = detached_settings(owner, cx);
            owner.open_general(cx).unwrap();
            (window, view, owner.link.record_controls())
        });
        let mut frame = VisualTestContext::from_window(window, cx);
        frame.update(|window, _| window.activate_window());
        frame.run_until_parked();
        frame.update(|window, cx| {
            view.read(cx).inputs[0].clone().update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            window.draw(cx).clear(cx);
        });
        frame.run_until_parked();
        frame.simulate_input("Cirrus");
        let name = AssistantName::new("Cirrus").unwrap();
        let chat = owner.update(cx, |owner, cx| {
            assert_eq!(view.read(cx).editing(), Some(Field::Assistant));
            assert!(controls.is_empty(), "the name is still an active draft");
            owner.open_text(cx).unwrap();
            assert!(owner.text_open());
            assert!(!owner.settings_open());
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Error {
                        generation: 7,
                        error: open_settings(ErrorKey::AnthropicKeyRejected),
                    }]),
                    cx,
                )
                .unwrap();
            owner.text.as_ref().unwrap().window
        });
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::Settings(SettingsOp::SetAssistantName(
                name.clone()
            )))
        );
        click_github(chat, "chat-recovery-action-0", cx);
        let mut accounts = VisualTestContext::from_window(window, cx);
        accounts.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            accounts.debug_bounds("account-provider").is_some(),
            "the painted recovery must open Accounts"
        );
        let error = UserError::from(ErrorKey::SettingsRefused);
        owner.update(cx, |owner, cx| {
            assert!(owner.settings_open());
            assert!(!owner.text_open());
            let state = owner.settings.read(cx);
            assert!(state.field(Field::Assistant).saving);
            assert_eq!(state.value(Field::Assistant), "Cirrus");
            let mut reply = Vec::new();
            if refused {
                reply.push(LinkEvent::Error {
                    generation: 7,
                    error: error.clone(),
                });
            } else {
                saved.assistant_name = name;
            }
            reply.push(settings(saved, true));
            owner.link_events(Batch::Plain(reply), cx).unwrap();
            owner.open_general(cx).unwrap();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let expected = if refused { "Kumo" } else { "Cirrus" };
            let state = view.model.read(cx);
            assert_eq!(view.tab, SettingsTab::General);
            assert_eq!(view.inputs[0].read(cx).value().as_ref(), expected);
            assert_eq!(state.value(Field::Assistant), expected);
            assert!(!state.field(Field::Assistant).saving);
            assert_eq!(
                state.field(Field::Assistant).error,
                refused.then_some(Error::Brain(error))
            );
        });
        assert!(controls.is_empty(), "navigation cannot send the name twice");
    }
}

fn drain(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| controls.try_recv().ok()).collect()
}

fn account_views(owner: &Entity<Presence>, events: Vec<LinkEvent>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(events), cx)
            .expect("current account views");
    });
    cx.run_until_parked();
}

fn chatgpt(view: AccountView) -> LinkEvent {
    LinkEvent::Account {
        generation: 7,
        view,
    }
}

fn claude_code(view: ClaudeCodeView) -> LinkEvent {
    LinkEvent::Connections {
        generation: 7,
        view: ConnectionsView {
            anthropic: KeyView::Missing,
            anthropic_revision: None,
            claude_code: view,
        },
    }
}

const CLAUDE_OUT: ClaudeCodeView = ClaudeCodeView::SignedOut {
    version: ClaudeVersion {
        major: 2,
        minor: 1,
        patch: 306,
    },
};

/// Settings on Accounts, signed out of `ChatGPT` and Claude Code.
fn accounts_panel(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<Presence>,
    async_channel::Receiver<ControlToBrain>,
) {
    let (window, owner, controls) = github_panel(cx);
    account_views(
        &owner,
        vec![
            chatgpt(AccountView::SignedOut {
                revocation_unconfirmed: false,
            }),
            claude_code(CLAUDE_OUT),
        ],
        cx,
    );
    (window, owner, controls)
}

#[gpui::test]
fn account_status_hides_only_errors_the_status_already_explains(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{AccountRecovery, Cleanup, ErrorKey, UserError};

    let signed_in = |recovery| AccountView::SignedIn {
        email: None,
        plan: PlanState::Unavailable,
        recovery,
    };
    let signing_out = |cleanup| AccountView::SigningOut {
        cleanup,
        revocation_unconfirmed: false,
    };
    let sign_in_again = Some(AccountRecovery::SignInAgain);
    for (view, key, shown) in [
        (signed_in(sign_in_again), ErrorKey::SessionEnded, false),
        (signed_in(None), ErrorKey::SessionEnded, true),
        (signed_in(None), ErrorKey::PlanUnavailable, false),
        (signed_in(sign_in_again), ErrorKey::PlanUnavailable, true),
        (
            signing_out(Cleanup::NotDurable),
            ErrorKey::SignOutNotDurable,
            false,
        ),
        (
            signing_out(Cleanup::Refused),
            ErrorKey::SignOutNotDurable,
            true,
        ),
    ] {
        let (window, owner, controls) = accounts_panel(cx);
        account_views(
            &owner,
            vec![
                chatgpt(view.clone()),
                LinkEvent::Error {
                    generation: 7,
                    error: key.into(),
                },
            ],
            cx,
        );
        owner.read_with(cx, |owner, cx| {
            assert_eq!(
                owner
                    .settings
                    .read(cx)
                    .accounts
                    .error
                    .as_ref()
                    .map(UserError::key),
                Some(key)
            );
        });
        let mut frame = VisualTestContext::from_window(window, cx);
        frame.update(|window, cx| window.draw(cx).clear(cx));
        assert!(frame.debug_bounds("account-status").is_some());
        assert_eq!(
            frame.debug_bounds("chatgpt-account-error").is_some(),
            shown,
            "{view:?}, {key:?}"
        );
        assert_eq!(drain(&controls), []);
    }
}

const SIGNED_IN: AccountView = AccountView::SignedIn {
    email: None,
    plan: PlanState::Enabled,
    recovery: None,
};

#[gpui::test]
fn sign_out_discloses_possible_registration_removal_before_the_plan_changes(
    cx: &mut TestAppContext,
) {
    use gpui_kit::component::WindowExt as _;

    const DESCRIPTION: &str = concat!(
        "This stops using your account now, tries to end its session at OpenAI, and removes ",
        "its saved sign-in from Windows Credential Manager. It doesn't delete your tasks.\n\n",
        "Signing out may also remove the app registration saved on this computer. ",
        "If it does, signing in again registers the app anew.",
    );

    test_fixture::without_desktop(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, owner, controls) = accounts_panel(cx);
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    click_github(window, "account-sign-out", cx);
    let assert_warning = |cx: &mut TestAppContext| {
        let mut frame = VisualTestContext::from_window(window, cx);
        frame.update(|window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        });
        assert!(
            frame.debug_bounds(DESCRIPTION).is_some(),
            "the dialog must disclose possible app registration removal before confirmation"
        );
        assert!(controls.is_empty());
    };
    assert_warning(cx);

    account_views(
        &owner,
        vec![chatgpt(AccountView::SignedIn {
            email: None,
            plan: PlanState::Unavailable,
            recovery: None,
        })],
        cx,
    );
    assert_warning(cx);
    click_github(window, "confirmation-danger", cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignOut)]
    );
    VisualTestContext::from_window(window, cx).update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
    });
}

#[gpui::test]
fn confirming_sign_out_during_a_recording_stops_only_that_hold(cx: &mut TestAppContext) {
    use crate::presence::voice::tests::{source, until};
    use crate::presence::voice_wiring_tests::{hold, ready, status};
    use crate::strings::Text;
    use gpui_kit::component::WindowExt as _;
    use kumochi_protocol::message::{GestureId, HoldStatus};
    use std::time::Duration;

    test_fixture::without_desktop(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (owner, data, controls) = ready(cx);
    account_views(
        &owner,
        vec![LinkEvent::Account {
            generation: 1,
            view: SIGNED_IN,
        }],
        cx,
    );
    owner.update(cx, |owner, _| {
        owner.panel_visibility = crate::presence::PanelVisibility::Visible;
    });
    let (open, finish, started, samples) = source(&owner, cx, None);
    let (token, _) = hold(&owner, 1, cx);
    let gesture = GestureId(1);
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized { gesture: id, .. } if id == gesture
    ));
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldListening)
        })
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();

    let (window, view) = settings_over_hold(&owner, SettingsTab::Accounts, cx);
    click_github(window, "account-sign-out", cx);
    click_github(window, "confirmation-cancel", cx);
    assert!(controls.is_empty(), "declining must not send or stop");
    owner.read_with(cx, |owner, _| assert!(owner.voice_working()));

    click_github(window, "account-sign-out", cx);
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, _| {
            assert!(state.accounts.receive(Some(2), 2, SIGNED_IN));
        });
    });
    click_github(window, "confirmation-danger", cx);
    assert!(
        controls.is_empty(),
        "a stale confirmation must not send or stop"
    );
    owner.read_with(cx, |owner, _| assert!(owner.voice_working()));
    account_views(
        &owner,
        vec![LinkEvent::Account {
            generation: 1,
            view: SIGNED_IN,
        }],
        cx,
    );

    click_github(window, "account-sign-out", cx);
    click_github(window, "confirmation-danger", cx);
    assert_eq!(
        drain(&controls)
            .into_iter()
            .filter(|message| message != &ControlToBrain::GestureCancel { gesture })
            .collect::<Vec<_>>(),
        [ControlToBrain::AccountOp(AccountOp::SignOut)]
    );
    assert!(!view.read_with(cx, |view, _| view.stop));
    assert!(!token.retired());
    owner.read_with(cx, |owner, _| {
        assert!(!owner.activity.working);
        assert_eq!(owner.voice.status, Some(Text::HoldStopping));
    });

    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| owner.voice.pending.is_none())
    });
    assert!(token.retired());
    assert_eq!(
        drain(&controls),
        [ControlToBrain::VoiceHoldStopped { gesture }]
    );
    assert!(data.is_empty());
    VisualTestContext::from_window(window, cx).update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
        assert!(view.read(cx).focus.contains_focused(window, cx));
    });
}

#[gpui::test]
fn confirmed_sign_out_reports_a_lost_session_before_the_account_snapshot_changes(
    cx: &mut TestAppContext,
) {
    use crate::accounts::StatusLine;
    use crate::strings::Text;
    use gpui_kit::component::WindowExt as _;

    test_fixture::without_desktop(cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, owner, controls) = accounts_panel(cx);
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    click_github(window, "account-sign-out", cx);
    owner.update(cx, Presence::end_scene_connection);
    click_github(window, "confirmation-danger", cx);

    assert!(controls.is_empty());
    assert!(owner.read_with(cx, |owner, cx| {
        let settings = owner.settings.read(cx);
        let (_, lines) = settings.accounts.status(settings.view());
        lines
            .iter()
            .any(|line| matches!(line, StatusLine::SendFailed(Text::AccountSendFailed)))
    }));
    VisualTestContext::from_window(window, cx).update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
    });
}

#[gpui::test]
fn resolved_sign_out_confirmation_does_not_steal_focus_on_account_change(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt;

    for decision in [
        Some("confirmation-cancel"),
        None,
        Some("confirmation-danger"),
    ] {
        let (window, owner, controls) = accounts_panel(cx);
        account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        click_github(window, "account-sign-out", cx);
        let mut frame = VisualTestContext::from_window(window, cx);
        assert!(frame.update(WindowExt::has_active_dialog));
        if let Some(selector) = decision {
            click_github(window, selector, cx);
        } else {
            frame.simulate_keystrokes("escape");
        }
        cx.run_until_parked();
        let expected = (decision == Some("confirmation-danger"))
            .then_some(ControlToBrain::AccountOp(AccountOp::SignOut));
        assert_eq!(drain(&controls), expected.into_iter().collect::<Vec<_>>());
        assert!(owner.read_with(cx, |owner, _| owner.settings_open()));
        let view = owner.read_with(cx, |owner, _| {
            owner.settings_panel.as_ref().unwrap().view.clone()
        });
        let mut frame = VisualTestContext::from_window(window, cx);
        let focus = frame.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
            window.focus_next(cx);
            let focus = window.focused(cx).expect("a Settings control");
            assert_ne!(focus, view.read(cx).focus);
            focus
        });
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                assert!(state.accounts.receive(Some(8), 8, SIGNED_IN));
                cx.notify();
            });
        });
        cx.run_until_parked();
        let mut frame = VisualTestContext::from_window(window, cx);
        assert_eq!(frame.update(|window, cx| window.focused(cx)), Some(focus));
        assert_eq!(drain(&controls), []);
    }
}

#[gpui::test]
fn sign_out_confirmation_can_reopen_during_the_previous_exit(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt;

    let (window, owner, controls) = accounts_panel(cx);
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    cx.update(|cx| cx.set_reduce_motion(false));
    click_github(window, "account-sign-out", cx);
    cx.background_executor
        .advance_clock(std::time::Duration::from_millis(500));
    click_github(window, "confirmation-cancel", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(!window.has_active_dialog(cx));
    });
    assert!(
        frame.debug_bounds("dialog-layer").is_some(),
        "the old dialog is exiting"
    );
    assert_eq!(drain(&controls), []);

    click_github(window, "account-sign-out", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    assert!(frame.update(WindowExt::has_active_dialog));
    assert_eq!(drain(&controls), []);
    frame.simulate_keystrokes("tab enter");
    frame.run_until_parked();
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignOut)]
    );
}

#[gpui::test]
fn sign_out_confirmation_invalidation_closes_owned_dialog_with_focus_elsewhere(
    cx: &mut TestAppContext,
) {
    use gpui_kit::component::WindowExt as _;

    let (window, owner, controls) = accounts_panel(cx);
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let view = owner.read_with(cx, |owner, _| {
        owner.settings_panel.as_ref().unwrap().view.clone()
    });
    click_github(window, "account-sign-out", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    let outside = frame.update(|window, cx| {
        let outside = view.read(cx).focus.clone();
        assert_ne!(window.focused(cx), Some(outside.clone()));
        window.focus(&outside, cx);
        assert!(window.has_active_dialog(cx));
        assert_eq!(window.focused(cx), Some(outside.clone()));
        outside
    });

    account_views(
        &owner,
        vec![chatgpt(AccountView::SignedOut {
            revocation_unconfirmed: false,
        })],
        cx,
    );
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        assert!(
            !window.has_active_dialog(cx),
            "the invalid owned alert must close"
        );
        assert!(view.read(cx).confirmation.is_none());
        assert_eq!(window.focused(cx), Some(outside));
    });
    assert_eq!(drain(&controls), []);
}

#[gpui::test]
fn sign_out_confirmation_ownership_ends_on_refusal_tab_change_and_dismissal(
    cx: &mut TestAppContext,
) {
    use gpui_kit::component::WindowExt as _;

    let (window, owner, controls) = accounts_panel(cx);
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    cx.update(|cx| cx.set_reduce_motion(true));
    let view = owner.read_with(cx, |owner, _| {
        owner.settings_panel.as_ref().unwrap().view.clone()
    });
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        window.open_alert_dialog(cx, |alert, _, _| alert.title("Other"));
        let other_focus = window.focused(cx);
        view.update(cx, |view, cx| view.confirm_sign_out(window, cx));
        assert!(view.read(cx).confirmation.is_none());
        assert_eq!(window.focused(cx), other_focus);
        window.close_dialog(cx);
    });
    click_github(window, "account-sign-out", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        assert!(window.has_active_dialog(cx));
        view.update(cx, |view, cx| {
            view.select_tab(SettingsTab::General, None, window, cx);
        });
        assert!(!window.has_active_dialog(cx));
        assert!(view.read(cx).confirmation.is_none());
        view.update(cx, |view, cx| {
            view.select_tab(SettingsTab::Accounts, None, window, cx);
        });
    });
    click_github(window, "account-sign-out", cx);
    owner.update(cx, |owner, cx| {
        assert!(owner.dismiss_account_confirmation(cx));
        assert!(owner.settings_open());
    });
    assert!(view.read_with(cx, |view, _| view.confirmation.is_none()));
    let mut frame = VisualTestContext::from_window(window, cx);
    let other_focus = frame.update(|window, cx| {
        window.open_alert_dialog(cx, |alert, _, _| alert.title("Other"));
        window.focused(cx).expect("the unrelated dialog owns focus")
    });
    account_views(
        &owner,
        vec![chatgpt(AccountView::SignedOut {
            revocation_unconfirmed: false,
        })],
        cx,
    );
    VisualTestContext::from_window(window, cx).update(|window, cx| {
        assert!(window.has_active_dialog(cx));
        assert_eq!(window.focused(cx), Some(other_focus));
        window.close_dialog(cx);
    });
    account_views(&owner, vec![chatgpt(SIGNED_IN)], cx);
    click_github(window, "account-sign-out", cx);
    owner.update(cx, |owner, cx| owner.dismiss_settings(cx).unwrap());
    assert!(view.read_with(cx, |view, _| view.confirmation.is_none()));
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(drain(&controls), []);
}

/// Both sign-ins started from Settings' own buttons.
fn start_both(
    window: AnyWindowHandle,
    controls: &async_channel::Receiver<ControlToBrain>,
    cx: &mut TestAppContext,
) {
    click_github(window, "account-sign-in", cx);
    click_github(window, "claude-review", cx);
    click_github(window, "claude-connect", cx);
    assert_eq!(
        drain(controls),
        [
            ControlToBrain::AccountOp(AccountOp::SignIn),
            ControlToBrain::Connection(ConnectionOp::ConnectClaudeCode)
        ]
    );
}

const CANCELS: [ControlToBrain; 2] = [
    ControlToBrain::AccountOp(AccountOp::CancelSignIn),
    ControlToBrain::Connection(ConnectionOp::CancelClaudeSignIn),
];

/// Before the brain answers, after an answer that isn't the wait (it may predate the op), or while
/// it waits, closing Settings cancels each sign-in it started, once.
#[gpui::test]
fn closing_settings_cancels_its_own_sign_ins_once(cx: &mut TestAppContext) {
    for answer in [None, Some(false), Some(true)] {
        let (window, owner, controls) = accounts_panel(cx);
        start_both(window, &controls, cx);
        match answer {
            Some(true) => account_views(
                &owner,
                vec![
                    chatgpt(AccountView::SigningIn),
                    claude_code(ClaudeCodeView::SigningIn),
                ],
                cx,
            ),
            Some(false) => account_views(
                &owner,
                vec![
                    chatgpt(AccountView::SignedOut {
                        revocation_unconfirmed: false,
                    }),
                    claude_code(CLAUDE_OUT),
                ],
                cx,
            ),
            None => {}
        }
        click_github(window, "close-settings", cx);
        assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
        assert_eq!(drain(&controls), CANCELS, "waiting: {answer:?}");
        account_views(
            &owner,
            vec![
                chatgpt(AccountView::SigningIn),
                claude_code(ClaudeCodeView::SigningIn),
            ],
            cx,
        );
        assert_eq!(drain(&controls), [], "a sent cancel isn't owed again");
    }
}

/// Open again replaces the waiting attempt without a new view, so once it completes, closing
/// Settings sends nothing.
#[gpui::test]
fn closing_settings_after_open_again_completes_sends_nothing(cx: &mut TestAppContext) {
    let (window, owner, controls) = accounts_panel(cx);
    owner.update(cx, |owner, cx| {
        owner.settings_account_op(AccountOp::SignIn, cx);
    });
    account_views(&owner, vec![chatgpt(AccountView::SigningIn)], cx);
    owner.update(cx, |owner, cx| {
        owner.settings_account_op(AccountOp::SignIn, cx);
    });
    account_views(
        &owner,
        vec![chatgpt(AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None,
        })],
        cx,
    );
    assert_eq!(drain(&controls).len(), 2, "two sign-ins");
    click_github(window, "close-settings", cx);
    assert_eq!(drain(&controls), []);
}

/// An op the user sends a connection from elsewhere, chat's Sign in included, drops Settings'
/// owed cancel for it, so the wait the next connection shows isn't cancelled.
#[gpui::test]
fn a_sign_in_from_chat_drops_settings_owed_cancels(cx: &mut TestAppContext) {
    let (window, owner, controls) = accounts_panel(cx);
    start_both(window, &controls, cx);
    account_views(&owner, vec![LinkEvent::State(LinkState::Reconnecting)], cx);
    click_github(window, "close-settings", cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            [owner.settings_chatgpt, owner.settings_claude],
            [SignInMark::CancelOwed { generation: 7 }; 2]
        );
    });
    owner.update(cx, |owner, cx| {
        assert!(
            !owner.account_op(AccountOp::SignIn, cx),
            "no link to send on"
        );
        assert!(!owner.claude_op(ConnectionOp::ConnectClaudeCode, cx));
    });
    account_views(
        &owner,
        vec![
            LinkEvent::Connected(Session::detached(8, Incarnation(2))),
            LinkEvent::Account {
                generation: 8,
                view: AccountView::SigningIn,
            },
            LinkEvent::Connections {
                generation: 8,
                view: ConnectionsView {
                    anthropic: KeyView::Missing,
                    anthropic_revision: None,
                    claude_code: ClaudeCodeView::SigningIn,
                },
            },
        ],
        cx,
    );
    let cancels: Vec<_> = drain(&controls)
        .into_iter()
        .filter(|control| CANCELS.contains(control))
        .collect();
    assert_eq!(cancels, []);
}

#[gpui::test]
fn closing_settings_leaves_a_sign_in_from_chat(cx: &mut TestAppContext) {
    let (window, owner, controls) = accounts_panel(cx);
    owner.update(cx, |owner, cx| {
        assert!(owner.account_op(AccountOp::SignIn, cx));
    });
    account_views(&owner, vec![chatgpt(AccountView::SigningIn)], cx);
    drain(&controls);
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(drain(&controls), []);
}

#[gpui::test]
fn closing_settings_after_its_sign_ins_complete_sends_nothing(cx: &mut TestAppContext) {
    let (window, owner, controls) = accounts_panel(cx);
    start_both(window, &controls, cx);
    account_views(
        &owner,
        vec![
            chatgpt(AccountView::SigningIn),
            claude_code(ClaudeCodeView::SigningIn),
        ],
        cx,
    );
    account_views(
        &owner,
        vec![
            chatgpt(AccountView::SignedIn {
                email: None,
                plan: PlanState::Enabled,
                recovery: None,
            }),
            claude_code(CLAUDE_OUT),
        ],
        cx,
    );
    click_github(window, "close-settings", cx);
    assert!(!owner.read_with(cx, |owner, _| owner.settings_open()));
    assert_eq!(drain(&controls), []);
}

/// The start of a hold closes Settings, so it cancels Settings' sign-in like any other close.
#[gpui::test]
fn a_hold_that_closes_settings_cancels_its_sign_in(cx: &mut TestAppContext) {
    use crate::input::TalkHeld;
    use crate::input::hold::Intent;
    use crate::presence::voice::Stamp;
    use kumochi_protocol::message::GestureId;

    let (owner, _data, controls) = crate::presence::voice_wiring_tests::ready(cx);
    owner.update(cx, |owner, cx| {
        let model = owner.settings.clone();
        let weak = cx.weak_entity();
        let events = owner.events.clone();
        let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(model, weak, events, window, cx))
        })
        .unwrap();
        owner.settings_panel = Some(SettingsPanel {
            window,
            view,
            visible: true,
            focused: true,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Account {
                    generation: 1,
                    view: AccountView::SignedOut {
                        revocation_unconfirmed: false,
                    },
                }]),
                cx,
            )
            .expect("a current account view");
        owner.settings_account_op(AccountOp::SignIn, cx);
    });
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignIn)]
    );
    owner.update(cx, |owner, cx| {
        crate::presence::voice_wiring_tests::gate(owner, 1, cx);
        let token = owner.link.microphone_gate().arm(GestureId(1)).unwrap();
        owner.link.track_input(GestureId(1));
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture: GestureId(1),
                generation: Ok(token),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert!(!owner.settings_open());
    });
    let cancels: Vec<_> = drain(&controls)
        .into_iter()
        .filter(|control| matches!(control, ControlToBrain::AccountOp(_)))
        .collect();
    assert_eq!(
        cancels,
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

#[gpui::test]
fn disabling_hold_to_talk_in_settings_stops_live_recording_without_moving_focus(
    cx: &mut TestAppContext,
) {
    use crate::presence::PanelVisibility;
    use crate::presence::voice::tests::{source, until};
    use crate::presence::voice_wiring_tests::{hold, ready, status};
    use crate::strings::Text;
    use kumochi_protocol::message::{GestureId, HoldStatus, SettingsOp};
    use std::time::Duration;

    test_fixture::without_desktop(cx);
    let (owner, data, controls) = ready(cx);
    owner.update(cx, |owner, _| {
        owner.panel_visibility = PanelVisibility::Visible;
    });
    let (open, finish, started, samples) = source(&owner, cx, None);
    let (token, _) = hold(&owner, 1, cx);
    let gesture = GestureId(1);
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized { gesture: id, .. } if id == gesture
    ));
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldListening)
        })
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();

    let (window, view) = owner.update(cx, |owner, cx| {
        assert!(owner.activity.working && owner.voice_working());
        let (window, view) = detached_settings(owner, cx);
        let panel = owner.settings_panel.as_mut().unwrap();
        panel.visible = true;
        panel.focused = true;
        owner.panel_visibility = PanelVisibility::Hidden;
        view.update(cx, |view, _| {
            view.tab = SettingsTab::Voice;
            view.stop = owner.escape_hold.shows_stop();
        });
        (window, view)
    });
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.simulate_resize(size(px(768.), px(1600.)));
    let focused = frame.update(|window, cx| {
        assert!(view.read(cx).stop);
        view.update(cx, |view, cx| view.focus(window, cx));
        window.draw(cx).clear(cx);
        window.focused(cx).expect("Settings frame has focus")
    });
    let checkbox = frame.debug_bounds("hold-to-talk").expect("Hold to talk");
    frame.simulate_click(checkbox.center(), gpui_kit::Modifiers::default());
    frame.run_until_parked();
    assert_eq!(
        drain(&controls)
            .into_iter()
            .filter(|message| message != &ControlToBrain::GestureCancel { gesture })
            .collect::<Vec<_>>(),
        [ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false))]
    );
    assert!(!view.read_with(cx, |view, _| view.stop));
    owner.read_with(cx, |owner, cx| {
        assert!(!owner.activity.working);
        assert_eq!(owner.voice.status, Some(Text::HoldStopping));
        assert_eq!(
            owner.settings.read(cx).hold_to_talk_notice(),
            Some(Text::HoldDisabled)
        );
    });
    assert!(!token.retired());

    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| owner.voice.pending.is_none())
    });
    assert!(token.retired());
    assert_eq!(
        drain(&controls),
        [ControlToBrain::VoiceHoldStopped { gesture }]
    );
    assert!(data.is_empty());
    frame.update(|window, cx| assert_eq!(window.focused(cx), Some(focused)));
}
