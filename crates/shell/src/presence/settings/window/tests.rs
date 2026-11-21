use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};

use super::{SettingsPanel, SettingsView};
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
            settings.receive_speech_input(Some(1), 1, PartState::NotInstalled);
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

#[gpui::test]
fn open_settings_in_the_chat_recovery_swaps_the_chat_for_settings(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{Action, ErrorKey, UserError};
    let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (window, view) = super::super::general::tests::open(cx);
    let chat = owner.update(cx, |owner, cx| {
        owner.settings_panel = Some(SettingsPanel {
            window,
            view,
            visible: false,
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        });
        owner.open_text(cx).expect("open chat");
        owner.chat_notice.present(
            UserError::new(
                ErrorKey::ModelUnavailable,
                Vec::new(),
                vec![Action::OpenSettings],
            )
            .expect("bounded settings recovery"),
            crate::chat_notice::Source::Settings(None),
        );
        owner.refresh_conversation(cx);
        owner.text.as_ref().expect("typing fixture").window
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
    owner.read_with(cx, |owner, _| {
        assert!(owner.settings_open(), "Settings is shown");
        assert!(!owner.text_open(), "the chat is hidden");
    });
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
