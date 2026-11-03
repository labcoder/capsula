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
    for (index, mutation) in trace.iter().enumerate() {
        for fault in [Fault::Refuse, Fault::CrashAfter] {
            let store = seed.fork();
            let mut vault = open(&store, &service, &path);
            store.arm(index, fault);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| match record {
                Chunked::Account => vault.save_account(&new_account),
                Chunked::Revoke => vault.save_revocations(&list("new")),
            }));
            let changed = save_changed_after_fault(outcome, mutation, fault, index, commit_at);
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

fn save_changed_after_fault(
    outcome: std::thread::Result<SaveResult>,
    mutation: &Mutation,
    fault: Fault,
    index: usize,
    commit_at: usize,
) -> bool {
    match fault {
        Fault::Refuse => {
            let result = outcome.expect("refusal returns, not crashes");
            if index > commit_at {
                assert!(
                    matches!(result, SaveResult::Committed { leftovers: true }),
                    "post-commit cleanup is not a failed save"
                );
            } else {
                assert!(
                    matches!(
                        (mutation, result),
                        (
                            Mutation::Set(_),
                            SaveResult::NotCommitted(VaultError::Store(
                                CredentialStoreError::Write(5)
                            ))
                        ) | (
                            Mutation::Delete,
                            SaveResult::NotCommitted(VaultError::Store(
                                CredentialStoreError::Delete(5)
                            ))
                        )
                    ),
                    "pre-commit refusal preserves the exact store error"
                );
            }
            index > commit_at
        }
        Fault::CrashAfter => {
            assert!(outcome.is_err(), "crash point reached");
            index >= commit_at
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
                        matches!(
                            outcome.expect("refusal returns"),
                            DeleteResult::Refused(VaultError::Store(CredentialStoreError::Delete(
                                5
                            )))
                        ),
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
    assert_eq!(
        store.names(),
        names,
        "a refusal leaves every record name intact"
    );
    committed(vault.save_revocations(&Secret::new(RevocationList::default())));
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::FinishSweep),
            Err(VaultError::NotGone)
        ),
        "an empty record still has names that must be deleted"
    );
    gone(vault.delete_revocations());
    assert_record_absent(&store, &service, "revoke");
    vault
        .update_marker(MarkerUpdate::FinishSweep)
        .expect("sweep finishes");
    assert!(
        store.names().is_empty(),
        "empty-list cleanup removes the marker too"
    );
}

#[test]
fn sign_out_reader_exposes_the_fenced_record_only_while_a_sign_out_is_pending() {
    let (_scratch, path, service, store) = setup("sign-out-reader");
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
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Keep))
        .expect("promotion");
    drop(vault);
    let vault = open(&store, &service, &path);
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

#[test]
fn unreadable_revocations_need_persisted_uncertainty_and_os_read_errors_stay_errors() {
    let (_scratch, path, service, store) = setup("revoke-unreadable-delete");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_revocations(&list("one")));
    let manifest = format!("{service}/revoke");
    store
        .set(&manifest, &Secret::new(Zeroizing::new(b"{".to_vec())))
        .expect("corrupt test manifest");
    let names = store.names();
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Unreadable)
        ),
        "unreadable data cannot be discarded without recording uncertainty"
    );
    assert_eq!(store.names(), names, "refused cleanup changes no names");
    vault
        .update_marker(MarkerUpdate::UnknownOutcome)
        .expect("persist uncertainty before deletion");
    drop(vault);
    let mut vault = open(&store, &service, &path);
    store.0.lock().expect("fake lock").denied_read = Some(manifest);
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Store(CredentialStoreError::Read(5)))
        ),
        "an OS read refusal remains a typed error"
    );
    assert_eq!(store.names(), names, "an OS refusal changes no names");
    store.0.lock().expect("fake lock").denied_read = None;
    gone(vault.delete_revocations());
    assert_record_absent(&store, &service, "revoke");
    assert!(
        vault.marker().expect("marker").outcome_unknown,
        "cleanup cannot reset the persisted uncertainty"
    );
}

#[cfg(windows)]
#[test]
fn windows_vault_persists_only_under_its_random_test_service() {
    use crate::platform::credential::WindowsCredentialStore;
    use crate::platform::credential::fixture::Fixture;

    let (_scratch, path, service, _) = setup("windows-vault");
    let mut targets: Vec<_> = ["host", "signout", "chatgpt", "revoke"]
        .into_iter()
        .map(|name| format!("{service}/{name}"))
        .collect();
    for record in ["chatgpt", "revoke"] {
        for slot in 0..2 {
            for index in 0..MAX_CHUNKS {
                targets.push(format!("{service}/{record}/{slot}/{index}"));
            }
        }
    }
    let _fixture = Fixture::new(targets);
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
}
