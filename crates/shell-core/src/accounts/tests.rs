use kumochi_protocol::message::AccountEmail;

use super::{
    AccountOp, AccountRecovery, AccountView, Accounts, Action, Cleanup, ErrorKey, PlanState,
    StatusLine, UserError, cleanup_notice,
};
use crate::link::LinkEvent;
use crate::strings::Text;

const OPS: [AccountOp; 5] = [
    AccountOp::SignIn,
    AccountOp::CancelSignIn,
    AccountOp::SignOut,
    AccountOp::EnablePlan,
    AccountOp::Retry,
];

fn signed_out() -> AccountView {
    AccountView::SignedOut {
        revocation_unconfirmed: false,
    }
}

fn signed_in(plan: PlanState, recovery: Option<AccountRecovery>) -> AccountView {
    AccountView::SignedIn {
        email: Some(AccountEmail::new("person@example.invalid").expect("valid display email")),
        plan,
        recovery,
    }
}

fn receive(accounts: &mut Accounts, view: AccountView) {
    assert!(accounts.receive(Some(7), 7, view.clone()));
    assert_eq!(accounts.view, Some(view));
    assert_eq!(accounts.waiting, None);
    assert!(!accounts.send_failed);
}

fn press(accounts: &mut Accounts, op: AccountOp, sent: &mut Vec<AccountOp>) {
    let before = accounts.view.clone();
    assert!(accounts.send(op, |op| {
        sent.push(op);
        true
    }));
    assert_eq!(accounts.view, before, "an intent cannot advance the view");
    assert_eq!(accounts.waiting, Some(op));
    assert_eq!(allowed(accounts), Vec::<AccountOp>::new());
}

fn allowed(accounts: &Accounts) -> Vec<AccountOp> {
    OPS.into_iter().filter(|op| accounts.allows(*op)).collect()
}

fn error(key: ErrorKey, actions: Vec<Action>) -> UserError {
    UserError::new(key, Vec::new(), actions).expect("bounded account error")
}

#[test]
fn a_platform_refusal_cannot_retry_or_sign_in_again() {
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_out());
    accounts.error(&error(ErrorKey::UnsupportedPlatform, vec![]));
    assert_eq!(
        allowed(&accounts),
        [AccountOp::SignIn],
        "unscoped errors aren't account state"
    );
    assert!(accounts.receive_snapshot(
        Some(7),
        LinkEvent::AccountUnsupported {
            generation: 7,
            view: signed_out(),
        },
    ));
    assert_eq!(allowed(&accounts), []);

    accounts.disconnected();
    assert!(accounts.receive_snapshot(
        Some(8),
        LinkEvent::AccountUnsupported {
            generation: 8,
            view: signed_in(PlanState::Enabled, Some(AccountRecovery::Retry)),
        },
    ));
    assert!(!accounts.receive(Some(8), 7, signed_out()));
    assert_eq!(allowed(&accounts), [AccountOp::SignOut]);
}

#[test]
fn enabling_plan_and_signing_in_again_preserve_the_previous_account_until_answered() {
    for (start, op) in [
        (signed_out(), AccountOp::SignIn),
        (signed_in(PlanState::Declined, None), AccountOp::EnablePlan),
        (
            signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain)),
            AccountOp::SignIn,
        ),
    ] {
        let mut accounts = Accounts::default();
        let mut sent = Vec::new();
        receive(&mut accounts, start.clone());
        press(&mut accounts, op, &mut sent);
        assert_eq!(accounts.view, Some(start.clone()));
        for blocked in OPS {
            assert!(!accounts.send(blocked, |_| panic!(
                "no operation may bypass a pending sign-in"
            )));
        }
        receive(&mut accounts, AccountView::SigningIn);
        assert_eq!(
            allowed(&accounts),
            [AccountOp::SignIn, AccountOp::CancelSignIn]
        );
        press(&mut accounts, AccountOp::CancelSignIn, &mut sent);
        receive(&mut accounts, start);
        assert!(accounts.allows(op));
        press(&mut accounts, op, &mut sent);
        receive(&mut accounts, AccountView::SigningIn);
        receive(&mut accounts, signed_in(PlanState::Enabled, None));
        assert_eq!(allowed(&accounts), [AccountOp::SignOut]);
        assert_eq!(sent, vec![op, AccountOp::CancelSignIn, op]);
    }
}

#[test]
fn open_again_replaces_an_attempt_but_immediate_duplicate_presses_are_rejected() {
    let mut accounts = Accounts::default();
    let mut sent = Vec::new();
    receive(&mut accounts, AccountView::SigningIn);
    press(&mut accounts, AccountOp::SignIn, &mut sent);
    assert!(!accounts.send(AccountOp::SignIn, |_| panic!(
        "duplicate intent must not be sent"
    )));
    assert_eq!(accounts.view, Some(AccountView::SigningIn));
    receive(&mut accounts, AccountView::SigningIn);
    press(&mut accounts, AccountOp::SignIn, &mut sent);
    assert_eq!(sent, vec![AccountOp::SignIn, AccountOp::SignIn]);
}

#[test]
fn every_plan_and_recovery_has_exact_operation_policy() {
    let mut accounts = Accounts::default();
    assert_eq!(allowed(&accounts), vec![]);
    for remote in [false, true] {
        receive(
            &mut accounts,
            AccountView::SignedOut {
                revocation_unconfirmed: remote,
            },
        );
        assert_eq!(allowed(&accounts), vec![AccountOp::SignIn]);
        for (cleanup, expected) in [
            (Cleanup::Running, vec![]),
            (Cleanup::Refused, vec![AccountOp::Retry]),
            (Cleanup::NotDurable, vec![AccountOp::Retry]),
        ] {
            receive(
                &mut accounts,
                AccountView::SigningOut {
                    cleanup,
                    revocation_unconfirmed: remote,
                },
            );
            assert_eq!(allowed(&accounts), expected, "{cleanup:?}, remote={remote}");
        }
    }
    receive(&mut accounts, AccountView::SigningIn);
    assert_eq!(
        allowed(&accounts),
        vec![AccountOp::SignIn, AccountOp::CancelSignIn]
    );
    receive(&mut accounts, AccountView::Busy);
    assert_eq!(allowed(&accounts), vec![AccountOp::Retry]);
    for plan in [
        PlanState::Enabled,
        PlanState::Declined,
        PlanState::Unavailable,
    ] {
        for (recovery, expected) in [
            (
                None,
                if plan == PlanState::Declined {
                    vec![AccountOp::SignOut, AccountOp::EnablePlan]
                } else {
                    vec![AccountOp::SignOut]
                },
            ),
            (
                Some(AccountRecovery::Retry),
                vec![AccountOp::SignOut, AccountOp::Retry],
            ),
            (
                Some(AccountRecovery::SignInAgain),
                vec![AccountOp::SignIn, AccountOp::SignOut],
            ),
        ] {
            receive(&mut accounts, signed_in(plan, recovery));
            assert_eq!(allowed(&accounts), expected, "{plan:?}, {recovery:?}");
        }
    }
    receive(
        &mut accounts,
        AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None,
        },
    );
    assert_eq!(allowed(&accounts), vec![AccountOp::SignOut]);
}

#[test]
fn any_authoritative_view_can_become_busy_and_retry_can_restore_the_account() {
    let starts = [
        signed_out(),
        AccountView::SigningIn,
        signed_in(PlanState::Declined, None),
        AccountView::SigningOut {
            cleanup: Cleanup::Refused,
            revocation_unconfirmed: true,
        },
        AccountView::Busy,
    ];
    for start in starts {
        let mut accounts = Accounts::default();
        let mut sent = Vec::new();
        receive(&mut accounts, start);
        receive(&mut accounts, AccountView::Busy);
        press(&mut accounts, AccountOp::Retry, &mut sent);
        receive(&mut accounts, signed_in(PlanState::Enabled, None));
        assert_eq!(sent, vec![AccountOp::Retry]);
    }
}

#[test]
fn sign_out_and_cleanup_retries_never_offer_sign_in_before_signed_out() {
    for remote in [false, true] {
        let mut accounts = Accounts::default();
        let mut sent = Vec::new();
        receive(&mut accounts, signed_in(PlanState::Unavailable, None));
        press(&mut accounts, AccountOp::SignOut, &mut sent);
        receive(
            &mut accounts,
            AccountView::SigningOut {
                cleanup: Cleanup::Running,
                revocation_unconfirmed: remote,
            },
        );
        assert_eq!(allowed(&accounts), vec![]);
        for cleanup in [Cleanup::Refused, Cleanup::NotDurable] {
            receive(
                &mut accounts,
                AccountView::SigningOut {
                    cleanup,
                    revocation_unconfirmed: remote,
                },
            );
            press(&mut accounts, AccountOp::Retry, &mut sent);
        }
        receive(
            &mut accounts,
            AccountView::SignedOut {
                revocation_unconfirmed: remote,
            },
        );
        assert_eq!(allowed(&accounts), vec![AccountOp::SignIn]);
        assert_eq!(
            sent,
            vec![AccountOp::SignOut, AccountOp::Retry, AccountOp::Retry]
        );
    }
}

#[test]
fn retry_errors_supplement_valid_states_without_unlocking_forbidden_operations() {
    let retry = error(ErrorKey::RevocationBacklog, vec![Action::Retry]);
    let mut accounts = Accounts::default();
    let mut sent = Vec::new();
    receive(&mut accounts, signed_out());
    accounts.error(&retry);
    assert_eq!(allowed(&accounts), vec![AccountOp::Retry]);
    receive(&mut accounts, signed_out());
    assert_eq!(accounts.error, Some(retry.clone()));
    press(&mut accounts, AccountOp::Retry, &mut sent);
    assert_eq!(accounts.error, None);
    assert_eq!(sent, vec![AccountOp::Retry]);

    let actions = error(
        ErrorKey::AccountOffline,
        vec![Action::SignIn, Action::Enable, Action::Retry],
    );
    for (view, expected) in [
        (
            AccountView::SigningOut {
                cleanup: Cleanup::Running,
                revocation_unconfirmed: false,
            },
            vec![],
        ),
        (
            AccountView::SigningOut {
                cleanup: Cleanup::Refused,
                revocation_unconfirmed: true,
            },
            vec![AccountOp::Retry],
        ),
        (AccountView::Busy, vec![AccountOp::Retry]),
        (
            signed_in(PlanState::Enabled, None),
            vec![AccountOp::SignOut, AccountOp::Retry],
        ),
        (
            signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain)),
            vec![AccountOp::SignIn, AccountOp::SignOut, AccountOp::Retry],
        ),
    ] {
        receive(&mut accounts, view);
        accounts.error(&actions);
        assert_eq!(allowed(&accounts), expected);
    }
}

#[test]
fn error_before_changed_view_survives_its_answer_and_repeats_then_later_change_clears_it() {
    let enabled = signed_in(PlanState::Enabled, None);
    for (key, action, before, answer, later) in [
        (
            ErrorKey::SignInTimedOut,
            Some(Action::SignIn),
            AccountView::SigningIn,
            signed_out(),
            AccountView::SigningIn,
        ),
        (
            ErrorKey::SignInFailed,
            Some(Action::SignIn),
            AccountView::SigningIn,
            signed_out(),
            AccountView::SigningIn,
        ),
        (
            ErrorKey::BrowserUnavailable,
            Some(Action::SignIn),
            AccountView::SigningIn,
            signed_out(),
            AccountView::SigningIn,
        ),
        (
            ErrorKey::SessionEnded,
            Some(Action::SignIn),
            enabled.clone(),
            signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain)),
            AccountView::SigningIn,
        ),
        (
            ErrorKey::SignOutNotDurable,
            Some(Action::Retry),
            enabled.clone(),
            AccountView::SigningOut {
                cleanup: Cleanup::NotDurable,
                revocation_unconfirmed: true,
            },
            signed_out(),
        ),
        (
            ErrorKey::PlanUnavailable,
            None,
            enabled,
            signed_in(PlanState::Unavailable, None),
            signed_out(),
        ),
    ] {
        let mut accounts = Accounts::default();
        receive(&mut accounts, before);
        let failure = error(key, action.into_iter().collect());
        accounts.error(&failure);
        assert_eq!(accounts.error, Some(failure.clone()), "{key:?}");
        receive(&mut accounts, answer.clone());
        assert_eq!(
            accounts.error,
            Some(failure.clone()),
            "first changed view, {key:?}"
        );
        receive(&mut accounts, answer);
        assert_eq!(
            accounts.error,
            Some(failure),
            "identical next view, {key:?}"
        );
        receive(&mut accounts, later);
        assert_eq!(accounts.error, None, "later different view, {key:?}");
    }
}

#[test]
fn failed_send_keeps_error_and_view_and_allows_retry_accepted_send_clears_error() {
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_out());
    let failure = error(ErrorKey::RevocationBacklog, vec![Action::Retry]);
    accounts.error(&failure);
    let mut sent = Vec::new();
    assert!(!accounts.send(AccountOp::Retry, |op| {
        sent.push(op);
        false
    }));
    assert_eq!(accounts.view, Some(signed_out()));
    assert_eq!(accounts.error, Some(failure.clone()));
    assert!(accounts.send_failed);
    assert_eq!(accounts.waiting, None);
    assert_eq!(allowed(&accounts), vec![AccountOp::Retry]);
    assert!(!accounts.send(AccountOp::SignOut, |_| panic!("invalid operation")));
    assert!(accounts.send_failed);
    assert!(!accounts.receive(Some(7), 6, AccountView::Busy));
    assert!(accounts.send_failed);
    assert_eq!(accounts.error, Some(failure.clone()));
    receive(&mut accounts, signed_out());
    assert_eq!(accounts.error, Some(failure));
    press(&mut accounts, AccountOp::Retry, &mut sent);
    assert_eq!(accounts.error, None);
    assert!(!accounts.send_failed);
    assert_eq!(sent, vec![AccountOp::Retry, AccountOp::Retry]);
    receive(&mut accounts, signed_out());
    assert_eq!(accounts.error, None);
}

#[test]
fn backlog_blocks_new_sign_in_until_retry_is_accepted() {
    for view in [
        signed_out(),
        signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain)),
    ] {
        let mut accounts = Accounts::default();
        receive(&mut accounts, view.clone());
        accounts.error(&error(ErrorKey::RevocationBacklog, vec![Action::Retry]));
        assert!(!accounts.allows(AccountOp::SignIn));
        assert!(!accounts.send(AccountOp::SignIn, |_| panic!(
            "backlog must not open a browser"
        )));
        let mut sent = Vec::new();
        press(&mut accounts, AccountOp::Retry, &mut sent);
        receive(&mut accounts, view);
        assert!(accounts.allows(AccountOp::SignIn));
        assert_eq!(sent, vec![AccountOp::Retry]);
    }
}

#[test]
fn stale_views_cannot_settle_pending_intents_or_consume_error_association() {
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_out());
    let mut sent = Vec::new();
    press(&mut accounts, AccountOp::SignIn, &mut sent);
    let failure = error(ErrorKey::BrowserUnavailable, vec![Action::SignIn]);
    accounts.error(&failure);
    for (current, generation) in [(Some(7), 6), (Some(8), 7), (None, 7)] {
        assert!(!accounts.receive(current, generation, AccountView::Busy));
        assert_eq!(accounts.view, Some(signed_out()));
        assert_eq!(accounts.error, Some(failure.clone()));
        assert_eq!(accounts.waiting, Some(AccountOp::SignIn));
        assert!(!accounts.send_failed);
    }
    receive(&mut accounts, AccountView::SigningIn);
    assert_eq!(accounts.error, Some(failure));
    receive(&mut accounts, signed_out());
    assert_eq!(accounts.error, None);
    assert_eq!(sent, vec![AccountOp::SignIn]);
}

#[test]
fn disconnect_removes_identity_errors_and_delivery_state_without_replaying_operations() {
    for accepted in [false, true] {
        let mut accounts = Accounts::default();
        receive(&mut accounts, signed_in(PlanState::Enabled, None));
        let mut sent = Vec::new();
        assert_eq!(
            accounts.send(AccountOp::SignOut, |op| {
                sent.push(op);
                accepted
            }),
            accepted
        );
        accounts.error(&UserError::from(ErrorKey::CredentialsUnavailable));
        accounts.disconnected();
        assert_eq!(accounts.view, None);
        assert_eq!(accounts.error, None);
        assert_eq!(accounts.waiting, None);
        assert!(!accounts.send_failed);
        assert_eq!(allowed(&accounts), vec![]);
        assert!(!accounts.receive(Some(8), 7, signed_in(PlanState::Enabled, None)));
        assert_eq!(accounts.view, None);
        assert!(accounts.receive(Some(8), 8, signed_out()));
        assert_eq!(accounts.view, Some(signed_out()));
        assert_eq!(allowed(&accounts), vec![AccountOp::SignIn]);
        assert_eq!(sent, vec![AccountOp::SignOut]);
    }
}

#[test]
fn settings_and_request_errors_do_not_replace_or_extend_an_account_error() {
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_out());
    let needed = error(ErrorKey::ConnectionNeeded, vec![Action::OpenSettings]);
    accounts.error(&needed);
    assert_eq!(accounts.error, None);
    let failure = UserError::from(ErrorKey::SignInFailed);
    accounts.error(&failure);
    receive(&mut accounts, signed_out());
    accounts.error(&needed);
    assert_eq!(accounts.error, Some(failure.clone()));
    for key in [
        ErrorKey::SettingsRefused,
        ErrorKey::ConfigUnreadable,
        ErrorKey::UsageLimit,
        ErrorKey::AccountRefused,
        ErrorKey::RequestRefused,
        ErrorKey::ResponseUnreadable,
        ErrorKey::GitHubUnavailable,
    ] {
        accounts.error(&error(key, vec![Action::Retry]));
        assert_eq!(accounts.error, Some(failure.clone()));
        assert_eq!(allowed(&accounts), vec![AccountOp::SignIn]);
    }
    receive(&mut accounts, AccountView::SigningIn);
    assert_eq!(accounts.error, None);
}

#[test]
fn provider_sign_in_actions_survive_an_unchanged_signed_in_snapshot() {
    for key in [ErrorKey::AccountRefused, ErrorKey::SignInNeeded] {
        let mut accounts = Accounts::default();
        let view = signed_in(PlanState::Enabled, None);
        receive(&mut accounts, view.clone());
        let failure = error(key, vec![Action::SignIn]);
        accounts.error(&failure);
        receive(&mut accounts, view);
        assert_eq!(accounts.error, Some(failure));
        assert_eq!(
            allowed(&accounts),
            vec![AccountOp::SignIn, AccountOp::SignOut]
        );
        let mut sent = Vec::new();
        press(&mut accounts, AccountOp::SignIn, &mut sent);
        assert_eq!(sent, vec![AccountOp::SignIn]);
        assert_eq!(accounts.error, None);
        accounts.error(&error(key, vec![Action::SignIn]));
        assert!(!accounts.send(AccountOp::SignIn, |_| panic!("still waiting")));
        accounts.disconnected();
        assert_eq!(allowed(&accounts), Vec::<AccountOp>::new());
    }
}

#[test]
fn sign_in_recovery_offer_uses_the_same_typed_policy_as_admission() {
    for key in [
        ErrorKey::AccountRefused,
        ErrorKey::SignInNeeded,
        ErrorKey::SessionEnded,
        ErrorKey::RenewalUnconfirmed,
    ] {
        let mut accounts = Accounts::default();
        receive(&mut accounts, signed_in(PlanState::Declined, None));
        accounts.error(&error(key, vec![Action::SignIn]));
        assert!(accounts.sign_in_again(), "{key:?}");
        assert!(accounts.allows(AccountOp::SignIn), "{key:?}");
        assert!(accounts.send(AccountOp::SignIn, |_| true));
        accounts.error(&error(key, vec![Action::SignIn]));
        assert!(accounts.sign_in_again());
        assert!(!accounts.allows(AccountOp::SignIn));
    }
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_in(PlanState::Declined, None));
    accounts.error(&error(ErrorKey::PlanUnavailable, vec![Action::SignIn]));
    assert!(!accounts.sign_in_again());
    assert!(!accounts.allows(AccountOp::SignIn));
    assert!(accounts.allows(AccountOp::EnablePlan));
}

#[test]
fn sign_in_actions_never_bypass_waiting_or_revocation_backlog() {
    let mut accounts = Accounts::default();
    receive(&mut accounts, signed_in(PlanState::Enabled, None));
    accounts.error(&error(ErrorKey::RevocationBacklog, vec![Action::SignIn]));
    assert!(!accounts.send(AccountOp::SignIn, |_| panic!("revocation backlog")));
    assert_eq!(accounts.waiting, None);
    accounts.error(&error(ErrorKey::AccountRefused, vec![Action::Retry]));
    assert_eq!(
        accounts.error.as_ref().map(UserError::key),
        Some(ErrorKey::RevocationBacklog)
    );
    accounts.error(&error(ErrorKey::SignInNeeded, vec![Action::SignIn]));
    assert_eq!(
        accounts.error.as_ref().map(UserError::key),
        Some(ErrorKey::RevocationBacklog)
    );
    assert!(!accounts.allows(AccountOp::SignIn));
    for view in [
        AccountView::Busy,
        AccountView::SigningOut {
            cleanup: Cleanup::Running,
            revocation_unconfirmed: false,
        },
    ] {
        let mut accounts = Accounts::default();
        receive(&mut accounts, view);
        accounts.error(&error(ErrorKey::SignInNeeded, vec![Action::SignIn]));
        assert!(!accounts.send(AccountOp::SignIn, |_| panic!(
            "account is busy or signing out"
        )));
        assert_eq!(accounts.waiting, None);
    }
}

#[test]
fn account_status_prefers_recovery_to_each_plan_state() {
    for plan in [
        PlanState::Enabled,
        PlanState::Declined,
        PlanState::Unavailable,
    ] {
        for (recovery, expected) in [
            (
                AccountRecovery::SignInAgain,
                (Text::AccountSessionEnded, vec![]),
            ),
            (
                AccountRecovery::Retry,
                (
                    Text::AccountAttention,
                    vec![StatusLine::Note(Text::AccountRetryHelp)],
                ),
            ),
        ] {
            let mut state = Accounts::default();
            assert!(state.receive(
                Some(7),
                7,
                AccountView::SignedIn {
                    email: None,
                    plan,
                    recovery: Some(recovery),
                },
            ));
            assert_eq!(state.status(None), expected, "{plan:?}, {recovery:?}");
        }
    }
}

#[test]
fn remote_sign_out_copy_follows_the_cleanup_phase() {
    use StatusLine::{Local, Note, Remote};
    let refused = || Local {
        heading: Text::AccountLocal,
        error: Text::AccountCleanupRefused,
        help: Text::AccountCleanupRefusedHelp,
    };
    let not_durable = || Local {
        heading: Text::AccountLocal,
        error: Text::AccountNotDurable,
        help: Text::AccountNotDurableHelp,
    };
    let remote_line = || Remote {
        heading: Text::AccountRemote,
        note: Text::AccountRevocationPending,
    };
    for (cleanup, remote, notice, lines) in [
        (
            Cleanup::Running,
            false,
            None,
            vec![Note(Text::AccountStopped)],
        ),
        (
            Cleanup::Running,
            true,
            None,
            vec![Note(Text::AccountStopped)],
        ),
        (Cleanup::Refused, false, None, vec![refused()]),
        (
            Cleanup::Refused,
            true,
            Some(Text::AccountRevocationPending),
            vec![refused(), remote_line()],
        ),
        (Cleanup::NotDurable, false, None, vec![not_durable()]),
        (
            Cleanup::NotDurable,
            true,
            Some(Text::AccountRevocationPending),
            vec![not_durable(), remote_line()],
        ),
    ] {
        let view = AccountView::SigningOut {
            cleanup,
            revocation_unconfirmed: remote,
        };
        assert_eq!(cleanup_notice(&view), notice, "{view:?}");
        let mut state = Accounts::default();
        assert!(state.receive(Some(7), 7, view));
        assert_eq!(
            state.status(None),
            (
                if cleanup == Cleanup::Running {
                    Text::AccountSigningOut
                } else {
                    Text::AccountSignOutNeedsRetry
                },
                lines
            )
        );
    }
    for (revocation_unconfirmed, line) in [
        (false, Note(Text::AccountUsePlan)),
        (
            true,
            Remote {
                heading: Text::AccountRemote,
                note: Text::AccountRevocationUnconfirmed,
            },
        ),
    ] {
        let view = AccountView::SignedOut {
            revocation_unconfirmed,
        };
        let mut state = Accounts::default();
        assert!(state.receive(Some(7), 7, view));
        assert_eq!(state.status(None), (Text::AccountSignedOut, vec![line]));
    }
}

#[test]
fn status_passes_complete_notice_copy_to_the_view_in_display_order() {
    let mut state = Accounts::default();
    receive(
        &mut state,
        AccountView::SignedOut {
            revocation_unconfirmed: true,
        },
    );
    state.error(&UserError::from(ErrorKey::RevocationBacklog));
    state.send_failed = true;
    assert_eq!(
        state.status(None),
        (
            Text::AccountSignedOut,
            vec![
                StatusLine::Remote {
                    heading: Text::AccountRemote,
                    note: Text::AccountRevocationUnconfirmed,
                },
                StatusLine::Backlog {
                    lead: Text::AccountBacklogLead,
                    followups: [
                        Text::AccountBacklogRetry,
                        Text::AccountBacklogExpiry,
                        Text::AccountBacklogRecovery
                    ],
                },
                StatusLine::SendFailed(Text::AccountSendFailed),
            ]
        )
    );
}
