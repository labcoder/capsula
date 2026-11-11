use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};

use super::{SettingsPanel, SettingsView};
use crate::presence::test_fixture;

use crate::link::{Batch, LinkEvent, Session};
use crate::presence::Presence;
use gpui_kit::{AnyWindowHandle, Entity, VisualTestContext, px, size};
use kumochi_protocol::message::{ControlToBrain, GitHubOp, GitHubView, Incarnation, UserCode};
use kumochi_scene::text::Bounded;

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
        view.tab = 1;
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
fn recovery_settings_clicks_open_the_exact_tab_through_the_hidden_panel_path(
    cx: &mut TestAppContext,
) {
    use crate::chat_notice::Source;
    use kumochi_protocol::message::{Action, ErrorKey, UserError};

    for (key, source, expected) in [
        (ErrorKey::AnthropicKeyRejected, Source::Account, 1),
        (ErrorKey::SettingsRefused, Source::Settings(None), 0),
    ] {
        let owner = test_fixture::presence(cx);
        let (window, view) = owner.update(cx, |owner, cx| {
            let model = owner.settings.clone();
            let weak = cx.weak_entity();
            let events = owner.events.clone();
            let (window, view) =
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
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
    use crate::chat_notice::Source;
    use kumochi_protocol::message::{Action, ErrorKey, MANAGE_USAGE_URL, UserError};

    let owner = test_fixture::presence(cx);
    let window = owner.update(cx, |owner, cx| {
        owner.chat_notice.present(
            UserError::new(ErrorKey::UsageLimit, Vec::new(), vec![Action::ManageUsage])
                .expect("bounded usage recovery"),
            Source::Account,
        );
        owner.refresh_conversation(cx);
        owner.text.as_ref().expect("typing fixture").window
    });
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None);
    {
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        let bounds = frame
            .debug_bounds("chat-recovery-action-0")
            .expect("Manage usage link");
        frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    }
    cx.run_until_parked();
    assert_eq!(cx.opened_url().as_deref(), Some(MANAGE_USAGE_URL));
}

#[gpui::test]
fn recovery_openers_select_the_exact_tab_without_toggling_a_visible_window_closed(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
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
            visible: true,
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner.open_accounts(cx).expect("accounts tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 1);
        owner.open_accounts(cx).expect("accounts stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 1);
        owner.open_general(cx).expect("general tab");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 0);
        owner.open_general(cx).expect("general stays open");
        assert!(owner.settings_open());
        assert_eq!(view.read(cx).tab, 0);
    });
}
