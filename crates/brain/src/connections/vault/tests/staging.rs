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
    assert_eq!(result.expose().as_str(), key);
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
    for commit in [false, true] {
        let plan = vault.begin_anthropic_paste(NEW.len()).expect("plan");
        plant(&store, &service, &plan, NEW.as_bytes());
        if commit {
            raw(&path).execute_batch("CREATE TRIGGER refuse_activation BEFORE UPDATE OF state ON generations WHEN NEW.state = 'active' BEGIN SELECT RAISE(ABORT, 'refused'); END;").expect("activation-only failure");
        } else {
            store.arm(0, crate::connections::vault::fake::Fault::Refuse);
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
            store.names().len(),
            1,
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
        vault.anthropic(KeyOp::Remove),
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
        vault.anthropic(KeyOp::Remove).expect("remove").map(|_| ()),
        None
    );
    assert_eq!(loaded(&mut vault), None);
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
        vault.anthropic(KeyOp::Remove).expect("remove").map(|_| ()),
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
        .remove(&[Kind::AnthropicKey], None)
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
    let (_scratch, path, service, store) = setup("paste-legacy-unread");
    let mut vault = open(&store, &service, &path);
    store
        .set(
            &kumochi_protocol::constants::anthropic_staging_target(&service),
            &Secret::new(Zeroizing::new(NEW.as_bytes().to_vec())),
        )
        .expect("legacy bytes");
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

#[test]
fn partial_candidate_writes_are_deleted_without_touching_the_active_key() {
    let (_scratch, path, service, store) = setup("paste-partial-candidate");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(OLD.as_bytes()).expect("old key");
    let plan = vault.begin_anthropic_paste(2048).expect("plan");
    let key = format!("sk-ant-api{}", "x".repeat(2038));
    plant(&store, &service, &plan, key.as_bytes());
    store.arm(1, crate::connections::vault::fake::Fault::Refuse);
    assert!(matches!(
        vault.adopt_anthropic_paste(plan.generation),
        Err(PasteError::Key(KeyError::Vault(VaultError::Store(
            CredentialStoreError::Write(5)
        ))))
    ));
    assert_eq!(loaded(&mut vault).as_deref(), Some(OLD));
    assert_eq!(store.names().len(), 1);
}
