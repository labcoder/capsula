//! Revocation outside a sign-out: passes at startup, when an entry is added and on Retry, and
//! age-out, which a full list gets before a sign-in is refused.

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
    h.advance(JUMP);
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.listening.len(),
        1,
        "an hour ahead of monotonic time isn't a jump"
    );

    let mut h = Harness::seeded("sweep-jump", listed(full(NOW - AGE_OUT + JUMP + 1)));
    h.feed(Input::Revoked(Vec::new()));
    h.advance(JUMP + 1);
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
