//! Refresh (design §3 rule 3): the 60 s rule, one flight through save and identity check, what
//! each refusal does, and what a sign-out landing mid-flight leaves. Results are fed in as data.

use super::*;
use crate::connections::http_client::HttpError;
use crate::connections::marker::MarkerUpdate;

const CLIENT: &str = "client-saved";

/// An account whose access token expires at `expires_at`; with `pending`, its set's identity
/// token still waits for its check.
fn account_at(
    expires_at: u64,
    pending: Option<&'static str>,
) -> impl FnOnce(&mut Vault<FakeStore>) {
    move |vault| {
        let mut account = saved_account("subject-1", "refresh-old").expose().clone();
        let tokens = account.tokens.as_mut().expect("tokens");
        tokens.expires_at = expires_at;
        tokens.pending_identity_validation = pending.is_some();
        tokens.id_token = pending.map(|token| Zeroizing::new(token.to_owned()));
        committed(&vault.save_account(&Secret::new(account)));
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a refresh's result, as the reducer takes it"
)]
fn rotated(id_token: Option<&str>) -> Result<TokenSet, ExchangeError> {
    Ok(TokenSet {
        access_token: secret("access-new"),
        refresh_token: Some(secret("refresh-new")),
        id_token: id_token.map(secret),
        scope: secret(PLAN),
        expires_in: 3600,
    })
}

#[expect(clippy::unnecessary_wraps, reason = "compared with recorded answers")]
fn bearer(text: &str) -> Result<String, Denied> {
    Ok(text.to_owned())
}

fn failed(key: ErrorKey, actions: &[Action]) -> Result<String, Denied> {
    Err(Denied::Failed(user_error(key, actions)))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "an identity check's result, as the reducer takes it"
)]
fn identity(subject: &str) -> Result<VerifiedIdentity, IdentityError> {
    Ok(VerifiedIdentity {
        subject: secret(subject),
        email: None,
    })
}

/// What a session the provider ended leaves: the registration and its display email.
fn ended() -> Account {
    Account {
        email: Some(Zeroizing::new(EMAIL.to_owned())),
        ..registration()
    }
}

fn full_list(vault: &mut Vault<FakeStore>) {
    let entries = (0..MAX_EXCHANGES)
        .map(|n| Revocable {
            client_id: Zeroizing::new(CLIENT.to_owned()),
            refresh_token: Zeroizing::new(format!("refresh-{n}")),
            added_at: NOW,
            origin: Origin::Exchange,
        })
        .collect();
    let list = crate::connections::records::RevocationList { entries };
    committed(&vault.save_revocations(&Secret::new(list)));
}

fn registration() -> Account {
    Account {
        client_id: Zeroizing::new(CLIENT.to_owned()),
        subject: Some(Zeroizing::new("subject-1".to_owned())),
        email: None,
        plan_enabled: false,
        tokens: None,
    }
}

impl Harness {
    fn access(&mut self, ticket: u64, now: u64) {
        self.feed(Input::Access {
            ticket,
            now: Some(now),
        });
    }

    fn refreshed(&mut self, result: Result<TokenSet, ExchangeError>) {
        let generation = self.refreshes.last().expect("a refresh sent").0;
        self.feed(Input::Refreshed {
            generation,
            result,
            now: Some(NOW),
        });
    }

    fn validated(&mut self, result: Result<VerifiedIdentity, IdentityError>) {
        let generation = self.validations.last().expect("a check started").0;
        self.feed(Input::Validated {
            generation,
            result,
            now: self.wall.unix_seconds(),
        });
    }

    fn tokens(&self) -> Tokens {
        self.account().expect("an account").tokens.expect("tokens")
    }

    /// Confirms every revocation run until the sign-out ends.
    fn sign_out_confirmed(&mut self) {
        let mut answered = 0;
        while self.revoking.len() > answered {
            let run = self.revoking[answered].clone();
            answered += 1;
            self.feed(Input::Revoked(run));
        }
    }
}

fn run(entries: &[Secret<Revocable>]) -> Vec<(String, Origin)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.expose().refresh_token.to_string(),
                entry.expose().origin,
            )
        })
        .collect()
}

/// The local 60 s rule, one second short of it and at it.
#[test]
fn a_bearer_is_refreshed_once_it_has_sixty_seconds_left() {
    let mut h = Harness::seeded("refresh-early", account_at(NOW + 61, None));
    h.access(1, NOW);
    assert_eq!(h.answers, [(1, bearer("old-access"))]);
    assert!(h.refreshes.is_empty());
    h.access(2, NOW + 1);
    assert_eq!(
        h.refreshes,
        [(0, CLIENT.to_owned(), "refresh-old".to_owned())]
    );
    assert_eq!(h.answers.len(), 1, "the caller waits for the flight");
    assert_eq!(h.marker(), marker(&["request_pending"]));
}

#[test]
fn nothing_is_handed_out_without_a_usable_account_or_its_plan() {
    let mut h = Harness::new("refresh-signed-out");
    h.access(1, NOW);
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);
    let mut h = Harness::seeded("refresh-no-plan", |vault| {
        let mut account = saved_account("subject-1", "refresh-old").expose().clone();
        account.plan_enabled = false;
        account.tokens.as_mut().expect("tokens").expires_at = NOW + 3600;
        committed(&vault.save_account(&Secret::new(account)));
    });
    h.access(1, NOW);
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::PlanNotEnabled, &[Action::Enable]))]
    );
}

#[test]
fn the_rotated_set_is_saved_before_its_identity_is_checked() {
    let mut h = Harness::seeded("refresh-save-first", account_at(NOW + 60, None));
    h.access(1, NOW);
    h.hold = true;
    h.refreshed(rotated(Some("id-new")));
    assert!(
        h.validations.is_empty(),
        "nothing else goes out before the save"
    );
    h.release();
    let tokens = h.tokens();
    assert_eq!(
        (
            tokens.access_token.as_str(),
            tokens.refresh_token.as_str(),
            tokens.id_token.as_deref().map(String::as_str),
            tokens.expires_at,
            tokens.pending_identity_validation,
        ),
        (
            "access-new",
            "refresh-new",
            Some("id-new"),
            NOW + 3600,
            true
        )
    );
    assert_eq!(h.marker(), None, "the saved set owns the request");
    assert_eq!(
        h.validations,
        [(1, "subject-1".to_owned(), "id-new".to_owned())]
    );
    assert!(h.answers.is_empty());
    h.validated(identity("subject-1"));
    assert_eq!(h.answers, [(1, bearer("access-new"))]);
    assert!(!h.tokens().pending_identity_validation);
}

#[test]
fn callers_join_one_flight_through_its_identity_check() {
    let mut h = Harness::seeded("refresh-one-flight", account_at(NOW, None));
    h.access(1, NOW);
    h.access(2, NOW);
    assert_eq!(h.refreshes.len(), 1);
    h.refreshed(rotated(Some("id-new")));
    h.access(3, NOW);
    assert_eq!(h.validations.len(), 1);
    h.validated(Err(IdentityError::Http(HttpError::Timeout)));
    let offline = failed(ErrorKey::AccountOffline, &[Action::Retry]);
    assert_eq!(
        h.answers,
        [(1, offline.clone()), (2, offline.clone()), (3, offline)]
    );
    assert!(h.tokens().pending_identity_validation, "still blocked");
    h.access(4, NOW);
    h.access(5, NOW);
    assert_eq!(h.validations.len(), 2, "a retried check is one more flight");
    assert_eq!(h.refreshes.len(), 1, "with no second refresh");
    h.validated(identity("subject-1"));
    assert_eq!(
        h.answers[3..],
        [(4, bearer("access-new")), (5, bearer("access-new"))]
    );
}

#[test]
fn a_refused_save_holds_the_rotated_set_until_the_next_use_saves_it() {
    let mut h = Harness::seeded("refresh-held", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(rotated(None));
    assert_eq!(
        h.answers,
        [(
            1,
            failed(ErrorKey::CredentialsUnavailable, &[Action::Retry])
        )]
    );
    assert_eq!(h.marker(), marker(&["request_pending"]), "held, not owned");
    h.store.0.lock().expect("fake lock").refusing = false;
    h.access(2, NOW);
    assert_eq!(
        h.refreshes.len(),
        1,
        "the rotated token is never asked for again"
    );
    assert_eq!(h.answers[1..], [(2, bearer("access-new"))]);
    assert_eq!(h.tokens().refresh_token.as_str(), "refresh-new");
    assert_eq!(h.marker(), None);
}

#[test]
fn a_session_the_provider_ended_keeps_only_the_registration() {
    for code in [OAuthCode::InvalidGrant, OAuthCode::SessionEnded] {
        let mut h = Harness::seeded("refresh-ended", account_at(NOW, None));
        h.access(1, NOW);
        h.refreshed(Err(ExchangeError::OAuth { status: 400, code }));
        assert_eq!(
            h.answers,
            [(1, failed(ErrorKey::SessionEnded, &[Action::SignIn]))],
            "{code:?}"
        );
        assert_eq!(h.account(), Some(ended()), "{code:?}");
        assert_eq!(
            h.view(),
            &signed_in(PlanState::Declined, Some(AccountRecovery::SignInAgain)),
            "{code:?}"
        );
        assert_eq!(h.marker(), None, "{code:?}: a typed refusal issued nothing");
        h.access(2, NOW);
        assert_eq!(h.answers[1..], [(2, Err(Denied::Unusable))], "{code:?}");
        h.op(AccountOp::SignOut);
        assert_eq!(h.view(), &signed_out(), "{code:?}: nothing was listed");
        assert_eq!(h.account(), Some(registration()), "{code:?}");
    }
}

#[test]
fn other_refusals_keep_the_credentials() {
    for (status, code, expected) in [
        (
            400,
            OAuthCode::PlanBusy,
            failed(ErrorKey::AccountOffline, &[Action::Retry]),
        ),
        (
            401,
            OAuthCode::InvalidClient,
            failed(ErrorKey::AccountMisconfigured, &[]),
        ),
        (
            403,
            OAuthCode::GrantRefused,
            failed(ErrorKey::AccountMisconfigured, &[]),
        ),
        (
            403,
            OAuthCode::NotEligible,
            failed(ErrorKey::PlanUnavailable, &[]),
        ),
        (
            429,
            OAuthCode::Other,
            failed(ErrorKey::AccountOffline, &[Action::Retry]),
        ),
        (
            400,
            OAuthCode::Other,
            failed(ErrorKey::AccountMisconfigured, &[]),
        ),
    ] {
        let mut h = Harness::seeded("refresh-refused", account_at(NOW, None));
        h.access(1, NOW);
        h.refreshed(Err(ExchangeError::OAuth { status, code }));
        assert_eq!(h.answers, [(1, expected)], "{status} {code:?}");
        assert_eq!(
            h.refresh_token(),
            Some("refresh-old".to_owned()),
            "{code:?}"
        );
        assert_eq!(h.marker(), None, "{code:?}");
    }
}

/// A timeout after the request went out may have consumed the token: the session ends, and the
/// token stays for sign-out to revoke but is never sent again, after a restart too.
#[test]
fn an_ambiguous_refresh_keeps_the_credentials_and_its_uncertainty() {
    let mut h = Harness::seeded("refresh-ambiguous", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(Err(ExchangeError::Http(HttpError::Timeout)));
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::SessionEnded, &[Action::SignIn]))]
    );
    assert_eq!(h.refresh_token(), Some("refresh-old".to_owned()));
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
    let ended = signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain));
    assert_eq!(h.view(), &ended);
    h.access(2, NOW);
    assert_eq!(h.answers[1..], [(2, Err(Denied::Unusable))]);
    assert_eq!(h.refreshes.len(), 1);
    let mut h = h.restart();
    assert_eq!(h.view(), &ended);
    h.access(3, NOW);
    assert_eq!(h.answers, [(3, Err(Denied::Unusable))]);
    assert!(h.refreshes.is_empty(), "never sent again");
    h.op(AccountOp::SignOut);
    h.sign_out_confirmed();
    assert_eq!(
        run(h.revoking.last().expect("a revocation run")),
        [("refresh-old".to_owned(), Origin::Account)]
    );
    assert_eq!(
        h.view(),
        &AccountView::SignedOut {
            revocation_unconfirmed: true
        }
    );
}

/// A set whose token is for another subject, or is no valid token, ends the session, and its
/// refresh token is listed as an exchange's would be; with the list full, it's unknown.
#[test]
fn a_rotated_set_that_fails_its_check_is_listed_and_ends_the_session() {
    for failure in [IdentityError::OtherAccount, IdentityError::Token] {
        let mut h = Harness::seeded("refresh-other", account_at(NOW, None));
        h.access(1, NOW);
        h.refreshed(rotated(Some("id-new")));
        h.validated(Err(failure));
        assert_eq!(
            h.answers,
            [(1, failed(ErrorKey::SessionEnded, &[Action::SignIn]))],
            "{failure:?}"
        );
        assert_eq!(h.account(), Some(ended()), "{failure:?}");
        assert_eq!(
            h.revocations(),
            [(
                CLIENT.to_owned(),
                "refresh-new".to_owned(),
                NOW,
                Origin::Exchange
            )],
            "{failure:?}"
        );
        assert_eq!(h.marker(), marker(&["sweep"]), "{failure:?}");
    }
    let mut h = Harness::seeded("refresh-other-full", |vault| {
        account_at(NOW, None)(vault);
        full_list(vault);
    });
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    h.validated(Err(IdentityError::OtherAccount));
    assert_eq!(h.revocations().len(), MAX_EXCHANGES);
    assert_eq!(h.marker(), marker(&["sweep", "outcome_unknown"]));
}

/// N1: a rejected set's token stays in `chatgpt` until its list entry commits, and only then is
/// the registration saved over it; with the list full, the unknown outcome is written first.
#[test]
fn a_rejected_set_is_listed_before_its_token_leaves_the_account() {
    let mut h = Harness::seeded("refresh-reject-order", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    let before = h.marker();
    h.hold = true;
    h.validated(Err(IdentityError::OtherAccount));
    assert!(matches!(h.held, Some(Job::List { .. })), "{:?}", h.held);
    assert_eq!(h.refresh_token(), Some("refresh-new".to_owned()));
    assert_eq!(h.marker(), before);
    h.release();
    assert_eq!(
        h.revocations(),
        [(
            CLIENT.to_owned(),
            "refresh-new".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
    assert_eq!(h.account(), Some(ended()));

    let mut h = Harness::seeded("refresh-reject-order-full", |vault| {
        account_at(NOW, None)(vault);
        full_list(vault);
    });
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    h.hold = true;
    h.validated(Err(IdentityError::OtherAccount));
    assert!(
        matches!(
            h.held,
            Some(Job::Marker(MarkerUpdate::RequestFinished {
                outcome_unknown: true
            }))
        ),
        "{:?}",
        h.held
    );
    assert_eq!(h.refresh_token(), Some("refresh-new".to_owned()));
    h.release();
    assert_eq!(h.marker(), marker(&["sweep", "outcome_unknown"]));
    assert_eq!(h.account(), Some(ended()));
}

/// H1: a check that lands while a sign-in's commit runs never overwrites the new account.
#[test]
fn a_check_that_lands_during_a_sign_in_commit_never_overwrites_it() {
    let mut h = Harness::seeded("refresh-commit-race", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    h.op(AccountOp::SignIn);
    h.callback(CLIENT);
    h.hold = true;
    h.reply(received(Some("refresh-signed-in"), "subject-1", PLAN));
    assert!(matches!(h.held, Some(Job::Commit(_))));
    h.validated(identity("subject-1"));
    h.release();
    assert_eq!(h.refresh_token(), Some("refresh-signed-in".to_owned()));
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);
}

/// H2: the check's save is off the lane, so it never clears an exchange's pending flag.
#[test]
fn a_check_saved_while_a_sign_in_is_out_leaves_its_flag_set() {
    let mut h = Harness::seeded("refresh-check-flag", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    h.op(AccountOp::SignIn);
    h.callback(CLIENT);
    assert_eq!(h.sent.len(), 1);
    h.validated(identity("subject-1"));
    assert_eq!(h.answers, [(1, bearer("access-new"))]);
    assert_eq!(h.marker(), marker(&["request_pending"]));
}

/// M1: a held list save goes again at the next use. The token the listed successor replaced
/// was consumed, so the session ended: that use is a sign-in, and the token is never sent again.
#[test]
fn a_held_list_save_is_saved_again_at_the_next_use() {
    let mut h = Harness::seeded("refresh-held-list", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-unusable")),
    }));
    h.store.0.lock().expect("fake lock").refusing = false;
    assert_eq!(
        h.view(),
        &signed_in(PlanState::Declined, Some(AccountRecovery::SignInAgain))
    );
    h.access(2, NOW);
    assert_eq!(h.answers[1..], [(2, Err(Denied::Unusable))]);
    assert_eq!(h.refreshes.len(), 1, "the consumed token never goes again");
    assert_eq!(h.refresh_token(), Some("refresh-old".to_owned()));
    h.op(AccountOp::SignIn);
    assert_eq!(h.revocations()[0].1, "refresh-unusable");
    assert_eq!(h.account(), Some(ended()));
}

/// A refresh out at sign-out that didn't rotate left the old token in place: nothing's unknown.
#[test]
fn a_refresh_out_at_sign_out_that_kept_its_token_leaves_nothing_unknown() {
    let mut h = Harness::seeded("refresh-kept-at-sign-out", account_at(NOW, None));
    h.access(1, NOW);
    h.op(AccountOp::SignOut);
    h.refreshed(Ok(TokenSet {
        refresh_token: None,
        ..rotated(None).expect("a set")
    }));
    assert_eq!(
        run(&h.revoking[0]),
        [("refresh-old".to_owned(), Origin::Account)]
    );
    h.sign_out_confirmed();
    assert_eq!(h.view(), &signed_out());
}

#[test]
fn a_set_left_pending_by_a_restart_is_checked_before_it_is_used() {
    let mut h = Harness::seeded("refresh-pending", account_at(NOW + 3600, Some("id-old")));
    h.access(1, NOW);
    assert_eq!(
        h.validations,
        [(0, "subject-1".to_owned(), "id-old".to_owned())]
    );
    assert!(h.refreshes.is_empty() && h.answers.is_empty());
    h.validated(identity("subject-1"));
    assert_eq!(h.answers, [(1, bearer("old-access"))]);
    assert!(!h.tokens().pending_identity_validation);
}

/// One request on the lane at a time, whichever arrives first.
#[test]
fn a_refresh_and_an_exchange_share_the_lane_in_either_order() {
    let mut h = Harness::seeded("refresh-lane-exchange-first", account_at(NOW, None));
    h.op(AccountOp::SignIn);
    h.callback(CLIENT);
    assert_eq!(h.sent.len(), 1);
    h.access(1, NOW);
    assert!(h.refreshes.is_empty(), "it waits for the exchange");
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert!(h.refreshes.is_empty(), "the account it was for is gone");
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);

    let mut h = Harness::seeded("refresh-lane-refresh-first", account_at(NOW, None));
    h.access(1, NOW);
    h.op(AccountOp::SignIn);
    h.callback(CLIENT);
    assert!(h.sent.is_empty(), "the code waits for the refresh");
    h.refreshed(rotated(None));
    assert_eq!(h.answers, [(1, bearer("access-new"))]);
    assert_eq!(h.sent.len(), 1, "then it goes");
}

#[test]
fn a_refresh_held_at_its_gate_while_a_sign_out_lands_is_never_sent() {
    let mut h = Harness::seeded("refresh-gate", account_at(NOW, None));
    h.hold = true;
    h.access(1, NOW);
    h.op(AccountOp::SignOut);
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);
    h.release();
    assert!(h.refreshes.is_empty());
    h.sign_out_confirmed();
    assert_eq!(
        run(&h.revoking[0]),
        [("refresh-old".to_owned(), Origin::Account)]
    );
    assert_eq!(h.view(), &signed_out());
}

#[test]
fn a_refresh_out_at_sign_out_leaves_its_successor_to_be_revoked() {
    let mut h = Harness::seeded("refresh-successor", account_at(NOW, None));
    h.access(1, NOW);
    h.op(AccountOp::SignOut);
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);
    assert!(h.revoking.is_empty(), "Prepare waits for the refresh");
    h.refreshed(rotated(Some("id-new")));
    assert_eq!(
        run(&h.revoking[0]),
        [("refresh-new".to_owned(), Origin::Account)],
        "the successor, not the token it rotated"
    );
    assert!(
        h.validations.is_empty(),
        "a replaced account's set is never checked"
    );
    h.sign_out_confirmed();
    assert_eq!(h.view(), &signed_out());
    assert_eq!(h.account(), Some(registration()));
}

#[test]
fn a_check_out_at_sign_out_saves_nothing() {
    let mut h = Harness::seeded("refresh-check-sign-out", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(rotated(Some("id-new")));
    h.op(AccountOp::SignOut);
    let jobs = h.markers.len();
    h.validated(identity("subject-1"));
    assert_eq!(
        h.markers.len(),
        jobs,
        "no save runs for the replaced account"
    );
    assert_eq!(h.answers, [(1, Err(Denied::Unusable))]);
    h.sign_out_confirmed();
    assert_eq!(h.view(), &signed_out());
}

/// An unusable answer that named a successor lists it as an exchange's would be, so the rotated
/// token is revoked; with the list's exchange slots full, it can't be owned and stays unknown.
/// The token it replaced was consumed, so the session ends and that token is never sent again.
#[test]
fn an_unusable_refresh_that_named_a_successor_lists_it() {
    let unusable = || {
        Err(ExchangeError::Unusable {
            refresh_token: Some(secret("refresh-unusable")),
        })
    };
    let mut h = Harness::seeded("refresh-unusable", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(unusable());
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::SessionEnded, &[Action::SignIn]))]
    );
    h.access(2, NOW);
    assert_eq!(h.answers[1..], [(2, Err(Denied::Unusable))]);
    assert_eq!(h.refreshes.len(), 1, "the consumed token never goes again");
    assert_eq!(
        h.revocations(),
        [(
            CLIENT.to_owned(),
            "refresh-unusable".to_owned(),
            NOW,
            Origin::Exchange
        )]
    );
    assert_eq!(h.account(), Some(ended()));
    assert_eq!(h.marker(), marker(&["sweep"]));

    let mut h = Harness::seeded("refresh-unusable-full", |vault| {
        account_at(NOW, None)(vault);
        full_list(vault);
    });
    h.access(1, NOW);
    h.refreshed(unusable());
    assert_eq!(
        h.revocations().len(),
        MAX_EXCHANGES,
        "nothing dropped, nothing added"
    );
    assert_eq!(h.marker(), marker(&["sweep", "outcome_unknown"]));
    assert_eq!(h.account(), Some(ended()));
}

/// The token is marked as sent before it goes out. A crash before its answer is saved, or with
/// the rotated set held after a refused save, ends the session at the next start rather than
/// sending it again; a successor saved before a crash is used as usual.
#[test]
fn a_refresh_unanswered_at_a_crash_is_never_sent_again() {
    let ended = signed_in(PlanState::Enabled, Some(AccountRecovery::SignInAgain));
    let mut h = Harness::seeded("refresh-crash", account_at(NOW, None));
    h.access(1, NOW);
    assert_eq!(h.refreshes.len(), 1);
    assert!(h.tokens().refresh_sent, "marked before it went out");
    let mut h = h.restart();
    assert_eq!(h.view(), &ended);
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
    h.access(2, NOW);
    assert_eq!(h.answers, [(2, Err(Denied::Unusable))]);
    assert!(h.refreshes.is_empty());

    let mut h = Harness::seeded("refresh-crash-held", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(rotated(None));
    h.store.0.lock().expect("fake lock").refusing = false;
    let mut h = h.restart();
    assert_eq!(h.view(), &ended);
    h.access(2, NOW);
    assert_eq!(h.answers, [(2, Err(Denied::Unusable))]);
    assert!(h.refreshes.is_empty());
    assert_eq!(
        h.refresh_token(),
        Some("refresh-old".to_owned()),
        "kept for sign-out"
    );

    let mut h = Harness::seeded("refresh-crash-saved", |vault| {
        account_at(NOW + 3600, None)(vault);
        vault
            .update_marker(MarkerUpdate::RequestStarted)
            .expect("pending flag");
    });
    h.access(1, NOW);
    assert_eq!(h.answers, [(1, bearer("old-access"))]);
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
}

/// A connection that never opened sent nothing: the token's mark is cleared, and the next use
/// sends it again.
#[test]
fn an_offline_refresh_stays_retryable() {
    let mut h = Harness::seeded("refresh-offline", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(Err(ExchangeError::Http(HttpError::Connect)));
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::AccountOffline, &[Action::Retry]))]
    );
    assert!(!h.tokens().refresh_sent);
    assert_eq!(h.marker(), None);
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    h.access(2, NOW);
    assert_eq!(
        h.refreshes[1..],
        [(0, CLIENT.to_owned(), "refresh-old".to_owned())]
    );
}

/// Access during a retried rotated save joins it: no second refresh goes out for the same
/// account, and the one check answers.
#[test]
fn access_during_a_retried_rotated_save_joins_it() {
    let mut h = Harness::seeded("refresh-retried", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(rotated(Some("id-new")));
    h.store.0.lock().expect("fake lock").refusing = false;
    h.hold = true;
    h.op(AccountOp::Retry);
    assert!(matches!(h.held, Some(Job::Rotate { .. })), "{:?}", h.held);
    h.access(2, NOW);
    h.release();
    assert_eq!(h.refreshes.len(), 1, "no second grant");
    h.validated(identity("subject-1"));
    assert_eq!(h.answers[1..], [(2, bearer("access-new"))]);
    assert_eq!(h.refreshes.len(), 1);
    assert_eq!(h.validations.len(), 1, "one check for the one set");

    // With no caller waiting, Retry's commit starts the check alone; its flight lasts through
    // the check's save, so Access then joins it.
    let mut h = Harness::seeded("refresh-retried-check", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(rotated(Some("id-new")));
    h.store.0.lock().expect("fake lock").refusing = false;
    h.op(AccountOp::Retry);
    h.hold = true;
    h.validated(identity("subject-1"));
    assert!(matches!(h.held, Some(Job::Rotate { .. })), "{:?}", h.held);
    h.access(2, NOW);
    assert_eq!(h.validations.len(), 1, "no second check");
    h.release();
    assert_eq!(h.answers[1..], [(2, bearer("access-new"))]);
}

/// R3-1, in the order a second check could once take: a check's save is built when it's
/// dispatched, so a late one never undoes a sent mark saved first, and a token sent without a
/// committed successor is never refreshable after a restart.
#[test]
fn a_late_check_never_unmarks_a_sent_token() {
    let mut h = Harness::seeded("refresh-late-check", account_at(NOW, None));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").refusing = true;
    h.refreshed(rotated(Some("id-new")));
    h.store.0.lock().expect("fake lock").refusing = false;
    h.op(AccountOp::Retry);
    h.hold = true;
    h.validated(identity("subject-1"));
    h.jump(3600);
    h.access(2, NOW + 3600);
    h.hold = true;
    h.release();
    assert!(matches!(h.held, Some(Job::Marker(_))), "{:?}", h.held);
    h.hold = true;
    h.release();
    assert!(
        matches!(h.held, Some(Job::Rotate { .. })),
        "the mark's save is held"
    );
    h.validated(identity("subject-1"));
    h.release();
    assert_eq!(
        h.refreshes[1..],
        [(1, CLIENT.to_owned(), "refresh-new".to_owned())]
    );
    assert!(h.tokens().refresh_sent, "the mark is still on disk");
    let mut h = h.restart();
    h.access(3, NOW + 3600);
    assert_eq!(h.answers, [(3, Err(Denied::Unusable))]);
    assert!(h.refreshes.is_empty(), "never sent again");
}

/// The bearer's expiry is checked again, at the time its save ends, once a pending set is
/// verified: one that expired before or during the check, or while its save was held, is
/// renewed first. A renewal that is already expired is neither handed out nor renewed again.
#[test]
fn a_verified_set_that_is_due_is_renewed_once_before_it_is_handed_out() {
    let mut h = Harness::seeded(
        "refresh-pending-expired",
        account_at(NOW - 10, Some("id-old")),
    );
    h.access(1, NOW);
    h.validated(identity("subject-1"));
    assert!(h.answers.is_empty(), "no expired bearer");
    assert_eq!(
        h.refreshes,
        [(0, CLIENT.to_owned(), "refresh-old".to_owned())]
    );
    h.refreshed(rotated(None));
    assert_eq!(h.answers, [(1, bearer("access-new"))]);

    let mut h = Harness::seeded("refresh-pending-due", account_at(NOW + 120, Some("id-old")));
    h.access(1, NOW);
    h.jump(60);
    h.validated(identity("subject-1"));
    assert!(h.answers.is_empty(), "due by the time the check ended");
    assert_eq!(h.refreshes.len(), 1);

    let mut h = Harness::seeded("refresh-pending-zero", account_at(NOW - 10, Some("id-old")));
    h.access(1, NOW);
    h.validated(identity("subject-1"));
    h.refreshed(Ok(TokenSet {
        expires_in: 0,
        ..rotated(Some("id-new")).expect("a set")
    }));
    h.validated(identity("subject-1"));
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::SignInFailed, &[Action::Retry]))]
    );
    assert_eq!(h.refreshes.len(), 1, "renewed once");
    assert_eq!(
        (h.tokens().refresh_token.as_str(), h.tokens().refresh_sent),
        ("refresh-new", false),
        "the newest token stays saved"
    );

    let mut h = Harness::seeded("refresh-expires-in-check", account_at(NOW, None));
    h.access(1, NOW);
    h.refreshed(Ok(TokenSet {
        expires_in: 100,
        ..rotated(Some("id-new")).expect("a set")
    }));
    h.jump(200);
    h.validated(identity("subject-1"));
    assert_eq!(
        h.answers,
        [(1, failed(ErrorKey::SignInFailed, &[Action::Retry]))]
    );
    assert_eq!(h.refreshes.len(), 1);

    let mut h = Harness::seeded(
        "refresh-held-verified",
        account_at(NOW + 120, Some("id-old")),
    );
    h.access(1, NOW);
    h.hold = true;
    h.validated(identity("subject-1"));
    h.jump(180);
    h.release();
    assert!(h.answers.is_empty(), "no expired bearer");
    assert_eq!(h.refreshes.len(), 1, "renewed instead");
}

/// A save that ends after the bearer expired doesn't hand it out: its expiry is judged when the
/// save ends, not when the save began.
#[test]
fn a_bearer_that_expires_during_its_save_is_renewed() {
    let mut h = Harness::seeded("refresh-slow-save", account_at(NOW + 120, Some("id-old")));
    h.access(1, NOW);
    h.store.0.lock().expect("fake lock").slow_write = Some((Arc::clone(&h.wall.unix), 180));
    h.validated(identity("subject-1"));
    assert!(h.answers.is_empty(), "no bearer that expired in its save");
    assert_eq!(
        h.refreshes,
        [(0, CLIENT.to_owned(), "refresh-old".to_owned())]
    );
}

/// A pending flag the last run left is uncertainty from the next start on, and a rejected set's
/// list save ends no request: a sign-out after it stays unconfirmed.
#[test]
fn a_pending_flag_left_by_a_crash_outlives_a_rejected_set() {
    let mut h = Harness::seeded("refresh-orphan", |vault| {
        account_at(NOW + 3600, Some("id-new"))(vault);
        vault
            .update_marker(MarkerUpdate::RequestStarted)
            .expect("pending flag");
    });
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
    h.access(1, NOW);
    h.validated(Err(IdentityError::OtherAccount));
    assert_eq!(h.revocations()[0].1, "refresh-old");
    h.op(AccountOp::SignOut);
    h.sign_out_confirmed();
    assert_eq!(
        h.view(),
        &AccountView::SignedOut {
            revocation_unconfirmed: true
        }
    );
}
