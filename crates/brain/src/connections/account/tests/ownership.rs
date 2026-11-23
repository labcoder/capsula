//! OSC-204: a refresh token leaves `chatgpt` only once the list or `outcome_unknown` owns it. A
//! sign-in's Commit owns the token it replaces, keeping a slot for its own set, and reports what
//! it listed; sign-out's Prepare owns a token the list can't hold through the marker.

use std::rc::Rc;

use super::*;
use crate::connections::marker::MarkerUpdate;
use crate::connections::records::{MAX_ID_BYTES, RevocationList};

type Owned = (String, String, u64, Origin);

/// What each partial-Commit case does once the set is held.
const SEQUENCES: [&[AccountOp]; 6] = [
    &[AccountOp::Retry],
    &[AccountOp::CancelSignIn],
    &[AccountOp::SignOut],
    &[AccountOp::SignIn, AccountOp::Retry],
    &[AccountOp::SignIn, AccountOp::CancelSignIn],
    &[AccountOp::SignIn, AccountOp::SignOut],
];

fn owned(client: &str, token: &str) -> Owned {
    (client.to_owned(), token.to_owned(), NOW, Origin::Exchange)
}

fn listed(count: usize) -> Vec<Revocable> {
    (0..count)
        .map(|n| Revocable {
            client_id: Zeroizing::new("client-listed".to_owned()),
            refresh_token: Zeroizing::new(format!("refresh-{n}")),
            added_at: NOW,
            origin: Origin::Exchange,
        })
        .collect()
}

fn owned_listed(count: usize) -> Vec<Owned> {
    (0..count)
        .map(|n| owned("client-listed", &format!("refresh-{n}")))
        .collect()
}

/// An account holding `refresh`, and `entries` listed.
fn seed(refresh: &str, entries: Vec<Revocable>) -> impl FnOnce(&mut Vault<FakeStore>) {
    let refresh = refresh.to_owned();
    move |vault| {
        committed(&vault.save_account(&saved_account("subject-1", &refresh)));
        if !entries.is_empty() {
            committed(&vault.save_revocations(&Secret::new(RevocationList { entries })));
        }
    }
}

/// Boots on `seed` and ends the startup pass over what's listed, unconfirmed.
fn booted(name: &str, seed: impl FnOnce(&mut Vault<FakeStore>)) -> Harness {
    let mut h = Harness::seeded(name, seed);
    if !h.revoking.is_empty() {
        h.feed(Input::Revoked(Vec::new()));
    }
    h
}

/// Signs in to `refresh-new` on the saved client and holds its Commit.
fn until_commit(h: &mut Harness) {
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.hold = true;
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
}

fn mutations(h: &Harness) -> Vec<Mutation> {
    h.store.0.lock().expect("fake lock").mutations.clone()
}

/// Where the `nth` save of `record` writes its last chunk in `trace`: refused there, it
/// never commits.
fn write_to(h: &Harness, trace: &[Mutation], record: &str, nth: usize) -> usize {
    let prefix = h.generations(record);
    let generation = |name: &str| name[prefix.len()..].split('/').next().map(str::to_owned);
    let mut seen: Vec<String> = Vec::new();
    let mut last = None;
    for (index, mutation) in trace.iter().enumerate() {
        let Mutation::Set(name) = mutation else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let generation = generation(name).expect(record);
        if !seen.contains(&generation) {
            seen.push(generation.clone());
        }
        if seen.len() == nth + 1 && seen[nth] == generation {
            last = Some(index);
        }
    }
    last.expect(record)
}

/// A run's tokens and origins, in the order they were sent.
fn tokens(run: &[Secret<Revocable>]) -> Vec<(String, Origin)> {
    run.iter()
        .map(|entry| {
            (
                entry.expose().refresh_token.to_string(),
                entry.expose().origin,
            )
        })
        .collect()
}

/// Answers each revocation run past the first `answered`, and retries each refusal, until the
/// sign-out ends.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the provider confirms, as the test decides"
)]
fn finish(h: &mut Harness, confirm: bool, mut answered: usize) {
    for _ in 0..16 {
        while h.revoking.len() > answered {
            let run = h.revoking[answered].clone();
            answered += 1;
            h.feed(Input::Revoked(if confirm { run } else { Vec::new() }));
        }
        let refused = matches!(
            h.view(),
            AccountView::SigningOut {
                cleanup: Cleanup::Refused | Cleanup::NotDurable,
                ..
            }
        );
        if !refused {
            assert!(
                !matches!(h.view(), AccountView::SigningOut { .. }),
                "stalled at {:?}",
                h.view()
            );
            return;
        }
        h.op(AccountOp::Retry);
    }
    panic!("the sign-out never finished");
}

fn unconfirmed() -> AccountView {
    AccountView::SignedOut {
        revocation_unconfirmed: true,
    }
}

/// The marker of a Keep sign-out still revoking, with or without `outcome_unknown`.
#[expect(
    clippy::unnecessary_wraps,
    clippy::fn_params_excessive_bools,
    reason = "compared with markers read from the store, which may be absent, for either fact"
)]
fn revoking(unknown: bool) -> Option<String> {
    Some(format!(
        r#"{{"v":1,"sign_out":{{"disposition":"Keep","phase":"Revoking"}},"sweep":true,"request_pending":false,"outcome_unknown":{unknown},"notice":true}}"#
    ))
}

/// Gap 2: a sign-in lists the token `chatgpt` holds before its commit replaces it, stamped with
/// the job's monotonic time, never a clock set back: a pending set a restart left, and one
/// rejected in this process, which stayed in `chatgpt`.
#[test]
fn a_sign_in_lists_the_token_it_replaces_before_its_commit() {
    for rejected in [false, true] {
        let mut h = Harness::seeded("ownership-replaced", |vault| {
            let mut account = saved_account("subject-1", "refresh-old").expose().clone();
            let tokens = account.tokens.as_mut().expect("tokens");
            tokens.expires_at = NOW + 3600;
            tokens.pending_identity_validation = true;
            tokens.id_token = Some(Zeroizing::new("id-pending".to_owned()));
            committed(&vault.save_account(&Secret::new(account)));
        });
        if rejected {
            h.feed(Input::Access {
                ticket: 1,
                now: Some(NOW),
            });
            let generation = h.validations.last().expect("a check").0;
            h.feed(Input::Validated {
                generation,
                result: Err(IdentityError::OtherAccount),
                now: Some(NOW),
            });
            assert!(
                h.revocations().is_empty(),
                "nothing written at the rejection"
            );
        }
        h.wall.unix.store(NOW - 600, Ordering::SeqCst);
        h.op(AccountOp::SignIn);
        h.callback("client-saved");
        h.store.arm(usize::MAX, Fault::Refuse);
        h.reply(received(Some("refresh-new"), "subject-1", PLAN));
        let trace = mutations(&h);
        assert!(
            write_to(&h, &trace, "revoke", 0) < write_to(&h, &trace, "chatgpt", 0),
            "{rejected}: listed before the commit"
        );
        assert_eq!(
            h.revocations(),
            [owned("client-saved", "refresh-old")],
            "{rejected}"
        );
        assert_eq!(
            h.refresh_token().as_deref(),
            Some("refresh-new"),
            "{rejected}"
        );
    }
}

fn quotes() -> String {
    "\"".repeat(MAX_REFRESH_BYTES)
}

/// A replaced token that fits `chatgpt` but no list entry, all quotes, is recorded unknown before
/// the commit, so the sign-out after it stays unconfirmed though every revocation is confirmed.
#[test]
fn an_unfit_replaced_token_is_marked_unknown_before_the_commit() {
    let mut h = Harness::seeded("ownership-unfit", seed(&quotes(), Vec::new()));
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.store.arm(usize::MAX, Fault::Refuse);
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert!(h.revocations().is_empty());
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-new"));
    h.op(AccountOp::SignOut);
    finish(&mut h, true, 0);
    assert_eq!(h.view(), &unconfirmed());
}

/// CAPS-152: a Commit whose `outcome_unknown` write is refused saves nothing, so the old account
/// stays and the sign-in is held; Retry writes the marker and commits.
#[test]
fn a_refused_unknown_write_keeps_the_old_account_until_retry() {
    let mut h = Harness::seeded("ownership-unknown", seed(&quotes(), Vec::new()));
    until_commit(&mut h);
    let before = h.marker();
    h.refuse_marker_writes(0);
    h.release();
    h.allow_marker_writes();
    assert_eq!(
        h.errors,
        [user_error(
            ErrorKey::CredentialsUnavailable,
            &[Action::Retry]
        )]
    );
    assert_eq!(h.refresh_token(), Some(quotes()));
    assert_eq!(h.marker(), before);
    h.op(AccountOp::Retry);
    assert_eq!(h.marker(), marker(&["outcome_unknown"]));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-new"));
    assert!(h.revocations().is_empty());
}

/// A restart between listing the replaced token and the account's commit: the next sign-in finds
/// it listed and keeps its first stamp, so it's listed once.
#[test]
fn a_crash_between_listing_and_commit_lists_the_token_once() {
    let mut dry = Harness::seeded("ownership-crash-trace", seed("refresh-old", Vec::new()));
    until_commit(&mut dry);
    dry.store.arm(usize::MAX, Fault::Refuse);
    dry.release();
    let flip = write_to(&dry, &mutations(&dry), "chatgpt", 0);
    let mut h = Harness::seeded("ownership-crash", seed("refresh-old", Vec::new()));
    until_commit(&mut h);
    h.store.arm(flip, Fault::Refuse);
    h.release();
    let old = [owned("client-saved", "refresh-old")];
    assert_eq!(h.revocations(), old);
    let mut h = h.restart();
    h.feed(Input::Revoked(Vec::new()));
    h.advance(100);
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.reply(received(Some("refresh-newer"), "subject-1", PLAN));
    assert_eq!(h.revocations(), old, "listed once, with its first stamp");
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-newer"));
}

/// A successor no entry can hold is unknown, with no list write, and the token it replaced stays
/// in `chatgpt` marked as sent, so a restart never sends it. A refused unknown write leaves the
/// pending flag, which the restart folds into the same uncertainty.
#[test]
fn an_oversized_successor_is_unknown_and_leaves_the_predecessor() {
    for refused in [false, true] {
        let mut h = Harness::seeded("ownership-oversized", |vault| {
            let mut account = saved_account("subject-1", "refresh-old").expose().clone();
            account.tokens.as_mut().expect("tokens").expires_at = NOW;
            committed(&vault.save_account(&Secret::new(account)));
        });
        h.feed(Input::Access {
            ticket: 1,
            now: Some(NOW),
        });
        let generation = h.refreshes.last().expect("a refresh sent").0;
        if refused {
            h.refuse_marker_writes(0);
        }
        h.feed(Input::Refreshed {
            generation,
            result: Err(ExchangeError::Unusable {
                refresh_token: Some(secret(&"r".repeat(MAX_REFRESH_BYTES + 1))),
            }),
            now: Some(NOW),
        });
        if refused {
            h.allow_marker_writes();
        }
        let flags: &[_] = if refused {
            &["request_pending"]
        } else {
            &["outcome_unknown"]
        };
        assert_eq!(h.marker(), marker(flags), "{refused}");
        assert!(h.revocations().is_empty(), "{refused}");
        let tokens = h
            .account()
            .and_then(|account| account.tokens)
            .expect("tokens");
        assert_eq!(tokens.refresh_token.as_str(), "refresh-old", "{refused}");
        assert!(tokens.refresh_sent, "{refused}");
        let mut h = h.restart();
        assert_eq!(
            h.marker(),
            marker(&["outcome_unknown"]),
            "{refused}: folded"
        );
        h.feed(Input::Access {
            ticket: 2,
            now: Some(NOW),
        });
        assert!(h.refreshes.is_empty(), "{refused}: never sent again");
    }
}

/// A refused startup fold leaves the pending flag, which a direct sign-out still counts.
#[test]
fn a_refused_startup_fold_still_signs_out_unconfirmed() {
    let scratch = Scratch::new("ownership-fold");
    let service = format!("test-account-{:016x}", getrandom::u64().expect("random"));
    let store = FakeStore::default();
    let seeding = scratch.db().with_file_name("seed.lock");
    let mut vault = Vault::with_service(store.clone(), &service, &seeding).expect("seed vault");
    committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    vault
        .update_marker(MarkerUpdate::RequestStarted)
        .expect("pending flag");
    drop(vault);
    let path = scratch.db().with_file_name(authority::FILE);
    refuse_marker_writes(&path, 0);
    let mut h = Harness::boot(
        Rc::new(scratch),
        service,
        store,
        Wall::new(),
        AccountState::start(),
    );
    assert_eq!(
        h.marker(),
        marker(&["request_pending"]),
        "the fold was refused"
    );
    allow_marker_writes(&path);
    h.op(AccountOp::SignOut);
    finish(&mut h, true, 0);
    assert_eq!(h.view(), &unconfirmed());
}

/// Prepare adopts the account's token only when nothing lists it: not when an equal entry is
/// listed, nor when an `Account` entry, its saved successor, is.
#[test]
fn prepare_lists_an_already_listed_token_once() {
    for (token, origin) in [
        ("refresh-old", Origin::Exchange),
        ("refresh-successor", Origin::Account),
    ] {
        let entry = Revocable {
            client_id: Zeroizing::new("client-saved".to_owned()),
            refresh_token: Zeroizing::new(token.to_owned()),
            added_at: NOW,
            origin,
        };
        let mut h = booted("ownership-prepare", seed("refresh-old", vec![entry]));
        h.op(AccountOp::SignOut);
        assert_eq!(
            tokens(h.revoking.last().expect("a run")),
            [(token.to_owned(), origin)],
            "{origin:?}"
        );
        assert_eq!(
            h.revocations(),
            [("client-saved".to_owned(), token.to_owned(), NOW, origin)],
            "{origin:?}"
        );
    }
}

/// Signs in to `refresh-new` on an account holding `refresh-old` with `count` exchanges listed,
/// and refuses the account's manifest flip, so the set is held.
fn partial(count: usize) -> Harness {
    let mut dry = booted(
        "ownership-partial-trace",
        seed("refresh-old", listed(count)),
    );
    until_commit(&mut dry);
    dry.store.arm(usize::MAX, Fault::Refuse);
    dry.release();
    let flip = write_to(&dry, &mutations(&dry), "chatgpt", 0);
    let mut h = booted("ownership-partial", seed("refresh-old", listed(count)));
    until_commit(&mut h);
    h.store.arm(flip, Fault::Refuse);
    h.release();
    assert_eq!(
        h.errors,
        [user_error(
            ErrorKey::CredentialsUnavailable,
            &[Action::Retry]
        )]
    );
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-old"));
    h
}

/// A `SignIn` while the held set fills the list's last slot: the admission's age check frees
/// nothing, so it's refused with `RevocationBacklog`, and nothing moves.
fn sign_in_again(h: &mut Harness, context: &str) {
    let before = (h.revocations(), h.marker(), h.refresh_token());
    h.op(AccountOp::SignIn);
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry])),
        "{context}"
    );
    assert_eq!(
        (h.listening.len(), h.sent.len()),
        (1, 1),
        "{context}: no new attempt or exchange"
    );
    assert_eq!(
        (h.revocations(), h.marker(), h.refresh_token()),
        before,
        "{context}: the set stays held"
    );
}

/// R2-A: with three exchanges listed, the replaced token takes `outcome_unknown`, not the slot the
/// held set needs. Cancel and `SignOut` list the held set in it; a repeated `SignIn` finds the list
/// full and changes nothing, and Retry, Cancel and `SignOut` still work after it.
#[test]
fn with_three_listed_the_commit_keeps_the_incoming_sets_slot() {
    let held = owned_listed(3);
    let mut with_new = held.clone();
    with_new.push(owned("client-saved", "refresh-new"));
    for sequence in SEQUENCES {
        let context = format!("{sequence:?}");
        let mut h = partial(3);
        assert_eq!(h.revocations(), held, "{context}");
        assert_eq!(
            h.marker(),
            marker(&["sweep", "request_pending", "outcome_unknown"]),
            "{context}"
        );
        assert_eq!(h.revoking.len(), 1, "{context}: only the startup pass");
        let (&last, first) = sequence.split_last().expect("an op");
        for _ in first {
            sign_in_again(&mut h, &context);
        }
        let answered = h.revoking.len();
        h.op(last);
        match last {
            AccountOp::Retry => {
                assert_eq!(
                    h.refresh_token().as_deref(),
                    Some("refresh-new"),
                    "{context}"
                );
                assert_eq!(h.revocations(), held, "{context}");
                assert_eq!(
                    h.marker(),
                    marker(&["sweep", "outcome_unknown"]),
                    "{context}"
                );
            }
            AccountOp::CancelSignIn => {
                assert_eq!(
                    h.refresh_token().as_deref(),
                    Some("refresh-old"),
                    "{context}"
                );
                assert_eq!(h.revocations(), with_new, "{context}");
                assert_eq!(
                    h.marker(),
                    marker(&["sweep", "outcome_unknown"]),
                    "{context}"
                );
            }
            _ => {
                finish(&mut h, false, answered);
                assert_eq!(
                    tokens(&h.revoking[answered]),
                    [
                        ("refresh-old".to_owned(), Origin::Account),
                        ("refresh-0".to_owned(), Origin::Exchange),
                        ("refresh-1".to_owned(), Origin::Exchange),
                        ("refresh-2".to_owned(), Origin::Exchange),
                        ("refresh-new".to_owned(), Origin::Exchange),
                    ],
                    "{context}"
                );
                assert_eq!(h.view(), &unconfirmed(), "{context}");
            }
        }
    }
}

/// R2-A: with two listed, the replaced token takes the third slot, and the completion reports it
/// though the account save was refused: the count and a revocation pass follow. Retry, with that
/// pass still out, commits with no second entry or pass; Cancel and `SignOut` list the held set in
/// the fourth; a repeated `SignIn` changes nothing.
#[test]
fn with_two_listed_a_partial_commit_reports_the_list_and_cleans_up_bounded() {
    let mut with_old = owned_listed(2);
    with_old.push(owned("client-saved", "refresh-old"));
    let mut with_new = with_old.clone();
    with_new.push(owned("client-saved", "refresh-new"));
    for sequence in SEQUENCES {
        let context = format!("{sequence:?}");
        let mut h = partial(2);
        assert_eq!(h.revocations(), with_old, "{context}");
        assert_eq!(
            h.marker(),
            marker(&["sweep", "request_pending"]),
            "{context}"
        );
        assert_eq!(
            h.revoking.len(),
            2,
            "{context}: the pass for the listed token"
        );
        assert_eq!(
            tokens(&h.revoking[1]),
            [
                ("refresh-0".to_owned(), Origin::Exchange),
                ("refresh-1".to_owned(), Origin::Exchange),
                ("refresh-old".to_owned(), Origin::Exchange),
            ],
            "{context}"
        );
        let (&last, first) = sequence.split_last().expect("an op");
        for _ in first {
            sign_in_again(&mut h, &context);
        }
        h.op(last);
        match last {
            AccountOp::Retry => {
                assert_eq!(
                    h.refresh_token().as_deref(),
                    Some("refresh-new"),
                    "{context}"
                );
                assert_eq!(h.revocations(), with_old, "{context}: listed once");
                assert_eq!(h.revoking.len(), 2, "{context}: no second pass");
                assert_eq!(h.marker(), marker(&["sweep"]), "{context}");
            }
            AccountOp::CancelSignIn => {
                assert_eq!(
                    h.refresh_token().as_deref(),
                    Some("refresh-old"),
                    "{context}"
                );
                assert_eq!(h.revocations(), with_new, "{context}");
                assert_eq!(h.marker(), marker(&["sweep"]), "{context}");
            }
            _ => {
                // The pass still out ends first, unconfirmed; then the sign-out's own run.
                finish(&mut h, false, 1);
                assert_eq!(
                    tokens(&h.revoking[2]),
                    [
                        ("refresh-0".to_owned(), Origin::Exchange),
                        ("refresh-1".to_owned(), Origin::Exchange),
                        ("refresh-old".to_owned(), Origin::Exchange),
                        ("refresh-new".to_owned(), Origin::Exchange),
                    ],
                    "{context}: the listed account token isn't adopted again"
                );
                assert_eq!(h.view(), &unconfirmed(), "{context}");
            }
        }
    }
}

/// R2-A: a Commit that lists the replaced token counts it before the next admission. With two
/// listed and a refresh holding the lane, a `SignIn` waits for the age check and is refused; the
/// old count would have let it listen.
#[test]
fn a_commit_that_lists_counts_before_the_next_admission() {
    let mut h = booted("ownership-count", seed("refresh-old", listed(2)));
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert_eq!(h.refresh_token().as_deref(), Some("refresh-new"));
    let mut with_old = owned_listed(2);
    with_old.push(owned("client-saved", "refresh-old"));
    assert_eq!(h.revocations(), with_old);
    h.feed(Input::Access {
        ticket: 1,
        now: Some(NOW + 3600),
    });
    assert_eq!(h.refreshes.len(), 1, "the refresh holds the lane");
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "no second attempt");
    assert_eq!(
        h.errors.last(),
        Some(&user_error(ErrorKey::RevocationBacklog, &[Action::Retry]))
    );
}

/// Four exchanges as long as an entry allows: a list with no room for an all-quotes account token.
fn crowded() -> Vec<Revocable> {
    (0..MAX_EXCHANGES)
        .map(|n| Revocable {
            client_id: Zeroizing::new("c".repeat(MAX_ID_BYTES)),
            refresh_token: Zeroizing::new(format!("{n}{}", "r".repeat(MAX_REFRESH_BYTES - 1))),
            added_at: NOW,
            origin: Origin::Exchange,
        })
        .collect()
}

/// Every token a sign-out on `crowded` revokes: the account's first, then the list's.
fn crowded_run() -> Vec<(String, Origin)> {
    let mut run = vec![(quotes(), Origin::Account)];
    run.extend(
        crowded()
            .into_iter()
            .map(|entry| (entry.refresh_token.to_string(), Origin::Exchange)),
    );
    run
}

/// R2-B: an account token the full list can't hold is recorded unknown first and still revoked
/// from memory; the list stays as it was, the token stays in `chatgpt` until the delete, and the
/// sign-out ends unconfirmed though no revocation succeeds.
#[test]
fn an_adopted_token_the_full_list_cant_hold_is_unknown_and_still_revoked() {
    let mut h = booted("ownership-crowded", seed(&quotes(), crowded()));
    let list = h.revocations();
    h.op(AccountOp::SignOut);
    assert_eq!(h.marker(), revoking(true));
    assert_eq!(h.revocations(), list, "the list stays as it was");
    assert_eq!(tokens(h.revoking.last().expect("a run")), crowded_run());
    assert!(h.holds("chatgpt"), "kept until the delete");
    let answered = h.revoking.len() - 1;
    finish(&mut h, false, answered);
    assert_eq!(h.view(), &unconfirmed());
    assert!(
        h.account().expect("the kept registration").tokens.is_none(),
        "gone with the delete"
    );
}

/// R2-B: a refused unknown write refuses the cleanup: nothing is revoked, checkpointed or deleted.
/// A restart that can write it finishes the sign-out, unconfirmed.
#[test]
fn a_refused_fallback_marker_refuses_cleanup_until_a_restart_can_write_it() {
    let mut h = booted("ownership-crowded-refused", seed(&quotes(), crowded()));
    let list = h.revocations();
    // The promotion is the sign-out's first marker write; the unknown write is its second.
    h.refuse_marker_writes(1);
    h.op(AccountOp::SignOut);
    assert_eq!(
        h.view(),
        &AccountView::SigningOut {
            cleanup: Cleanup::Refused,
            revocation_unconfirmed: true,
        }
    );
    assert_eq!(h.revoking.len(), 1, "only the startup pass");
    assert_eq!(h.marker(), revoking(false));
    assert_eq!(h.revocations(), list);
    assert!(h.holds("chatgpt"));
    h.allow_marker_writes();
    let mut h = h.restart();
    finish(&mut h, false, 0);
    assert_eq!(h.view(), &unconfirmed());
    assert!(
        h.revoking.iter().any(|run| tokens(run) == crowded_run()),
        "the account's token is still revoked"
    );
}
