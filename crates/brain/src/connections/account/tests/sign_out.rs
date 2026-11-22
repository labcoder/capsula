//! Sign-out (design §3 rule 5): every persisted phase replayed after a crash or a refusal at each
//! store mutation, Keep and Erase against confirmed and unconfirmed outcomes, Delete driven by
//! the marker alone, and the crosses with the lane and revocation passes (§4).
#![expect(
    clippy::fn_params_excessive_bools,
    reason = "fixtures take the persisted flags and outcomes they assert"
)]

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::rc::Rc;

use super::*;
use crate::connections::http_client::HttpError;
use crate::connections::marker::{Disposition, MarkerUpdate};
use crate::connections::records::RevocationList;

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
    let mut names: Vec<String> = h
        .store
        .names()
        .iter()
        .map(|name| shape(&h.service, name))
        .collect();
    names.sort();
    names
}

/// What a finished sign-out leaves: the host, and the registration Keep retains. Each is one
/// chunk of its current generation.
fn left(keep: bool) -> Vec<&'static str> {
    if keep {
        vec!["chatgpt/0", "host/0"]
    } else {
        vec!["host/0"]
    }
}

/// Answers each revocation run after the first `answered` until the sign-out ends or a step is
/// refused; returns how many runs are answered then.
fn answer(h: &mut Harness, confirm: bool, mut answered: usize) -> usize {
    while h.revoking.len() > answered {
        let run = h.revoking[answered].clone();
        answered += 1;
        h.feed(Input::Revoked(if confirm { run } else { Vec::new() }));
    }
    answered
}

/// Answers each revocation run after the first `answered`, and retries each refusal, until the
/// sign-out ends.
fn finish(h: &mut Harness, confirm: bool, mut answered: usize) {
    for _ in 0..16 {
        answered = answer(h, confirm, answered);
        if matches!(
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

/// A store and the authority beside it, as a seed or a run left them.
struct Seeded {
    store: FakeStore,
    service: String,
    scratch: Rc<Scratch>,
}

impl Seeded {
    fn authority(&self) -> std::path::PathBuf {
        self.scratch.db().with_file_name(authority::FILE)
    }
}

/// A store `seed` prepares through a vault and, for damage, by record: `damage("chatgpt")`
/// garbles the current account's first chunk, keeping its length.
fn prepared(seed: impl FnOnce(&mut Vault<FakeStore>, &dyn Fn(&str))) -> Seeded {
    let scratch = Rc::new(Scratch::new("sign-out-seed"));
    let service = format!("test-account-{:016x}", getrandom::u64().expect("random"));
    let store = FakeStore::default();
    let path = scratch.db().with_file_name(authority::FILE);
    let mut vault = Vault::with_service(
        store.clone(),
        &service,
        &scratch.db().with_file_name("seed.lock"),
    )
    .expect("seed vault");
    let damage = |record: &str| {
        let name = current_chunk(&path, &service, record);
        let length = store
            .get(&name)
            .expect("fake read")
            .expect("the chunk")
            .expose()
            .len();
        let mut planted = store.snapshot();
        planted.insert(
            name.clone(),
            Secret::new(Zeroizing::new(vec![b'{'; length])),
        );
        store.rollback(&planted, &[name]);
    };
    seed(&mut vault, &damage);
    drop(vault);
    Seeded {
        store,
        service,
        scratch,
    }
}

/// A copy of `seeded`, store and authority both, for one run to change.
fn fork(seeded: &Seeded) -> Seeded {
    let scratch = Rc::new(Scratch::new("sign-out"));
    let to = scratch.db().with_file_name(authority::FILE);
    std::fs::create_dir_all(to.parent().expect("scratch folder")).expect("scratch folder");
    let from = seeded.authority();
    std::fs::copy(&from, &to).expect("copy authority");
    let log = |path: &Path| path.with_file_name(format!("{}-wal", authority::FILE));
    if log(&from).exists() {
        std::fs::copy(log(&from), log(&to)).expect("copy authority log");
    }
    Seeded {
        store: seeded.store.fork(),
        service: seeded.service.clone(),
        scratch,
    }
}

/// A signed-in account and its host, and with `erase`, a promoted Erase sign-out a crash
/// interrupted right after promotion.
fn store(erase: bool) -> Seeded {
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

fn boot(seeded: &Seeded) -> Harness {
    Harness::boot(
        Rc::clone(&seeded.scratch),
        seeded.service.clone(),
        seeded.store.clone(),
        Wall::new(),
    )
}

/// The store and authority at a fault, before any recovery.
struct Boundary {
    marker: Option<String>,
    names: Vec<String>,
    /// Refresh tokens still held in `chatgpt` or durably listed.
    tokens: Vec<String>,
}

/// Reads `run` as it is; the tokens come from a copy, since opening a vault sweeps.
fn boundary(run: &Seeded) -> Boundary {
    let (names, marker) = (run.store.names(), persisted_marker(&run.authority()));
    let copy = fork(run);
    let reader = Vault::with_service(
        copy.store.clone(),
        &copy.service,
        &copy.scratch.db().with_file_name("reader.lock"),
    )
    .expect("reader");
    let account = reader
        .load_for_sign_out()
        .or_else(|_| reader.load_account())
        .expect("readable account");
    let mut tokens: Vec<String> = account
        .and_then(|account| {
            account
                .expose()
                .tokens
                .as_ref()
                .map(|t| t.refresh_token.to_string())
        })
        .into_iter()
        .collect();
    if let Some(list) = reader.load_revocations().expect("readable list") {
        tokens.extend(
            list.expose()
                .entries
                .iter()
                .map(|e| e.refresh_token.to_string()),
        );
    }
    Boundary {
        marker,
        names,
        tokens,
    }
}

/// A generation name without its random id, `chatgpt/0`, so two runs' names compare.
fn shape(service: &str, name: &str) -> String {
    let rest = name
        .strip_prefix(&format!("{service}/gen/"))
        .expect("a generation name");
    let mut parts = rest.split('/');
    let kind = parts.next().expect("a kind");
    let index = parts.nth(1).expect("an index");
    format!("{kind}/{index}")
}

/// The names, by shape, that a clean run's first mutations leave on top of `seed`.
fn replay(service: &str, seed: &[String], log: &[Mutation]) -> Vec<String> {
    let mut names: std::collections::BTreeSet<&String> = seed.iter().collect();
    for mutation in log {
        match mutation {
            Mutation::Set(name) => names.insert(name),
            Mutation::Delete(name) => names.remove(name),
        };
    }
    let mut shapes: Vec<String> = names.into_iter().map(|name| shape(service, name)).collect();
    shapes.sort();
    shapes
}

/// A whole sign-out, live Keep or resumed Erase, with `fault` at mutation `at`, on a copy of
/// `seeded`. The state is captured at the fault: after the crash, or when the refusal stops the
/// sign-out (or it ends). Then a crash restarts the brain and a refusal is retried. Whether it
/// crashed comes back too. With `notes`, each mutation notes the marker it ran under.
fn faulted(
    seeded: &Seeded,
    erase: bool,
    at: usize,
    fault: Fault,
    notes: bool,
) -> (Harness, Boundary, bool) {
    let run = fork(seeded);
    let arm = || {
        run.store.arm(at, fault);
        if notes {
            run.store.0.lock().expect("fake lock").note_markers =
                Some((run.authority(), Vec::new()));
        }
    };
    let mut first = None;
    let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
        if erase {
            arm();
        }
        let h = first.insert(boot(&run));
        if !erase {
            arm();
            h.op(AccountOp::SignOut);
        }
        answer(h, true, 0);
    }))
    .is_err();
    let at_fault = boundary(&run);
    if let Some(mut h) = first.filter(|_| !crashed) {
        let answered = h.revoking.len();
        finish(&mut h, true, answered);
        return (h, at_fault, false);
    }
    let mut h = boot(&run);
    finish(&mut h, true, 0);
    (h, at_fault, true)
}

/// OSC-117, CAPS-138 M2 and OSC-233: a crash after, or a refusal of, any one of the store
/// mutations a whole sign-out makes. At the fault, before recovery, the marker is the one that
/// mutation ran under, a crash leaves exactly the clean run's names so far, and while Revoking the
/// account record is whole and still holds its token. Afterwards the next start or Retry
/// finishes: the outcome is decided once, every token name goes, and a restart after the
/// checkpoint only deletes and finishes.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one matrix, with its boundary and final assertions side by side"
)]
fn a_crash_or_refusal_at_every_mutation_still_ends_signed_out() {
    for erase in [false, true] {
        let seeded = store(erase);
        let seed = seeded.store.names();
        let account = format!("{}/gen/chatgpt/", seeded.service);
        let account_names: Vec<&String> = seed.iter().filter(|n| n.starts_with(&account)).collect();
        assert_eq!(account_names.len(), 1, "one account chunk");
        let (dry, _, _) = faulted(&seeded, erase, usize::MAX, Fault::Refuse, true);
        let (mutations, notes) = {
            let state = dry.store.0.lock().expect("fake lock");
            let notes = state.note_markers.as_ref().expect("notes").1.clone();
            (state.mutations.clone(), notes)
        };
        assert_eq!(notes.len(), mutations.len());
        let generations = format!("{}/gen/", seeded.service);
        // An Erase arms before its boot, whose open first deletes every legacy name.
        let legacy = mutations
            .iter()
            .take_while(|m| matches!(m, Mutation::Delete(name) if !name.starts_with(&generations)))
            .count();
        assert_eq!(legacy, if erase { 262 } else { 0 });
        let list = format!("{}/gen/revoke/", seeded.service);
        let flips: Vec<usize> = (0..mutations.len())
            .filter(|&at| matches!(&mutations[at], Mutation::Set(name) if name.starts_with(&list)))
            .collect();
        // Prepare's adoption, then the checkpoint's rewrite without the confirmed entry.
        assert_eq!(flips.len(), 2, "{mutations:?}");
        let rewrite = flips[0] + 1..=flips[1];
        let disposition = if erase { "Erase" } else { "Keep" };
        let revoking = pending(disposition, "Revoking", false, false, true).expect("a marker");
        let deleting = pending(disposition, "Deleting", false, false, false).expect("a marker");
        let checkpoint = notes
            .iter()
            .position(|note| *note == deleting)
            .expect("the checkpoint");
        assert!(
            checkpoint > flips[1],
            "the checkpoint follows the list's rewrite"
        );
        assert!(
            notes[..checkpoint].iter().all(|note| *note == revoking)
                && notes[checkpoint..].iter().all(|note| *note == deleting),
            "every mutation runs under Revoking, then Deleting: {notes:?}"
        );
        for at in legacy..mutations.len() {
            for fault in [Fault::CrashAfter, Fault::Refuse] {
                let (h, at_fault, crashed) = faulted(&seeded, erase, at, fault, false);
                let context = format!("erase {erase}, mutation {at}, {fault:?}");
                assert_eq!(crashed, matches!(fault, Fault::CrashAfter), "{context}");
                // A refused delete only leaves its names for the next sweep, and a refusal in the
                // checkpoint's own list rewrite doesn't stop the sign-out.
                let ran_on = !crashed
                    && (matches!(mutations[at], Mutation::Delete(_)) || rewrite.contains(&at));
                // A refused rewrite leaves the confirmed entry listed, so the checkpoint can't
                // claim confirmation.
                let notice = ran_on && rewrite.contains(&at);
                let expected = if ran_on {
                    notice.then(|| NOTICE_ONLY.to_owned())
                } else {
                    Some(notes[at].clone())
                };
                assert_eq!(
                    at_fault.marker, expected,
                    "{context}: the marker at the fault"
                );
                if crashed {
                    let mut names: Vec<String> = at_fault
                        .names
                        .iter()
                        .map(|name| shape(&seeded.service, name))
                        .collect();
                    names.sort();
                    assert_eq!(
                        names,
                        replay(&seeded.service, &seed, &mutations[legacy..=at]),
                        "{context}: the names at the crash"
                    );
                }
                if !ran_on && notes[at] == revoking {
                    assert!(
                        account_names
                            .iter()
                            .all(|name| at_fault.names.contains(name)),
                        "{context}: nothing is deleted before the checkpoint"
                    );
                    assert!(
                        at_fault.tokens.iter().any(|token| token == "refresh-old"),
                        "{context}: the token is held until it's confirmed revoked"
                    );
                }
                assert_eq!(h.view(), &signed_out_with(notice), "{context}");
                assert_eq!(
                    h.marker(),
                    notice.then(|| NOTICE_ONLY.to_owned()),
                    "{context}"
                );
                // A refused delete's name waits for the next start's sweep.
                let lingering = match &mutations[at] {
                    Mutation::Delete(name) if ran_on => Some(shape(&seeded.service, name)),
                    _ => None,
                };
                let mut expected: Vec<String> =
                    left(!erase).into_iter().map(str::to_owned).collect();
                expected.extend(lingering.clone());
                expected.sort();
                assert_eq!(names(&h), expected, "{context}");
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
                if crashed && notes[at] == deleting {
                    assert!(h.revoking.is_empty(), "{context}: Deleting revokes nothing");
                    assert!(
                        h.markers.iter().flatten().all(|m| !m.contains("Revoking")),
                        "{context}: Deleting never returns to Revoking"
                    );
                }
                assert!(h.sent.is_empty(), "{context}");
                if lingering.is_some() {
                    let h = h.restart();
                    assert_eq!(
                        names(&h),
                        left(!erase),
                        "{context}: the next start swept it"
                    );
                }
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
    assert_eq!(names(&h), left(true));
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
    assert_eq!(names(&h), left(true));
    let h = h.restart();
    assert_eq!(h.views, [signed_out_with(true)]);
}

/// R3-H1: a crash right after an Erase promotion resumes at Prepare and erases the account.
#[test]
fn a_resumed_erase_removes_the_whole_account_record() {
    for confirm in [true, false] {
        let seeded = store(true);
        let mut h = boot(&seeded);
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
        assert_eq!(names(&h), left(false), "{confirm}");
        assert_eq!(h.account(), None, "{confirm}");
    }
}

/// OSC-233: Delete and Finish follow the authority alone. Deletes that report success but leave
/// their names, as Credential Manager's did, never hold the sign-out, and the next start sweeps
/// them; Keep retains only the registration.
#[test]
fn delete_and_finish_never_wait_for_the_store_to_forget() {
    for disposition in [Disposition::Keep, Disposition::Erase] {
        let context = format!("{disposition:?}");
        let seeded = prepared(|vault, _| {
            account_and_listed(vault);
            vault
                .update_marker(MarkerUpdate::Promote(disposition))
                .expect("promoted");
            vault
                .update_marker(MarkerUpdate::Checkpoint {
                    list_empty_on_disk: false,
                })
                .expect("checkpoint");
        });
        seeded.store.0.lock().expect("fake lock").sticky_deletes = usize::MAX;
        let h = boot(&seeded);
        assert_eq!(
            h.views,
            [signing_out(Cleanup::Running, true), signed_out_with(true)],
            "{context}"
        );
        assert!(h.revoking.is_empty() && h.sent.is_empty(), "{context}");
        assert!(
            h.holds("revoke"),
            "{context}: the list's names outlived their delete"
        );
        h.store.0.lock().expect("fake lock").sticky_deletes = 0;
        let h = h.restart();
        assert_eq!(h.views, [signed_out_with(true)], "{context}");
        assert_eq!(
            names(&h),
            left(disposition == Disposition::Keep),
            "{context}"
        );
    }
}

/// Row 2m: a record Prepare can't read persists uncertainty before anything is revoked; Keep
/// erases an account it can't read.
#[test]
fn an_unreadable_record_makes_the_sign_out_unconfirmed() {
    for broken in ["revoke", "chatgpt"] {
        let seeded = prepared(|vault, damage| {
            account_and_listed(vault);
            vault
                .update_marker(MarkerUpdate::Promote(Disposition::Keep))
                .expect("promoted");
            damage(broken);
        });
        let mut h = boot(&seeded);
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
        assert_eq!(names(&h), left(broken == "revoke"), "{broken}");
    }
}

/// H3 and row 1r: a refused promotion fences use in this process only; Retry writes it again.
#[test]
fn a_refused_promotion_is_not_durable_until_retry_writes_it() {
    for restart in [true, false] {
        let mut h = Harness::seeded("sign-out-not-durable", account_only);
        h.refuse_commits();
        h.op(AccountOp::SignOut);
        assert_eq!(h.view(), &signing_out(Cleanup::NotDurable, true));
        assert_eq!(
            h.errors,
            [user_error(ErrorKey::SignOutNotDurable, &[Action::Retry])]
        );
        assert_eq!(h.marker(), None);
        h.op(AccountOp::SignIn);
        assert!(h.listening.is_empty(), "use stays fenced");
        h.allow_commits();
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

/// CAPS-130 87-H1: a sign-out during the current attempt's dispatched save fences at once. No op
/// before or after it can replace it, the save lands without a usable account being shown or the
/// notice cleared, and Prepare adopts what it saved; the token it replaced was listed first.
#[test]
fn a_sign_out_during_a_dispatched_save_fences_at_once_and_revokes_the_saved_set() {
    for (op, first) in [
        (AccountOp::CancelSignIn, false),
        (AccountOp::SignIn, false),
        (AccountOp::EnablePlan, false),
        (AccountOp::SignIn, true),
    ] {
        let context = format!("{op:?}, before the sign-out: {first}");
        let mut h = Harness::seeded("sign-out-commit", account_only);
        h.op(AccountOp::SignIn);
        h.callback("client-saved");
        h.hold = true;
        h.reply(received(Some("refresh-new"), "subject-1", PLAN));
        if first {
            h.op(op);
        }
        let shown = h.views.len();
        h.op(AccountOp::SignOut);
        assert_eq!(h.view(), &signing_out(Cleanup::Running, true), "{context}");
        if !first {
            h.op(op);
        }
        h.release();
        assert_eq!(
            h.view(),
            &signing_out(Cleanup::Running, true),
            "{context}: the save doesn't clear the notice"
        );
        assert_eq!(h.listening.len(), 1, "{context}: no replacement listener");
        assert_eq!(
            tokens(&h.revoking[0]),
            [
                ("refresh-new".to_owned(), Origin::Account),
                ("refresh-old".to_owned(), Origin::Exchange)
            ],
            "{context}"
        );
        finish(&mut h, true, 0);
        let mut expected = vec![signing_out(Cleanup::Running, true)];
        if !first {
            expected.push(signing_out(Cleanup::Running, true));
        }
        expected.extend([signing_out(Cleanup::Running, false), signed_out()]);
        assert_eq!(h.views[shown..], expected, "{context}");
        assert!(h.errors.is_empty(), "{context}");
        assert_eq!(names(&h), left(true), "{context}");
        assert_eq!(h.account(), Some(registration()), "{context}");
    }
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
        assert_eq!(names(&h), left(true), "{ambiguous}");
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

/// Prepare follows an outstanding pass, whether relist or promotion was dispatched first.
#[test]
fn a_sign_out_during_a_revocation_pass_waits_for_it() {
    for (held_relist, name) in [(false, "sign-out-pass"), (true, "sign-out-held-sweep")] {
        let mut h = Harness::seeded(name, account_and_listed);
        assert_eq!(
            tokens(&h.revoking[0]),
            [("refresh-listed".to_owned(), Origin::Exchange)]
        );
        let pass = h.revoking[0].clone();
        if held_relist {
            h.hold = true;
            h.feed(Input::Revoked(pass));
            h.op(AccountOp::SignOut);
            h.release();
        } else {
            h.op(AccountOp::SignOut);
            assert_eq!(h.revoking.len(), 1, "Prepare waits for the pass");
            h.feed(Input::Revoked(pass));
        }
        let promoted = pending("Keep", "Revoking", false, false, true);
        let second = if held_relist { None } else { promoted.clone() };
        assert_eq!(
            h.markers,
            [marker(&["sweep"]), second, promoted.clone(), promoted]
        );
        assert_eq!(
            tokens(&h.revoking[1]),
            [("refresh-old".to_owned(), Origin::Account)]
        );
        finish(&mut h, true, 1);
        assert_eq!(h.view(), &signed_out());
        assert_eq!(names(&h), left(true));
    }
}

/// An unreadable marker fails closed: an Erase sign-out with its uncertainty already set.
#[test]
fn an_unreadable_marker_erases_and_warns() {
    let h = Harness::seeded("sign-out-unreadable-marker", account_only);
    h.plant_marker(r#"{"v":2}"#);
    let mut h = h.restart();
    finish(&mut h, true, 0);
    assert_eq!(h.view(), &signed_out_with(true));
    assert_eq!(names(&h), left(false));
}

/// Refuses reads of the current `record`, or of the marker for `signout`, until `allow_reads`.
fn deny_read(h: &Harness, record: &str) {
    if record == "signout" {
        h.hide_marker();
    } else {
        h.store.0.lock().expect("fake lock").denied_read = Some(h.current_chunk(record));
    }
}

fn allow_reads(h: &Harness) {
    h.store.0.lock().expect("fake lock").denied_read = None;
    if h.marker().as_deref() == Some(UNREADABLE) {
        h.restore_marker();
    }
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
        assert_eq!(names(&h), left(true), "{record}");
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
        ["chatgpt/0", "host/0", "revoke/0"],
        "nothing is deleted, the account included"
    );
    allow_reads(&h);
    h.op(AccountOp::Retry);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(names(&h), left(true));
    assert_eq!(h.account(), Some(registration()));
}

/// CAPS-138 M1: a refused startup marker read keeps use fenced and decides nothing, not even a
/// fail-closed Erase. Once reads recover, Retry reads the real marker: no sign-out leaves the
/// account usable, Revoking resumes there, and Deleting only deletes and finishes.
#[test]
fn a_refused_marker_read_at_startup_waits_for_the_real_marker() {
    for phase in ["absent", "Revoking", "Deleting"] {
        let seeded = prepared(|vault, _| {
            account_only(vault);
            vault.host_id().expect("host");
            if phase != "absent" {
                vault
                    .update_marker(MarkerUpdate::Promote(Disposition::Keep))
                    .expect("promoted");
            }
            if phase == "Deleting" {
                vault
                    .update_marker(MarkerUpdate::Checkpoint {
                        list_empty_on_disk: false,
                    })
                    .expect("checkpoint");
            }
        });
        hide_marker(&seeded.authority());
        let mut h = boot(&seeded);
        assert_eq!(h.views, [signing_out(Cleanup::Refused, true)], "{phase}");
        let (names_before, marker_before) = (names(&h), h.marker());
        for op in [
            AccountOp::SignIn,
            AccountOp::EnablePlan,
            AccountOp::CancelSignIn,
            AccountOp::SignOut,
        ] {
            h.op(op);
        }
        assert!(h.listening.is_empty() && h.revoking.is_empty(), "{phase}");
        assert_eq!(h.markers.len(), 1, "{phase}: only the refused load ran");
        assert_eq!(names(&h), names_before, "{phase}");
        assert_eq!(h.marker(), marker_before, "{phase}");
        allow_reads(&h);
        h.op(AccountOp::Retry);
        match phase {
            "absent" => {
                assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
                assert!(h.revoking.is_empty());
                assert_eq!(names(&h), names_before);
                assert_eq!(h.marker(), None);
            }
            "Revoking" => {
                assert_eq!(h.view(), &signing_out(Cleanup::Running, true));
                assert_eq!(
                    tokens(&h.revoking[0]),
                    [("refresh-old".to_owned(), Origin::Account)]
                );
                finish(&mut h, true, 0);
                assert_eq!(h.view(), &signed_out());
                assert_eq!(names(&h), left(true));
            }
            _ => {
                assert_eq!(h.view(), &signed_out_with(true));
                assert!(h.revoking.is_empty(), "Deleting sends no revocation");
                assert!(h.markers.iter().flatten().all(|m| !m.contains("Revoking")));
                assert_eq!(names(&h), left(true));
            }
        }
    }
}

/// CAPS-138 M2: a refused checkpoint holds the sign-out in Revoking with nothing deleted, and
/// Retry runs Prepare, Revoke and the checkpoint again.
#[test]
fn a_refused_checkpoint_deletes_nothing_before_retry() {
    let mut h = Harness::seeded("sign-out-refused-checkpoint", account_only);
    h.op(AccountOp::SignOut);
    let before = names(&h);
    assert_eq!(before, ["chatgpt/0", "host/0", "revoke/0"]);
    h.refuse_commits();
    h.feed(Input::Revoked(h.revoking[0].clone()));
    assert_eq!(h.view(), &signing_out(Cleanup::Refused, true));
    assert_eq!(h.marker(), pending("Keep", "Revoking", false, false, true));
    assert_eq!(names(&h), before, "nothing is deleted");
    h.allow_commits();
    assert_eq!(
        h.revocations(),
        [(
            "client-saved".to_owned(),
            "refresh-old".to_owned(),
            NOW,
            Origin::Account
        )],
        "the refused rewrite left the entry listed"
    );
    h.op(AccountOp::Retry);
    assert_eq!(h.revoking.len(), 2, "Revoke runs again");
    finish(&mut h, true, 1);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(names(&h), left(true));
}

/// A crash before Finish and a refused Finish both preserve the decided notice.
#[test]
fn a_refused_finish_keeps_the_decided_notice_across_a_restart() {
    for (refused, name) in [(false, "sign-out-r4h1"), (true, "sign-out-refused-finish")] {
        let mut h = Harness::seeded(name, account_only);
        h.op(AccountOp::SignOut);
        h.hold = true;
        h.feed(Input::Revoked(Vec::new()));
        h.hold = true;
        h.release();
        assert_eq!(h.marker(), pending("Keep", "Deleting", false, false, true));
        if refused {
            // Delete writes no marker, so Finish's is the job's first.
            h.refuse_marker_writes(0);
            h.release();
            assert_eq!(h.view(), &signing_out(Cleanup::Refused, true));
            assert_eq!(h.marker(), pending("Keep", "Deleting", false, false, true));
            assert_eq!(names(&h), left(true), "only the marker write was refused");
            h.allow_marker_writes();
        } else {
            assert!(matches!(
                h.reader().delete_for_sign_out(),
                crate::connections::vault::DeleteResult::Gone
            ));
        }
        let h = h.restart();
        assert_eq!(
            h.views,
            [signing_out(Cleanup::Running, true), signed_out_with(true)]
        );
        assert!(h.revoking.is_empty());
        assert_eq!(h.marker(), Some(NOTICE_ONLY.to_owned()));
        assert_eq!(names(&h), left(true));
    }
}

/// CAPS-130 87-H2: with no revocation list and a damaged account, the uncertainty Prepare persists
/// lets the checkpoint through without rereading that account, so Keep and Erase both finish
/// unconfirmed and every token-bearing name goes.
#[test]
fn a_damaged_account_with_no_list_still_reaches_delete() {
    for disposition in [Disposition::Keep, Disposition::Erase] {
        let seeded = prepared(|vault, damage| {
            account_only(vault);
            vault.host_id().expect("host");
            vault
                .update_marker(MarkerUpdate::Promote(disposition))
                .expect("promoted");
            damage("chatgpt");
        });
        let h = boot(&seeded);
        assert_eq!(
            h.views,
            [signing_out(Cleanup::Running, true), signed_out_with(true)],
            "{disposition:?}"
        );
        assert!(h.revoking.is_empty(), "{disposition:?}: nothing was listed");
        assert_eq!(h.marker(), Some(NOTICE_ONLY.to_owned()), "{disposition:?}");
        assert_eq!(names(&h), left(false), "{disposition:?}");
    }
}

/// CAPS-130 87-M1: a sign-in while the load after Finish is still out starts nothing. Once that
/// load lands, the next sign-in reuses the kept registration's client and keeps its subject.
#[test]
fn a_sign_in_before_the_final_load_waits_for_the_kept_registration() {
    let mut h = Harness::seeded("sign-out-final-load", account_only);
    h.op(AccountOp::SignOut);
    h.hold = true;
    h.feed(Input::Revoked(h.revoking[0].clone()));
    for _ in 0..2 {
        h.hold = true;
        h.release();
    }
    assert_eq!(
        h.markers.last(),
        Some(&None),
        "Finish ran; its reload is held"
    );
    h.op(AccountOp::SignIn);
    assert!(
        h.listening.is_empty(),
        "no sign-in before the registration is back"
    );
    assert_eq!(h.view(), &signing_out(Cleanup::Running, false));
    h.release();
    assert_eq!(h.view(), &signed_out());
    h.op(AccountOp::SignIn);
    assert_eq!(h.saved_clients, [Some("client-saved".to_owned())]);
    h.callback("client-saved");
    assert_eq!(
        h.account(),
        Some(registration()),
        "nothing is registered anew"
    );
    h.reply(received(Some("refresh-new"), "subject-1", PLAN));
    assert_eq!(h.view(), &signed_in(PlanState::Enabled, None));
    let account = h.account().expect("signed in");
    assert_eq!(
        (
            account.client_id.as_str(),
            account.subject.as_deref().map(String::as_str)
        ),
        ("client-saved", Some("subject-1"))
    );
}

/// CAPS-130 87-R1: a load Retry dispatched before a sign-out was admitted can't undo it, whether
/// it reads the marker or is refused: the sign-out keeps its fence and notice, and finishes.
#[test]
fn a_load_dispatched_before_the_sign_out_leaves_it_fenced() {
    for refused_marker in [false, true] {
        let seeded = prepared(|vault, _| {
            account_only(vault);
            vault.host_id().expect("host");
            committed(&vault.save_revocations(&Secret::new(RevocationList::default())));
        });
        seeded.store.0.lock().expect("fake lock").denied_read = Some(current_chunk(
            &seeded.authority(),
            &seeded.service,
            "revoke",
        ));
        let mut h = boot(&seeded);
        assert_eq!(
            h.views,
            [signed_in(PlanState::Enabled, None)],
            "{refused_marker}"
        );
        allow_reads(&h);
        if refused_marker {
            deny_read(&h, "signout");
        }
        h.hold = true;
        h.op(AccountOp::Retry);
        assert!(
            matches!(h.held, Some(Job::Load { .. })),
            "{refused_marker}: Retry's load is held"
        );
        let shown = h.views.len();
        h.op(AccountOp::SignOut);
        assert_eq!(
            h.view(),
            &signing_out(Cleanup::Running, true),
            "{refused_marker}"
        );
        h.release();
        allow_reads(&h);
        finish(&mut h, true, 0);
        let (cleanup, last) = h.views[shown..].split_at(h.views.len() - shown - 1);
        assert!(
            cleanup
                .iter()
                .all(|view| matches!(view, AccountView::SigningOut { .. })),
            "{refused_marker}: {cleanup:?}"
        );
        assert_eq!(last, [signed_out()], "{refused_marker}");
        assert_eq!(
            tokens(&h.revoking[0]),
            [("refresh-old".to_owned(), Origin::Account)],
            "{refused_marker}: the saved token is revoked"
        );
        assert_eq!(names(&h), left(true), "{refused_marker}");
    }
}

/// OSC-117: a lock error from the load after Finish leaves the sign-out refused, so Retry runs
/// that load again instead of the view staying Running with nothing to retry.
#[test]
fn a_lock_error_on_the_final_load_is_refused_for_retry() {
    let mut h = Harness::seeded("sign-out-final-lock", account_only);
    h.op(AccountOp::SignOut);
    h.hold = true;
    h.feed(Input::Revoked(h.revoking[0].clone()));
    for _ in 0..2 {
        h.hold = true;
        h.release();
    }
    assert!(matches!(h.held.take(), Some(Job::Load { .. })));
    h.feed(Input::Vault(Done::Loaded(Err(VaultError::Lock(
        std::io::Error::other("lock"),
    )))));
    assert_eq!(h.view(), &signing_out(Cleanup::Refused, false));
    h.op(AccountOp::Retry);
    assert_eq!(h.view(), &signed_out());
    assert_eq!(h.account(), Some(registration()));
}
