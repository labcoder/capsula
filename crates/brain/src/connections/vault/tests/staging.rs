use super::*;
use crate::connections::anthropic::{KeyError, KeyOp};
use crate::connections::vault::staging::{PasteError, PastePlan};
use kumochi_protocol::message::KeyRejection;

const OLD: &str = "sk-ant-api-old-fake-sentinel";
const NEW: &str = "sk-ant-api-new-fake-sentinel";

struct FastClock(std::time::Instant, std::cell::Cell<std::time::Duration>);
impl FastClock {
    fn new() -> Self {
        Self(
            std::time::Instant::now(),
            std::cell::Cell::new(std::time::Duration::ZERO),
        )
    }
}
impl generation::Clock for FastClock {
    fn now(&self) -> std::time::Instant {
        self.0 + self.1.get()
    }
    fn sleep(&self, delay: std::time::Duration) {
        self.1.set(self.1.get() + delay);
    }
}

fn loaded(vault: &mut Vault<FakeStore>) -> Option<String> {
    vault
        .anthropic(KeyOp::Load)
        .1
        .expect("load")
        .map(|key| key.expose().as_str().to_owned())
}

fn plant(store: &FakeStore, service: &str, plan: &PastePlan, value: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for (index, bytes) in value.chunks(2000).enumerate() {
        let name = generation_target(service, "anthropic", &plan.generation.hex(), index);
        store
            .set(&name, &Secret::new(Zeroizing::new(bytes.to_vec())))
            .expect("shell write");
        names.push(name);
    }
    names
}

#[test]
fn paste_bounds_preserve_the_current_key_and_mint_distinct_names() {
    let (_scratch, path, service, store) = setup("paste-bounds");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let before = store.names();
    for len in [0, 15, 2049, usize::MAX] {
        assert!(matches!(
            vault.begin_anthropic_paste(len),
            Err(KeyError::Vault(VaultError::Bounds))
        ));
        assert_eq!(store.names(), before);
        assert_eq!(
            raw(&path)
                .query_row(
                    "SELECT count(*) FROM generations WHERE state = 'pending'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("pending count"),
            0
        );
    }
    let mut previous = None;
    for len in [16, 2000, 2001, 2048] {
        let plan = vault.begin_anthropic_paste(len).expect("plan");
        assert_eq!(plan.bytes, len);
        assert_ne!(previous, Some(plan.generation));
        previous = Some(plan.generation);
        assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
        assert_eq!(store.names(), before);
    }
}

#[test]
fn adoption_returns_the_revision_written_by_the_authority_transaction() {
    let (_scratch, path, service, store) = setup("paste-committed-revision");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&store, &service, &plan, NEW.as_bytes());
    raw(&path)
        .execute_batch(
            "CREATE TRIGGER test_revision AFTER UPDATE OF revision ON heads
             WHEN NEW.kind = 'anthropic' AND NEW.revision = 2
             BEGIN
                 UPDATE heads SET revision = 9 WHERE kind = NEW.kind;
             END;",
        )
        .expect("vary the committed revision in this test database");
    let (adopted, _) = vault.adopt_anthropic_paste(plan.generation).expect("adopt");
    assert_eq!(vault.anthropic_revision(), 9);
    assert_eq!(adopted, 9);
}

#[test]
fn paste_adoption_copies_to_fresh_active_names_and_is_single_use() {
    let (_scratch, path, service, store) = setup("paste-commit");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    store.0.lock().expect("fake").write_once = true;
    let key = format!("sk-ant-api{}", "x".repeat(2038));
    let plan = vault.begin_anthropic_paste(2048).expect("plan");
    let names = plant(&store, &service, &plan, key.as_bytes());
    assert_eq!(
        store
            .get(&names[0])
            .expect("read")
            .expect("first")
            .expose()
            .len(),
        2000
    );
    assert_eq!(
        store
            .get(&names[1])
            .expect("read")
            .expect("last")
            .expose()
            .len(),
        48
    );
    let result = vault.adopt_anthropic_paste(plan.generation).expect("adopt");
    assert_eq!(result.1.expose().as_str(), key);
    assert_eq!(loaded(&mut vault), Some(key.clone()));
    assert!(names.iter().all(|name| !store.names().contains(name)));
    assert_eq!(store.names().len(), 2);
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Changed)
    ));
    assert_eq!(loaded(&mut vault), Some(key));
}

#[test]
fn stale_paste_ids_do_not_consume_the_newer_permit() {
    let (_scratch, path, service, store) = setup("paste-replace");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let first = vault.begin_anthropic_paste(NEW.len()).expect("first");
    let second = vault.begin_anthropic_paste(NEW.len()).expect("second");
    plant(&store, &service, &second, NEW.as_bytes());
    assert!(matches!(
        vault.adopt_anthropic_paste(first.generation),
        Err(PasteError::Changed)
    ));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
    assert_eq!(
        vault
            .adopt_anthropic_paste(second.generation)
            .expect("second adopts")
            .1
            .expose()
            .as_str(),
        NEW
    );
}

#[test]
fn invalid_present_pastes_are_rejected_without_replacing_the_saved_key() {
    let (_scratch, path, service, store) = setup("paste-invalid");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    for (bytes, reason) in [
        (
            b"sk-ant-oat-fake-sentinel".as_slice(),
            KeyRejection::SubscriptionToken,
        ),
        (
            b"sk-ant-admin-fake-sentinel".as_slice(),
            KeyRejection::AdminKey,
        ),
        (b"\xff123456789012345".as_slice(), KeyRejection::Format),
    ] {
        let plan = vault.begin_anthropic_paste(bytes.len()).expect("plan");
        plant(&store, &service, &plan, bytes);
        assert!(
            matches!(vault.adopt_anthropic_paste(plan.generation), Err(PasteError::Key(KeyError::Rejected(got))) if got == reason)
        );
        assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
        assert!(
            store
                .names()
                .iter()
                .all(|name| !name.contains(&plan.generation.hex()))
        );
        assert!(matches!(
            vault.adopt_anthropic_paste(plan.generation),
            Err(PasteError::Changed)
        ));
    }
}

#[test]
fn failed_candidate_writes_and_failed_commits_preserve_the_saved_key() {
    let (_scratch, path, service, store) = setup("paste-write-failure");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let previous_names = store.names();
    let long = format!("{NEW}{}", "x".repeat(2048 - NEW.len()));
    for (failed_chunk, key) in [(Some(0), NEW), (Some(1), long.as_str()), (None, NEW)] {
        let commit = failed_chunk.is_none();
        let plan = vault.begin_anthropic_paste(key.len()).expect("plan");
        plant(&store, &service, &plan, key.as_bytes());
        if let Some(index) = failed_chunk {
            store.arm(index, Fault::Refuse);
        } else {
            raw(&path).execute_batch("CREATE TRIGGER refuse_activation BEFORE UPDATE OF state ON generations WHEN NEW.state = 'active' BEGIN SELECT RAISE(ABORT, 'refused'); END;").expect("activation-only failure");
        }
        let result = vault.adopt_anthropic_paste(plan.generation);
        if commit {
            assert!(matches!(
                result,
                Err(PasteError::Key(KeyError::Vault(VaultError::Authority)))
            ));
        } else {
            assert!(matches!(
                result,
                Err(PasteError::Key(KeyError::Vault(VaultError::Store(
                    CredentialStoreError::Write(5)
                ))))
            ));
        }
        if commit {
            raw(&path)
                .execute_batch("DROP TRIGGER refuse_activation;")
                .expect("allow activation");
        }
        assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
        assert!(
            store
                .names()
                .iter()
                .all(|name| !name.contains(&plan.generation.hex())),
            "a write/activation failure deletes the staged copy"
        );
        assert_eq!(
            store.names(),
            previous_names,
            "only the previous active key remains"
        );
        assert!(matches!(
            vault.adopt_anthropic_paste(plan.generation),
            Err(PasteError::Changed)
        ));
    }
}

#[test]
fn remove_consumes_the_permit_even_if_its_commit_fails() {
    let (_scratch, path, service, store) = setup("paste-remove-failure");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&store, &service, &plan, NEW.as_bytes());
    refuse_commits(&path);
    assert!(matches!(
        vault.anthropic(KeyOp::Remove(vault.anthropic_revision())).1,
        Err(KeyError::Vault(VaultError::Authority))
    ));
    allow_commits(&path);
    assert!(
        store
            .names()
            .iter()
            .all(|name| !name.contains(&plan.generation.hex()))
    );
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Changed)
    ));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
    assert_eq!(
        vault
            .anthropic(KeyOp::Remove(vault.anthropic_revision()))
            .1
            .expect("remove")
            .map(|_| ()),
        None
    );
    assert_eq!(loaded(&mut vault), None);
}

#[test]
fn removal_and_retry_for_an_old_revision_keep_the_new_head_and_its_plan() {
    let (_scratch, path, service, store) = setup("paste-stale-remove");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let revision = vault.anthropic_revision();
    refuse_commits(&path);
    assert!(matches!(
        vault.anthropic(KeyOp::Remove(revision)).1,
        Err(KeyError::Vault(VaultError::Authority))
    ));
    allow_commits(&path);
    vault.save_anthropic_key(NEW.as_bytes()).expect("new key");
    let plan = vault.begin_anthropic_paste(OLD.len()).expect("new plan");
    plant(&store, &service, &plan, OLD.as_bytes());
    let names = store.names();
    for _ in 0..2 {
        assert!(matches!(
            vault.anthropic(KeyOp::Remove(revision)).1,
            Err(KeyError::Changed)
        ));
        assert_eq!(loaded(&mut vault).as_deref(), Some(NEW));
        assert_eq!(store.names(), names);
    }
    assert_eq!(
        vault
            .adopt_anthropic_paste(plan.generation)
            .expect("plan still works")
            .1
            .expose()
            .as_str(),
        OLD
    );
}

#[test]
fn abandoned_pastes_are_retired_on_open_and_late_writes_never_authorize() {
    for written in [0, 2000, 2048] {
        let (_scratch, path, service, store) = setup("paste-reopen");
        let mut vault = open(&store, &service, &path);
        vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
        let plan = vault.begin_anthropic_paste(2048).expect("plan");
        let key = format!("sk-ant-api{}", "x".repeat(2038));
        if written > 0 {
            plant(&store, &service, &plan, &key.as_bytes()[..written]);
        }
        drop(vault);
        let mut vault = open(&store, &service, &path);
        assert_eq!(
            raw(&path)
                .query_row(
                    "SELECT count(*) FROM generations WHERE state = 'pending'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("pending"),
            0
        );
        assert!(matches!(
            vault.adopt_anthropic_paste(plan.generation),
            Err(PasteError::Changed)
        ));
        assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
    }
    let (_scratch, path, service, store) = setup("paste-late");
    let mut vault = open(&store, &service, &path);
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    assert_eq!(
        vault
            .anthropic(KeyOp::Remove(vault.anthropic_revision()))
            .1
            .expect("remove")
            .map(|_| ()),
        None
    );
    plant(&store, &service, &plan, NEW.as_bytes());
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Changed)
    ));
    assert_eq!(loaded(&mut vault), None);
    let fresh = vault.begin_anthropic_paste(NEW.len()).expect("fresh");
    plant(&store, &service, &fresh, NEW.as_bytes());
    assert_eq!(
        vault
            .adopt_anthropic_paste(fresh.generation)
            .expect("fresh adopt")
            .1
            .expose()
            .as_str(),
        NEW
    );
}

#[test]
fn staging_errors_and_metadata_do_not_print_secret_material() {
    let (_scratch, path, service, store) = setup("paste-debug");
    let mut vault = open(&store, &service, &path);
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&store, &service, &plan, NEW.as_bytes());
    let id = plan.generation.hex();
    let key = vault.adopt_anthropic_paste(plan.generation);
    let text = format!(
        "{plan:?} {key:?} {vault:?} {:?}",
        PasteError::Key(KeyError::Rejected(KeyRejection::Format))
    );
    assert!(!text.contains(NEW));
    assert!(!text.contains(&format!("{:?}", NEW.as_bytes())));
    assert!(!text.contains(&id));
}

#[test]
fn adoption_never_rebases_a_paste_over_a_changed_authority_head() {
    let (_scratch, path, service, store) = setup("paste-cas");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&store, &service, &plan, NEW.as_bytes());
    vault
        .authority
        .remove(&[Kind::AnthropicKey], None, None)
        .expect("concurrent fence");
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Key(KeyError::Vault(VaultError::Authority)))
    ));
    assert_eq!(loaded(&mut vault), None);
}

#[test]
fn absent_staging_is_unavailable_not_format_and_consumes_the_permit() {
    let (_scratch, path, service, store) = setup("paste-absent");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    assert!(matches!(
        vault.adopt_with_clock(plan.generation, &FastClock::new()),
        Err(PasteError::Key(KeyError::Vault(VaultError::Store(
            CredentialStoreError::Unsettled
        ))))
    ));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Changed)
    ));
}

#[test]
fn legacy_bytes_cannot_authorize_a_fresh_plan() {
    use kumochi_protocol::constants::{anthropic_key_target, anthropic_staging_target};
    use kumochi_protocol::message::KeyStageId;

    let (_scratch, path, service, store) = setup("paste-legacy-unread");
    let staged = anthropic_staging_target(&service);
    let plant_legacy = |name: &str| {
        store
            .set(name, &Secret::new(Zeroizing::new(NEW.as_bytes().to_vec())))
            .expect("legacy bytes");
    };
    plant_legacy(&anthropic_key_target(&service));
    plant_legacy(&staged);
    let mut vault = open(&store, &service, &path);
    assert_eq!(loaded(&mut vault), None);
    assert!(matches!(
        vault.adopt_anthropic_paste(authority::Generation::from_stage_id(KeyStageId([0; 16]))),
        Err(PasteError::Changed)
    ));
    assert!(
        store.names().is_empty(),
        "the open deleted both legacy names"
    );
    assert!(!is_current(&vault, Kind::AnthropicKey));
    plant_legacy(&staged);
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    assert!(matches!(
        vault.adopt_with_clock(plan.generation, &FastClock::new()),
        Err(PasteError::Key(KeyError::Vault(VaultError::Store(
            CredentialStoreError::Unsettled
        ))))
    ));
    assert_eq!(loaded(&mut vault), None);
}

#[test]
fn failed_deletes_do_not_undo_committed_adoption() {
    let (_scratch, path, service, store) = setup("paste-delete-refused");
    let mut vault = open(&store, &service, &path);
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    let names = plant(&store, &service, &plan, NEW.as_bytes());
    store.0.lock().expect("fake").refuse_deletes = true;
    assert_eq!(
        vault
            .adopt_anthropic_paste(plan.generation)
            .expect("committed")
            .1
            .expose()
            .as_str(),
        NEW
    );
    assert_eq!(loaded(&mut vault).as_deref(), Some(NEW));
    assert!(names.iter().all(|name| store.names().contains(name)));
}

#[test]
fn every_begin_requires_fresh_brain_minted_names() {
    let (_scratch, path, service, store) = setup("paste-fresh-names");
    let mut vault = open(&store, &service, &path);
    let first = vault.begin_anthropic_paste(16).expect("first");
    assert_ne!(first.generation.hex(), "00000000000000000000000000000000");
    let second = vault.begin_anthropic_paste(16).expect("second");
    assert_eq!((first.bytes, second.bytes), (16, 16));
    assert_ne!(first.generation, second.generation);
    assert_ne!(second.generation.hex(), "00000000000000000000000000000000");
}

fn open_split(
    private: &FakeStore,
    staging: &FakeStore,
    service: &str,
    path: &Path,
) -> Vault<FakeStore> {
    Vault::with_stores(private.clone(), Some(staging.clone()), service, path)
        .expect("split test vault")
}

/// The staged chunks land in the staging store; the same names in the private store are decoys a
/// wrong route would read or delete.
#[test]
fn a_paste_is_read_and_cleaned_only_in_the_staging_store() {
    let (_scratch, path, service, private) = setup("paste-split-adopt");
    let staging = FakeStore::default();
    let mut vault = open_split(&private, &staging, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&staging, &service, &plan, NEW.as_bytes());
    let decoys = plant(&private, &service, &plan, OLD.as_bytes());
    let (_, key) = vault
        .adopt_anthropic_paste(plan.generation)
        .expect("adopted");
    assert_eq!(key.expose().as_str(), NEW);
    assert!(
        staging.names().is_empty(),
        "the staged chunks were deleted there, and the saved key went elsewhere"
    );
    let names = private.names();
    assert!(decoys.iter().all(|name| names.contains(name)));
    drop(vault);
    let mut vault = open_split(&private, &staging, &service, &path);
    assert_eq!(loaded(&mut vault).as_deref(), Some(NEW));
}

#[test]
fn a_rejected_paste_is_deleted_only_in_the_staging_store() {
    let (_scratch, path, service, private) = setup("paste-split-reject");
    let staging = FakeStore::default();
    let mut vault = open_split(&private, &staging, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&staging, &service, &plan, &[0; NEW.len()]);
    let decoys = plant(&private, &service, &plan, NEW.as_bytes());
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Key(KeyError::Rejected(_)))
    ));
    assert!(staging.names().is_empty());
    let names = private.names();
    assert!(decoys.iter().all(|name| names.contains(name)));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
}

#[test]
fn superseded_removed_and_interrupted_pastes_are_cleaned_in_the_staging_store() {
    let (_scratch, path, service, private) = setup("paste-split-cleanup");
    let staging = FakeStore::default();
    let mut vault = open_split(&private, &staging, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let mut decoys = Vec::new();
    let mut stage = |vault: &mut Vault<FakeStore>| {
        let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
        plant(&staging, &service, &plan, NEW.as_bytes());
        decoys.extend(plant(&private, &service, &plan, NEW.as_bytes()));
    };
    stage(&mut vault);
    stage(&mut vault);
    assert_eq!(
        staging.names().len(),
        1,
        "the superseding begin swept the first paste"
    );
    let revision = vault.anthropic_revision();
    vault.anthropic(KeyOp::Remove(revision)).1.expect("removed");
    assert!(staging.names().is_empty(), "Remove deleted its paste");
    stage(&mut vault);
    drop(vault);
    let mut vault = open_split(&private, &staging, &service, &path);
    assert!(
        staging.names().is_empty(),
        "the next open swept the interrupted paste"
    );
    let names = private.names();
    assert!(decoys.iter().all(|name| names.contains(name)));
    assert_eq!(loaded(&mut vault), None);
    let states: Vec<String> = raw(&path)
        .prepare("SELECT state FROM generations ORDER BY rowid")
        .expect("states")
        .query_map([], |row| row.get(0))
        .expect("states")
        .collect::<Result<_, _>>()
        .expect("states");
    assert_eq!(
        states,
        ["swept", "swept", "swept", "removed"],
        "each sweep read back the store it deleted from, so the old key and the two earlier pastes are Swept"
    );
}

#[test]
fn a_refused_staging_read_never_falls_back_to_the_private_store() {
    let (_scratch, path, service, private) = setup("paste-split-denied");
    let staging = FakeStore::default();
    let mut vault = open_split(&private, &staging, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    let staged = plant(&staging, &service, &plan, NEW.as_bytes());
    plant(&private, &service, &plan, NEW.as_bytes());
    staging.0.lock().expect("fake").denied_read = Some(staged[0].clone());
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Key(KeyError::Vault(VaultError::Store(
            CredentialStoreError::Read(5)
        ))))
    ));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
}

#[test]
fn failed_candidate_writes_and_refused_removes_delete_only_staged_names() {
    let (_scratch, path, service, private) = setup("paste-split-failures");
    let staging = FakeStore::default();
    let mut vault = open_split(&private, &staging, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&staging, &service, &plan, NEW.as_bytes());
    plant(&private, &service, &plan, OLD.as_bytes());
    let before = private.names();
    private.arm(0, Fault::Refuse);
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Key(KeyError::Vault(VaultError::Store(
            CredentialStoreError::Write(5)
        ))))
    ));
    assert!(
        staging.names().is_empty(),
        "the failed adoption deleted the paste"
    );
    assert_eq!(
        private.names(),
        before,
        "only the saved key and the decoy remain"
    );
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));

    let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
    plant(&staging, &service, &plan, NEW.as_bytes());
    plant(&private, &service, &plan, OLD.as_bytes());
    let before = private.names();
    refuse_commits(&path);
    assert!(matches!(
        vault.anthropic(KeyOp::Remove(vault.anthropic_revision())).1,
        Err(KeyError::Vault(VaultError::Authority))
    ));
    allow_commits(&path);
    assert!(
        staging.names().is_empty(),
        "the refused Remove deleted its paste directly"
    );
    assert_eq!(private.names(), before);
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
}
