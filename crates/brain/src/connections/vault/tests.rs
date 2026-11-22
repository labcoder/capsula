use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use kumochi_protocol::constants::{INSTANCE_SERVICE, SESSION_LOCK, generation_target};
use rusqlite::Connection;

use super::fake::{FakeStore, Fault, Mutation};
use super::*;
use crate::connections::marker::SignOut;
use crate::connections::records::{Origin, Revocable, Tokens};
use crate::platform::credential::CredentialStoreError;
use crate::test_support::Scratch;

mod anthropic;
mod bounds;
mod github;
mod inventory;
mod marker;
mod staging;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    Account,
    Registration,
    Revocations,
}

fn setup(name: &str) -> (Scratch, PathBuf, String, FakeStore) {
    let scratch = Scratch::new(name);
    let path = scratch.db().with_file_name(SESSION_LOCK);
    let service = format!(
        "{CREDENTIAL_SERVICE}-vault-test-{:016x}",
        getrandom::u64().expect("test randomness")
    );
    (scratch, path, service, FakeStore::default())
}

fn open(store: &FakeStore, service: &str, path: &Path) -> Vault<FakeStore> {
    Vault::with_service(store.clone(), service, path).expect("test vault lock")
}

/// The vault's authority file, as another process would see it.
fn raw(path: &Path) -> Connection {
    Connection::open(path.with_file_name(authority::FILE)).expect("raw authority connection")
}

/// A copy of a closed vault's authority file in a new folder, as a crash test's restart point.
fn fork_dir(path: &Path, name: &str) -> (Scratch, PathBuf) {
    let scratch = Scratch::new(name);
    let copy = scratch.db().with_file_name(SESSION_LOCK);
    std::fs::create_dir_all(copy.parent().expect("scratch folder")).expect("scratch folder");
    std::fs::copy(
        path.with_file_name(authority::FILE),
        copy.with_file_name(authority::FILE),
    )
    .expect("copy authority");
    (scratch, copy)
}

const TABLES: [(&str, &str); 4] = [
    ("heads", "UPDATE"),
    ("control", "UPDATE"),
    ("generations", "UPDATE"),
    ("generations", "INSERT"),
];

/// Makes the authority refuse every commit at once, as a failing disk would, until allowed.
fn refuse_commits(path: &Path) {
    let conn = raw(path);
    for (index, (table, operation)) in TABLES.iter().enumerate() {
        conn.execute_batch(&format!(
            "CREATE TRIGGER refuse_{index} BEFORE {operation} ON {table} \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;"
        ))
        .expect("refusal trigger");
    }
}

fn allow_commits(path: &Path) {
    let conn = raw(path);
    for index in 0..TABLES.len() {
        conn.execute_batch(&format!("DROP TRIGGER refuse_{index};"))
            .expect("drop refusal trigger");
    }
}

fn account(label: &str) -> Secret<Account> {
    Secret::new(Account {
        client_id: Zeroizing::new(format!("client-{label}")),
        subject: Some(Zeroizing::new(format!("subject-{label}"))),
        email: Some(Zeroizing::new(format!("{label}@example.invalid"))),
        plan_enabled: true,
        tokens: Some(Tokens {
            access_token: Zeroizing::new(label.repeat(2_000)),
            refresh_token: Zeroizing::new(format!("refresh-{label}")),
            id_token: Some(Zeroizing::new(format!("identity-{label}"))),
            scope: Zeroizing::new("model.request".to_owned()),
            expires_at: 1_900_000_000,
            pending_identity_validation: true,
            refresh_sent: false,
            connection: 0,
        }),
    })
}

fn registration(account: &Secret<Account>) -> Secret<Account> {
    let mut record = account.expose().clone();
    record.tokens = None;
    record.email = None;
    record.plan_enabled = false;
    Secret::new(record)
}

fn list(label: &str) -> Secret<RevocationList> {
    Secret::new(RevocationList {
        entries: vec![Revocable {
            client_id: Zeroizing::new(format!("client-{label}")),
            refresh_token: Zeroizing::new(label.repeat(2_000)),
            added_at: 1_900_000_000,
            origin: Origin::Exchange,
        }],
    })
}

fn committed(result: SaveResult) {
    match result {
        SaveResult::Committed => {}
        SaveResult::NotCommitted(error) => panic!("expected a commit: {error:?}"),
    }
}

fn gone(result: DeleteResult) {
    match result {
        DeleteResult::Gone => {}
        DeleteResult::Refused(error) => panic!("expected the record removed: {error:?}"),
    }
}

fn is_current(vault: &Vault<FakeStore>, kind: Kind) -> bool {
    vault
        .authority
        .current(kind)
        .expect("authority head")
        .active
        .is_some()
}

/// The exact names of every record's current generation, sorted.
fn current_names(vault: &Vault<FakeStore>) -> Vec<String> {
    let mut names = Vec::new();
    for kind in [
        Kind::Host,
        Kind::Account,
        Kind::Revocations,
        Kind::AnthropicKey,
    ] {
        if let Some(active) = vault
            .authority
            .current(kind)
            .expect("authority head")
            .active
        {
            names.extend((0..active.layout.chunks()).map(|index| {
                generation_target(
                    &vault.service,
                    kind.label(),
                    &active.generation.hex(),
                    index,
                )
            }));
        }
    }
    names.sort();
    names
}

/// Every removed generation was swept: the store holds current generations and nothing else.
fn assert_only_current(vault: &Vault<FakeStore>, store: &FakeStore) {
    assert_eq!(
        store.names(),
        current_names(vault),
        "only current names remain"
    );
}

#[test]
fn records_and_host_round_trip_and_a_second_client_is_busy() {
    let (_scratch, path, service, store) = setup("vault-round-trip");
    let mut vault = open(&store, &service, &path);
    assert!(
        matches!(
            Vault::with_service(store.clone(), &service, &path),
            Err(VaultError::Busy)
        ),
        "second vault is Busy"
    );
    let host = vault.host_id().expect("host id");
    let uuid = host
        .expose()
        .strip_prefix("urn:uuid:")
        .expect("UUID URN prefix");
    let groups: Vec<_> = uuid.split('-').collect();
    assert_eq!(
        groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
        [8, 4, 4, 4, 12]
    );
    assert!(
        groups.iter().all(|group| group
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())),
        "canonical lowercase hexadecimal"
    );
    assert!(
        u16::from_str_radix(groups[2], 16).expect("version group") & 0xf000 == 0x4000,
        "version four"
    );
    assert!(
        u16::from_str_radix(groups[3], 16).expect("variant group") & 0xc000 == 0x8000,
        "RFC UUID variant"
    );
    committed(vault.save_account(&account("old")));
    committed(vault.save_revocations(&list("one")));
    drop(vault);
    let mut vault = open(&store, &service, &path);
    assert!(
        vault.host_id().expect("same host").expose() == host.expose(),
        "host persists"
    );
    assert!(
        vault
            .load_account()
            .expect("account read")
            .expect("account exists")
            .expose()
            == account("old").expose(),
        "account round trip"
    );
    assert!(
        vault
            .load_revocations()
            .expect("list read")
            .expect("list exists")
            .expose()
            == list("one").expose(),
        "list round trip"
    );
    assert!(
        store
            .names()
            .iter()
            .all(|name| name.starts_with(&format!("{service}/gen/"))),
        "every name is a write-once generation name"
    );
    assert_only_current(&vault, &store);
}

/// A current host whose bytes went missing stays an error and is never replaced (L2).
#[test]
fn a_host_that_stops_reading_back_is_never_replaced() {
    let (_scratch, path, service, store) = setup("vault-host-missing");
    let mut vault = open(&store, &service, &path);
    let host = vault.host_id().expect("host");
    let saved = store.snapshot();
    store.rollback(&BTreeMap::new(), &store.names());
    assert!(matches!(
        vault.host_id(),
        Err(VaultError::Store(CredentialStoreError::Unsettled))
    ));
    assert!(store.names().is_empty(), "no new host was written");
    store.rollback(&saved, &saved.keys().cloned().collect::<Vec<_>>());
    assert!(
        vault.host_id().expect("host again").expose() == host.expose(),
        "the host id survives a reopen"
    );
}

#[test]
fn failure_and_crash_at_each_save_mutation_preserve_the_commit_boundary() {
    exercise_save_faults(Record::Account);
    exercise_save_faults(Record::Registration);
    exercise_save_faults(Record::Revocations);
}

fn exercise_save_faults(record: Record) {
    let (_scratch, path, service, seed) = setup("vault-save-faults");
    {
        let mut vault = open(&seed, &service, &path);
        if record == Record::Revocations {
            committed(vault.save_revocations(&list("old")));
        } else {
            committed(vault.save_account(&account("old")));
        }
        if record == Record::Registration {
            vault
                .update_marker(MarkerUpdate::Promote(Disposition::Keep))
                .expect("promote");
            vault
                .update_marker(MarkerUpdate::Checkpoint {
                    list_empty_on_disk: false,
                })
                .expect("checkpoint");
        }
    }
    let replacement = if record == Record::Registration {
        registration(&account("old"))
    } else {
        account("new")
    };
    let save = |vault: &mut Vault<FakeStore>| {
        if record == Record::Revocations {
            vault.save_revocations(&list("new"))
        } else {
            vault.save_account(&replacement)
        }
    };
    let trace = {
        let baseline = seed.fork();
        let (_copy, copy) = fork_dir(&path, "vault-save-baseline");
        let mut vault = open(&baseline, &service, &copy);
        baseline.arm(usize::MAX, Fault::Refuse);
        committed(save(&mut vault));
        baseline.0.lock().expect("fake lock").mutations.clone()
    };
    let commit_at = trace
        .iter()
        .position(|mutation| matches!(mutation, Mutation::Delete(_)))
        .expect("the old generation is swept after the commit");
    assert!(commit_at > 0, "the new generation is written first");
    for index in 0..trace.len() {
        for fault in [Fault::Refuse, Fault::CrashAfter] {
            let store = seed.fork();
            let (_copy, copy) = fork_dir(&path, "vault-save-fault");
            let mut vault = open(&store, &service, &copy);
            store.arm(index, fault);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| save(&mut vault)));
            let changed = index >= commit_at;
            match (fault, outcome) {
                (Fault::Refuse, Ok(SaveResult::Committed)) => {
                    assert!(changed, "a refused write never commits");
                }
                (
                    Fault::Refuse,
                    Ok(SaveResult::NotCommitted(VaultError::Store(CredentialStoreError::Write(5)))),
                ) => assert!(!changed, "a refused sweep isn't a failed save"),
                (Fault::CrashAfter, Err(_)) => {}
                (fault, outcome) => panic!("unexpected {fault:?} outcome: {outcome:?}"),
            }
            drop(vault);
            let mut restarted = open(&store, &service, &copy);
            if record == Record::Revocations {
                let expected = if changed { list("new") } else { list("old") };
                assert!(
                    restarted
                        .load_revocations()
                        .expect("list after the fault")
                        .expect("a list")
                        .expose()
                        == expected.expose(),
                    "exactly the committed list"
                );
            } else {
                let expected = if changed {
                    replacement.clone()
                } else {
                    account("old")
                };
                assert!(
                    restarted
                        .read::<Account>(Kind::Account)
                        .expect("account after the fault")
                        .expect("an account")
                        .expose()
                        == expected.expose(),
                    "exactly the committed account"
                );
            }
            committed(save(&mut restarted));
            assert_only_current(&restarted, &store);
        }
    }
}

fn assert_key_removal_only_clears_its_names(
    vault: &mut Vault<FakeStore>,
    store: &FakeStore,
    previous_names: &[String],
) {
    use crate::connections::anthropic::KeyOp;

    let revision = vault.anthropic_revision();
    assert!(
        vault
            .anthropic(KeyOp::Remove(revision))
            .1
            .expect("retry removal")
            .is_none()
    );
    assert!(
        vault
            .anthropic(KeyOp::Load)
            .1
            .expect("read removed key")
            .is_none()
    );
    assert_eq!(
        store.names(),
        previous_names
            .iter()
            .filter(|name| !name.contains("/gen/anthropic/"))
            .cloned()
            .collect::<Vec<_>>(),
        "the retry swept only the removed key's names"
    );
}

/// CAPS-179 M2: begin and every write land, then the head update inside the commit is
/// refused. Nothing changes, even after the removal's generations retired in the same
/// transaction, and a retry commits a fresh generation.
#[test]
fn a_refused_head_update_rolls_back_the_whole_commit() {
    use crate::connections::anthropic::{KeyError, KeyOp};

    let (_scratch, path, service, store) = setup("vault-late-refusal");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    let key = b"sk-ant-api-fake-late-refusal";
    vault.save_anthropic_key(key).expect("key");
    let retained_key_is_exact = |vault: &mut Vault<FakeStore>| {
        vault
            .anthropic(KeyOp::Load)
            .1
            .expect("read")
            .expect("saved key")
            .expose()
            .as_str()
            .as_bytes()
            == key
    };
    let old = current_names(&vault);
    let marker = vault.marker().expect("marker");
    let refuse = raw(&path);
    refuse
        .execute_batch(
            "CREATE TRIGGER refuse_head BEFORE UPDATE ON heads \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .expect("refusal trigger");
    let before = store.0.lock().expect("fake lock").mutations.len();
    let result = vault.save_account(&account("new"));
    assert!(
        matches!(result, SaveResult::NotCommitted(VaultError::Authority)),
        "{result:?}"
    );
    let attempted: Vec<String> = store.0.lock().expect("fake lock").mutations[before..]
        .iter()
        .filter_map(|mutation| match mutation {
            Mutation::Set(name) => Some(name.clone()),
            Mutation::Delete(_) => None,
        })
        .collect();
    assert!(
        !attempted.is_empty(),
        "every write landed before the commit"
    );
    assert!(matches!(
        vault.anthropic(KeyOp::Remove(vault.anthropic_revision())).1,
        Err(KeyError::Vault(VaultError::Authority))
    ));
    assert!(retained_key_is_exact(&mut vault));
    refuse
        .execute_batch("DROP TRIGGER refuse_head;")
        .expect("drop refusal trigger");
    drop(vault);
    let mut vault = open(&store, &service, &path);
    assert_eq!(current_names(&vault), old, "the head is unchanged");
    assert_eq!(vault.marker().expect("marker"), marker, "and the marker");
    assert_eq!(
        vault
            .load_account()
            .expect("read")
            .expect("the old account")
            .expose()
            .email
            .as_deref()
            .map(String::as_str),
        Some("old@example.invalid")
    );
    assert!(
        is_current(&vault, Kind::AnthropicKey),
        "the removal rolled back"
    );
    assert!(retained_key_is_exact(&mut vault));
    assert_key_removal_only_clears_its_names(&mut vault, &store, &old);
    committed(vault.save_account(&account("new")));
    let fresh = current_names(&vault);
    assert!(
        fresh
            .iter()
            .filter(|name| name.contains("/gen/chatgpt/"))
            .all(|name| !attempted.contains(name) && !old.contains(name)),
        "the retry commits a fresh generation"
    );
    assert_eq!(
        vault
            .load_account()
            .expect("read")
            .expect("the new account")
            .expose()
            .email
            .as_deref()
            .map(String::as_str),
        Some("new@example.invalid")
    );
}

#[test]
fn an_interrupted_first_write_leaves_orphans_that_the_next_open_sweeps() {
    let (_scratch, path, service, store) = setup("vault-first-write");
    let mut vault = open(&store, &service, &path);
    store.arm(0, Fault::CrashAfter);
    assert!(
        panic::catch_unwind(AssertUnwindSafe(|| vault.save_account(&account("old")))).is_err(),
        "crash after the first chunk"
    );
    assert_eq!(store.names().len(), 1, "the orphan really exists");
    assert!(!is_current(&vault, Kind::Account), "no commit yet");
    drop(vault);
    let mut restarted = open(&store, &service, &path);
    assert!(store.names().is_empty(), "the reopen swept the orphan");
    committed(restarted.save_account(&registration(&account("new"))));
    assert_only_current(&restarted, &store);
}

#[test]
fn every_sweep_refusal_or_crash_after_a_removal_is_retryable_without_losing_the_marker_or_instance()
{
    let (_scratch, path, service, seed) = setup("vault-delete-faults");
    let instance = format!(
        "{INSTANCE_SERVICE}/test-{:016x}",
        getrandom::u64().expect("test randomness")
    );
    let capability = Secret::new(Zeroizing::new(b"test-only-capability".to_vec()));
    seed.set(&instance, &capability).expect("fake instance");
    {
        let mut vault = open(&seed, &service, &path);
        committed(vault.save_account(&account("old")));
        committed(vault.save_revocations(&list("one")));
        vault
            .update_marker(MarkerUpdate::Promote(Disposition::Erase))
            .expect("promote");
        vault
            .update_marker(MarkerUpdate::Checkpoint {
                list_empty_on_disk: false,
            })
            .expect("checkpoint");
    }
    for record in [Kind::Account, Kind::Revocations] {
        let delete = |vault: &mut Vault<FakeStore>| match record {
            Kind::Account => vault.delete_account(),
            _ => vault.delete_revocations(),
        };
        let count = {
            let dry = seed.fork();
            let (_copy, copy) = fork_dir(&path, "vault-delete-dry");
            let mut vault = open(&dry, &service, &copy);
            dry.arm(usize::MAX, Fault::Refuse);
            gone(delete(&mut vault));
            dry.0.lock().expect("fake lock").mutations.len()
        };
        assert!(count > 0, "the removal's names are swept");
        for index in 0..count {
            for fault in [Fault::Refuse, Fault::CrashAfter] {
                let store = seed.fork();
                let (_copy, copy) = fork_dir(&path, "vault-delete-fault");
                let mut vault = open(&store, &service, &copy);
                store.arm(index, fault);
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| delete(&mut vault)));
                match fault {
                    Fault::Refuse => gone(outcome.expect("a refused sweep returns")),
                    Fault::CrashAfter => {}
                }
                drop(vault);
                let mut restarted = open(&store, &service, &copy);
                assert!(!is_current(&restarted, record), "the removal committed");
                let marker = restarted.marker().expect("marker survives");
                assert!(
                    marker
                        .sign_out
                        .is_some_and(|pending| pending.phase == Phase::Deleting),
                    "deletion remains pending"
                );
                assert!(marker.notice, "remote warning remains");
                gone(restarted.delete_account());
                gone(restarted.delete_revocations());
                restarted
                    .update_marker(MarkerUpdate::FinishSignOut)
                    .expect("finish after the removals");
                drop(restarted);
                let restarted = open(&store, &service, &copy);
                assert!(
                    store
                        .get(&instance)
                        .expect("fake instance read")
                        .expect("instance survives")
                        .expose()
                        == capability.expose(),
                    "instance capability is untouched"
                );
                assert_eq!(
                    store.names(),
                    vec![instance.clone()],
                    "only the instance remains once the sweep catches up"
                );
                assert!(restarted.marker().expect("notice").notice);
            }
        }
    }
}

#[test]
fn outside_sign_out_only_an_empty_revocation_list_may_be_deleted() {
    let (_scratch, path, service, store) = setup("revoke-delete-guard");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_revocations(&list("one")));
    let names = store.names();
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Fenced)
        ),
        "unconfirmed entries cannot be discarded by a sweep"
    );
    assert_eq!(store.names(), names, "a refusal leaves every name intact");
    assert!(is_current(&vault, Kind::Revocations));
    committed(vault.save_revocations(&Secret::new(RevocationList::default())));
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::FinishSweep),
            Err(VaultError::NotGone)
        ),
        "an empty record is still current until it's removed"
    );
    gone(vault.delete_revocations());
    vault
        .update_marker(MarkerUpdate::FinishSweep)
        .expect("sweep finishes");
    assert!(
        store.names().is_empty(),
        "the empty list's names were swept"
    );
    assert_eq!(vault.marker().expect("marker"), SignOutMarker::default());
}

/// L6: a read of the current generation makes one exact pass, so a miss is Unsettled, never
/// a signed-out account, while a fresh save waits its own misses out (M2).
#[test]
fn a_missed_read_of_the_current_generation_is_unsettled_and_never_absent() {
    let (_scratch, path, service, store) = setup("missed-read");
    let mut vault = open(&store, &service, &path);
    let saved = account("one");
    store.0.lock().expect("fake lock").missed_reads = 3;
    committed(vault.save_account(&saved));
    assert_eq!(store.0.lock().expect("fake lock").missed_reads, 0);
    store.0.lock().expect("fake lock").missed_reads = 1;
    assert!(
        matches!(
            vault.load_account(),
            Err(VaultError::Store(CredentialStoreError::Unsettled))
        ),
        "a miss is an error, not a signed-out account"
    );
    assert!(
        vault
            .load_account()
            .expect("read")
            .expect("the account")
            .expose()
            == saved.expose(),
        "the saved account reads back"
    );
}

/// L6: a delete that didn't stick can't bring a removed record back; a later sweep retries it.
#[test]
fn a_delete_that_does_not_stick_never_brings_the_record_back() {
    let (_scratch, path, service, store) = setup("sticky-delete");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_revocations(&Secret::new(RevocationList::default())));
    store.0.lock().expect("fake lock").sticky_deletes = 1;
    gone(vault.delete_revocations());
    assert_eq!(store.names().len(), 1, "the name outlived its delete");
    assert!(
        vault.load_revocations().expect("read").is_none(),
        "the removal is final"
    );
    drop(vault);
    let vault = open(&store, &service, &path);
    assert!(store.names().is_empty(), "the next open's sweep deleted it");
    assert!(vault.load_revocations().expect("read").is_none());
}

/// OSC-233: Credential Manager served a confirmed delete again; a removal stays final.
#[test]
fn resurrected_bytes_of_a_removed_record_are_never_read() {
    let (_scratch, path, service, store) = setup("vault-resurrect");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    let before = store.snapshot();
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promote");
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("checkpoint");
    gone(vault.delete_for_sign_out());
    assert!(store.names().is_empty(), "swept");
    store.rollback(&before, &before.keys().cloned().collect::<Vec<_>>());
    assert!(vault.load_for_sign_out().expect("read").is_none());
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("finish over resurrected names");
    assert!(vault.load_account().expect("read").is_none());
}

#[test]
fn sign_out_reader_exposes_the_fenced_record_only_while_a_sign_out_is_pending() {
    for (disposition, name) in [
        (Disposition::Keep, "sign-out-reader"),
        (Disposition::Erase, "marker-fence"),
    ] {
        let (_scratch, path, service, store) = setup(name);
        let mut vault = open(&store, &service, &path);
        let expected = account("old");
        committed(vault.save_account(&expected));
        assert!(
            matches!(vault.load_for_sign_out(), Err(VaultError::Fenced)),
            "ordinary sessions cannot use the sign-out reader"
        );
        vault
            .update_marker(MarkerUpdate::BeginSweep)
            .expect("sweep-only state");
        assert!(
            matches!(vault.load_for_sign_out(), Err(VaultError::Fenced)),
            "a sweep alone grants no sign-out access"
        );
        let loaded = vault
            .load_account()
            .expect("sweep-only read")
            .expect("account");
        assert!(
            loaded.expose() == expected.expose(),
            "a sweep alone never fences"
        );
        vault
            .update_marker(MarkerUpdate::Promote(disposition))
            .expect("promotion");
        drop(vault);
        let vault = open(&store, &service, &path);
        assert_only_current(&vault, &store);
        assert!(is_current(&vault, Kind::Account));
        let fenced = vault
            .load_for_sign_out()
            .expect("Prepare reader")
            .expect("account is present");
        assert!(
            fenced.expose().client_id == expected.expose().client_id,
            "Prepare receives the owning client"
        );
        assert!(
            fenced.expose().tokens == expected.expose().tokens,
            "Prepare receives the original token set"
        );
        assert!(
            vault
                .marker()
                .expect("marker")
                .sign_out
                .is_some_and(|sign_out| sign_out.phase == Phase::Revoking),
            "the preparation phase remains unchanged"
        );
        assert!(
            vault.load_account().expect("ordinary reader").is_none(),
            "ordinary account use remains fenced"
        );
    }
}

#[test]
fn unreadable_revocations_need_persisted_uncertainty_and_os_read_errors_stay_errors() {
    let (_scratch, path, service, store) = setup("revoke-unreadable-delete");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_revocations(&list("one")));
    let first = current_names(&vault)[0].clone();
    let mut corrupt = store.snapshot();
    let length = corrupt[&first].expose().len();
    corrupt.insert(
        first.clone(),
        Secret::new(Zeroizing::new(vec![b'{'; length])),
    );
    store.rollback(&corrupt, std::slice::from_ref(&first));
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Unreadable)
        ),
        "unreadable data cannot be discarded without recording uncertainty"
    );
    assert!(
        is_current(&vault, Kind::Revocations),
        "refused cleanup removes nothing"
    );
    vault
        .update_marker(MarkerUpdate::UnknownOutcome)
        .expect("persist uncertainty before deletion");
    drop(vault);
    let mut vault = open(&store, &service, &path);
    store.0.lock().expect("fake lock").denied_read = Some(first);
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Store(CredentialStoreError::Read(5)))
        ),
        "an OS read refusal remains a typed error"
    );
    assert!(
        is_current(&vault, Kind::Revocations),
        "an OS refusal removes nothing"
    );
    store.0.lock().expect("fake lock").denied_read = None;
    gone(vault.delete_revocations());
    assert!(!is_current(&vault, Kind::Revocations));
    assert!(
        vault.marker().expect("marker").outcome_unknown,
        "cleanup cannot reset the persisted uncertainty"
    );
}

#[cfg(windows)]
#[test]
fn windows_vault_persists_only_under_its_random_test_service() {
    use crate::platform::credential::NativeCredentialStore;
    use crate::platform::credential::fixture::Fixture;

    let (_scratch, path, service, _) = setup("windows-vault");
    let mut vault =
        Vault::with_service(NativeCredentialStore, &service, &path).expect("test vault");
    let host = vault.host_id().expect("host");
    committed(vault.save_account(&account("old")));
    committed(vault.save_revocations(&list("one")));
    let names = {
        let mut names = Vec::new();
        for kind in [Kind::Host, Kind::Account, Kind::Revocations] {
            let active = vault
                .authority
                .current(kind)
                .expect("head")
                .active
                .expect("saved");
            names.extend((0..active.layout.chunks()).map(|index| {
                generation_target(&service, kind.label(), &active.generation.hex(), index)
            }));
        }
        names
    };
    let _fixture = Fixture::new(names);
    drop(vault);
    let mut vault =
        Vault::with_service(NativeCredentialStore, &service, &path).expect("reopen vault");
    assert!(
        vault.host_id().expect("persisted host").expose() == host.expose(),
        "same host"
    );
    assert!(
        vault
            .load_account()
            .expect("account")
            .expect("exists")
            .expose()
            == account("old").expose(),
        "persisted account"
    );
    assert!(
        vault
            .load_revocations()
            .expect("list")
            .expect("exists")
            .expose()
            == list("one").expose(),
        "persisted revocation record"
    );
}
