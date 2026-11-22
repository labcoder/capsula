use super::super::SettingsTab;
use super::*;
use crate::link::{Batch, LinkEvent, Session};
use crate::presence::Presence;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext, VisualTestContext, px, size};
use kumochi_protocol::message::{ControlToBrain, ErrorKey, GitHubLogin, Incarnation, UserCode};
use kumochi_scene::text::Bounded;

#[derive(Clone, Copy)]
enum AppConfiguration {
    Product,
    Fixture,
    Unconfigured,
}

fn open(
    cx: &mut TestAppContext,
    configuration: AppConfiguration,
) -> (
    AnyWindowHandle,
    Entity<SettingsView>,
    Entity<Presence>,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = crate::presence::test_fixture::presence(cx);
    let (window, view) = super::super::general::tests::open(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        view.fixture = matches!(configuration, AppConfiguration::Fixture);
        view.github_configured =
            matches!(configuration, AppConfiguration::Unconfigured).then_some(false);
        view.tab = SettingsTab::Accounts;
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.simulate_resize(size(px(768.), px(1600.)));
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    (window, view, owner, controls)
}

fn receive(owner: &Entity<Presence>, view: GitHubView, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view,
                }]),
                cx,
            )
            .expect("current-session GitHub snapshot");
    });
    cx.run_until_parked();
}

fn click(frame: &mut VisualTestContext, selector: &'static str) {
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = frame
        .debug_bounds(selector)
        .expect("rendered production GitHub control");
    frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    frame.run_until_parked();
}

fn press(frame: &mut VisualTestContext, keys: &str) {
    let os = frame.update(|_, cx| cx.platform_info().os);
    for key in keys.split_whitespace() {
        let keystroke = gpui_kit::Keystroke::parse_with_os(key, os).unwrap();
        frame.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        frame.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        frame.run_until_parked();
        frame.update(|window, cx| window.draw(cx).clear(cx));
    }
}

fn connected(default: &str) -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new("example").unwrap(),
        repos: Bounded::new(vec![
            RepoName::new("Example/First").unwrap(),
            RepoName::new("Example/Second").unwrap(),
        ])
        .unwrap(),
        truncated: false,
        default: Some(RepoName::new(default).unwrap()),
    }
}

fn awaiting() -> GitHubView {
    GitHubView::AwaitingUser {
        user_code: UserCode::new("ABCD-EFGH").unwrap(),
        expires_in_s: 30,
    }
}

fn sent(controls: &async_channel::Receiver<ControlToBrain>, expected: &GitHubOp) {
    assert!(matches!(controls.try_recv(), Ok(ControlToBrain::GitHub(op)) if &op == expected));
    assert!(matches!(
        controls.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[gpui::test]
fn an_unconfigured_build_explains_the_missing_app_and_cannot_send_connect(cx: &mut TestAppContext) {
    let (window, _, owner, controls) = open(cx, AppConfiguration::Unconfigured);
    receive(&owner, GitHubView::SignedOut { cleanup: None }, cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    assert!(frame.debug_bounds("github-configuration").is_some());
    click(&mut frame, "github-connect");
    assert!(matches!(
        controls.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    assert_eq!(frame.opened_url(), None);
}

#[gpui::test]
#[cfg(debug_assertions)]
fn the_dev_registration_enables_the_production_connect_control(cx: &mut TestAppContext) {
    let (window, _, owner, controls) = open(cx, AppConfiguration::Product);
    receive(&owner, GitHubView::SignedOut { cleanup: None }, cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    assert_eq!(frame.debug_bounds("github-configuration"), None);
    click(&mut frame, "github-connect");
    sent(&controls, &GitHubOp::Connect);
    assert_eq!(frame.opened_url(), None);
}

#[gpui::test]
fn accounts_connect_uses_keyboard_and_cancel_uses_the_current_presence_session(
    cx: &mut TestAppContext,
) {
    let (window, view, owner, controls) = open(cx, AppConfiguration::Fixture);
    receive(&owner, GitHubView::SignedOut { cleanup: None }, cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.tab = SettingsTab::General;
            view.focus(window, cx);
            cx.notify();
        });
    });
    press(&mut frame, "ctrl-tab shift-tab enter");
    sent(&controls, &GitHubOp::Connect);
    frame.update(|_, cx| {
        assert_eq!(
            view.read(cx).model.read(cx).github.view,
            Some(GitHubView::SignedOut { cleanup: None })
        );
    });
    receive(&owner, GitHubView::Requesting, cx);
    receive(&owner, awaiting(), cx);
    click(&mut frame, "github-verification");
    assert_eq!(
        frame.opened_url(),
        None,
        "configured rendering fixtures never open a browser"
    );
    click(&mut frame, "github-cancel");
    sent(&controls, &GitHubOp::CancelConnect);
    frame.update(|_, cx| {
        assert_eq!(
            view.read(cx)
                .model
                .read(cx)
                .github
                .code(Some(7), Instant::now()),
            None
        );
    });
}

#[gpui::test]
fn verification_activation_rechecks_session_origin_and_monotonic_code_expiry(
    cx: &mut TestAppContext,
) {
    for stale in 0..3 {
        let (window, view, owner, _) = open(cx, AppConfiguration::Product);
        receive(&owner, awaiting(), cx);
        let mut frame = VisualTestContext::from_window(window, cx);
        frame.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = frame
            .debug_bounds("github-verification")
            .expect("fresh verification link");
        frame.simulate_mouse_down(
            bounds.center(),
            gpui_kit::MouseButton::Left,
            gpui_kit::Modifiers::default(),
        );
        frame.update(|window, cx| window.draw(cx).clear(cx));
        frame.update(|_, cx| {
            let model = view.read(cx).model.clone();
            match stale {
                0 => owner.update(cx, |owner, _| {
                    owner.session = Some(Session::detached(8, Incarnation(1)));
                }),
                1 => model.update(cx, |state, _| {
                    state.github.controls_origin = Arc::default();
                }),
                _ => model.update(cx, |state, _| {
                    state.github.receive(
                        Some(7),
                        7,
                        awaiting(),
                        Instant::now()
                            .checked_sub(Duration::from_secs(31))
                            .expect("the fixture has a challenge issued 31 seconds ago"),
                    );
                }),
            }
        });
        frame.simulate_mouse_up(
            bounds.center(),
            gpui_kit::MouseButton::Left,
            gpui_kit::Modifiers::default(),
        );
        frame.run_until_parked();
        assert_eq!(frame.opened_url(), None, "stale activation {stale}");
    }
    let (window, _, owner, _) = open(cx, AppConfiguration::Product);
    receive(&owner, awaiting(), cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    click(&mut frame, "github-verification");
    assert_eq!(frame.opened_url().as_deref(), Some(VERIFICATION_URI));
}

#[gpui::test]
fn keyboard_repository_selection_waits_for_commit_and_errors_keep_the_prior_default(
    cx: &mut TestAppContext,
) {
    let (window, view, owner, controls) = open(cx, AppConfiguration::Fixture);
    receive(&owner, connected("Example/First"), cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    click(&mut frame, "github-repository");
    press(&mut frame, "down enter");
    sent(
        &controls,
        &GitHubOp::SetDefaultRepo(RepoName::new("Example/Second").unwrap()),
    );
    frame.update(|_, cx| {
        let state = view.read(cx);
        assert_eq!(
            state.model.read(cx).github.status(),
            Text::GitHubSavingDefault
        );
        assert_eq!(
            state
                .github_repos
                .read(cx)
                .selected_value()
                .map(|choice| choice.repo.as_str()),
            Some("Example/First")
        );
    });
    frame.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::GitHubError {
                        generation: 7,
                        error: ErrorKey::SettingsRefused.into(),
                    }]),
                    cx,
                )
                .expect("default refusal");
        });
    });
    frame.run_until_parked();
    frame.update(|_, cx| {
        assert_eq!(
            view.read(cx).model.read(cx).github.error_text(),
            Some(Text::GitHubDefaultRetained)
        );
    });
    click(&mut frame, "github-repository");
    press(&mut frame, "down enter");
    sent(
        &controls,
        &GitHubOp::SetDefaultRepo(RepoName::new("Example/Second").unwrap()),
    );
    receive(&owner, connected("Example/Second"), cx);
    frame.update(|_, cx| {
        let state = view.read(cx);
        assert_eq!(
            state
                .github_repos
                .read(cx)
                .selected_value()
                .map(|choice| choice.repo.as_str()),
            Some("Example/Second")
        );
        assert_eq!(state.model.read(cx).github.status(), Text::GitHubConnected);
    });
}

#[gpui::test]
fn refresh_sign_out_and_cleanup_retry_emit_the_existing_github_ops(cx: &mut TestAppContext) {
    let (window, _, owner, controls) = open(cx, AppConfiguration::Fixture);
    receive(&owner, connected("Example/First"), cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    click(&mut frame, "github-refresh");
    sent(&controls, &GitHubOp::RefreshRepos);
    receive(&owner, connected("Example/First"), cx);
    click(&mut frame, "github-sign-out");
    sent(&controls, &GitHubOp::SignOut);
    receive(
        &owner,
        GitHubView::SignedOut {
            cleanup: Some(kumochi_protocol::message::Cleanup::Running),
        },
        cx,
    );
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("github-connect").is_none());
    receive(
        &owner,
        GitHubView::SignedOut {
            cleanup: Some(kumochi_protocol::message::Cleanup::Refused),
        },
        cx,
    );
    click(&mut frame, "github-retry-cleanup");
    sent(&controls, &GitHubOp::RetryCleanup);
}

#[test]
fn repository_choice_debug_does_not_reveal_the_repository() {
    let choice = RepoChoice {
        repo: RepoName::new("sentinel/repo").unwrap(),
        generation: Some(7),
        origin: Arc::default(),
    };
    let debug = format!("{choice:?}");
    assert!(!debug.contains("sentinel"));
    assert!(!debug.contains("115, 101, 110, 116, 105, 110, 101, 108"));
}

#[gpui::test]
fn an_unverified_saved_login_checks_again_through_the_settings_control_without_granting_authority(
    cx: &mut TestAppContext,
) {
    let (window, view, owner, controls) = open(cx, AppConfiguration::Product);
    let unverified = GitHubView::NotVerified {
        login: GitHubLogin::new("example").unwrap(),
    };
    receive(&owner, connected("Example/First"), cx);
    receive(&owner, unverified.clone(), cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("github-login").is_some());
    assert!(frame.debug_bounds("github-connect").is_none());
    assert!(frame.debug_bounds("github-repository").is_none());
    assert!(frame.debug_bounds("github-verification").is_none());
    frame.update(|_, cx| {
        let state = &view.read(cx).model.read(cx).github;
        assert_eq!(state.status(), Text::GitHubNotVerified);
        assert_eq!(state.login().map(GitHubLogin::as_str), Some("example"));
        assert!(!state.allows(Some(7), &GitHubOp::Connect));
        assert!(!state.allows(
            Some(7),
            &GitHubOp::SetDefaultRepo(RepoName::new("Example/First").unwrap())
        ));
        assert_eq!(view.read(cx).github_repos.read(cx).selected_value(), None);
    });
    click(&mut frame, "github-refresh");
    sent(&controls, &GitHubOp::RefreshRepos);
    frame.update(|_, cx| {
        assert_eq!(
            view.read(cx).model.read(cx).github.view,
            Some(unverified.clone())
        );
    });
    receive(&owner, unverified, cx);
    click(&mut frame, "github-sign-out");
    sent(&controls, &GitHubOp::SignOut);
}
