//! Revocation outside a sign-out: passes at startup, when an entry is added and on Retry, and
//! age-out, which a full list gets before a sign-in is refused.

use std::panic::{self, AssertUnwindSafe};

use super::*;
use crate::connections::records::RevocationList;

/// 30 days plus the 2-day margin, as documented, not read from the code.
const AGE_OUT: u64 = 32 * 86_400;
const JUMP: u64 = 3_600;

fn entry(token: &str, added_at: u64) -> Revocable {
    Revocable {
        client_id: Zeroizing::new("client-listed".to_owned()),
        refresh_token: Zeroizing::new(token.to_owned()),
        added_at,
        origin: Origin::Exchange,
    }
}

fn listed(entries: Vec<Revocable>) -> impl FnOnce(&mut Vault<FakeStore>) {
    move |vault| committed(&vault.save_revocations(&Secret::new(RevocationList { entries })))
}

fn tokens(entries: &[Secret<Revocable>]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.expose().refresh_token.to_string())
        .collect()
}

fn listed_tokens(h: &Harness) -> Vec<String> {
    h.revocations().into_iter().map(|entry| entry.1).collect()
}

/// A full list: three fresh exchanges and one added at `added_at`.
fn full(added_at: u64) -> Vec<Revocable> {
    vec![
        entry("refresh-a", NOW),
        entry("refresh-b", NOW),
        entry("refresh-c", NOW),
        entry("refresh-old", added_at),
    ]
}

#[test]
fn an_added_entry_is_revoked_at_once_and_unlisted_once_confirmed() {
    let mut h = Harness::new("sweep-added");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-unusable")),
    }));
    assert_eq!(h.revoking.len(), 1);
    assert_eq!(tokens(&h.revoking[0]), ["refresh-unusable"]);
    let confirmed = h.revoking[0].clone();
    h.feed(Input::Revoked(confirmed));
    assert!(h.revocations().is_empty());
    assert_eq!(h.marker(), None, "the sweep fact cleared with the list");
    assert!(
        !h.store.names().iter().any(|name| name.contains("/revoke/")),
        "every list name is gone"
    );
}

#[test]
fn an_unconfirmed_pass_keeps_the_list_and_retry_runs_another() {
    let mut h = Harness::seeded("sweep-retry", listed(vec![entry("refresh-a", NOW)]));
    assert_eq!(h.revoking.len(), 1, "startup runs a pass");
    h.op(AccountOp::Retry);
    assert_eq!(h.revoking.len(), 1, "one pass at a time");
    h.feed(Input::Revoked(Vec::new()));
    assert_eq!(listed_tokens(&h), ["refresh-a"]);
    assert_eq!(
        h.revoking.len(),
        2,
        "the Retry that came during it runs after"
    );
    h.feed(Input::Revoked(Vec::new()));
    assert_eq!(h.revoking.len(), 2, "and only once");
    h.op(AccountOp::Retry);
    assert_eq!(h.revoking.len(), 3);
    assert_eq!(tokens(&h.revoking[2]), ["refresh-a"]);
}

#[test]
fn startup_ages_out_an_entry_exactly_32_days_old_and_keeps_a_younger_one() {
    let h = Harness::seeded(
        "sweep-startup-age",
        listed(vec![
            entry("refresh-expired", NOW - AGE_OUT),
            entry("refresh-kept", NOW - AGE_OUT + 1),
        ]),
    );
    assert_eq!(listed_tokens(&h), ["refresh-kept"]);
    assert_eq!(h.revoking.len(), 1);
    assert_eq!(tokens(&h.revoking[0]), ["refresh-kept"]);
}

/// L3: a full list isn't a dead end; what aged out since startup frees a slot.
#[test]
fn a_full_list_ages_out_before_a_sign_in_is_refused() {
    let mut h = Harness::seeded("sweep-admit", listed(full(NOW - AGE_OUT + 1_800)));
    h.feed(Input::Revoked(Vec::new()));
    h.advance(1_799);
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::RevocationBacklog, &[Action::Retry])]
    );
    assert!(h.listening.is_empty());
    h.advance(1);
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "the slot it freed admits the sign-in");
    assert_eq!(listed_tokens(&h), ["refresh-a", "refresh-b", "refresh-c"]);
}

#[test]
fn a_wall_clock_jump_suspends_age_out_until_the_next_start() {
    let mut h = Harness::seeded("sweep-no-jump", listed(full(NOW - AGE_OUT + JUMP)));
    h.feed(Input::Revoked(Vec::new()));
    h.jump(JUMP);
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.listening.len(),
        1,
        "an hour ahead of monotonic time isn't a jump"
    );

    let mut h = Harness::seeded("sweep-running", listed(full(NOW - AGE_OUT + 2 * JUMP)));
    h.feed(Input::Revoked(Vec::new()));
    h.advance(2 * JUMP);
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "a running clock is never a jump");

    let mut h = Harness::seeded("sweep-jump", listed(full(NOW - AGE_OUT + JUMP + 1)));
    h.feed(Input::Revoked(Vec::new()));
    h.jump(JUMP + 1);
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::RevocationBacklog, &[Action::Retry])]
    );
    assert_eq!(
        listed_tokens(&h).len(),
        4,
        "nothing aged out across the jump"
    );
    let mut h = h.restart();
    assert_eq!(
        listed_tokens(&h).len(),
        3,
        "the next start trusts the clock again"
    );
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1);
}

/// H1 (CAPS-131): every committed listing counts, so the list fills to exactly four and the
/// fifth sign-in is refused before anything is sent.
#[test]
fn a_list_filled_one_sign_in_at_a_time_refuses_the_fifth() {
    let mut h = Harness::new("sweep-fill");
    for n in 1..=4 {
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(Err(ExchangeError::Unusable {
            refresh_token: Some(secret(&format!("refresh-{n}"))),
        }));
        h.feed(Input::Revoked(Vec::new()));
    }
    assert_eq!(h.sent.len(), 4);
    assert_eq!(
        listed_tokens(&h),
        ["refresh-1", "refresh-2", "refresh-3", "refresh-4"]
    );
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry]))
    );
    assert_eq!(h.listening.len(), 4, "no fifth listener");
    assert_eq!(h.sent.len(), 4, "no fifth request");
}

/// H1 (CAPS-131): an entry listed while another pass runs counts at once, not when that pass's
/// rewrite lands.
#[test]
fn a_listing_during_an_active_pass_counts_at_once() {
    let mut h = Harness::seeded(
        "sweep-listing-mid-pass",
        listed(vec![
            entry("refresh-a", NOW),
            entry("refresh-b", NOW),
            entry("refresh-c", NOW),
        ]),
    );
    assert_eq!(h.revoking.len(), 1, "the startup pass is still out");
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-new")),
    }));
    assert_eq!(h.revoking.len(), 1, "the new entry waits for the next pass");
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry]))
    );
    assert_eq!(h.listening.len(), 1);
    assert_eq!(h.sent.len(), 1);
}

/// M1 (CAPS-131): Cancel during a full list's age check means the sign-in waiting on it never
/// starts, though the check itself still finishes.
#[test]
fn cancel_during_the_age_check_never_starts_the_waiting_sign_in() {
    let mut h = Harness::seeded(
        "sweep-admit-cancel",
        listed(vec![
            entry("refresh-a", NOW),
            entry("refresh-b", NOW),
            entry("refresh-old", NOW - AGE_OUT + 10),
        ]),
    );
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    assert_eq!(h.sent.len(), 1, "its request holds the fourth slot");
    h.advance(10);
    h.hold = true;
    h.op(AccountOp::SignIn);
    assert!(matches!(h.held, Some(Job::Relist { age_out: true, .. })));
    h.op(AccountOp::CancelSignIn);
    assert_eq!(h.view(), &signed_out());
    h.release();
    assert_eq!(listed_tokens(&h), ["refresh-a", "refresh-b"]);
    assert_eq!(h.listening.len(), 1, "the waiting sign-in never started");
    assert_eq!(h.view(), &signed_out());
}

/// M1 (CAPS-131): a sign-in that starts while an older one waits for its age check replaces
/// it, so the check's end doesn't start the older one over it.
#[test]
fn a_sign_in_started_during_the_age_check_keeps_its_attempt() {
    let mut h = Harness::seeded(
        "sweep-admit-replaced",
        listed(vec![
            entry("refresh-a", NOW),
            entry("refresh-b", NOW),
            entry("refresh-c", NOW),
        ]),
    );
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.op(AccountOp::SignIn);
    assert!(matches!(h.held, Some(Job::Relist { age_out: true, .. })));
    h.reply(Err(ExchangeError::OAuth {
        status: 400,
        code: OAuthCode::InvalidGrant,
    }));
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.listening.len(),
        2,
        "the slot is free again, so it starts now"
    );
    let started = h.listening[1];
    h.release();
    assert_eq!(h.listening.len(), 2);
    assert_eq!(h.listening.last(), Some(&started));
}

/// Retry on a full list where nothing has aged out runs a pass; an entry it confirms frees a
/// slot for the next sign-in.
#[test]
fn retry_on_a_full_list_runs_a_pass_that_can_free_a_slot() {
    let mut h = Harness::seeded("sweep-full-retry", listed(full(NOW)));
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors,
        [user_error(ErrorKey::RevocationBacklog, &[Action::Retry])]
    );
    h.op(AccountOp::Retry);
    assert_eq!(h.revoking.len(), 2, "Retry ran a pass");
    assert_eq!(
        tokens(&h.revoking[1]),
        ["refresh-a", "refresh-b", "refresh-c", "refresh-old"]
    );
    let confirmed = vec![h.revoking[1][0].clone()];
    h.feed(Input::Revoked(confirmed));
    assert_eq!(listed_tokens(&h), ["refresh-b", "refresh-c", "refresh-old"]);
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "the freed slot admits the sign-in");
}

/// H1 (CAPS-130's trace): three fresh entries and a failed startup pass, then a fourth token is
/// listed and its pass fails too. The next exchange is never sent.
#[test]
fn a_fourth_listing_after_failed_passes_blocks_the_next_exchange() {
    let mut h = Harness::seeded(
        "sweep-fourth-listing",
        listed(vec![
            entry("refresh-a", NOW),
            entry("refresh-b", NOW),
            entry("refresh-c", NOW),
        ]),
    );
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.reply(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-new")),
    }));
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry]))
    );
    assert_eq!(h.listening.len(), 1);
    assert_eq!(h.sent.len(), 1, "the next exchange is never sent");
    assert_eq!(
        listed_tokens(&h),
        ["refresh-a", "refresh-b", "refresh-c", "refresh-new"]
    );
}

/// H2 (CAPS-130): emptying the list survives a crash after, or a refusal of, each of its writes
/// and deletes. A restart or Retry finishes the cleanup, and sign-in proceeds.
#[test]
fn empty_list_cleanup_recovers_from_a_crash_or_refusal_at_every_step() {
    let listed_one = || {
        let mut h = Harness::new("sweep-cleanup-fault");
        h.op(AccountOp::SignIn);
        h.callback("client-issued");
        h.reply(Err(ExchangeError::Unusable {
            refresh_token: Some(secret("refresh-unusable")),
        }));
        h
    };
    let mut h = listed_one();
    h.store.0.lock().expect("fake lock").mutations.clear();
    let confirmed = h.revoking[0].clone();
    h.feed(Input::Revoked(confirmed));
    let steps = h.store.0.lock().expect("fake lock").mutations.len();
    for step in 0..steps {
        for fault in [Fault::CrashAfter, Fault::Refuse] {
            let mut h = listed_one();
            let confirmed = h.revoking[0].clone();
            h.store.arm(step, fault);
            let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
                h.feed(Input::Revoked(confirmed));
            }))
            .is_err();
            assert_eq!(
                crashed,
                matches!(fault, Fault::CrashAfter),
                "{step} {fault:?}"
            );
            let (mut h, passes) = if crashed {
                (h.restart(), 0)
            } else {
                let passes = h.revoking.len();
                h.op(AccountOp::Retry);
                (h, passes)
            };
            // Where the entry outlived the fault, the pass recovery started confirms it again.
            if let Some(entries) = h.revoking.get(passes).cloned() {
                h.feed(Input::Revoked(entries));
            }
            assert!(h.revocations().is_empty(), "{step} {fault:?}");
            assert_eq!(h.marker(), None, "{step} {fault:?}");
            assert!(
                !h.store.names().iter().any(|name| name.contains("/revoke")),
                "{step} {fault:?}"
            );
            let started = h.listening.len();
            h.op(AccountOp::SignIn);
            assert_eq!(h.listening.len(), started + 1, "{step} {fault:?}");
        }
    }
}

/// H3 (CAPS-130's trace): a token listed while the wall clock is 33 days behind is stamped no
/// earlier than this process's own time, so correcting the clock doesn't age it out.
#[test]
fn a_token_listed_during_a_backward_clock_excursion_never_ages_out_early() {
    let mut h = Harness::seeded(
        "sweep-backward",
        listed(vec![
            entry("refresh-a", NOW),
            entry("refresh-b", NOW),
            entry("refresh-c", NOW),
        ]),
    );
    h.feed(Input::Revoked(Vec::new()));
    h.op(AccountOp::SignIn);
    h.callback("client-issued");
    h.hold = true;
    h.reply(Err(ExchangeError::Unusable {
        refresh_token: Some(secret("refresh-new")),
    }));
    assert!(matches!(h.held, Some(Job::List(_))));
    h.wall.unix.store(NOW - 33 * 86_400, Ordering::SeqCst);
    h.release();
    h.wall.unix.store(NOW, Ordering::SeqCst);
    h.feed(Input::Revoked(Vec::new()));
    let kept = (
        "client-issued".to_owned(),
        "refresh-new".to_owned(),
        NOW,
        Origin::Exchange,
    );
    assert_eq!(h.revocations().last(), Some(&kept));
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry]))
    );
    assert_eq!(h.revocations().last(), Some(&kept), "admission kept it");
    let h = h.restart();
    assert_eq!(h.revocations().last(), Some(&kept), "a restart kept it");
}
