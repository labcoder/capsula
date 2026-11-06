//! The credential lane and its save queue driven through the machine: dispatched saves, held
//! sets, the pending flag and the revocation list's room.

use super::*;

/// R2-4: a sign-in arriving while the current attempt's save runs waits for it; the save is
/// published first, and the new attempt starts against it.
#[test]
fn a_sign_in_during_a_dispatched_save_waits_and_the_save_is_published_first() {
    let mut h = Harness::new("account-boundary");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    assert!(h.held.is_some(), "the commit is dispatched and held");
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "nothing starts while the save runs");
    assert_eq!(h.view(), &AccountView::SigningIn);
    let views = h.views.len();
    h.release();
    assert_eq!(
        h.views[views..],
        [signed_in(PlanState::Enabled, None), AccountView::SigningIn],
        "published, then signing in again"
    );
    assert_eq!(h.listening.len(), 2);
    assert_eq!(
        h.refresh_token().as_deref(),
        Some("refresh-a"),
        "disk agrees"
    );
    h.op(AccountOp::CancelSignIn);
    assert_eq!(
        h.view(),
        &signed_in(PlanState::Enabled, None),
        "memory agrees"
    );
    assert!(h.revocations().is_empty());
}

#[test]
fn a_refused_dispatched_save_goes_to_the_list_when_a_sign_in_waited_on_it() {
    let mut h = Harness::new("account-boundary-refused");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    h.op(AccountOp::SignIn);
    h.store.arm(0, Fault::Refuse);
    h.release();
    assert_eq!(h.listening.len(), 2, "the waiting sign-in started");
    assert_eq!(h.view(), &AccountView::SigningIn);
    assert!(h.account().expect("registration").tokens.is_none());
    assert_eq!(h.revocations().len(), 1);
    assert_eq!(h.revocations()[0].1, "refresh-a");
    assert_eq!(h.marker(), marker(&["sweep"]));
}

#[test]
fn cancel_during_a_dispatched_save_lets_the_save_win() {
    let mut h = Harness::new("account-boundary-cancel");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.closed, 0);
    h.release();
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-a"));
}

/// Every mutation of a returning sign-in's commit refused in turn. The token it replaces is listed
/// first: refused up to that list's commit, nothing is listed; past it the token stays listed, and
/// its revocation pass starts, whatever the account save does. Up to the account's commit the set
/// is held and Retry saves it with no second request, entry or pass; after it, it's committed,
/// with old names left to sweep at worst.
#[test]
fn a_commit_refused_at_each_mutation_is_held_or_committed_by_the_flip() {
    let seed = |vault: &mut Vault<FakeStore>| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    };
    let until_commit = |h: &mut Harness| {
        h.op(AccountOp::SignIn);
        h.callback("client-saved");
        h.hold = true;
        h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    };
    let mut dry = Harness::seeded("account-commit-trace", seed);
    until_commit(&mut dry);
    dry.store.arm(usize::MAX, Fault::Refuse);
    dry.release();
    let trace = dry.store.0.lock().expect("fake lock").mutations.clone();
    let last_set = |record: &str| {
        let prefix = dry.generations(record);
        trace
            .iter()
            .rposition(|m| matches!(m, Mutation::Set(set) if set.starts_with(&prefix)))
            .expect(record)
    };
    let (listed, flip) = (last_set("revoke"), last_set("chatgpt"));
    assert!(listed < flip, "listed before the account changes");
    assert!(
        flip + 1 < trace.len(),
        "the old account's names are swept after the commit"
    );
    let old = [(
        "client-saved".to_owned(),
        "refresh-old".to_owned(),
        NOW,
        Origin::Exchange,
    )];
    let mut leftovers = 0;
    for at in 0..trace.len() {
        let mut h = Harness::seeded("account-commit-fault", seed);
        until_commit(&mut h);
        h.store.arm(at, Fault::Refuse);
        h.release();
        let owned: &[_] = if at > listed { &old } else { &[] };
        assert_eq!(h.revocations(), owned, "at {at}");
        assert_eq!(h.revoking.len(), usize::from(at > listed), "at {at}");
        if at <= flip {
            assert_eq!(h.refresh_token().as_deref(), Some("refresh-old"), "at {at}");
            assert_eq!(
                h.errors,
                [user_error(
                    ErrorKey::CredentialsUnavailable,
                    &[Action::Retry]
                )]
            );
            assert_eq!(
                h.view(),
                &signed_in(PlanState::Enabled, Some(AccountRecovery::Retry))
            );
            let flags: &[_] = if at > listed {
                &["sweep", "request_pending"]
            } else {
                &["request_pending"]
            };
            assert_eq!(h.marker(), marker(flags), "at {at}: the lane stays held");
            h.op(AccountOp::Retry);
        } else {
            leftovers += 1;
        }
        assert_eq!(h.refresh_token().as_deref(), Some("refresh-new"), "at {at}");
        assert_eq!(h.view(), &signed_in(PlanState::Enabled, None), "at {at}");
        assert_eq!(h.sent.len(), 1, "no second token request at {at}");
        assert_eq!(h.revocations(), old, "listed once at {at}");
        assert_eq!(h.revoking.len(), 1, "one pass at {at}");
        assert_eq!(h.marker(), marker(&["sweep"]), "at {at}");
    }
    assert!(
        leftovers > 0,
        "a cleanup refusal after the flip was exercised"
    );
}

#[test]
fn a_held_set_goes_to_the_list_once_its_attempt_is_cancelled() {
    let mut h = Harness::new("account-held-cancel");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    h.store.arm(0, Fault::Refuse);
    h.release();
    assert_eq!(h.view(), &signed_out());
    assert_eq!(h.marker(), marker(&["request_pending"]));
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.revocations().len(), 1);
    assert_eq!(h.revocations()[0].1, "refresh-a");
    assert_eq!(h.marker(), marker(&["sweep"]));
    h.op(AccountOp::Retry);
    assert!(
        h.account().expect("registration").tokens.is_none(),
        "never committed"
    );
}

/// Everything the store has been asked to change since it was last armed.
fn mutations(h: &Harness) -> usize {
    h.store.0.lock().expect("fake lock").mutations.len()
}

/// Every mutation is refused from now on, until `allow`.
fn refuse_all(h: &Harness) {
    h.store.0.lock().expect("fake lock").refusing = true;
}

fn allow(h: &Harness) {
    h.store.0.lock().expect("fake lock").refusing = false;
}

/// H1: a held set is saved again once per attempt move or Retry, never on every pass, however
/// long the store keeps refusing.
#[test]
fn a_held_set_under_a_lasting_refusal_is_retried_once_per_move_or_retry() {
    let mut h = Harness::new("account-held-lasting");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    refuse_all(&h);
    h.release();
    let jobs = h.markers.len();
    let before = mutations(&h);
    h.op(AccountOp::CancelSignIn);
    assert_eq!(
        h.markers.len(),
        jobs + 1,
        "one list save for the attempt's move"
    );
    let one_save = mutations(&h) - before;
    assert!((1..=129).contains(&one_save), "{one_save}");
    h.feed(Input::Connected);
    assert_eq!(h.markers.len(), jobs + 1, "nothing retries on its own");
    h.op(AccountOp::Retry);
    assert_eq!(h.markers.len(), jobs + 2, "one save per Retry");
    assert_eq!(
        mutations(&h) - before,
        2 * one_save,
        "the store saw two saves"
    );
    let refused = user_error(ErrorKey::CredentialsUnavailable, &[Action::Retry]);
    assert_eq!(h.errors, [refused.clone(), refused.clone(), refused]);
    allow(&h);
    h.op(AccountOp::Retry);
    assert_eq!(h.revocations().len(), 1);
    assert_eq!(h.revocations()[0].1, "refresh-a");
    assert_eq!(h.marker(), marker(&["sweep"]));
}

/// M1: a sign-in whose code would wait behind a held set ends when that set's save is refused
/// again, rather than staying `SigningIn` with nothing said.
#[test]
fn a_sign_in_behind_a_held_set_ends_when_the_set_is_refused_again() {
    let mut h = Harness::new("account-held-waiting");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    refuse_all(&h);
    h.release();
    h.op(AccountOp::SignIn);
    assert_eq!(h.view(), &signed_out(), "the new attempt ended");
    assert_eq!(h.closed, 1, "its listener closed");
    let refused = user_error(ErrorKey::CredentialsUnavailable, &[Action::Retry]);
    assert_eq!(h.errors, [refused.clone(), refused]);
    allow(&h);
    h.op(AccountOp::Retry);
    assert_eq!(h.revocations()[0].1, "refresh-a");
    h.op(AccountOp::SignIn);
    assert_eq!(h.view(), &AccountView::SigningIn, "the lane is free again");
}

#[test]
fn a_refused_pending_flag_sends_no_request() {
    let mut h = Harness::new("account-flag-refused");
    h.op(AccountOp::SignIn);
    h.hold = true;
    h.callback("client-issued");
    // The registration's save runs first; the pending flag is the next job.
    h.hold = true;
    h.release();
    h.refuse_commits();
    h.release();
    h.allow_commits();
    assert!(h.sent.is_empty());
    assert_eq!(
        h.errors,
        [user_error(
            ErrorKey::CredentialsUnavailable,
            &[Action::SignIn]
        )]
    );
    assert_eq!(h.marker(), None);
    assert_eq!(h.view(), &signed_out());
}

fn full_list(exchanges: usize) -> impl FnOnce(&mut Vault<FakeStore>) {
    move |vault| {
        let entries = (0..exchanges)
            .map(|n| Revocable {
                client_id: Zeroizing::new("client-old".to_owned()),
                refresh_token: Zeroizing::new(format!("refresh-{n}")),
                added_at: NOW,
                origin: Origin::Exchange,
            })
            .collect();
        let list = crate::connections::records::RevocationList { entries };
        committed(&vault.save_revocations(&Secret::new(list)));
    }
}

#[test]
fn a_full_revocation_list_refuses_sign_in_and_drops_nothing() {
    let mut h = Harness::seeded("account-backlog", full_list(MAX_EXCHANGES));
    h.op(AccountOp::SignIn);
    h.op(AccountOp::EnablePlan);
    assert!(h.listening.is_empty());
    assert_eq!(
        h.errors,
        [
            user_error(ErrorKey::RevocationBacklog, &[Action::Retry]),
            user_error(ErrorKey::RevocationBacklog, &[Action::Retry])
        ]
    );
    assert_eq!(h.view(), &signed_out());
    assert_eq!(h.revocations().len(), MAX_EXCHANGES);
}

#[test]
fn the_outstanding_requests_reserved_slot_refuses_the_next_sign_in_before_it_sends() {
    let mut h = Harness::seeded("account-reserved", full_list(MAX_EXCHANGES - 1));
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1);
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "no second attempt starts");
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::RevocationBacklog, &[Action::Retry])]
    );
    h.reply(received(Some("refresh-a"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    assert_eq!(h.sent.len(), 1);
}
