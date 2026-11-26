//! Forget: giving up a saved sign-in this computer can't read, as in a profile copied from
//! another one, only at the revision its view offered, and through a `Discard` sign-out.

use super::*;
use crate::connections::records::RevocationList;
use crate::platform::credential::SecretBytes;

/// A host, an account and a list holding one entry.
fn listed_account(vault: &mut Vault<FakeStore>) {
    vault.host_id().expect("host");
    committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    committed(&vault.save_revocations(&Secret::new(RevocationList {
        entries: vec![listed()],
    })));
}

/// A profile copied from another computer: its host, account and list name chunks this one
/// doesn't hold. Returns the harness and the store's bytes before the copy lost them.
fn copied(name: &str) -> (Harness, BTreeMap<String, SecretBytes>) {
    let h = Harness::seeded(name, listed_account);
    let saved = h.store.snapshot();
    let names = h.store.names();
    h.store.rollback(&BTreeMap::new(), &names);
    (h.restart(), saved)
}

fn listed() -> Revocable {
    Revocable {
        client_id: Zeroizing::new("client-listed".to_owned()),
        refresh_token: Zeroizing::new("refresh-listed".to_owned()),
        added_at: NOW,
        origin: Origin::Exchange,
    }
}

fn unavailable(reason: SavedSignInFailure, forget: Option<u64>) -> AccountView {
    AccountView::SavedSignInUnavailable {
        reason,
        checkable: reason != SavedSignInFailure::Changed,
        forget,
        checking: false,
        revocation_unconfirmed: false,
    }
}

/// Whether `record` still has a committed head.
fn committed_head(h: &Harness, record: &str) -> bool {
    h.authority()
        .query_row(
            "SELECT count(*) FROM generations WHERE kind = ?1 AND state = 'active'",
            [record],
            |row| row.get::<_, i64>(0),
        )
        .expect("active generations")
        == 1
}

#[test]
fn forget_gives_up_a_copied_sign_in_and_an_ordinary_sign_in_follows() {
    let (mut h, _) = copied("forget-copied");
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Missing, Some(1)));
    let revoked = h.revoking.len();
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert_eq!(
        h.revoking.len(),
        revoked,
        "nothing here is readable to revoke"
    );
    assert_eq!(
        h.view(),
        &AccountView::SignedOut {
            revocation_unconfirmed: true
        }
    );
    assert!(!committed_head(&h, "chatgpt"), "the account is gone");
    assert!(!committed_head(&h, "revoke"), "the list is gone");
    h.op(AccountOp::SignIn);
    assert_eq!(h.listening.len(), 1, "an ordinary sign-in starts");
    assert_eq!(
        h.saved_clients,
        [None],
        "with no client from the other computer"
    );
}

/// Forget writes nothing unless the offered revision is still the one whose chunk is missing.
#[test]
fn forget_writes_nothing_for_any_other_saved_sign_in() {
    let (mut h, _) = copied("forget-other-revision");
    let before = h.marker();
    h.op(AccountOp::SignIn);
    h.op(AccountOp::ForgetSavedSignIn(2));
    assert_eq!(
        h.marker(),
        before,
        "neither Sign in nor another revision discards"
    );
    assert!(h.listening.is_empty());
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Missing, Some(1)));

    let (mut h, saved) = copied("forget-readable-again");
    let names: Vec<_> = saved.keys().cloned().collect();
    h.store.rollback(&saved, &names);
    let before = h.marker();
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert_eq!(
        h.marker(),
        before,
        "a sign-in that reads again is recovered"
    );
    assert!(
        matches!(h.view(), AccountView::SignedIn { .. }),
        "{:?}",
        h.view()
    );

    let (mut h, saved) = copied("forget-refused-read");
    let names: Vec<_> = saved.keys().cloned().collect();
    h.store.rollback(&saved, &names);
    let chunk = h.current_chunk("chatgpt");
    h.store.0.lock().expect("fake lock").denied_read = Some(chunk);
    let before = h.marker();
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert_eq!(h.marker(), before, "a refused read proves nothing");
    assert_eq!(
        h.view(),
        &unavailable(SavedSignInFailure::Unavailable, None)
    );

    let (mut h, _) = copied("forget-changed");
    h.authority()
        .execute(
            "UPDATE heads SET revision = revision + 1 WHERE kind = 'chatgpt'",
            [],
        )
        .expect("simulate head change");
    let before = h.marker();
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert_eq!(
        h.marker(),
        before,
        "a changed sign-in is re-checked, never discarded"
    );
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Changed, None));
}

/// A refused step stays refused, and a restart resumes the durable discard to its end.
#[test]
fn a_refused_discard_step_resumes_after_a_restart() {
    let (mut h, _) = copied("forget-restart");
    h.refuse_marker_writes(1);
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert!(
        matches!(
            h.view(),
            AccountView::SigningOut {
                cleanup: Cleanup::Refused,
                ..
            }
        ),
        "{:?}",
        h.view()
    );
    h.allow_marker_writes();
    let mut h = h.restart();
    assert_eq!(
        h.view(),
        &AccountView::SignedOut {
            revocation_unconfirmed: true
        }
    );
    assert!(!committed_head(&h, "chatgpt"));
    h.op(AccountOp::SignIn);
    assert_eq!(h.saved_clients, [None]);
}

/// An ordinary sign-out still refuses chunks it can't find: only a chosen discard gives them up.
#[test]
fn an_ordinary_sign_out_still_refuses_a_missing_list_chunk() {
    let h = Harness::seeded("forget-ordinary", listed_account);
    let prefix = h.generations("revoke");
    let list: Vec<_> = h
        .store
        .names()
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();
    assert!(!list.is_empty(), "the list had chunks to lose");
    h.store.rollback(&BTreeMap::new(), &list);
    let mut h = h.restart();
    h.op(AccountOp::SignOut);
    assert!(
        matches!(
            h.view(),
            AccountView::SigningOut {
                cleanup: Cleanup::Refused,
                ..
            }
        ),
        "{:?}",
        h.view()
    );
    assert!(committed_head(&h, "chatgpt"), "nothing was deleted");
}

/// Forget still revokes the list this computer can read, and its checkpoint turns the discard
/// into an ordinary erase, so a restart or an older reader finishes it as one.
#[test]
fn forget_revokes_a_readable_list_and_checkpoints_as_erase() {
    let h = Harness::seeded("forget-listed", listed_account);
    let prefix = h.generations("chatgpt");
    let account: Vec<_> = h
        .store
        .names()
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();
    h.store.rollback(&BTreeMap::new(), &account);
    let mut h = h.restart();
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Missing, Some(1)));
    h.store.0.lock().expect("fake lock").refuse_deletes = true;
    h.op(AccountOp::ForgetSavedSignIn(1));
    let [run] = h.revoking.as_slice() else {
        panic!("one revocation run, not {}", h.revoking.len());
    };
    let tokens: Vec<_> = run
        .iter()
        .map(|entry| entry.expose().refresh_token.to_string())
        .collect();
    assert_eq!(tokens, ["refresh-listed"]);
    let run = run.clone();
    h.feed(Input::Revoked(run));
    let written: Vec<&String> = h.markers.iter().flatten().collect();
    assert!(
        written
            .iter()
            .any(|marker| marker.contains(r#""disposition":"Discard","phase":"Revoking""#)),
        "{written:?}"
    );
    let deleting: Vec<_> = written
        .iter()
        .filter(|marker| marker.contains(r#""phase":"Deleting""#))
        .collect();
    assert!(!deleting.is_empty(), "{written:?}");
    assert!(
        deleting
            .iter()
            .all(|marker| marker.contains(r#""disposition":"Erase""#)),
        "{deleting:?}"
    );
    h.store.0.lock().expect("fake lock").refuse_deletes = false;
    let mut h = h.restart();
    assert!(
        matches!(h.view(), AccountView::SignedOut { .. }),
        "{:?}",
        h.view()
    );
    assert!(!committed_head(&h, "chatgpt"));
    assert!(!committed_head(&h, "revoke"));
    h.op(AccountOp::SignIn);
    assert_eq!(h.saved_clients, [None]);
}

/// A chunk of the wrong length is malformed, not missing: Forget isn't offered and writes nothing.
#[test]
fn forget_is_not_offered_for_a_wrong_length_chunk() {
    let h = Harness::seeded("forget-wrong-length", |vault| {
        committed(&vault.save_account(&saved_account("subject-1", "refresh-old")));
    });
    let chunk = h.current_chunk("chatgpt");
    let mut bytes = h.store.snapshot()[&chunk].expose().to_vec();
    bytes.push(0);
    let longer = BTreeMap::from([(chunk.clone(), Secret::new(Zeroizing::new(bytes)))]);
    h.store.rollback(&longer, &[chunk]);
    let mut h = h.restart();
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Malformed, None));
    h.op(AccountOp::ForgetSavedSignIn(1));
    assert_eq!(h.marker(), None);
    assert_eq!(h.view(), &unavailable(SavedSignInFailure::Malformed, None));
}
