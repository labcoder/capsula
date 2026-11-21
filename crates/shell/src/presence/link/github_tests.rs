use super::*;
use crate::link::Session;
use crate::presence::test_fixture;
use gpui_kit::TestAppContext;
use kumochi_protocol::message::{ErrorKey, GitHubLogin, GitHubView, Incarnation, RepoName};
use kumochi_scene::text::Bounded;
use std::sync::Arc;

fn connected() -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new("octo").unwrap(),
        repos: Bounded::new(vec![RepoName::new("o/r").unwrap()]).unwrap(),
        default: None,
        truncated: false,
    }
}

#[gpui::test]
fn tagged_refusals_without_a_view_are_session_fenced_and_leave_accounts_untouched(
    cx: &mut TestAppContext,
) {
    use crate::strings::{Strings, Text};
    use kumochi_protocol::message::GitHubOp;
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHubError {
                    generation: 7,
                    error: ErrorKey::GitHubUnavailable.into(),
                }]),
                cx,
            )
            .unwrap();
        let state = owner.settings.read(cx);
        let origin = Arc::clone(&state.github.origin);
        assert_eq!(state.github.view, None);
        assert_eq!(state.github.generation, Some(7));
        assert_eq!(state.github.error, Some(ErrorKey::GitHubUnavailable.into()));
        assert_eq!(state.github.error_text(), Some(Text::GitHubUnavailable));
        assert_eq!(
            Strings::english().get(Text::GitHubUnavailable),
            "GitHub connection controls aren't available in this build."
        );
        assert_eq!(
            Strings::english().error(&ErrorKey::GitHubUnavailable.into()),
            "GitHub connection controls aren't available in this build."
        );
        assert!(!state.github.allows(Some(7), &GitHubOp::Connect));
        assert_eq!(state.accounts.error, None);
        assert_eq!(state.notice(), None);
        assert!(owner.chat_notice.shown().is_none());
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHubError {
                    generation: 6,
                    error: ErrorKey::SignInTimedOut.into(),
                }]),
                cx,
            )
            .unwrap();
        assert_eq!(
            owner.settings.read(cx).github.error,
            Some(ErrorKey::GitHubUnavailable.into())
        );
        assert!(Arc::ptr_eq(&origin, &owner.settings.read(cx).github.origin));
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Connected(Session::detached(
                    8,
                    Incarnation(1),
                ))]),
                cx,
            )
            .unwrap();
        assert_eq!(owner.settings.read(cx).github.error, None);
        assert_eq!(owner.settings.read(cx).github.view, None);
        assert_eq!(owner.settings.read(cx).github.generation, None);
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHubError {
                        generation: 7,
                        error: ErrorKey::SignInFailed.into(),
                    },
                    LinkEvent::GitHubError {
                        generation: 8,
                        error: ErrorKey::SignInTimedOut.into(),
                    },
                ]),
                cx,
            )
            .unwrap();
        assert_eq!(
            owner.settings.read(cx).github.error,
            Some(ErrorKey::SignInTimedOut.into())
        );
        assert_eq!(owner.settings.read(cx).github.view, None);
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::State(LinkState::Reconnecting),
                    LinkEvent::GitHubError {
                        generation: 8,
                        error: ErrorKey::GitHubUnavailable.into(),
                    },
                ]),
                cx,
            )
            .unwrap();
        assert_eq!(owner.settings.read(cx).github.error, None);
        assert_eq!(owner.settings.read(cx).github.view, None);
        assert_eq!(owner.settings.read(cx).github.generation, None);
        assert_eq!(owner.settings.read(cx).accounts.error, None);
        assert_eq!(owner.settings.read(cx).notice(), None);
        assert!(owner.chat_notice.shown().is_none());
    });
}

#[gpui::test]
fn tagged_existing_errors_clear_pending_intent_and_survive_their_authoritative_view(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{Action, ErrorArg, GitHubOp, UserCode};
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        let awaiting = GitHubView::AwaitingUser {
            user_code: UserCode::new("ABCD-EFGH").unwrap(),
            expires_in_s: 30,
        };
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view: awaiting.clone(),
                }]),
                cx,
            )
            .unwrap();
        owner.settings.update(cx, |state, _| {
            assert!(
                state
                    .github
                    .send(Some(7), GitHubOp::CancelConnect, |_| true)
            );
        });
        assert!(
            !owner
                .settings
                .read(cx)
                .github
                .allows(Some(7), &GitHubOp::CancelConnect)
        );
        let origin = Arc::clone(&owner.settings.read(cx).github.origin);
        let error = UserError::new(
            ErrorKey::SignInTimedOut,
            vec![ErrorArg::Seconds(5)],
            vec![Action::Retry],
        )
        .unwrap();
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHubError {
                    generation: 7,
                    error: error.clone(),
                }]),
                cx,
            )
            .unwrap();
        assert_eq!(owner.settings.read(cx).github.error, Some(error.clone()));
        assert_eq!(owner.settings.read(cx).github.view, Some(awaiting));
        assert!(Arc::ptr_eq(&origin, &owner.settings.read(cx).github.origin));
        assert!(
            owner
                .settings
                .read(cx)
                .github
                .allows(Some(7), &GitHubOp::CancelConnect)
        );
        for _ in 0..3 {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::GitHub {
                        generation: 7,
                        view: GitHubView::SignedOut { cleanup: None },
                    }]),
                    cx,
                )
                .unwrap();
            assert_eq!(owner.settings.read(cx).github.error, Some(error.clone()));
            assert_eq!(
                owner.settings.read(cx).github.view,
                Some(GitHubView::SignedOut { cleanup: None })
            );
        }
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view: GitHubView::Requesting,
                }]),
                cx,
            )
            .unwrap();
        assert_eq!(owner.settings.read(cx).github.error, None);
        assert_eq!(owner.settings.read(cx).accounts.error, None);
        assert_eq!(owner.settings.read(cx).notice(), None);
        assert!(owner.chat_notice.shown().is_none());
    });
}

#[gpui::test]
fn snapshots_follow_the_current_session_and_generic_errors_stay_with_accounts(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 7,
                        view: connected(),
                    },
                    LinkEvent::GitHub {
                        generation: 6,
                        view: GitHubView::Requesting,
                    },
                    LinkEvent::Error {
                        generation: 7,
                        error: ErrorKey::AccountOffline.into(),
                    },
                ]),
                cx,
            )
            .expect("snapshots");
        assert_eq!(owner.settings.read(cx).github.generation, Some(7));
        assert_eq!(owner.settings.read(cx).github.view, Some(connected()));
        assert_eq!(owner.settings.read(cx).github.error, None);
        assert_eq!(
            owner
                .settings
                .read(cx)
                .accounts
                .error
                .as_ref()
                .map(UserError::key),
            Some(ErrorKey::AccountOffline)
        );
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::State(LinkState::Reconnecting),
                    LinkEvent::Connected(Session::detached(8, Incarnation(1))),
                ]),
                cx,
            )
            .expect("replacement");
        assert_eq!(owner.settings.read(cx).github.generation, None);
        assert_eq!(owner.settings.read(cx).github.view, None);
        let previous_notice = owner.settings.read(cx).notice().cloned();
        let error: UserError = ErrorKey::SessionEnded.into();
        for (generation, expected) in [(7, None), (8, Some(&error))] {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Error {
                        generation,
                        error: error.clone(),
                    }]),
                    cx,
                )
                .expect("generic error after reconnect");
            let settings = owner.settings.read(cx);
            assert_eq!(
                settings.accounts.error.as_ref(),
                expected,
                "generation {generation}"
            );
            assert_eq!(settings.notice(), expected.or(previous_notice.as_ref()));
            assert_eq!(
                owner.chat_notice.shown().map(|notice| &notice.error),
                expected
            );
        }
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 8,
                        view: connected(),
                    },
                    LinkEvent::State(LinkState::Reconnecting),
                    LinkEvent::GitHub {
                        generation: 8,
                        view: connected(),
                    },
                ]),
                cx,
            )
            .expect("disconnect");
        assert_eq!(owner.settings.read(cx).github.generation, None);
        assert_eq!(owner.settings.read(cx).github.view, None);
    });
}

#[gpui::test]
fn a_batched_scope_loss_and_same_return_retires_the_old_origin_before_observers(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view: connected(),
                }]),
                cx,
            )
            .expect("initial");
        let old = Arc::clone(&owner.settings.read(cx).github.origin);
        let mut truncated = connected();
        if let GitHubView::Connected { truncated, .. } = &mut truncated {
            *truncated = true;
        }
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 7,
                        view: connected(),
                    },
                    LinkEvent::GitHub {
                        generation: 6,
                        view: GitHubView::SignedOut { cleanup: None },
                    },
                    LinkEvent::GitHub {
                        generation: 7,
                        view: truncated,
                    },
                ]),
                cx,
            )
            .expect("identical and stale snapshots");
        assert!(Arc::ptr_eq(&old, &owner.settings.read(cx).github.origin));
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::GitHub {
                        generation: 7,
                        view: GitHubView::SignedOut { cleanup: None },
                    },
                    LinkEvent::GitHub {
                        generation: 7,
                        view: connected(),
                    },
                ]),
                cx,
            )
            .expect("loss and return");
        assert!(!Arc::ptr_eq(&old, &owner.settings.read(cx).github.origin));
        assert_eq!(owner.settings.read(cx).github.view, Some(connected()));
        let fresh = Arc::clone(&owner.settings.read(cx).github.origin);
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::GitHub {
                    generation: 7,
                    view: connected(),
                }]),
                cx,
            )
            .expect("fresh snapshot");
        assert!(Arc::ptr_eq(&fresh, &owner.settings.read(cx).github.origin));
        assert!(!Arc::ptr_eq(&old, &owner.settings.read(cx).github.origin));
    });
}
