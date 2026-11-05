use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::generation_target;
use rusqlite::Connection;
use zeroize::Zeroizing;

use super::io::{self, SETTLE};
use super::*;
use crate::connections::vault::fake::FakeStore;
use crate::platform::credential::{CredentialStore, SecretBytes};
use crate::test_support::Scratch;

const SERVICE: &str = "authority-test";

struct Setup {
    scratch: Scratch,
    lock: SessionLock,
    store: FakeStore,
}

impl Setup {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let lock = SessionLock::try_acquire(&scratch.dir().join("session.lock")).expect("lock");
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").write_once = true;
        Self {
            scratch,
            lock,
            store,
        }
    }

    fn path(&self) -> PathBuf {
        self.scratch.dir().join(FILE)
    }

    /// Opening again is the crash cut: every commit before it survives, nothing in memory does.
    fn open(&self) -> Authority {
        Authority::open(&self.path(), &self.lock).expect("open authority")
    }

    fn raw(&self) -> Connection {
        Connection::open(self.path()).expect("raw connection")
    }

    fn save(&self, authority: &mut Authority, kind: Kind, secret: &SecretBytes) -> ActiveRef {
        let revision = authority.current(kind).expect("head").revision;
        let plan = authority
            .begin(kind, revision, secret.expose().len())
            .expect("begin");
        let verified = io::write(&self.store, SERVICE, plan, secret, SETTLE).expect("write");
        authority.activate(verified, None).expect("activate")
    }
}

fn secret(byte: u8, len: usize) -> SecretBytes {
    Secret::new(Zeroizing::new(vec![byte; len]))
}

fn names(active: &ActiveRef) -> Vec<String> {
    (0..active.layout.chunks())
        .map(|index| {
            generation_target(
                SERVICE,
                active.kind.label(),
                &active.generation.hex(),
                index,
            )
        })
        .collect()
}

/// Serves other bytes at every present name.
struct Other(FakeStore);

impl CredentialStore for Other {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        Ok(self.0.get(target)?.map(|_| secret(7, 10)))
    }
    fn set(&self, target: &str, value: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.0.set(target, value)
    }
    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.0.delete(target)
    }
}

#[test]
fn a_saved_value_reads_back_by_its_generation_names_alone() {
    let setup = Setup::new("auth-round-trip");
    let mut authority = setup.open();
    for (kind, len) in [
        (Kind::Account, 4_017),
        (Kind::Host, 45),
        (Kind::Revocations, 0),
    ] {
        let value = secret(42, len);
        let active = setup.save(&mut authority, kind, &value);
        assert_eq!(authority.current(kind).expect("head").active, Some(active));
        let read = io::read(&setup.store, SERVICE, &active).expect("read");
        assert!(
            read.expose() == value.expose(),
            "the value reads back exactly"
        );
    }
    let layout = Layout::for_len(4_017).expect("layout");
    assert_eq!(
        (layout.chunks(), layout.chunk_len(0), layout.chunk_len(2)),
        (3, 2_000, 17)
    );
    assert_eq!(Layout::for_len(MAX_RECORD_BYTES).expect("cap").chunks(), 25);
    assert!(matches!(
        Layout::for_len(MAX_RECORD_BYTES + 1),
        Err(AuthorityError::Bounds)
    ));
    let mut seen: Vec<String> = setup.store.names();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen.len(),
        3 + 1 + 1,
        "every generation wrote its own names"
    );
}

#[test]
fn begin_and_activate_compare_and_swap_the_head() {
    let setup = Setup::new("auth-cas");
    let mut authority = setup.open();
    assert!(matches!(
        authority.begin(Kind::Account, 7, 10),
        Err(AuthorityError::Conflict)
    ));
    let first = authority.begin(Kind::Account, 0, 10).expect("first");
    let second = authority.begin(Kind::Account, 0, 10).expect("second");
    assert_ne!(first.generation, second.generation, "fresh generations");
    let stale = io::write(&setup.store, SERVICE, first, &secret(1, 10), SETTLE).expect("first");
    assert!(
        matches!(
            authority.activate(stale, None),
            Err(AuthorityError::Conflict)
        ),
        "a superseded candidate never activates"
    );
    let newer = io::write(&setup.store, SERVICE, second, &secret(2, 10), SETTLE).expect("second");
    authority.activate(newer, None).expect("current candidate");
    let moved = authority.begin(Kind::Account, 1, 10).expect("third");
    let moved_generation = moved.generation;
    authority.remove(&[Kind::Account], None).expect("remove");
    let late = io::write(&setup.store, SERVICE, moved, &secret(3, 10), SETTLE).expect("third");
    assert!(
        matches!(
            authority.activate(late, None),
            Err(AuthorityError::Conflict)
        ),
        "removal fences a pending candidate"
    );
    assert_eq!(authority.current(Kind::Account).expect("head").active, None);
    assert_eq!(
        authority.current(Kind::Host).expect("other head").revision,
        0
    );
    let retired = authority.retired(16).expect("retired");
    assert!(
        retired
            .iter()
            .any(|retired| retired.generation == moved_generation),
        "the removal itself retired the pending candidate"
    );
}

#[test]
fn a_candidate_never_activates_over_a_head_that_moved() {
    let setup = Setup::new("auth-moved-head");
    let mut authority = setup.open();
    let plan = authority.begin(Kind::Host, 0, 45).expect("begin");
    let verified = io::write(&setup.store, SERVICE, plan, &secret(1, 45), SETTLE).expect("write");
    setup
        .raw()
        .execute("UPDATE heads SET revision = 5 WHERE kind = 'host'", [])
        .expect("another writer moved the head");
    assert!(matches!(
        authority.activate(verified, None),
        Err(AuthorityError::Conflict)
    ));
    assert_eq!(authority.current(Kind::Host).expect("head").active, None);
}

#[test]
fn a_candidate_from_before_a_restart_never_activates() {
    let setup = Setup::new("auth-restart");
    let mut authority = setup.open();
    let old = setup.save(&mut authority, Kind::Account, &secret(1, 30));
    let plan = authority.begin(Kind::Account, 1, 30).expect("begin");
    let verified = io::write(&setup.store, SERVICE, plan, &secret(2, 30), SETTLE).expect("write");
    drop(authority);
    let mut authority = setup.open();
    assert!(matches!(
        authority.activate(verified, None),
        Err(AuthorityError::Conflict)
    ));
    assert_eq!(
        authority.current(Kind::Account).expect("head").active,
        Some(old)
    );
    assert_eq!(
        authority.retired(16).expect("retired").len(),
        1,
        "the candidate is owed a sweep"
    );
}

#[test]
fn a_removal_stays_final_when_the_store_serves_the_deleted_value_again() {
    let setup = Setup::new("auth-resurrect");
    let mut authority = setup.open();
    let active = setup.save(&mut authority, Kind::AnthropicKey, &secret(9, 108));
    let before = setup.store.snapshot();
    authority
        .remove(&[Kind::AnthropicKey], None)
        .expect("remove");
    let retired = authority.retired(16).expect("retired");
    assert_eq!(
        io::sweep(&setup.store, SERVICE, &retired[0]),
        Observed::Absent
    );
    setup.store.rollback(&before, &names(&active));
    drop(authority);
    let authority = setup.open();
    assert_eq!(
        authority.current(Kind::AnthropicKey).expect("head").active,
        None,
        "valid resurrected bytes authorize nothing"
    );
}

#[test]
fn a_replaced_generation_is_never_read_when_its_successor_goes_missing() {
    let setup = Setup::new("auth-no-fallback");
    let mut authority = setup.open();
    let a = setup.save(&mut authority, Kind::Account, &secret(1, 2_500));
    let before_b = setup.store.snapshot();
    let b = setup.save(&mut authority, Kind::Account, &secret(2, 2_500));
    setup.store.rollback(&before_b, &names(&b));
    let current = authority.current(Kind::Account).expect("head").active;
    assert_eq!(current, Some(b), "the successor stays current");
    assert!(matches!(
        io::read(&setup.store, SERVICE, &b),
        Err(AuthorityError::Unsettled)
    ));
    assert!(
        setup
            .store
            .names()
            .iter()
            .any(|name| names(&a).contains(name)),
        "the predecessor's bytes are still in the store, unread"
    );
}

#[test]
fn a_chunk_of_the_wrong_length_or_a_missing_one_is_unsettled() {
    let setup = Setup::new("auth-lengths");
    let mut authority = setup.open();
    let active = setup.save(&mut authority, Kind::Account, &secret(4, 4_017));
    let chunks = names(&active);
    for (name, value) in [
        (&chunks[2], Some(secret(4, 16))),
        (&chunks[0], Some(secret(4, 2_001))),
        (&chunks[1], None),
    ] {
        let saved = setup.store.snapshot();
        let mut altered: BTreeMap<String, SecretBytes> = saved.clone();
        match value {
            Some(value) => altered.insert(name.clone(), value),
            None => altered.remove(name),
        };
        setup.store.rollback(&altered, std::slice::from_ref(name));
        assert!(matches!(
            io::read(&setup.store, SERVICE, &active),
            Err(AuthorityError::Unsettled)
        ));
        setup.store.rollback(&saved, std::slice::from_ref(name));
    }
    io::read(&setup.store, SERVICE, &active).expect("restored");
}

#[test]
fn a_save_waits_out_absence_but_not_other_bytes() {
    let setup = Setup::new("auth-settle");
    let mut authority = setup.open();
    setup.store.0.lock().expect("fake lock").missed_reads = 20;
    setup.save(&mut authority, Kind::Account, &secret(5, 4_017));
    setup.store.0.lock().expect("fake lock").missed_reads = usize::MAX;
    let plan = authority.begin(Kind::Account, 1, 10).expect("begin");
    assert!(matches!(
        io::write(
            &setup.store,
            SERVICE,
            plan,
            &secret(6, 10),
            Duration::from_millis(50)
        ),
        Err(AuthorityError::Unsettled)
    ));
    setup.store.0.lock().expect("fake lock").missed_reads = 0;

    let plan = authority.begin(Kind::Account, 1, 10).expect("begin");
    let started = Instant::now();
    assert!(matches!(
        io::write(
            &Other(setup.store.clone()),
            SERVICE,
            plan,
            &secret(6, 10),
            Duration::from_secs(30)
        ),
        Err(AuthorityError::Unsettled)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "other bytes fail without waiting"
    );
    let plan = authority.begin(Kind::Account, 1, 10).expect("begin");
    assert!(matches!(
        io::write(&setup.store, SERVICE, plan, &secret(6, 11), SETTLE),
        Err(AuthorityError::Bounds)
    ));
}

#[test]
fn a_removed_generation_is_swept_only_after_a_clean_read_in_a_later_open() {
    let setup = Setup::new("auth-sweep");
    let mut authority = setup.open();
    let active = setup.save(&mut authority, Kind::Host, &secret(1, 45));
    let before = setup.store.snapshot();
    authority.remove(&[Kind::Host], None).expect("remove");
    let retired = authority.retired(16).expect("retired")[0];
    assert_eq!(io::sweep(&setup.store, SERVICE, &retired), Observed::Absent);
    authority
        .observed(&retired, Observed::Absent)
        .expect("observed");
    assert_eq!(
        authority.retired(16).expect("still owed").len(),
        1,
        "same open isn't enough"
    );
    drop(authority);
    let mut authority = setup.open();
    setup.store.rollback(&before, &names(&active));
    setup.store.0.lock().expect("fake lock").sticky_deletes = 1;
    assert_eq!(
        io::sweep(&setup.store, SERVICE, &retired),
        Observed::Remains,
        "a resurrected name that survives its delete is seen"
    );
    authority
        .observed(&retired, Observed::Remains)
        .expect("observed");
    assert_eq!(io::sweep(&setup.store, SERVICE, &retired), Observed::Absent);
    authority
        .observed(&retired, Observed::Absent)
        .expect("observed");
    assert_eq!(
        authority.retired(16).expect("retired").len(),
        1,
        "the resurrection reset the clock to this open"
    );
    drop(authority);
    let mut authority = setup.open();
    assert_eq!(io::sweep(&setup.store, SERVICE, &retired), Observed::Absent);
    authority
        .observed(&retired, Observed::Absent)
        .expect("observed");
    assert!(
        authority.retired(16).expect("retired").is_empty(),
        "now Swept"
    );
    assert!(matches!(
        authority.observed(&retired, Observed::Absent),
        Err(AuthorityError::Conflict)
    ));
}

#[test]
fn one_sweep_pass_is_bounded_however_many_generations_retired() {
    let setup = Setup::new("auth-bounded");
    let mut authority = setup.open();
    for round in 0..20 {
        setup.save(&mut authority, Kind::Account, &secret(round, 10));
    }
    assert_eq!(authority.retired(100).expect("all").len(), 19);
    assert_eq!(authority.retired(16).expect("one pass").len(), 16);
}

#[test]
fn missing_or_invalid_rows_and_newer_files_are_refused() {
    let setup = Setup::new("auth-corrupt");
    drop(setup.open());
    setup
        .raw()
        .execute("DELETE FROM heads WHERE kind = 'anthropic'", [])
        .expect("drop a head");
    let authority = setup.open();
    assert!(matches!(
        authority.current(Kind::AnthropicKey),
        Err(AuthorityError::Corrupt)
    ));
    drop(authority);
    setup
        .raw()
        .execute(
            "INSERT INTO generations (generation, kind, state, chunks, bytes)
             VALUES (zeroblob(16), 'host', 'active', 3, 10)",
            [],
        )
        .expect("a layout that doesn't add up");
    let authority = setup.open();
    assert!(matches!(
        authority.current(Kind::Host),
        Err(AuthorityError::Corrupt)
    ));
    drop(authority);
    setup
        .raw()
        .execute("DELETE FROM control", [])
        .expect("drop control");
    assert!(matches!(
        Authority::open(&setup.path(), &setup.lock),
        Err(AuthorityError::Corrupt)
    ));
    setup
        .raw()
        .execute_batch("PRAGMA user_version = 2")
        .expect("newer");
    assert!(matches!(
        Authority::open(&setup.path(), &setup.lock),
        Err(AuthorityError::NewerSchema)
    ));
}

#[test]
fn the_marker_never_defaults_and_commits_with_its_transition() {
    let setup = Setup::new("auth-marker");
    let mut authority = setup.open();
    assert_eq!(
        authority.marker().expect("marker"),
        SignOutMarker::default()
    );
    let promoted = SignOutMarker::fail_closed();
    setup.save(&mut authority, Kind::Account, &secret(1, 10));
    authority
        .remove(&[Kind::Account, Kind::Revocations], Some(promoted))
        .expect("remove with marker");
    drop(authority);
    let authority = setup.open();
    assert_eq!(
        authority.marker().expect("marker"),
        promoted,
        "committed together"
    );
    let mut authority = authority;
    authority
        .set_marker(SignOutMarker::default())
        .expect("clear");
    assert_eq!(
        authority.marker().expect("marker"),
        SignOutMarker::default()
    );
    drop(authority);
    setup
        .raw()
        .execute("UPDATE control SET marker = '{\"v\":9}'", [])
        .expect("malformed");
    assert_eq!(
        setup.open().marker().expect("marker"),
        SignOutMarker::fail_closed()
    );
    setup
        .raw()
        .execute("DELETE FROM control", [])
        .expect("drop control");
    let authority = Authority {
        conn: setup.raw(),
        lifetime: 1,
    };
    assert!(matches!(authority.marker(), Err(AuthorityError::Corrupt)));
}

#[test]
fn debug_output_names_no_generation_or_value() {
    let setup = Setup::new("auth-debug");
    let mut authority = setup.open();
    let plan = authority.begin(Kind::Account, 0, 4).expect("begin");
    let hex = plan.generation.hex();
    let shown = format!("{plan:?} {authority:?}");
    assert!(!shown.contains(&hex), "no generation in Debug");
    let verified = io::write(&setup.store, SERVICE, plan, &secret(b'S', 4), SETTLE).expect("write");
    let active = authority.activate(verified, None).expect("activate");
    let shown = format!("{active:?} {:?}", authority.retired(16).expect("retired"));
    assert!(
        !shown.contains(&hex) && !shown.contains("SSSS"),
        "no secret or generation"
    );
}

#[test]
#[should_panic(expected = "a write-once credential name was written twice")]
fn the_fake_store_fails_a_second_write_of_one_name() {
    let store = FakeStore::default();
    store.0.lock().expect("fake lock").write_once = true;
    store.set("name", &secret(1, 1)).expect("first");
    store.delete("name").expect("delete");
    store.set("name", &secret(1, 1)).ok();
}
