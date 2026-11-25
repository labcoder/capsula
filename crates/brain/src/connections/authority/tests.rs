use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::generation_target;
use rusqlite::Connection;
use zeroize::Zeroizing;

use super::io;
use super::*;
use crate::connections::records::MAX_RECORD_BYTES;
use crate::connections::vault::fake::FakeStore;
use crate::platform::credential::{CredentialStore, SecretBytes};
use crate::test_support::Scratch;

mod github;
mod staging;

const SERVICE: &str = "authority-test";

struct Setup {
    lock: SessionLock,
    scratch: Scratch,
    store: FakeStore,
}

impl Setup {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let lock = SessionLock::try_acquire(&scratch.dir().join("session.lock")).expect("lock");
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").write_once = true;
        Self {
            lock,
            scratch,
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

    /// How many generations are Removed and not yet Swept.
    fn removed(&self) -> i64 {
        self.raw()
            .query_row(
                "SELECT count(*) FROM generations WHERE state = 'removed'",
                [],
                |row| row.get(0),
            )
            .expect("count")
    }

    fn save(&self, authority: &mut Authority, kind: Kind, secret: &SecretBytes) -> ActiveRef {
        let revision = authority.current(kind).expect("head").revision;
        let plan = authority
            .begin(kind, revision, secret.expose().len())
            .expect("begin");
        let verified = io::write(&self.store, SERVICE, plan, secret).expect("write");
        authority.activate(verified, None).expect("activate").0
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

/// A monotonic clock that moves only when a wait asks it to.
struct FakeClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("clock lock")
    }
}

impl io::Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + self.elapsed()
    }
    fn sleep(&self, duration: Duration) {
        *self.elapsed.lock().expect("clock lock") += duration;
    }
}

/// Hides chunk `i` of every name until the clock reaches `visible[i]` milliseconds.
struct Late<'a> {
    store: &'a FakeStore,
    clock: &'a FakeClock,
    visible: &'a [u64],
}

impl CredentialStore for Late<'_> {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        let index: usize = target
            .rsplit('/')
            .next()
            .and_then(|index| index.parse().ok())
            .expect("chunk index");
        if self.clock.elapsed() < Duration::from_millis(self.visible[index]) {
            return Ok(None);
        }
        self.store.get(target)
    }
    fn set(&self, target: &str, value: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.store.set(target, value)
    }
    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.store.delete(target)
    }
}

/// Begins an Account candidate of `len` bytes and writes it under `clock` within `budget`.
fn settle(
    authority: &mut Authority,
    store: &dyn CredentialStore,
    clock: &FakeClock,
    len: usize,
    budget: Duration,
) -> Result<(), AuthorityError> {
    let revision = authority.current(Kind::Account).expect("head").revision;
    let plan = authority
        .begin(Kind::Account, revision, len)
        .expect("begin");
    io::write_within(store, SERVICE, plan, &secret(6, len), budget, clock).map(drop)
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
    let stale = io::write(&setup.store, SERVICE, first, &secret(1, 10)).expect("first");
    assert!(
        matches!(
            authority.activate(stale, None),
            Err(AuthorityError::Conflict)
        ),
        "a superseded candidate never activates"
    );
    let newer = io::write(&setup.store, SERVICE, second, &secret(2, 10)).expect("second");
    authority.activate(newer, None).expect("current candidate");
    let moved = authority.begin(Kind::Account, 1, 10).expect("third");
    let moved_generation = moved.generation;
    authority
        .remove(&[Kind::Account], None, None)
        .expect("remove");
    let late = io::write(&setup.store, SERVICE, moved, &secret(3, 10)).expect("third");
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

    let plan = authority.begin(Kind::Host, 0, 45).expect("begin");
    let verified = io::write(&setup.store, SERVICE, plan, &secret(1, 45)).expect("write");
    setup
        .raw()
        .execute("UPDATE heads SET revision = 5 WHERE kind = 'host'", [])
        .expect("another writer moved the head without clearing the pending row");
    assert!(matches!(
        authority.activate(verified, None),
        Err(AuthorityError::Conflict)
    ));
    let head = authority.current(Kind::Host).expect("host head");
    assert_eq!((head.revision, head.active), (5, None));
}

#[test]
fn a_candidate_from_before_a_restart_never_activates() {
    let setup = Setup::new("auth-restart");
    let mut authority = setup.open();
    let old = setup.save(&mut authority, Kind::Account, &secret(1, 30));
    let plan = authority.begin(Kind::Account, 1, 30).expect("begin");
    let verified = io::write(&setup.store, SERVICE, plan, &secret(2, 30)).expect("write");
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
        .remove(&[Kind::AnthropicKey], None, None)
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
    let clock = FakeClock::new();
    setup.store.0.lock().expect("fake lock").missed_reads = 20;
    settle(
        &mut authority,
        &setup.store,
        &clock,
        4_017,
        Duration::from_secs(2),
    )
    .expect("twenty misses fit the budget");
    assert_eq!(clock.elapsed(), Duration::from_millis(200));

    let clock = FakeClock::new();
    setup.store.0.lock().expect("fake lock").missed_reads = usize::MAX;
    assert!(matches!(
        settle(
            &mut authority,
            &setup.store,
            &clock,
            10,
            Duration::from_millis(50)
        ),
        Err(AuthorityError::Unsettled)
    ));
    assert_eq!(
        clock.elapsed(),
        Duration::from_millis(50),
        "it waits the budget, no more"
    );
    setup.store.0.lock().expect("fake lock").missed_reads = 0;

    let clock = FakeClock::new();
    assert!(matches!(
        settle(
            &mut authority,
            &Other(setup.store.clone()),
            &clock,
            10,
            Duration::from_secs(30)
        ),
        Err(AuthorityError::Unsettled)
    ));
    assert_eq!(
        clock.elapsed(),
        Duration::ZERO,
        "other bytes fail without waiting"
    );
    let plan = authority.begin(Kind::Account, 0, 10).expect("begin");
    assert!(matches!(
        io::write(&setup.store, SERVICE, plan, &secret(6, 11)),
        Err(AuthorityError::Bounds)
    ));
    assert_eq!(authority.current(Kind::Account).expect("head").active, None);
}

#[test]
fn the_settle_deadline_is_exact_and_shared_by_every_chunk() {
    let setup = Setup::new("auth-deadline");
    let mut authority = setup.open();
    // (ms each chunk first reads present, budget ms, outcome, ms waited); polls are 10 ms apart.
    for (visible, budget, expected, waited) in [
        (&[20][..], 25, "settled", 20),
        (&[25][..], 25, "unsettled", 25),
        (&[26][..], 25, "unsettled", 25),
        (&[60, 90][..], 100, "settled", 90),
        (&[60, 120][..], 100, "unsettled", 100),
    ] {
        let clock = FakeClock::new();
        let late = Late {
            store: &setup.store,
            clock: &clock,
            visible,
        };
        let result = settle(
            &mut authority,
            &late,
            &clock,
            2_000 * visible.len(),
            Duration::from_millis(budget),
        );
        let outcome = match result {
            Ok(()) => "settled",
            Err(AuthorityError::Unsettled) => "unsettled",
            Err(_) => "other",
        };
        assert_eq!(
            (outcome, clock.elapsed()),
            (expected, Duration::from_millis(waited)),
            "visible at {visible:?} within {budget} ms"
        );
    }
    assert_eq!(authority.current(Kind::Account).expect("head").active, None);
}

#[test]
fn a_removed_generation_is_swept_only_after_a_clean_read_in_a_later_open() {
    let setup = Setup::new("auth-sweep");
    let mut authority = setup.open();
    let active = setup.save(&mut authority, Kind::Host, &secret(1, 45));
    let before = setup.store.snapshot();
    authority.remove(&[Kind::Host], None, None).expect("remove");
    let retired = authority.retired(16).expect("retired")[0];
    assert_eq!(io::sweep(&setup.store, SERVICE, &retired), Observed::Absent);
    authority
        .observed(&retired, Observed::Absent)
        .expect("observed");
    assert_eq!(setup.removed(), 1, "same open isn't enough");
    assert!(
        authority.retired(16).expect("retired").is_empty(),
        "this open has swept it"
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
        setup.removed(),
        1,
        "the resurrection reset the clock to this open"
    );
    drop(authority);
    let mut authority = setup.open();
    assert_eq!(io::sweep(&setup.store, SERVICE, &retired), Observed::Absent);
    authority
        .observed(&retired, Observed::Absent)
        .expect("observed");
    assert_eq!(setup.removed(), 0, "now Swept");
    assert!(matches!(
        authority.observed(&retired, Observed::Absent),
        Err(AuthorityError::Conflict)
    ));
}

#[test]
fn a_bounded_sweep_reaches_every_retired_generation() {
    let setup = Setup::new("auth-sweep-cursor");
    let mut authority = setup.open();
    let mut active = None;
    for round in 0..33 {
        active = Some(setup.save(&mut authority, Kind::Account, &secret(round, 10)));
    }
    let first = authority.retired(16).expect("first pass");
    for retired in &first {
        assert_eq!(io::sweep(&setup.store, SERVICE, retired), Observed::Absent);
        authority
            .observed(retired, Observed::Absent)
            .expect("observed");
    }
    let second = authority.retired(16).expect("second pass");
    assert_eq!((first.len(), second.len()), (16, 16));
    assert!(
        second.iter().all(|retired| !first.contains(retired)),
        "the second pass moves past the first"
    );
    for retired in &second {
        assert_eq!(io::sweep(&setup.store, SERVICE, retired), Observed::Absent);
        authority
            .observed(retired, Observed::Absent)
            .expect("observed");
    }
    assert_eq!(
        setup.store.names(),
        names(&active.expect("saved")),
        "every retired generation's names were visited"
    );
    assert!(
        authority.retired(16).expect("end").is_empty(),
        "this open has visited them all"
    );
    drop(authority);
    let authority = setup.open();
    assert_eq!(
        authority.retired(16).expect("next open"),
        first,
        "the next open revisits what is still owed"
    );
}

/// CAPS-176 233-2-M1: 16 generations whose deletes are always refused can't hold back a 17th
/// across opens that each run a single pass.
#[test]
fn refused_sweeps_never_starve_a_later_generation_across_opens() {
    let setup = Setup::new("auth-sweep-fair");
    let mut authority = setup.open();
    for round in 0..18 {
        setup.save(&mut authority, Kind::Account, &secret(round, 10));
    }
    let owed = authority.retired(100).expect("every retired generation");
    assert_eq!(owed.len(), 17);
    setup.store.0.lock().expect("fake lock").refusing = true;
    for retired in &authority.retired(16).expect("first open's pass") {
        let observed = io::sweep(&setup.store, SERVICE, retired);
        assert_eq!(observed, Observed::Remains);
        authority.observed(retired, observed).expect("observed");
    }
    drop(authority);
    let authority = setup.open();
    let next = authority.retired(16).expect("second open's pass");
    assert_eq!(next.len(), 16);
    assert_eq!(
        next[0], owed[16],
        "the never-attempted generation comes first"
    );
    assert_eq!(next[1..], owed[..15], "then the longest waiting");
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
        .execute_batch("PRAGMA user_version = 4")
        .expect("newer");
    assert!(matches!(
        Authority::open(&setup.path(), &setup.lock),
        Err(AuthorityError::NewerSchema)
    ));
}

/// A version 1 file gains the legacy cleanup's columns, still owed, and keeps its rows exactly.
#[test]
fn a_version_one_file_is_migrated_with_the_legacy_cleanup_owed() {
    let setup = Setup::new("auth-migrate");
    let mut authority = setup.open();
    let value = secret(7, 45);
    let saved = setup.save(&mut authority, Kind::Host, &value);
    authority
        .set_marker(SignOutMarker::fail_closed())
        .expect("a nondefault marker");
    drop(authority);
    setup
        .raw()
        .execute_batch(
            "ALTER TABLE control DROP COLUMN legacy_done;
             ALTER TABLE control DROP COLUMN legacy_deleted_in;
             PRAGMA user_version = 1;",
        )
        .expect("a version 1 file");
    let authority = setup.open();
    assert!(authority.legacy_owed().expect("owed"));
    assert_eq!(
        authority.current(Kind::Host).expect("head").active,
        Some(saved)
    );
    let read = io::read(&setup.store, SERVICE, &saved).expect("read");
    assert!(
        read.expose() == value.expose(),
        "the saved bytes read back exactly"
    );
    assert_eq!(
        authority.marker().expect("marker"),
        SignOutMarker::fail_closed()
    );
    let version: i64 = setup
        .raw()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 3);
}

/// The legacy cleanup ends only after a clean pass in any open after the first pass's open.
#[test]
fn the_legacy_cleanup_ends_after_a_clean_pass_in_a_later_open() {
    let setup = Setup::new("auth-legacy");
    let mut authority = setup.open();
    assert!(authority.legacy_owed().expect("owed"));
    authority.legacy_deleted(0).expect("first clean pass");
    assert!(authority.legacy_owed().expect("one open isn't enough"));
    drop(authority);
    let mut authority = setup.open();
    authority.legacy_deleted(1).expect("a refused pass");
    assert!(authority.legacy_owed().expect("a refusal never ends it"));
    drop(authority);
    let mut authority = setup.open();
    authority.legacy_deleted(0).expect("a later clean pass");
    assert!(!authority.legacy_owed().expect("done"));
    drop(authority);
    assert!(!setup.open().legacy_owed().expect("done for good"));
}

/// A second pass in the same open never ends the cleanup; the next open's clean pass does.
#[test]
fn two_clean_passes_in_one_open_leave_the_legacy_cleanup_owed() {
    let setup = Setup::new("auth-legacy-same-open");
    let mut authority = setup.open();
    authority.legacy_deleted(0).expect("first clean pass");
    authority.legacy_deleted(0).expect("second clean pass");
    assert!(authority.legacy_owed().expect("the same open isn't later"));
    drop(authority);
    let mut authority = setup.open();
    authority.legacy_deleted(0).expect("a later clean pass");
    assert!(!authority.legacy_owed().expect("done"));
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
        .remove(&[Kind::Account, Kind::Revocations], Some(promoted), None)
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
        account_fenced: false,
        refuse_rollback: false,
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
    let verified = io::write(&setup.store, SERVICE, plan, &secret(b'S', 4)).expect("write");
    let (active, _) = authority.activate(verified, None).expect("activate");
    let shown = format!("{active:?} {:?}", authority.retired(16).expect("retired"));
    assert!(
        !shown.contains(&hex) && !shown.contains("SSSS"),
        "no secret or generation"
    );
}
