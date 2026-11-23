use super::*;

#[test]
fn a_replayed_platform_refusal_prohibits_connect_without_hiding_local_cleanup() {
    let mut state = GitHubConnections::default();
    for generation in [7, 8] {
        state.disconnected();
        assert!(state.receive_error(
            Some(generation),
            generation,
            ErrorKey::UnsupportedPlatform.into()
        ));
        let now = Instant::now();
        assert!(state.receive(Some(generation), generation, signed_out(), now));
        assert!(!state.allows(Some(generation), &GitHubOp::Connect));
        assert_eq!(state.error_text(), Some(Text::UnsupportedPlatform));
        assert!(state.receive(
            Some(generation),
            generation,
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::NotDurable),
            },
            now
        ));
        assert!(state.allows(Some(generation), &GitHubOp::RetryCleanup));
    }
}
use crate::strings::Strings;
use kumochi_protocol::message::{ErrorKey, GitHubLogin, RepoName};
use kumochi_scene::text::Bounded;

fn signed_out() -> GitHubView {
    GitHubView::SignedOut { cleanup: None }
}

fn connected() -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new("example").unwrap(),
        repos: Bounded::new(vec![RepoName::new("Example/Repo").unwrap()]).unwrap(),
        truncated: false,
        default: None,
    }
}

fn awaiting(seconds: u16) -> GitHubView {
    GitHubView::AwaitingUser {
        user_code: UserCode::new("ABCD-EFGH").unwrap(),
        expires_in_s: seconds,
    }
}

fn not_verified() -> GitHubView {
    GitHubView::NotVerified {
        login: GitHubLogin::new("example").unwrap(),
    }
}

fn model(view: GitHubView) -> GitHubConnections {
    let mut state = GitHubConnections::default();
    assert!(state.receive(Some(7), 7, view, Instant::now()));
    state
}

fn timeout(state: &mut GitHubConnections) {
    assert!(state.receive_error(Some(7), 7, ErrorKey::SignInTimedOut.into()));
    assert_eq!(state.waiting, None);
    assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
    assert_eq!(
        Strings::english().get(state.error_text().unwrap()),
        "GitHub sign-in timed out. Connect again when you're ready."
    );
}

#[test]
fn each_view_has_exact_copy_and_commands() {
    let login = GitHubLogin::new("example").unwrap();
    for (view, status, commands) in [
        (
            signed_out(),
            Text::GitHubSignedOut,
            vec![(Text::GitHubConnect, GitHubOp::Connect)],
        ),
        (
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running),
            },
            Text::GitHubRemoving,
            vec![],
        ),
        (
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::Refused),
            },
            Text::GitHubRemovalFailed,
            vec![(Text::Retry, GitHubOp::RetryCleanup)],
        ),
        (
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::NotDurable),
            },
            Text::GitHubNotDurable,
            vec![(Text::Retry, GitHubOp::RetryCleanup)],
        ),
        (
            GitHubView::Requesting,
            Text::GitHubRequesting,
            vec![(Text::AccountCancel, GitHubOp::CancelConnect)],
        ),
        (
            awaiting(30),
            Text::GitHubAwaiting,
            vec![(Text::AccountCancel, GitHubOp::CancelConnect)],
        ),
        (
            connected(),
            Text::GitHubConnected,
            vec![
                (Text::GitHubRefresh, GitHubOp::RefreshRepos),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
        ),
        (
            GitHubView::NotInstalled {
                login: login.clone(),
            },
            Text::GitHubNotInstalled,
            vec![(Text::GitHubSignOut, GitHubOp::SignOut)],
        ),
        (
            not_verified(),
            Text::GitHubNotVerified,
            vec![
                (Text::GitHubCheckAgain, GitHubOp::RefreshRepos),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
        ),
        (
            GitHubView::NeedsSignIn { login },
            Text::GitHubNeedsSignIn,
            vec![
                (Text::GitHubSignInAgain, GitHubOp::Connect),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
        ),
    ] {
        let state = model(view.clone());
        assert_eq!(state.status(), status, "{view:?}");
        assert_eq!(state.commands(), commands, "{view:?}");
        let pick = GitHubOp::SetDefaultRepo(RepoName::new("Example/Repo").unwrap());
        let mut expected: Vec<_> = commands.iter().map(|(_, op)| op.clone()).collect();
        if matches!(view, GitHubView::Connected { .. }) {
            expected.push(pick.clone());
        }
        let allowed: Vec<_> = [
            GitHubOp::Connect,
            GitHubOp::CancelConnect,
            GitHubOp::RetryCleanup,
            GitHubOp::RefreshRepos,
            GitHubOp::SignOut,
            pick,
        ]
        .into_iter()
        .filter(|op| state.allows(Some(7), op))
        .collect();
        assert_eq!(allowed, expected, "{view:?}");
        for (_, command) in commands {
            assert!(state.allows(Some(7), &command), "{view:?} {command:?}");
            assert!(!state.allows(Some(8), &command));
        }
    }
}

#[test]
fn an_absent_snapshot_waits_without_connect_authority() {
    let state = GitHubConnections::default();
    assert_eq!(state.status(), Text::GitHubWaiting);
    assert_eq!(
        state.commands(),
        &[(Text::GitHubConnect, GitHubOp::Connect)]
    );
    assert!(!state.allows(None, &GitHubOp::Connect));
    assert!(!state.allows(Some(7), &GitHubOp::Connect));
    assert_eq!(
        Strings::english().get(Text::GitHubRequesting),
        "Connecting to GitHub…"
    );
}

#[test]
fn connect_cancel_and_second_connect_wait_for_authority() {
    let mut state = model(signed_out());
    let mut sent = Vec::new();
    assert!(state.send(Some(7), GitHubOp::Connect, |op| {
        sent.push(op);
        true
    }));
    assert!(!state.send(Some(7), GitHubOp::Connect, |_| panic!("no duplicate")));
    assert_eq!(state.view, Some(signed_out()));
    assert!(state.receive(Some(7), 7, GitHubView::Requesting, Instant::now()));
    assert!(state.send(Some(7), GitHubOp::CancelConnect, |op| {
        sent.push(op);
        true
    }));
    assert!(!state.send(Some(7), GitHubOp::CancelConnect, |_| panic!(
        "no duplicate cancel"
    )));
    assert!(!state.send(Some(7), GitHubOp::Connect, |_| panic!("no early reconnect")));
    assert!(state.receive(Some(7), 7, signed_out(), Instant::now()));
    assert!(state.send(Some(7), GitHubOp::Connect, |op| {
        sent.push(op);
        true
    }));
    assert_eq!(
        sent,
        vec![
            GitHubOp::Connect,
            GitHubOp::CancelConnect,
            GitHubOp::Connect
        ]
    );
}

#[test]
fn awaiting_verification_and_saving_share_neutral_requesting_then_authoritative_connected() {
    let mut state = model(awaiting(30));
    assert_eq!(
        state.commands(),
        &[(Text::AccountCancel, GitHubOp::CancelConnect)]
    );
    assert!(state.receive(Some(7), 7, GitHubView::Requesting, Instant::now()));
    assert_eq!(state.status(), Text::GitHubRequesting);
    assert_eq!(
        state.commands(),
        &[(Text::AccountCancel, GitHubOp::CancelConnect)]
    );
    assert_eq!(state.code(Some(7), Instant::now()), None);
    assert!(state.receive(Some(7), 7, connected(), Instant::now()));
    assert_eq!(state.status(), Text::GitHubConnected);
    assert_eq!(state.login().unwrap().as_str(), "example");
    assert_eq!(
        state.commands(),
        &[
            (Text::GitHubRefresh, GitHubOp::RefreshRepos),
            (Text::GitHubSignOut, GitHubOp::SignOut)
        ]
    );
}

#[test]
fn cleanup_retry_never_exposes_connected_or_connect_controls() {
    for cleanup in [Cleanup::Refused, Cleanup::NotDurable] {
        let view = GitHubView::SignedOut {
            cleanup: Some(cleanup),
        };
        let mut state = model(view.clone());
        assert_eq!(state.commands(), &[(Text::Retry, GitHubOp::RetryCleanup)]);
        for op in [GitHubOp::Connect, GitHubOp::SignOut, GitHubOp::RefreshRepos] {
            assert!(!state.allows(Some(7), &op));
        }
        let mut sent = Vec::new();
        assert!(state.send(Some(7), GitHubOp::RetryCleanup, |op| {
            sent.push(op);
            true
        }));
        assert!(!state.send(Some(7), GitHubOp::RetryCleanup, |_| panic!("once")));
        assert_eq!(sent, vec![GitHubOp::RetryCleanup]);
        assert_eq!(state.view, Some(view));
    }
    let state = model(GitHubView::NotInstalled {
        login: GitHubLogin::new("example").unwrap(),
    });
    assert!(!state.allows(Some(7), &GitHubOp::RefreshRepos));
}

#[test]
fn timeout_survives_first_changed_snapshot_and_identical_repeats_until_superseded() {
    let mut state = model(awaiting(30));
    assert!(state.send(Some(7), GitHubOp::CancelConnect, |_| true));
    timeout(&mut state);
    for _ in 0..3 {
        assert!(state.receive(Some(7), 7, signed_out(), Instant::now()));
        assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
        assert_eq!(state.status(), Text::GitHubSignedOut);
        assert_eq!(
            state.commands(),
            &[(Text::GitHubConnect, GitHubOp::Connect)]
        );
        assert!(state.allows(Some(7), &GitHubOp::Connect));
    }
    assert!(state.receive(Some(7), 7, GitHubView::Requesting, Instant::now()));
    assert_eq!(state.error, None);
}

#[test]
fn first_identical_snapshot_preserves_error_but_next_changed_one_supersedes() {
    let mut state = model(signed_out());
    timeout(&mut state);
    assert!(state.receive(Some(7), 7, signed_out(), Instant::now()));
    assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
    assert!(state.receive(Some(7), 7, GitHubView::Requesting, Instant::now()));
    assert_eq!(state.error, None);
}

#[test]
fn rejected_queue_preserves_authoritative_error_and_retryable_intent() {
    let mut state = model(signed_out());
    timeout(&mut state);
    let mut calls = Vec::new();
    assert!(!state.send(Some(7), GitHubOp::Connect, |op| {
        calls.push(op);
        false
    }));
    assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
    assert_eq!(state.waiting, None);
    assert!(state.send_failed);
    assert!(state.allows(Some(7), &GitHubOp::Connect));
    assert_eq!(calls, vec![GitHubOp::Connect]);
    assert!(state.send(Some(7), GitHubOp::Connect, |_| true));
    assert_eq!(state.error, None);
    assert!(!state.send_failed);
    assert_eq!(state.waiting, Some(GitHubOp::Connect));
    assert_eq!(state.view, Some(signed_out()));
}

#[test]
fn every_current_view_and_error_finishes_the_pending_operation() {
    let mut state = model(connected());
    assert!(state.send(Some(7), GitHubOp::RefreshRepos, |_| true));
    assert!(state.receive(Some(7), 7, connected(), Instant::now()));
    assert_eq!(state.waiting, None);
    assert!(state.send(Some(7), GitHubOp::RefreshRepos, |_| true));
    timeout(&mut state);
    assert!(state.allows(Some(7), &GitHubOp::RefreshRepos));
}

#[test]
fn a_newly_admitted_intent_clears_the_authoritative_error_without_changing_the_view() {
    let mut state = model(awaiting(30));
    timeout(&mut state);
    assert!(state.receive(Some(7), 7, signed_out(), Instant::now()));
    assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
    let mut sent = Vec::new();
    assert!(state.send(Some(7), GitHubOp::Connect, |op| {
        sent.push(op);
        true
    }));
    assert_eq!(sent, vec![GitHubOp::Connect]);
    assert_eq!(state.error_text(), None);
    assert_eq!(state.view, Some(signed_out()));
    assert_eq!(state.waiting, Some(GitHubOp::Connect));
}

#[test]
fn zero_and_maximum_wire_expiries_are_exact_and_session_fenced() {
    let now = Instant::now();
    let mut state = GitHubConnections::default();
    assert!(state.receive(Some(7), 7, awaiting(0), now));
    assert_eq!(state.code(Some(7), now), None);
    state.disconnected();
    assert!(state.receive(Some(8), 8, awaiting(u16::MAX), now));
    assert_eq!(
        state.code(Some(8), now).map(|(_, seconds)| seconds),
        Some(65535)
    );
    assert_eq!(state.code(Some(7), now), None);
    assert_eq!(state.code(Some(8), now + Duration::from_secs(65535)), None);
    assert_eq!(
        state.commands(),
        &[(Text::AccountCancel, GitHubOp::CancelConnect)]
    );
}

#[test]
fn stale_session_inputs_and_effects_are_inert_and_replacement_clears_everything() {
    let mut state = model(awaiting(30));
    timeout(&mut state);
    assert!(!state.receive_error(Some(7), 6, ErrorKey::AccountOffline.into()));
    assert!(!state.receive(Some(7), 6, connected(), Instant::now()));
    assert!(!state.send(Some(8), GitHubOp::CancelConnect, |_| panic!("stale send")));
    assert_eq!(state.code(Some(8), Instant::now()), None);
    assert_eq!(state.error_text(), Some(Text::GitHubTimeout));
    assert_eq!(state.view, Some(awaiting(30)));
    assert!(state.receive(Some(8), 8, signed_out(), Instant::now()));
    assert_eq!(state.generation, Some(8));
    assert_eq!(state.error, None);
    assert_eq!(state.challenge, None);
    state.disconnected();
    assert_eq!(state.generation, None);
    assert_eq!(state.view, None);
    assert!(!state.allows(Some(8), &GitHubOp::Connect));
}

#[test]
fn exact_listed_repository_spelling_is_required_and_saving_is_not_optimistic() {
    let mut state = model(connected());
    for repo in ["example/repo", "Example/Other"] {
        assert!(!state.send(
            Some(7),
            GitHubOp::SetDefaultRepo(RepoName::new(repo).unwrap()),
            |_| panic!("not listed")
        ));
    }
    let command = GitHubOp::SetDefaultRepo(RepoName::new("Example/Repo").unwrap());
    let mut sent = Vec::new();
    assert!(state.send(Some(7), command.clone(), |op| {
        sent.push(op);
        true
    }));
    assert_eq!(sent, vec![command]);
    assert_eq!(state.view, Some(connected()));
    assert!(state.receive_error(Some(7), 7, ErrorKey::SettingsRefused.into()));
    let GitHubView::Connected { login, .. } = connected() else {
        unreachable!()
    };
    for size in [0, 200] {
        let repos = (0..size)
            .map(|i| RepoName::new(&format!("Example/Repo{i}")).unwrap())
            .collect();
        assert!(state.receive(
            Some(7),
            7,
            GitHubView::Connected {
                login: login.clone(),
                repos: Bounded::new(repos).unwrap(),
                truncated: size == 200,
                default: None
            },
            Instant::now()
        ));
        assert_eq!(state.waiting, None);
        assert!(!state.allows(
            Some(7),
            &GitHubOp::SetDefaultRepo(RepoName::new("Example/Absent").unwrap())
        ));
    }
}

#[test]
fn the_same_code_never_extends_its_monotonic_deadline_or_reappears_after_expiry() {
    let now = Instant::now();
    let mut state = GitHubConnections::default();
    assert!(state.receive(Some(7), 7, awaiting(30), now));
    assert_eq!(
        state
            .code(Some(7), now)
            .map(|(code, seconds)| (code.as_str(), seconds)),
        Some(("ABCD-EFGH", 30))
    );
    assert!(state.receive(Some(7), 7, awaiting(30), now + Duration::from_secs(20)));
    assert_eq!(
        state
            .code(Some(7), now + Duration::from_secs(20))
            .map(|(_, seconds)| seconds),
        Some(10)
    );
    assert!(state.receive(
        Some(7),
        7,
        GitHubView::Requesting,
        now + Duration::from_secs(21)
    ));
    assert!(state.receive(Some(7), 7, awaiting(30), now + Duration::from_secs(22)));
    assert_eq!(state.code(Some(7), now + Duration::from_secs(30)), None);
    assert_eq!(state.view, Some(awaiting(30)));
    assert!(state.allows(Some(7), &GitHubOp::CancelConnect));
    assert!(!state.allows(Some(7), &GitHubOp::Connect));
    assert!(state.receive(Some(7), 7, signed_out(), now + Duration::from_secs(31)));
    assert!(state.send(Some(7), GitHubOp::Connect, |_| true));
    assert!(state.receive(Some(7), 7, awaiting(30), now + Duration::from_secs(32)));
    assert_eq!(
        state
            .code(Some(7), now + Duration::from_secs(32))
            .map(|(_, seconds)| seconds),
        Some(30)
    );
}

#[test]
fn error_copy_uses_only_github_localized_keys() {
    let mut state = model(signed_out());
    for (key, text, copy) in [
        (
            ErrorKey::GitHubUnavailable,
            Text::GitHubUnavailable,
            "GitHub connection controls aren't available in this build.",
        ),
        (
            ErrorKey::SignInFailed,
            Text::GitHubFailure,
            "Couldn't complete the GitHub operation. Try again.",
        ),
        (
            ErrorKey::AccountOffline,
            Text::GitHubOffline,
            "Couldn't reach GitHub. Check your connection and try again.",
        ),
        (
            ErrorKey::CredentialsUnavailable,
            Text::GitHubCredentials,
            "Couldn't access the saved GitHub sign-in. Try again.",
        ),
        (
            ErrorKey::RequestRefused,
            Text::GitHubRefused,
            "GitHub refused that operation. Check the connection and try again.",
        ),
        (
            ErrorKey::SettingsRefused,
            Text::GitHubDefaultRetained,
            "Couldn't save the default repository. The previous default is unchanged. Try again.",
        ),
        (
            ErrorKey::AccountMisconfigured,
            Text::GitHubConfiguration,
            "Connecting is unavailable until this build has a registered GitHub App client ID. No token or client secret can be entered here.",
        ),
        (
            ErrorKey::PlanUnavailable,
            Text::GitHubFailure,
            "Couldn't complete the GitHub operation. Try again.",
        ),
    ] {
        assert!(state.receive_error(Some(7), 7, key.into()));
        assert_eq!(state.error_text(), Some(text));
        assert_eq!(Strings::english().get(text), copy);
    }
}

#[test]
fn every_state_uses_the_reviewed_fixed_github_copy() {
    let strings = Strings::english();
    for (text, copy) in [
        (
            Text::GitHubWaiting,
            "GitHub connections are unavailable. Waiting for the brain.",
        ),
        (Text::GitHubSignedOut, "Signed out"),
        (Text::GitHubRemoving, "Removing saved sign-in…"),
        (
            Text::GitHubRemovalFailed,
            "Signed out. Couldn't remove the saved sign-in. Retry removal before connecting again.",
        ),
        (
            Text::GitHubNotDurable,
            "Signed out. Couldn't save the removal. Retry before restarting, or the saved sign-in may return.",
        ),
        (Text::GitHubRequesting, "Connecting to GitHub…"),
        (Text::GitHubAwaiting, "Enter this code on GitHub"),
        (Text::GitHubConnected, "Connected"),
        (
            Text::GitHubNotInstalled,
            "Install the GitHub App on your repositories, then sign out and connect again.",
        ),
        (
            Text::GitHubNeedsSignIn,
            "Sign in again to use this GitHub account.",
        ),
        (Text::GitHubNotVerified, "Saved sign-in, not verified"),
        (Text::GitHubCheckAgain, "Check again"),
    ] {
        assert_eq!(strings.get(text), copy);
    }
}

#[test]
fn leaving_a_challenge_clears_its_text_without_extending_its_deadline() {
    let now = Instant::now();
    let mut state = GitHubConnections::default();
    state.receive(Some(7), 7, awaiting(30), now);
    let origin = Arc::clone(&state.controls_origin);
    assert!(state.is_current(Some(7), Some(7), &origin));
    state.receive(
        Some(7),
        7,
        GitHubView::Requesting,
        now + Duration::from_secs(1),
    );
    assert_eq!(state.challenge, None);
    assert!(!state.is_current(Some(7), Some(7), &origin));
    state.receive(Some(7), 7, awaiting(30), now + Duration::from_secs(2));
    assert_eq!(state.code(Some(7), now + Duration::from_secs(30)), None);
    assert!(state.send(Some(7), GitHubOp::CancelConnect, |_| true));
    state.receive(Some(7), 7, awaiting(30), now + Duration::from_secs(3));
    assert_eq!(state.challenge, None);
    assert_eq!(state.code(Some(7), now + Duration::from_secs(3)), None);
    assert!(!state.allows(Some(7), &GitHubOp::CancelConnect));
    state.receive(Some(7), 7, signed_out(), now + Duration::from_secs(4));
    assert!(state.allows(Some(7), &GitHubOp::Connect));
}

#[test]
fn only_a_matching_default_or_error_settles_a_default_request() {
    let mut state = model(connected());
    let repo = RepoName::new("Example/Repo").unwrap();
    assert!(state.send(Some(7), GitHubOp::SetDefaultRepo(repo.clone()), |_| true));
    assert_eq!(state.status(), Text::GitHubSavingDefault);
    state.receive(Some(7), 7, connected(), Instant::now());
    assert_eq!(state.status(), Text::GitHubSavingDefault);
    assert!(!state.allows(Some(7), &GitHubOp::SignOut));
    let mut committed = connected();
    let GitHubView::Connected { default, .. } = &mut committed else {
        unreachable!()
    };
    *default = Some(repo.clone());
    state.receive(Some(7), 7, committed.clone(), Instant::now());
    assert_eq!(state.status(), Text::GitHubConnected);
    assert_eq!(state.waiting, None);
    assert_eq!(state.view, Some(committed));
    assert!(!state.allows(Some(7), &GitHubOp::SetDefaultRepo(repo)));
    let origin = Arc::clone(&state.controls_origin);
    assert!(!state.is_current(Some(8), Some(7), &origin));
    assert!(!state.is_current(None, Some(7), &origin));
    state.receive_error(Some(7), 7, ErrorKey::SettingsRefused.into());
    assert!(!state.is_current(Some(7), Some(7), &origin));
    assert_eq!(state.error_text(), Some(Text::GitHubDefaultRetained));
}

#[test]
fn an_incomplete_listing_never_authorizes_a_default_even_when_the_name_is_present() {
    let mut view = connected();
    let GitHubView::Connected { truncated, .. } = &mut view else {
        unreachable!()
    };
    *truncated = true;
    let mut state = model(connected());
    let origin = Arc::clone(&state.controls_origin);
    state.receive(Some(7), 7, view, Instant::now());
    assert!(!state.is_current(Some(7), Some(7), &origin));
    assert!(!state.send(
        Some(7),
        GitHubOp::SetDefaultRepo(RepoName::new("Example/Repo").unwrap()),
        |_| panic!("an incomplete listing has no repository authority"),
    ));
}

#[test]
fn not_verified_metadata_keeps_consent_and_control_fences_separate() {
    let mut state = model(connected());
    let consent = Arc::clone(&state.origin);
    let controls = Arc::clone(&state.controls_origin);
    assert!(state.receive(Some(7), 7, not_verified(), Instant::now()));
    assert!(!Arc::ptr_eq(&consent, &state.origin));
    assert!(!Arc::ptr_eq(&controls, &state.controls_origin));
    assert_eq!(state.login().map(GitHubLogin::as_str), Some("example"));
    assert_eq!(state.code(Some(7), Instant::now()), None);
    for op in [
        GitHubOp::Connect,
        GitHubOp::CancelConnect,
        GitHubOp::RetryCleanup,
        GitHubOp::SetDefaultRepo(RepoName::new("Example/Repo").unwrap()),
    ] {
        assert!(!state.send(Some(7), op, |_| panic!(
            "unverified metadata is not authority"
        )));
    }
    let consent = Arc::clone(&state.origin);
    let controls = Arc::clone(&state.controls_origin);
    assert!(state.receive_error(Some(7), 7, ErrorKey::AccountOffline.into()));
    assert!(Arc::ptr_eq(&consent, &state.origin));
    assert!(!Arc::ptr_eq(&controls, &state.controls_origin));
    assert_eq!(state.view, Some(not_verified()));
    assert!(state.allows(Some(7), &GitHubOp::RefreshRepos));
    assert!(state.allows(Some(7), &GitHubOp::SignOut));
}
