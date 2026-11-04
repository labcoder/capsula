//! Sign-out (design §3 rule 5): every persisted phase replayed after a crash or a refusal at each
//! store mutation, Keep and Erase against confirmed and unconfirmed outcomes, Delete driven by
//! the marker alone, and the crosses with the lane and revocation passes (§4).
#![expect(
    clippy::fn_params_excessive_bools,
    reason = "fixtures take the persisted flags and outcomes they assert"
)]

use std::panic::{self, AssertUnwindSafe};

use super::*;
use crate::connections::http_client::HttpError;
use crate::connections::marker::{Disposition, MarkerUpdate};
use crate::connections::records::RevocationList;
use crate::platform::credential::CredentialStore as _;

const NOTICE_ONLY: &str = r#"{"v":1,"sign_out":null,"sweep":false,"request_pending":false,"outcome_unknown":false,"notice":true}"#;

/// The marker's exact JSON with a sign-out pending.
#[expect(
    clippy::unnecessary_wraps,
    reason = "compared with markers read from the store, which may be absent"
)]
fn pending(
    disposition: &str,
    phase: &str,
    request_pending: bool,
    outcome_unknown: bool,
    notice: bool,
) -> Option<String> {
    Some(format!(
        r#"{{"v":1,"sign_out":{{"disposition":"{disposition}","phase":"{phase}"}},"sweep":true,"request_pending":{request_pending},"outcome_unknown":{outcome_unknown},"notice":{notice}}}"#
    ))
}

fn signing_out(cleanup: Cleanup, revocation_unconfirmed: bool) -> AccountView {
    AccountView::SigningOut {
        cleanup,
        revocation_unconfirmed,
    }
}

fn signed_out_with(revocation_unconfirmed: bool) -> AccountView {
    AccountView::SignedOut {
        revocation_unconfirmed,
    }
}

fn account_only(vault: &mut Vault<FakeStore>) {
    committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
}

fn listed_entry() -> Revocable {
    Revocable {
        client_id: Zeroizing::new("client-listed".to_owned()),
        refresh_token: Zeroizing::new("refresh-listed".to_owned()),
        added_at: NOW,
        origin: Origin::Exchange,
    }
}

fn account_and_listed(vault: &mut Vault<FakeStore>) {
    account_only(vault);
    committed(&vault.save_revocations(&Secret::new(RevocationList {
        entries: vec![listed_entry()],
    })));
}

fn registration() -> Account {
    Account {
        client_id: Zeroizing::new("client-saved".to_owned()),
        subject: Some(Zeroizing::new("subject-1".to_owned())),
        email: None,
        plan_enabled: false,
        tokens: None,
    }
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

fn names(h: &Harness) -> Vec<String> {
    let prefix = format!("{}/", h.service);
    h.store
        .names()
        .iter()
        .map(|name| name.strip_prefix(&prefix).expect("own service").to_owned())
        .collect()
}

/// What a finished sign-out leaves: the host, the registration Keep retains, and the notice.
fn left(keep: bool, notice: bool) -> Vec<&'static str> {
    let mut names = if keep {
        vec!["chatgpt", "chatgpt/1/0"]
    } else {
        Vec::new()
    };
    names.push("host");
    if notice {
        names.push("signout");
    }
    names
}

/// Answers each revocation run after the first `answered`, and retries each refusal, until the
/// sign-out ends.
fn finish(h: &mut Harness, confirm: bool, mut answered: usize) {
    for _ in 0..16 {
        if h.revoking.len() > answered {
            let run = h.revoking[answered].clone();
            answered += 1;
            h.feed(Input::Revoked(if confirm { run } else { Vec::new() }));
        } else if matches!(
            h.view(),
            AccountView::SigningOut {
                cleanup: Cleanup::Refused | Cleanup::NotDurable,
                ..
            }
        ) {
            h.op(AccountOp::Retry);
        } else {
            assert!(
                !matches!(h.view(), AccountView::SigningOut { .. }),
                "stalled at {:?}",
                h.view()
            );
            return;
        }
    }
    panic!("the sign-out never finished");
}

/// A store `seed` prepares through a vault and, for damage, by raw name.
fn prepared(seed: impl FnOnce(&mut Vault<FakeStore>, &dyn Fn(&str, &[u8]))) -> (FakeStore, String) {
    let scratch = Scratch::new("sign-out-seed");
    let service = format!("test-account-{:016x}", getrandom::u64().expect("random"));
    let store = FakeStore::default();
    let mut vault = Vault::with_service(
        store.clone(),
        &service,
        &scratch.db().with_file_name("seed.lock"),
    )
    .expect("seed vault");
    let raw = |name: &str, bytes: &[u8]| {
        let target = format!("{service}/{name}");
        if bytes.is_empty() {
            store.delete(&target).expect("raw delete");
        } else {
            let bytes = Secret::new(Zeroizing::new(bytes.to_vec()));
            store.set(&target, &bytes).expect("raw write");
        }
    };
    seed(&mut vault, &raw);
    drop(vault);
    (store, service)
}

/// A signed-in account and its host, and with `erase`, a promoted Erase sign-out a crash
/// interrupted right after promotion.
fn store(erase: bool) -> (FakeStore, String) {
    prepared(|vault, _| {
        account_only(vault);
        vault.host_id().expect("host");
        if erase {
            vault
                .update_marker(MarkerUpdate::Promote(Disposition::Erase))
                .expect("promoted");
        }
    })
}

fn boot(store: &FakeStore, service: &str) -> Harness {
    Harness::boot(
        Scratch::new("sign-out"),
        service.to_owned(),
        store.clone(),
        Wall::new(),
    )
}

/// A whole sign-out, live Keep or resumed Erase, with `fault` at mutation `at`: a crash restarts
/// the brain, a refusal is retried. Whether it crashed comes back too.
fn faulted(seeded: &(FakeStore, String), erase: bool, at: usize, fault: Fault) -> (Harness, bool) {
    let (store, service) = (seeded.0.fork(), seeded.1.clone());
    let mut first = None;
    let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
        if erase {
            store.arm(at, fault);
        }
        let h = first.insert(boot(&store, &service));
        if !erase {
            store.arm(at, fault);
            h.op(AccountOp::SignOut);
        }
        finish(h, true, 0);
    }))
    .is_err();
    if let Some(h) = first.filter(|_| !crashed) {
        return (h, false);
    }
    let mut h = boot(&store, &service);
    finish(&mut h, true, 0);
    (h, true)
}

/// OSC-117: a crash after, or a refusal of, any one of the store mutations a whole sign-out makes
/// leaves it to finish on the next start or Retry. The outcome is decided once, every token name
/// goes, and a restart after the checkpoint only deletes and finishes.
#[test]
fn a_crash_or_refusal_at_every_mutation_still_ends_signed_out() {
    for erase in [false, true] {
        let seeded = store(erase);
        let (dry, _) = faulted(&seeded, erase, usize::MAX, Fault::Refuse);
        let mutations = dry.store.0.lock().expect("fake lock").mutations.clone();
        let (list, marker) = (
            format!("{}/revoke", dry.service),
            format!("{}/signout", dry.service),
        );
        let flips: Vec<usize> = (0..mutations.len())
            .filter(|&at| matches!(&mutations[at], Mutation::Set(name) if *name == list))
            .collect();
        // Prepare's adoption, then the checkpoint's rewrite without the confirmed entry.
        assert_eq!(flips.len(), 2, "{mutations:?}");
        let rewrite = flips[0] + 1..=flips[1];
        let checkpoint = (flips[1]..mutations.len())
            .find(|&at| matches!(&mutations[at], Mutation::Set(name) if *name == marker))
            .expect("the checkpoint write");
        for at in 0..mutations.len() {
            for fault in [Fault::CrashAfter, Fault::Refuse] {
                let (h, crashed) = faulted(&seeded, erase, at, fault);
                let context = format!("erase {erase}, mutation {at}, {fault:?}");
                assert_eq!(crashed, matches!(fault, Fault::CrashAfter), "{context}");
                // A refused rewrite leaves the confirmed entry listed, so the checkpoint can't
                // claim confirmation.
                let notice = !crashed && rewrite.contains(&at);
                assert_eq!(h.view(), &signed_out_with(notice), "{context}");
                assert_eq!(
                    h.marker(),
                    notice.then(|| NOTICE_ONLY.to_owned()),
                    "{context}"
                );
                assert_eq!(names(&h), left(!erase, notice), "{context}");
                if !erase {
                    assert_eq!(h.account(), Some(registration()), "{context}");
                }
                for run in &h.revoking {
                    assert_eq!(
                        tokens(run),
                        [("refresh-old".to_owned(), Origin::Account)],
                        "{context}: the adopted token is listed once"
                    );
                }
                if crashed && at >= checkpoint {
                    assert!(h.revoking.is_empty(), "{context}: Deleting revokes nothing");
                    assert!(
                        h.markers.iter().flatten().all(|m| !m.contains("Revoking")),
                        "{context}: Deleting never returns to Revoking"
                    );
                }
                assert!(h.sent.is_empty(), "{context}");
            }
        }
    }
}

#[test]
fn signing_out_with_no_signed_in_account_is_answered_with_the_view() {
    let mut h = Harness::new("sign-out-nothing");
    h.op(AccountOp::SignOut);
    assert_eq!(h.views, [signed_out(), signed_out()]);
    assert_eq!(h.markers, [None]);
}

#[test]
fn signing_out_revokes_the_account_token_then_keeps_only_the_registration() {
    let mut h = Harness::seeded("sign-out-keep", account_only);
    h.op(AccountOp::SignOut);
    assert_eq!(h.view(), &signing_out(Cleanup::Running, true));
    assert_eq!(
        h.revocations(),
        [(
            "client-saved".to_owned(),
            "refresh-old".to_owned(),
            NOW,
            Origin::Account
        )],
        "Prepare adopts the account token before anything is revoked"
    );
    finish(&mut h, true, 0);
    assert_eq!(h.revoking.len(), 1);
    assert_eq!(
        h.views,
        [
            signed_in(PlanState::Enabled, None),
            signing_out(Cleanup::Running, true),
            // The checkpoint decided the outcome; Delete and Finish follow.
            signing_out(Cleanup::Running, false),
            signed_out(),
        ]
    );
    let revoking = pending("Keep", "Revoking", false, false, true);
    // Load, promotion, Prepare, checkpoint, Finish, the reload.
    assert_eq!(
        h.markers,
        [
            None,
            revoking.clone(),
            revoking,
            pending("Keep", "Deleting", false, false, false),
            None,
            None,
        ]
    );
    assert_eq!(h.account(), Some(registration()));
    assert_eq!(names(&h), left(true, false));
    // The kept registration's client serves the next sign-in, with nothing registered anew.
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    assert_eq!(h.view(), &AccountView::SigningIn);
    assert_eq!(h.sent.len(), 1);
    assert_eq!(h.account(), Some(registration()));
}

#[test]
fn an_unconfirmed_revocation_is_decided_once_and_its_notice_outlives_a_restart() {
    let mut h = Harness::seeded("sign-out-unconfirmed", account_only);
    h.op(AccountOp::SignOut);
    finish(&mut h, false, 0);
    assert_eq!(h.view(), &signed_out_with(true));
    assert_eq!(
        h.markers[3..],
        [
            pending("Keep", "Deleting", false, false, true),
            Some(NOTICE_ONLY.to_owned()),
            Some(NOTICE_ONLY.to_owned()),
        ]
    );
    assert_eq!(names(&h), left(true, true));
    let h = h.restart();
    assert_eq!(h.views, [signed_out_with(true)]);
}

/// R3-H1: a crash right after an Erase promotion resumes at Prepare and erases the account.
#[test]
fn a_resumed_erase_removes_the_whole_account_record() {
    for confirm in [true, false] {
        let (store, service) = store(true);
        let mut h = boot(&store, &service);
        assert_eq!(h.views, [signing_out(Cleanup::Running, true)]);
        finish(&mut h, confirm, 0);
        assert_eq!(h.view(), &signed_out_with(!confirm), "{confirm}");
        assert_eq!(
            h.markers[2..],
            [
                pending("Erase", "Deleting", false, false, !confirm),
                (!confirm).then(|| NOTICE_ONLY.to_owned()),
                (!confirm).then(|| NOTICE_ONLY.to_owned()),
            ],
            "{confirm}"
        );
        assert_eq!(names(&h), left(false, !confirm), "{confirm}");
        assert_eq!(h.account(), None, "{confirm}");
    }
}

/// CAPS-89's R4-H1 trace: an unconfirmed revocation, the checkpoint, every delete, then a crash
/// before Finish. The restart only finishes, and still warns.
#[test]
fn a_crash_between_delete_and_finish_keeps_the_decided_warning() {
    let mut h = Harness::seeded("sign-out-r4h1", account_only);
    h.op(AccountOp::SignOut);
    h.hold = true;
    h.feed(Input::Revoked(Vec::new()));
    h.hold = true;
    h.release();
    assert_eq!(h.marker(), pending("Keep", "Deleting", false, false, true));
    assert!(matches!(
        h.reader().delete_for_sign_out(),
        crate::connections::vault::DeleteResult::Gone
    ));
    let h = h.restart();
    assert_eq!(
        h.views,
        [signing_out(Cleanup::Running, true), signed_out_with(true)]
    );
    assert!(h.revoking.is_empty());
    assert_eq!(names(&h), left(true, true));
}

/// CAPS-130: Delete follows the marker even when a record's manifest is gone, deleting its chunks
/// by name, up to the last one, for Keep and Erase; Keep retains only a live registration's slot.
#[test]
fn delete_removes_chunks_whose_manifest_is_gone() {
    for disposition in [Disposition::Keep, Disposition::Erase] {
        for gone in ["chatgpt", "revoke", "registration"] {
            let context = format!("{disposition:?} {gone}");
            let (store, service) = prepared(|vault, raw| {
                account_and_listed(vault);
                vault
                    .update_marker(MarkerUpdate::Promote(disposition))
                    .expect("promoted");
                vault
                    .update_marker(MarkerUpdate::Checkpoint {
                        list_empty_on_disk: false,
                    })
                    .expect("checkpoint");
                if gone == "registration" {
                    // An earlier Delete rewrote the registration into slot 1, then crashed.
                    if disposition == Disposition::Keep {
                        committed(&vault.save_account(&Secret::new(registration())));
                    }
                    raw("chatgpt/0/63", b"orphaned-token");
                    raw("chatgpt/1/63", b"orphaned-token");
                } else {
                    raw(gone, b"");
                    raw(&format!("{gone}/1/63"), b"orphaned-token");
                }
            });
            let h = boot(&store, &service);
            assert_eq!(
                h.views,
                [signing_out(Cleanup::Running, true), signed_out_with(true)],
                "{context}"
            );
            let kept = disposition == Disposition::Keep && gone != "chatgpt";
            assert_eq!(names(&h), left(kept, true), "{context}");
            assert!(h.revoking.is_empty() && h.sent.is_empty(), "{context}");
        }
    }
}

/// Row 2m: a record Prepare can't read persists uncertainty before anything is revoked; Keep
/// erases an account it can't read.
#[test]
fn an_unreadable_record_makes_the_sign_out_unconfirmed() {
    for broken in ["revoke", "chatgpt"] {
        let (store, service) = prepared(|vault, raw| {
            account_and_listed(vault);
            vault
                .update_marker(MarkerUpdate::Promote(Disposition::Keep))
                .expect("promoted");
            raw(broken, b"{");
        });
        let mut h = boot(&store, &service);
        assert_eq!(
            h.markers,
            [
                pending("Keep", "Revoking", false, false, true),
                pending("Keep", "Revoking", false, true, true),
            ],
            "{broken}"
        );
        let expected = if broken == "revoke" {
            ("refresh-old", Origin::Account)
        } else {
            ("refresh-listed", Origin::Exchange)
        };
        assert_eq!(
            tokens(&h.revoking[0]),
            [(expected.0.to_owned(), expected.1)],
            "{broken}"
        );
        finish(&mut h, true, 0);
        assert_eq!(h.view(), &signed_out_with(true), "{broken}");
        assert_eq!(names(&h), left(broken == "revoke", true), "{broken}");
    }
}

/// H3 and row 1r: a refused promotion fences use in this process only; Retry writes it again.
#[test]
fn a_refused_promotion_is_not_durable_until_retry_writes_it() {
    for restart in [true, false] {
        let mut h = Harness::seeded("sign-out-not-durable", account_only);
        h.store.0.lock().expect("fake lock").refusing = true;
        h.op(AccountOp::SignOut);
        assert_eq!(h.view(), &signing_out(Cleanup::NotDurable, true));
        assert_eq!(
            h.errors,
            [user_error(ErrorKey::SignOutNotDurable, &[Action::Retry])]
        );
        assert_eq!(h.marker(), None);
        h.op(AccountOp::SignIn);
        assert!(h.listening.is_empty(), "use stays fenced");
        h.store.0.lock().expect("fake lock").refusing = false;
        if restart {
            let h = h.restart();
            assert_eq!(h.views, [signed_in(PlanState::Enabled, None)]);
            continue;
        }
        h.op(AccountOp::Retry);
        assert_eq!(h.marker(), pending("Keep", "Revoking", false, false, true));
        finish(&mut h, true, 0);
        assert_eq!(h.view(), &signed_out());
    }
}

/// R2-4: a sign-out during the current attempt's dispatched save waits for it, and then revokes
/// what it saved.
#[test]
fn a_sign_out_during_a_dispatched_save_waits_and_revokes_the_saved_set() {
    let mut h = Harness::seeded("sign-out-commit", account_only);
    h.op(AccountOp::SignIn);
    h.callback("client-saved");
    h.hold = true;
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    h.op(AccountOp::SignOut);
    assert_eq!(h.view(), &AccountView::SigningIn);
    h.release();
    assert_eq!(
        h.views[h.views.len() - 2..],
        [
            signed_in(PlanState::Enabled, None),
            signing_out(Cleanup::Running, true)
        ]
    );
    assert_eq!(
        tokens(&h.revoking[0]),
        [("refresh-new".to_owned(), Origin::Account)]
    );
    finish(&mut h, true, 0);
    assert_eq!(h.view(), &signed_out());
}

/// Step 3: the sent exchange runs to its end and its set gets its own entry. An ambiguous ending
/// can't be confirmed by any revocation (pick 3).
#[test]
fn a_sign_out_during_a_sent_exchange_waits_and_owns_its_outcome() {
    for ambiguous in [false, true] {
        let mut h = Harness::seeded("sign-out-exchange", account_only);
        h.op(AccountOp::SignIn);
        h.callback("client-saved");
        h.op(AccountOp::SignOut);
        assert_eq!(h.closed, 1, "{ambiguous}");
        assert_eq!(h.view(), &signing_out(Cleanup::Running, true));
        assert!(h.revoking.is_empty(), "Prepare waits for the request");
        assert_eq!(h.marker(), pending("Keep", "Revoking", true, false, true));
        h.reply(if ambiguous {
            Err(ExchangeError::Http(HttpError::Timeout))
        } else {
            received(Some("refresh-new"), "subject-1", PLAN)
        });
        let mut expected = vec![("refresh-old".to_owned(), Origin::Account)];
        if !ambiguous {
            expected.push(("refresh-new".to_owned(), Origin::Exchange));
        }
        assert_eq!(tokens(&h.revoking[0]), expected, "{ambiguous}");
        assert_eq!(
            h.marker(),
            pending("Keep", "Revoking", false, ambiguous, true),
            "{ambiguous}"
        );
        finish(&mut h, true, 0);
        assert_eq!(h.view(), &signed_out_with(ambiguous), "{ambiguous}");
        assert_eq!(h.sent.len(), 1, "no other code is sent");
        assert_eq!(names(&h), left(true, ambiguous), "{ambiguous}");
    }
}

/// Row 2r': a refused save of the settling set holds the sign-out for Retry. After a crash the
/// set is gone, and its persisted pending flag keeps the warning even when revocation succeeds.
#[test]
fn a_refused_save_of_the_settling_set_holds_the_sign_out() {
    for restart in [false, true] {
        let mut h = Harness::seeded("sign-out-held", account_only);
        h.op(AccountOp::SignIn);
        h.callback("client-saved");
        h.op(AccountOp::SignOut);
        h.store.0.lock().expect("fake lock").refusing = true;
        h.reply(received(Some("refresh-new"), "subject-1", PLAN));
        assert_eq!(h.view(), &signing_out(Cleanup::Refused, true));
        assert_eq!(
            h.errors.last(),
            Some(&user_error(
                ErrorKey::CredentialsUnavailable,
                &[Action::Retry]
            ))
        );
        assert!(h.revoking.is_empty());
        assert_eq!(h.marker(), pending("Keep", "Revoking", true, false, true));
        h.store.0.lock().expect("fake lock").refusing = false;
        let mut h = if restart {
            h.restart()
        } else {
            h.op(AccountOp::Retry);
            h
        };
        let mut expected = vec![("refresh-old".to_owned(), Origin::Account)];
        if !restart {
            expected.push(("refresh-new".to_owned(), Origin::Exchange));
        }
        assert_eq!(tokens(&h.revoking[0]), expected, "{restart}");
        finish(&mut h, true, 0);
        assert_eq!(h.view(), &signed_out_with(restart), "{restart}");
    }
}

/// A sign-out that lands while a revocation pass runs: the pass ends first, dropping what it
/// confirmed without deleting the list or touching the promotion, then Prepare runs.
#[test]
fn a_sign_out_during_a_revocation_pass_waits_for_it() {
    let mut h = Harness::seeded("sign-out-pass", account_and_listed);
    assert_eq!(
        tokens(&h.revoking[0]),
        [("refresh-listed".to_owned(), Origin::Exchange)]
    );
    h.op(AccountOp::SignOut);
    assert_eq!(h.revoking.len(), 1, "Prepare waits for the pass");
    let pass = h.revoking[0].clone();
    h.feed(Input::Revoked(pass));
    let promoted = pending("Keep", "Revoking", false, false, true);
    // Load, promotion, the pass's relist, Prepare.
    assert_eq!(
        h.markers,
        [
            marker(&["sweep"]),
            promoted.clone(),
            promoted.clone(),
            promoted
        ]
    );
    assert_eq!(
        tokens(&h.revoking[1]),
        [("refresh-old".to_owned(), Origin::Account)]
    );
    finish(&mut h, true, 1);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(names(&h), left(true, false));
}

/// The held-sweep fixture: a pass's relist dispatched before the promotion runs first, and the
/// promotion then lands on what it left.
#[test]
fn a_sweep_dispatched_before_the_promotion_runs_first() {
    let mut h = Harness::seeded("sign-out-held-sweep", account_and_listed);
    let pass = h.revoking[0].clone();
    h.hold = true;
    h.feed(Input::Revoked(pass));
    h.op(AccountOp::SignOut);
    h.release();
    let promoted = pending("Keep", "Revoking", false, false, true);
    // Load, the pass's relist (list deleted, sweep cleared), promotion, Prepare.
    assert_eq!(
        h.markers,
        [marker(&["sweep"]), None, promoted.clone(), promoted]
    );
    assert_eq!(
        tokens(&h.revoking[1]),
        [("refresh-old".to_owned(), Origin::Account)]
    );
    finish(&mut h, true, 1);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(names(&h), left(true, false));
}

/// An unreadable marker fails closed: an Erase sign-out with its uncertainty already set.
#[test]
fn an_unreadable_marker_erases_and_warns() {
    let h = Harness::seeded("sign-out-unreadable-marker", account_only);
    h.store
        .set(
            &format!("{}/signout", h.service),
            &Secret::new(Zeroizing::new(br#"{"v":2}"#.to_vec())),
        )
        .expect("unreadable marker");
    let mut h = h.restart();
    finish(&mut h, true, 0);
    assert_eq!(h.view(), &signed_out_with(true));
    assert_eq!(names(&h), left(false, true));
}

fn deny_read(h: &Harness, record: &str) {
    h.store.0.lock().expect("fake lock").denied_read = Some(format!("{}/{record}", h.service));
}

fn allow_reads(h: &Harness) {
    h.store.0.lock().expect("fake lock").denied_read = None;
}

/// H1 (r2): Prepare's refused read of either record is a refused step, not damaged data. Nothing is
/// persisted as unknown, and after Retry every entry, the account's token included, is revoked
/// and confirmation leaves the notice clear.
#[test]
fn a_refused_read_in_prepare_is_retried_and_loses_no_revocation() {
    for record in ["revoke", "chatgpt"] {
        let mut h = Harness::seeded("sign-out-refused-read", account_and_listed);
        h.op(AccountOp::SignOut);
        deny_read(&h, record);
        // The startup pass ends unconfirmed, so Prepare runs and is refused.
        h.feed(Input::Revoked(Vec::new()));
        assert_eq!(h.view(), &signing_out(Cleanup::Refused, true), "{record}");
        assert_eq!(
            h.revoking.len(),
            1,
            "{record}: nothing revoked without the record"
        );
        assert_eq!(
            h.marker(),
            pending("Keep", "Revoking", false, false, true),
            "{record}: a refused read isn't uncertainty"
        );
        allow_reads(&h);
        h.op(AccountOp::Retry);
        assert_eq!(
            tokens(&h.revoking[1]),
            [
                ("refresh-old".to_owned(), Origin::Account),
                ("refresh-listed".to_owned(), Origin::Exchange),
            ],
            "{record}"
        );
        finish(&mut h, true, 1);
        assert_eq!(h.view(), &signed_out(), "{record}");
        assert_eq!(names(&h), left(true, false), "{record}");
    }
}

/// H1 (r2): under Keep, a refused read of the account during Delete is retried; it never erases
/// the registration Keep retains.
#[test]
fn a_refused_account_read_during_delete_keeps_the_registration() {
    let mut h = Harness::seeded("sign-out-refused-delete", account_only);
    h.op(AccountOp::SignOut);
    h.hold = true;
    h.feed(Input::Revoked(h.revoking[0].clone()));
    h.hold = true;
    h.release();
    deny_read(&h, "chatgpt");
    h.release();
    assert_eq!(h.view(), &signing_out(Cleanup::Refused, false));
    assert_eq!(
        names(&h),
        [
            "chatgpt",
            "chatgpt/0/0",
            "host",
            "revoke",
            "revoke/1/0",
            "signout"
        ],
        "nothing is deleted, the account included"
    );
    allow_reads(&h);
    h.op(AccountOp::Retry);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(names(&h), left(true, false));
    assert_eq!(h.account(), Some(registration()));
}
