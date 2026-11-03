use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kumochi_protocol::constants::{INSTANCE_SERVICE, SESSION_LOCK};

use super::*;
use crate::connections::marker::SignOut;
use crate::connections::records::{Origin, Revocable, Tokens};
use crate::platform::credential::CredentialStoreError;
use crate::test_support::Scratch;

mod bounds;
mod marker;

#[derive(Debug, Clone, Copy)]
enum Fault {
    Refuse,
    CrashAfter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replacement {
    Tokens,
    Registration,
}

#[derive(Debug, Clone)]
enum Mutation {
    Set(String),
    Delete,
}

#[derive(Debug, Default)]
struct FakeState {
    values: BTreeMap<String, SecretBytes>,
    mutations: Vec<Mutation>,
    fault: Option<(usize, Fault)>,
    denied_read: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct FakeStore(Arc<Mutex<FakeState>>);

impl FakeStore {
    fn fork(&self) -> Self {
        let values = self.0.lock().expect("fake lock").values.clone();
        Self(Arc::new(Mutex::new(FakeState {
            values,
            ..FakeState::default()
        })))
    }

    fn names(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("fake lock")
            .values
            .keys()
            .cloned()
            .collect()
    }

    fn arm(&self, index: usize, fault: Fault) {
        let mut state = self.0.lock().expect("fake lock");
        state.mutations.clear();
        state.fault = Some((index, fault));
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "fault injection deliberately models process death after a durable mutation"
    )]
    fn mutate(
        &self,
        target: &str,
        value: Option<&SecretBytes>,
    ) -> Result<(), CredentialStoreError> {
        let fault = {
            let mut state = self.0.lock().expect("fake lock");
            let index = state.mutations.len();
            state
                .mutations
                .push(value.map_or(Mutation::Delete, |_| Mutation::Set(target.to_owned())));
            let fault = if state.fault.is_some_and(|(at, _)| at == index) {
                state.fault.take().map(|(_, fault)| fault)
            } else {
                None
            };
            if !matches!(fault, Some(Fault::Refuse)) {
                if let Some(value) = value {
                    state.values.insert(target.to_owned(), value.clone());
                } else {
                    state.values.remove(target);
                }
            }
            fault
        };
        match fault {
            Some(Fault::Refuse) => Err(if value.is_some() {
                CredentialStoreError::Write(5)
            } else {
                CredentialStoreError::Delete(5)
            }),
            Some(Fault::CrashAfter) => panic!("simulated process crash"),
            None => Ok(()),
        }
    }
}

impl CredentialStore for FakeStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        let state = self.0.lock().expect("fake lock");
        if state.denied_read.as_deref() == Some(target) {
            return Err(CredentialStoreError::Read(5));
        }
        Ok(state.values.get(target).cloned())
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.mutate(target, Some(secret))
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.mutate(target, None)
    }
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
        SaveResult::Committed { leftovers: false } => {}
        SaveResult::Committed { leftovers: true } => panic!("unexpected cleanup leftovers"),
        SaveResult::NotCommitted(error) => panic!("expected a commit: {error:?}"),
    }
}

fn gone(result: DeleteResult) {
    match result {
        DeleteResult::Gone => {}
        DeleteResult::Refused(error) => panic!("expected every name removed: {error:?}"),
    }
}

fn assert_record_absent(store: &FakeStore, service: &str, record: &str) {
    let prefix = format!("{service}/{record}");
    assert!(
        store
            .names()
            .iter()
            .all(|name| name != &prefix && !name.starts_with(&format!("{prefix}/"))),
        "no manifest or orphan chunk remains"
    );
}

fn assert_only_active_slot(vault: &Vault<FakeStore>, store: &FakeStore, record: Chunked) {
    let manifest = vault
        .manifest(record)
        .expect("manifest read")
        .expect("manifest exists");
    let names = store.names();
    for slot in 0..2 {
        for index in 0..MAX_CHUNKS {
            let name = format!("{}/{}/{slot}/{index}", vault.service, record.name());
            assert_eq!(
                names.contains(&name),
                slot == manifest.slot && index < manifest.chunks,
                "only active chunks remain"
            );
        }
    }
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
    assert!(valid_host(host.expose()), "host is a canonical UUIDv4 URN");
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
}

#[test]
fn failure_and_crash_at_each_save_mutation_preserve_the_commit_boundary() {
    exercise_save_faults(Chunked::Account, Replacement::Tokens);
    exercise_save_faults(Chunked::Account, Replacement::Registration);
    exercise_save_faults(Chunked::Revoke, Replacement::Tokens);
}

fn exercise_save_faults(record: Chunked, replacement: Replacement) {
    let (_scratch, path, service, seed) = setup("vault-save-faults");
    {
        let mut vault = open(&seed, &service, &path);
        match record {
            Chunked::Account => committed(vault.save_account(&account("old"))),
            Chunked::Revoke => committed(vault.save_revocations(&list("old"))),
        }
        if replacement == Replacement::Registration {
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
    let new_account = if replacement == Replacement::Registration {
        registration(&account("old"))
    } else {
        account("new")
    };
    let baseline = seed.fork();
    {
        let mut vault = open(&baseline, &service, &path);
        match record {
            Chunked::Account => committed(vault.save_account(&new_account)),
            Chunked::Revoke => committed(vault.save_revocations(&list("new"))),
        }
    }
    let trace = baseline.0.lock().expect("fake lock").mutations.clone();
    let commit_at = trace.iter().position(|op| matches!(op, Mutation::Set(name) if name == &format!("{service}/{}", record.name()))).expect("manifest commit in trace");
    for index in 0..trace.len() {
        for fault in [Fault::Refuse, Fault::CrashAfter] {
            let store = seed.fork();
            let mut vault = open(&store, &service, &path);
            store.arm(index, fault);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| match record {
                Chunked::Account => vault.save_account(&new_account),
                Chunked::Revoke => vault.save_revocations(&list("new")),
            }));
            let changed = match fault {
                Fault::Refuse => {
                    let result = outcome.expect("refusal returns, not crashes");
                    if index > commit_at {
                        assert!(
                            matches!(result, SaveResult::Committed { leftovers: true }),
                            "post-commit cleanup is not a failed save"
                        );
                    } else {
                        assert!(
                            matches!(result, SaveResult::NotCommitted(_)),
                            "pre-commit refusal does not commit"
                        );
                    }
                    index > commit_at
                }
                Fault::CrashAfter => {
                    assert!(outcome.is_err(), "crash point reached");
                    index >= commit_at
                }
            };
            drop(vault);
            let mut restarted = open(&store, &service, &path);
            match record {
                Chunked::Account => {
                    let loaded = restarted
                        .load::<Account>(record)
                        .expect("account after crash")
                        .expect("old or new account");
                    let expected = if changed {
                        new_account.clone()
                    } else {
                        account("old")
                    };
                    assert!(
                        loaded.expose() == expected.expose(),
                        "manifest selects exactly the committed account"
                    );
                    committed(restarted.save_account(&new_account));
                }
                Chunked::Revoke => {
                    let loaded = restarted
                        .load_revocations()
                        .expect("list after crash")
                        .expect("old or new list");
                    let expected = if changed { list("new") } else { list("old") };
                    assert!(
                        loaded.expose() == expected.expose(),
                        "manifest selects exactly the committed list"
                    );
                    committed(restarted.save_revocations(&list("new")));
                }
            }
            assert_only_active_slot(&restarted, &store, record);
        }
    }
}

#[test]
fn an_interrupted_first_write_leaves_orphans_that_the_next_writer_sweeps() {
    let (_scratch, path, service, store) = setup("vault-first-write");
    let mut vault = open(&store, &service, &path);
    store.arm(MAX_CHUNKS, Fault::CrashAfter);
    assert!(
        panic::catch_unwind(AssertUnwindSafe(|| vault.save_account(&account("old")))).is_err(),
        "crash after first chunk"
    );
    assert!(
        store.names().contains(&format!("{service}/chatgpt/0/0")),
        "orphan really exists"
    );
    assert!(
        !store.names().contains(&format!("{service}/chatgpt")),
        "no commit yet"
    );
    drop(vault);
    let mut restarted = open(&store, &service, &path);
    committed(restarted.save_account(&registration(&account("new"))));
    assert_only_active_slot(&restarted, &store, Chunked::Account);
}

#[test]
fn every_delete_refusal_or_crash_is_retryable_without_losing_the_marker_or_instance() {
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
    for record in [Chunked::Account, Chunked::Revoke] {
        for index in 0..=(2 * MAX_CHUNKS) {
            for fault in [Fault::Refuse, Fault::CrashAfter] {
                let store = seed.fork();
                let mut vault = open(&store, &service, &path);
                store.arm(index, fault);
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| match record {
                    Chunked::Account => vault.delete_account(),
                    Chunked::Revoke => vault.delete_revocations(),
                }));
                match fault {
                    Fault::Refuse => assert!(
                        matches!(outcome.expect("refusal returns"), DeleteResult::Refused(_)),
                        "refused deletion is reported"
                    ),
                    Fault::CrashAfter => assert!(outcome.is_err(), "crash point reached"),
                }
                drop(vault);
                let mut restarted = open(&store, &service, &path);
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
                assert_record_absent(&store, &service, "chatgpt");
                assert_record_absent(&store, &service, "revoke");
                restarted
                    .update_marker(MarkerUpdate::FinishSignOut)
                    .expect("finish after checks");
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
                    vec![instance.clone(), format!("{service}/signout")],
                    "only the instance and token-free notice remain"
                );
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_vault_persists_only_under_its_random_test_service() {
    use crate::platform::credential::WindowsCredentialStore;

    let (_scratch, path, service, _) = setup("windows-vault");
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut vault =
            Vault::with_service(WindowsCredentialStore, &service, &path).expect("test vault");
        let host = vault.host_id().expect("host");
        committed(vault.save_account(&account("old")));
        committed(vault.save_revocations(&list("one")));
        drop(vault);
        let mut vault =
            Vault::with_service(WindowsCredentialStore, &service, &path).expect("reopen vault");
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
    }));
    let mut cleanup = Ok(());
    for name in ["host", "signout", "chatgpt", "revoke"] {
        cleanup = cleanup.and(WindowsCredentialStore.delete(&format!("{service}/{name}")));
    }
    for record in ["chatgpt", "revoke"] {
        for slot in 0..2 {
            for index in 0..MAX_CHUNKS {
                cleanup = cleanup.and(
                    WindowsCredentialStore.delete(&format!("{service}/{record}/{slot}/{index}")),
                );
            }
        }
    }
    cleanup.expect("remove every exact name in this test service");
    if let Err(payload) = outcome {
        panic::resume_unwind(payload);
    }
}
