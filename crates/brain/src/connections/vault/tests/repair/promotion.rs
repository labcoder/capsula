use super::*;

use std::sync::{Arc, mpsc};
use std::time::Duration;

use kumochi_protocol::message::{EventId, RunId, ThreadId};

use super::exchange::admitted;
use crate::connections::clock::FixedClock;
use crate::connections::oauth::exchange::authorization;
use crate::connections::oauth::id_token::{FixtureSignature, repair_exchange_fixture};
use crate::connections::vault::repair::RepairAttempt;
use crate::connections::vault::repair::exchange::RepairSettlement;
use crate::connections::vault::repair::promotion::{
    CutoffReceipt, CutoffRefused, PendingReplacement, RepairProof, VerifiedReplacement,
};
use crate::connections::vault::{CutoffRequest, HistoryLink};
use crate::coordinator::StoreOp;
use crate::runtime::{Attempt, Schedule, StoreThread};
use crate::store::{Boundary, Cancel, Copied, RunEnd, Store, StoreError, Swap};

const NOW: u64 = 1_800_000_000;
const PLAN: &str = "chatgpt.tokens.use.direct";

async fn settled_proof(
    vault: &mut Vault<FakeStore>,
    attempt: &mut RepairAttempt,
    script: &impl crate::connections::http_client::Http,
) -> RepairProof {
    let dispatch = vault
        .charge_repair_exchange(attempt, authorization())
        .unwrap();
    let mut receipt = dispatch.receive(script, &FixedClock(NOW)).await;
    let RepairSettlement::Escrowed(Some(proof)) =
        vault.settle_repair_exchange(attempt, &mut receipt).unwrap()
    else {
        panic!("the exact live settlement must release the signed proof");
    };
    proof
}

fn escrow_name(path: &Path, service: &str) -> String {
    let generation: String = raw(path)
        .query_row(
            "SELECT lower(hex(credential)) FROM chatgpt_repair WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    generation_target(service, "repair", &generation, 0)
}

/// The profile's one history writer on its own thread, as the store thread owns it.
struct HistoryOwner {
    serve: Option<mpsc::Sender<Serve>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum Serve {
    Cutoff(CutoffRequest),
    Run(Box<dyn FnOnce(&mut Store) + Send>),
}

impl HistoryOwner {
    fn open(history: &Path) -> Self {
        let mut store = Store::open(history).unwrap();
        let (serve, requests) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            for request in requests {
                match request {
                    Serve::Cutoff(request) => store.serve_cutoff(request),
                    Serve::Run(job) => job(&mut store),
                }
            }
        });
        Self {
            serve: Some(serve),
            thread: Some(thread),
        }
    }

    fn link(&self) -> HistoryLink {
        let serve = self.serve.clone().unwrap();
        HistoryLink::new(move |request| serve.send(Serve::Cutoff(request)).is_ok())
    }

    fn check(&self, tag: u64) -> Result<Boundary, StoreError> {
        self.link().check(tag)
    }

    fn run<T: Send + 'static>(&self, job: impl FnOnce(&mut Store) -> T + Send + 'static) -> T {
        let (done, result) = mpsc::channel();
        self.serve
            .as_ref()
            .unwrap()
            .send(Serve::Run(Box::new(move |store| {
                done.send(job(store)).unwrap();
            })))
            .unwrap();
        result.recv().unwrap()
    }
}

/// Joins the writer once every link has gone, so its store is closed; drop the vault first.
impl Drop for HistoryOwner {
    fn drop(&mut self) {
        drop(self.serve.take());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn bound(store: &FakeStore, service: &str, path: &Path, owner: &HistoryOwner) -> Vault<FakeStore> {
    Vault::bound_to_history(store.clone(), service, path, owner.link()).expect("test vault lock")
}

async fn bound_replacement(
    vault: &mut Vault<FakeStore>,
    store: &FakeStore,
) -> (RepairAttempt, VerifiedReplacement) {
    let (_, mut attempt) = admitted(vault, store);
    let script = repair_exchange_fixture(
        "issued-client",
        "expected-nonce",
        NOW + 90,
        FixtureSignature::Valid,
        PLAN,
    );
    let proof = settled_proof(vault, &mut attempt, &script).await;
    let verified = proof.verify(&script, &FixedClock(NOW)).await.unwrap();
    (attempt, verified)
}

#[tokio::test]
async fn an_exact_pending_replacement_can_establish_one_history_cutoff() {
    let (_scratch, path, service, store) = setup("repair-history-cutoff");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (successor, tag) = pending.binding();
    assert!(matches!(owner.check(tag), Err(StoreError::RecoveryContext)));
    let pending = returned(vault.establish_history_cutoff(pending));
    assert_eq!(history_version(&history), (13, 1));

    let old = owner.run(plant_history);
    let pending = assert_sequences_cover_history(&vault, pending, &history);
    assert_eq!(
        owner.run(|store| store.high_water().unwrap()),
        (ThreadId(9), RunId(11))
    );
    let receipt = vault.establish_history_cutoff(pending).unwrap();
    let cut = owner.check(tag).unwrap();
    assert_cut_bounds(cut, old);
    owner.run(|store| {
        store
            .begin_turn(
                ThreadId(10),
                Some("New task"),
                RunId(12),
                "postcutoffmarker",
            )
            .unwrap();
    });
    establish_and_confirm(&mut vault, receipt, cut);
    assert_established_row(&path, &successor.hex(), tag);
    let wrong_tag = if tag == 1 { 2 } else { 1 };
    assert!(matches!(
        owner.check(wrong_tag),
        Err(StoreError::RecoveryContext)
    ));
    assert_retained_history_and_refuse_broken_cutoff(&owner, &history, tag, old);
    assert!(matches!(
        vault.established_repair_context(),
        Err(VaultError::Fenced)
    ));
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    drop(attempt);
    drop(vault);
    let established: bool = raw(&path)
        .query_row(
            "SELECT context_established FROM chatgpt_repair",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(established);
}

#[tokio::test]
async fn a_cutoff_receipt_needs_its_own_row_from_an_independent_check() {
    for edit in [
        "UPDATE chatgpt_recovery_context SET last_run=10 WHERE tag=?1",
        "DELETE FROM chatgpt_recovery_context WHERE tag=?1",
    ] {
        let (_scratch, path, service, store) = setup("repair-cutoff-receipt");
        let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
        let owner = HistoryOwner::open(&history);
        let mut vault = bound(&store, &service, &path, &owner);
        let (attempt, verified) = bound_replacement(&mut vault, &store).await;
        owner.run(plant_history);
        let pending = vault
            .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
            .unwrap();
        let (_, tag) = pending.binding();
        let receipt = vault.establish_history_cutoff(pending).unwrap();
        assert_eq!(owner.check(tag).unwrap().bounds(), (9, 11));
        let db = Connection::open(&history).unwrap();
        assert_eq!(db.execute(edit, [tag.to_be_bytes()]).unwrap(), 1, "{edit}");
        assert!(
            matches!(
                vault.establish_repair_context(receipt),
                Err(VaultError::Fenced)
            ),
            "{edit}"
        );
        assert!(matches!(
            vault.established_repair_context(),
            Err(VaultError::Authority)
        ));
        assert_eq!(context_state(&path), (false, None, None), "{edit}");
    }
}

#[tokio::test]
async fn a_refused_context_commit_is_never_read_back_as_established() {
    for rollback_refused in [false, true] {
        let (_scratch, path, service, store) = setup("repair-context-commit");
        let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
        let owner = HistoryOwner::open(&history);
        let mut vault = bound(&store, &service, &path, &owner);
        let (attempt, verified) = bound_replacement(&mut vault, &store).await;
        owner.run(plant_history);
        let pending = vault
            .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
            .unwrap();
        let receipt = vault.establish_history_cutoff(pending).unwrap();
        raw(&path)
            .execute_batch(
                "CREATE TABLE refuse_commit (
                    repair_id INTEGER REFERENCES chatgpt_repair(id) DEFERRABLE INITIALLY DEFERRED
                );
                CREATE TRIGGER refuse_establish AFTER UPDATE OF context_established ON chatgpt_repair
                WHEN NEW.context_established=1 BEGIN INSERT INTO refuse_commit VALUES (2); END;",
            )
            .unwrap();
        vault.authority.refuse_rollback = rollback_refused;
        assert!(matches!(
            vault.establish_repair_context(receipt),
            Err(VaultError::Authority)
        ));
        assert_eq!(vault.authority.in_transaction(), rollback_refused);
        assert_eq!(context_state(&path), (false, None, None));
        assert!(
            matches!(
                vault.established_repair_context(),
                Err(VaultError::Authority)
            ),
            "{rollback_refused}"
        );
        drop(attempt);
        drop(vault);
        assert_eq!(context_state(&path), (false, None, None));
    }
}

#[tokio::test]
async fn a_copy_taken_before_establish_cannot_remove_the_cutoff() {
    let (_scratch, path, service, store) = setup("repair-cutoff-purge");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    owner.run(plant_history);
    let early = owner.run(|store| clean_copy(store));
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (_, tag) = pending.binding();
    let receipt = vault.establish_history_cutoff(pending).unwrap();
    let cut = owner.check(tag).unwrap();
    assert!(matches!(
        owner.run(move |store| store.swap(&early).unwrap()),
        Swap::Stale
    ));
    owner.run(|store| store.leftovers().remove().unwrap());
    assert_eq!(owner.check(tag).unwrap(), cut);
    vault.establish_repair_context(receipt).unwrap();
    assert_eq!(vault.established_repair_context().unwrap().tag(), tag);

    let swapped = owner.run(|store| {
        let copied = clean_copy(store);
        store.swap(&copied).unwrap()
    });
    assert!(matches!(swapped, Swap::Done));
    assert!(!owner.run(|store| store.purge_pending()));
    assert_eq!(history_version(&history), (13, 1));
    assert_eq!(owner.check(tag).unwrap(), cut);
    assert_eq!(vault.established_repair_context().unwrap().tag(), tag);
    assert_eq!(context_state(&path), (true, Some(9), Some(11)));
    drop(attempt);
}

#[tokio::test]
async fn a_replacement_token_is_spent_only_through_its_issuing_vault() {
    let (_scratch, path, service, store) = setup("repair-cutoff-issuer");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    let (_other_scratch, other_path, other_service, other_store) = setup("repair-cutoff-other");
    let other_history = other_path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let other_owner = HistoryOwner::open(&other_history);
    let other = bound(&other_store, &other_service, &other_path, &other_owner);
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    owner.run(plant_history);
    other_owner.run(plant_history);
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (_, tag) = pending.binding();
    let pending = returned(other.establish_history_cutoff(pending));
    assert_eq!(history_version(&other_history), (13, 1));
    assert!(matches!(
        other_owner.check(tag),
        Err(StoreError::RecoveryContext)
    ));
    let receipt = vault.establish_history_cutoff(pending).unwrap();
    assert_eq!(owner.check(tag).unwrap().bounds(), (9, 11));
    vault.establish_repair_context(receipt).unwrap();
    assert_eq!(vault.established_repair_context().unwrap().tag(), tag);
    assert_eq!(history_version(&other_history), (13, 1));
    drop(attempt);
    drop(other);
}

#[tokio::test]
async fn a_refused_history_commit_spends_the_binding() {
    let (_scratch, path, service, store) = setup("repair-cutoff-commit");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    owner.run(plant_history);
    Connection::open(&history)
        .unwrap()
        .execute_batch(
            "CREATE TABLE refuse_cutoff (
                thread INTEGER REFERENCES threads(id) DEFERRABLE INITIALLY DEFERRED
            );
            CREATE TRIGGER refuse_cutoff AFTER INSERT ON chatgpt_recovery_context
            BEGIN INSERT INTO refuse_cutoff VALUES (999999); END;",
        )
        .unwrap();
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (_, tag) = pending.binding();
    let Err(CutoffRefused {
        pending: None,
        error: StoreError::Sql(_),
    }) = vault.establish_history_cutoff(pending)
    else {
        panic!("a refused commit spends the binding");
    };
    assert!(matches!(owner.check(tag), Err(StoreError::RecoveryContext)));
    let rows: i64 = Connection::open(&history)
        .unwrap()
        .query_row("SELECT count(*) FROM chatgpt_recovery_context", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
    assert!(matches!(
        vault.established_repair_context(),
        Err(VaultError::Authority)
    ));
    drop(attempt);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one cutoff scenario through the real store thread, kept in order"
)]
async fn the_store_thread_writes_the_cutoff_and_its_purge_carries_it() {
    let (_scratch, path, service, store) = setup("repair-cutoff-store-thread");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let mut writer = Store::open(&history).unwrap();
    writer.recover().unwrap();
    for (thread, run) in [(5, 7), (9, 11)] {
        writer
            .begin_turn(ThreadId(thread), Some("Prior task"), RunId(run), "prior")
            .unwrap();
        writer
            .end_run(ThreadId(thread), RunId(run), &[], RunEnd::Done)
            .unwrap();
    }
    let (acks, mut replies) = tokio::sync::mpsc::unbounded_channel();
    let (heard, attempts) = mpsc::channel();
    let custody = crate::test_support::custody(&writer);
    let thread = StoreThread::spawn(
        writer,
        custody,
        acks,
        Schedule {
            quiet: Duration::from_millis(1),
            first_retry: Duration::from_millis(20),
            last_retry: Duration::from_millis(20),
            free_space: |_| Ok(u64::MAX),
            attempts: Some(heard),
            ..Schedule::default()
        },
    )
    .unwrap();
    let mut vault =
        Vault::bound_to_history(store.clone(), &service, &path, thread.history_link()).unwrap();
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (_, tag) = pending.binding();
    let receipt = vault.establish_history_cutoff(pending).unwrap();
    vault.establish_repair_context(receipt).unwrap();
    let cut = vault.established_repair_context().unwrap();
    assert_eq!(cut.tag(), tag);
    // A read sent once the cutoff's reply is back finds its row, though cutoff ops take no stamp.
    for (require, kept_out) in [
        (cut, true),
        (crate::store::Boundary::fixture(tag, 9, 10), false),
    ] {
        assert!(thread.send(StoreOp::ReadJob(crate::coordinator::ReadJob {
            require_recovery: Some(require),
            control: crate::recall::control::Control::new(
                std::time::Instant::now() + Duration::from_secs(30),
            ),
            key: crate::coordinator::JobKey {
                thread: ThreadId(9),
                serial: 1,
            },
            kind: crate::coordinator::JobKind::Title,
            origin: crate::context::Origin::Task(ThreadId(9)),
            snapshot: Some(crate::context::RunSnapshot {
                origin: crate::context::Origin::Task(ThreadId(9)),
                assistant: kumochi_protocol::message::AssistantName::new("Kumo").unwrap(),
                user: None,
                choice: None,
                connection: crate::context::ConnectionTag(tag),
            }),
            compact_at: crate::context::compact::COMPACT_AT,
        })));
        let Some(crate::coordinator::Input::JobBuilt(built)) = replies.recv().await else {
            panic!("a job read answers");
        };
        if kept_out {
            assert!(
                matches!(built.result, Ok(None)),
                "the read finds the row and keeps thread 9 out"
            );
        } else {
            assert!(
                matches!(built.result, Err(crate::coordinator::Unbuilt::Failed)),
                "a cutoff that differs from the row refuses"
            );
        }
    }

    assert!(thread.send(StoreOp::Delete {
        thread: ThreadId(9)
    }));
    loop {
        match attempts.recv_timeout(Duration::from_secs(30)).unwrap() {
            Attempt::Done => break,
            Attempt::Busy | Attempt::Stale | Attempt::Blocked => {}
            other => panic!("the purge didn't finish: {other:?}"),
        }
    }
    assert_eq!(history_version(&history), (13, 1));
    assert_eq!(vault.established_repair_context().unwrap().tag(), tag);
    assert_eq!(context_state(&path), (true, Some(9), Some(11)));
    raw(&path)
        .execute("UPDATE chatgpt_repair SET context_last_run=10", [])
        .unwrap();
    assert!(
        matches!(vault.established_repair_context(), Err(VaultError::Fenced)),
        "recorded bounds that differ from the row hand over no cutoff"
    );
    drop(attempt);
    drop(vault);
    thread.close().await.unwrap();
}

fn clean_copy(store: &Store) -> Copied {
    store
        .owed()
        .unwrap()
        .expect("a purge is owed and no run is active")
        .copy(&Arc::new(Cancel::default()))
        .unwrap()
        .expect("nothing stopped the copy")
}

fn assert_established_row(path: &Path, successor: &str, tag: u64) {
    let persisted = raw(path)
        .query_row(
            "SELECT lower(hex(context_successor)), context_tag, context_established,
         context_last_thread, context_last_run FROM chatgpt_repair",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        persisted,
        (
            successor.to_owned(),
            i64::try_from(tag).unwrap(),
            true,
            9,
            11
        )
    );
}

fn context_state(path: &Path) -> (bool, Option<i64>, Option<i64>) {
    raw(path)
        .query_row(
            "SELECT context_established, context_last_thread, context_last_run
             FROM chatgpt_repair",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

fn assert_cut_bounds(cut: Boundary, old: ThreadId) {
    assert!(!cut.permits_thread(old));
    assert!(!cut.permits_thread(ThreadId(9)));
    assert!(cut.permits_thread(ThreadId(10)));
    assert!(!cut.permits_run(RunId(7)));
    assert!(!cut.permits_run(RunId(11)));
    assert!(cut.permits_run(RunId(12)));
    let largest = i64::MAX.unsigned_abs();
    assert!(cut.permits_thread(ThreadId(largest)));
    assert!(cut.permits_run(RunId(largest)));
    assert!(!cut.permits_thread(ThreadId(largest + 1)));
    assert!(!cut.permits_run(RunId(largest + 1)));
}

fn establish_and_confirm(vault: &mut Vault<FakeStore>, receipt: CutoffReceipt, cut: Boundary) {
    assert!(matches!(
        vault.established_repair_context(),
        Err(VaultError::Authority)
    ));
    vault.establish_repair_context(receipt).unwrap();
    assert_eq!(vault.established_repair_context().unwrap(), cut);
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
}

/// Each refusal before the commit hands the binding back unspent.
fn assert_sequences_cover_history(
    vault: &Vault<FakeStore>,
    mut pending: PendingReplacement,
    history: &Path,
) -> PendingReplacement {
    let db = Connection::open(history).unwrap();
    for (edit, restore) in [
        (
            "DELETE FROM sqlite_sequence WHERE name='runs'",
            "INSERT INTO sqlite_sequence(name, seq) VALUES ('runs', 11)",
        ),
        (
            "UPDATE sqlite_sequence SET seq=6 WHERE name='runs'",
            "UPDATE sqlite_sequence SET seq=11 WHERE name='runs'",
        ),
        (
            "UPDATE sqlite_sequence SET seq=4 WHERE name='threads'",
            "UPDATE sqlite_sequence SET seq=9 WHERE name='threads'",
        ),
    ] {
        assert_eq!(db.execute(edit, []).unwrap(), 1);
        pending = returned(vault.establish_history_cutoff(pending));
        assert_eq!(history_version(history), (13, 1), "{edit}");
        assert_eq!(db.execute(restore, []).unwrap(), 1);
    }
    assert_eq!(
        db.execute("UPDATE sqlite_sequence SET seq=NULL WHERE name='runs'", [])
            .unwrap(),
        1
    );
    let Err(CutoffRefused {
        pending: Some(pending),
        error: StoreError::Sql(rusqlite::Error::InvalidColumnType(..)),
    }) = vault.establish_history_cutoff(pending)
    else {
        panic!("a NULL sequence refuses before the commit");
    };
    assert_eq!(history_version(history), (13, 1));
    assert_eq!(
        db.execute("UPDATE sqlite_sequence SET seq=11 WHERE name='runs'", [])
            .unwrap(),
        1
    );
    pending
}

/// The binding a refusal before the commit hands back.
fn returned(result: Result<CutoffReceipt, CutoffRefused>) -> PendingReplacement {
    let Err(CutoffRefused {
        pending: Some(pending),
        error: StoreError::RecoveryContext,
    }) = result
    else {
        panic!("a refusal before the commit returns the binding");
    };
    pending
}

fn assert_retained_history_and_refuse_broken_cutoff(
    owner: &HistoryOwner,
    history: &Path,
    tag: u64,
    old: ThreadId,
) {
    assert_eq!(history_version(history), (13, 1));
    assert_eq!(
        owner.run(|store| store.pages().search_ids("precutoffmarker").unwrap()),
        vec![old]
    );
    let db = Connection::open(history).unwrap();
    let summary: String = db
        .query_row("SELECT text FROM summaries WHERE thread = 5", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(summary, "Retained summary");
    db.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    assert_eq!(
        db.execute(
            "UPDATE chatgpt_recovery_context SET last_run=-1 WHERE tag=?1",
            [tag.to_be_bytes()]
        )
        .unwrap(),
        1
    );
    assert!(matches!(owner.check(tag), Err(StoreError::RecoveryContext)));
    assert_eq!(
        db.execute(
            "DELETE FROM chatgpt_recovery_context WHERE tag=?1",
            [tag.to_be_bytes()]
        )
        .unwrap(),
        1
    );
    drop(db);
    drop(Store::open(history).unwrap());
    assert!(matches!(owner.check(tag), Err(StoreError::RecoveryContext)));
    assert_eq!(history_version(history), (13, 1));
}

fn plant_history(writer: &mut Store) -> ThreadId {
    let kept = ThreadId(5);
    writer.recover().unwrap();
    writer
        .begin_turn(kept, Some("Prior task"), RunId(7), "precutoffmarker")
        .unwrap();
    writer.end_run(kept, RunId(7), &[], RunEnd::Done).unwrap();
    assert!(
        writer
            .store_summary(
                kept,
                crate::store::Generation(0),
                (EventId(1), EventId(1)),
                "Retained summary",
                None
            )
            .unwrap()
    );
    let deleted = ThreadId(9);
    writer
        .begin_turn(deleted, Some("Deleted task"), RunId(11), "deleted")
        .unwrap();
    writer
        .end_run(deleted, RunId(11), &[], RunEnd::Done)
        .unwrap();
    writer.begin_delete(deleted).unwrap();
    writer.finish_delete(deleted).unwrap();
    kept
}

fn history_version(history: &Path) -> (i64, i64) {
    Connection::open(history)
        .unwrap()
        .query_row(
            "SELECT (SELECT user_version FROM pragma_user_version),
             (SELECT count(*) FROM sqlite_schema WHERE name='chatgpt_recovery_context')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn a_signed_escrow_commits_only_an_exact_locked_binding_after_restart() {
    let (_scratch, path, service, store) = setup("repair-bound-restart");
    let mut vault = open(&store, &service, &path);
    vault.save_anthropic_key(b"other-connection").unwrap();
    let (old, mut attempt) = admitted(&mut vault, &store);

    let expiry = NOW + 90;
    let script = repair_exchange_fixture(
        "issued-client",
        "expected-nonce",
        expiry,
        FixtureSignature::Valid,
        PLAN,
    );
    let proof = settled_proof(&mut vault, &mut attempt, &script).await;
    let verified = proof.verify(&script, &FixedClock(NOW)).await.unwrap();
    let (successor, tag) = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap()
        .binding();
    assert!(tag > 0);
    assert_ne!(successor, old.active.unwrap().generation);
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    assert_eq!(
        vault
            .read_bytes(Kind::AnthropicKey)
            .unwrap()
            .unwrap()
            .expose()
            .as_slice(),
        b"other-connection"
    );
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    assert!(!history.exists());

    let names = store.names();
    drop(attempt);
    drop(vault);
    assert!(matches!(
        Vault::with_service(store.clone(), &service, &path),
        Err(VaultError::Authority)
    ));
    assert_eq!(store.names(), names);
    let conn = raw(&path);
    let row: (String, i64, bool, bool, bool, i64) = conn
        .query_row(
            "SELECT lower(hex(r.context_successor)), r.context_tag, r.context_established,
          r.fresh_text_only, r.context_successor=g.generation, h.revision
         FROM chatgpt_repair r JOIN generations g ON g.kind='chatgpt' AND g.state='active'
         JOIN heads h ON h.kind=g.kind WHERE r.phase='idle' AND r.fenced IS NULL",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        row,
        (
            successor.hex(),
            i64::try_from(tag).unwrap(),
            false,
            true,
            true,
            i64::try_from(old.revision + 1).unwrap()
        )
    );
    let retired: i64 = conn
        .query_row(
            "SELECT count(*) FROM generations WHERE state='removed' AND
         ((kind='chatgpt' AND lower(hex(generation))=?1) OR kind='repair')",
            [old.active.unwrap().generation.hex()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired, 3);
    let len: i64 = conn
        .query_row(
            "SELECT bytes FROM generations WHERE kind='chatgpt' AND state='active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active = authority::ActiveRef {
        kind: Kind::Account,
        generation: successor,
        layout: authority::Layout::for_len(usize::try_from(len).unwrap()).unwrap(),
    };
    let persisted = generation::read_active(&store, &service, &active).unwrap();
    let persisted: Account = decode(&persisted, MAX_RECORD_BYTES).unwrap();
    let tokens = persisted.tokens.unwrap();
    assert_eq!(persisted.subject.unwrap().as_str(), "user-test-subject");
    assert_eq!(tokens.connection, tag);
    assert_eq!(tokens.expires_at, expiry);
    assert!(!history.exists());
}

#[tokio::test]
async fn bad_signature_nonce_scope_or_expiry_keeps_the_issued_handle_in_escrow() {
    for (name, nonce, expiry, signature, scope) in [
        (
            "signature",
            "expected-nonce",
            NOW + 90,
            FixtureSignature::WrongKey,
            PLAN,
        ),
        (
            "nonce",
            "wrong-nonce",
            NOW + 90,
            FixtureSignature::Valid,
            PLAN,
        ),
        (
            "scope",
            "expected-nonce",
            NOW + 90,
            FixtureSignature::Valid,
            "openid",
        ),
        (
            "expiry",
            "expected-nonce",
            NOW,
            FixtureSignature::Valid,
            PLAN,
        ),
    ] {
        let (_scratch, path, service, store) = setup(name);
        let mut vault = open(&store, &service, &path);
        let (_, mut attempt) = admitted(&mut vault, &store);
        let script = repair_exchange_fixture("issued-client", nonce, expiry, signature, scope);
        let proof = settled_proof(&mut vault, &mut attempt, &script).await;
        let name = escrow_name(&path, &service);
        assert!(proof.verify(&script, &FixedClock(NOW)).await.is_none());
        assert!(
            store
                .get(&name)
                .unwrap()
                .unwrap()
                .expose()
                .ends_with(b"replacement-refresh-SENTINEL")
        );
        assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    }
}

#[derive(Debug)]
enum Veto {
    Cancel,
    Expiry,
    Escrow,
    Commit,
    StagedBytes,
}

#[tokio::test]
async fn a_verified_candidate_stays_unbound_when_the_final_checks_or_commit_refuse() {
    for veto in [
        Veto::Cancel,
        Veto::Expiry,
        Veto::Escrow,
        Veto::Commit,
        Veto::StagedBytes,
    ] {
        let (_scratch, path, service, store) = setup("repair-binding-veto");
        let mut vault = open(&store, &service, &path);
        let (old, mut attempt) = admitted(&mut vault, &store);
        let script = repair_exchange_fixture(
            "issued-client",
            "expected-nonce",
            NOW + 90,
            FixtureSignature::Valid,
            PLAN,
        );
        let proof = settled_proof(&mut vault, &mut attempt, &script).await;
        let verified = proof.verify(&script, &FixedClock(NOW)).await.unwrap();
        let conn = raw(&path);
        let escrow = escrow_name(&path, &service);
        match veto {
            Veto::Cancel => vault.cancel_repair_admission(&attempt).unwrap(),
            Veto::Expiry => {}
            Veto::Escrow => store.delete(&escrow).unwrap(),
            Veto::Commit => conn
                .execute_batch(
                    "CREATE TABLE refuse_commit (
                        repair_id INTEGER REFERENCES chatgpt_repair(id) DEFERRABLE INITIALLY DEFERRED
                    );
                    CREATE TRIGGER refuse_bind AFTER UPDATE OF context_tag ON chatgpt_repair
                    WHEN NEW.context_tag IS NOT NULL BEGIN INSERT INTO refuse_commit VALUES (2); END;",
                )
                .unwrap(),
            Veto::StagedBytes => store.change_after_next_write_readback(),
        }
        let now = if matches!(veto, Veto::Expiry) {
            NOW + 90
        } else {
            NOW
        };
        let result = vault.bind_repair_replacement(&attempt, verified, &FixedClock(now));
        match veto {
            Veto::Cancel | Veto::Expiry => assert!(matches!(result, Err(VaultError::Fenced))),
            Veto::Escrow => assert!(matches!(
                result,
                Err(VaultError::Store(CredentialStoreError::Unsettled))
            )),
            Veto::Commit | Veto::StagedBytes => {
                assert!(matches!(result, Err(VaultError::Authority)), "{veto:?}");
                let pending: String = conn
                    .query_row(
                        "SELECT lower(hex(g.generation)) FROM chatgpt_repair r
                         JOIN generations g ON g.generation=r.candidate
                         WHERE g.kind='chatgpt' AND g.state='pending'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(
                    store
                        .names()
                        .contains(&generation_target(&service, "chatgpt", &pending, 0))
                );
            }
        }
        assert_escrow_unchanged(&conn, old, &service, &escrow);
        if !matches!(veto, Veto::Escrow) {
            assert!(
                store
                    .get(&escrow)
                    .unwrap()
                    .unwrap()
                    .expose()
                    .ends_with(b"replacement-refresh-SENTINEL")
            );
        }
        assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    }
}

fn assert_escrow_unchanged(
    conn: &Connection,
    old: authority::Current,
    service: &str,
    escrow: &str,
) {
    let (phase, unbound, account, revision, repair): (String, bool, String, i64, String) = conn
        .query_row(
            "SELECT r.phase, r.context_tag IS NULL AND r.context_successor IS NULL,
                lower(hex(g.generation)), h.revision, lower(hex(e.generation))
             FROM chatgpt_repair r JOIN generations g ON g.kind='chatgpt' AND g.state='active'
             JOIN heads h ON h.kind=g.kind
             JOIN generations e ON e.generation=r.credential AND e.kind='repair' AND e.state='active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!(phase, "escrow");
    assert!(unbound);
    assert_eq!(account, old.active.unwrap().generation.hex());
    assert_eq!(revision, i64::try_from(old.revision).unwrap());
    assert_eq!(generation_target(service, "repair", &repair, 0), escrow);
}

/// A pending link queues nothing until it's bound, then only to the first writer it was bound to,
/// through every clone. Once released, by its binding or by dropping that binding, it queues
/// nothing again, and a later bind changes nothing.
#[test]
fn a_pending_link_queues_only_while_bound_to_its_first_writer() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let writer = |heard: &Arc<AtomicUsize>| {
        let heard = Arc::clone(heard);
        HistoryLink::new(move |request| {
            drop(request);
            heard.fetch_add(1, Ordering::SeqCst);
            true
        })
    };
    let heard = |count: &Arc<AtomicUsize>| count.load(Ordering::SeqCst);
    let refused = |link: &HistoryLink| matches!(link.check(7), Err(StoreError::RecoveryContext));
    for drop_binding in [false, true] {
        let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (link, binding) = HistoryLink::pending();
        assert!(refused(&link));
        assert_eq!((heard(&first), heard(&second)), (0, 0));
        binding.bind(&writer(&first));
        binding.bind(&writer(&second));
        assert!(refused(&link), "a dropped request is answered as refused");
        assert!(refused(&link.clone()));
        assert_eq!((heard(&first), heard(&second)), (2, 0));
        if drop_binding {
            drop(binding);
        } else {
            binding.release();
            binding.bind(&writer(&second));
        }
        assert!(refused(&link));
        assert_eq!((heard(&first), heard(&second)), (2, 0));
    }
}

/// A sign-in is recovered only as the exact established replacement under its own connection
/// while it isn't signing out. The ordinary schema is ordinary; every other repaired state,
/// a fenced predecessor whose admission was cancelled included, proves nothing.
#[tokio::test]
async fn a_sign_in_is_recovered_only_as_the_exact_established_replacement() {
    use crate::connections::marker::{Disposition, Phase, SignOut};
    use crate::connections::vault::Proof;

    let (_scratch, path, service, store) = setup("repair-proof");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    assert_eq!(vault.proof(7), Proof::Ordinary, "the ordinary schema");
    let (_, cancelled) = admitted(&mut vault, &store);
    assert_eq!(vault.proof(7), Proof::Unproved, "an admitted repair");
    vault.cancel_repair_admission(&cancelled).unwrap();
    assert_eq!(vault.proof(7), Proof::Unproved, "a fenced predecessor");
    drop(cancelled);
    drop(vault);

    let (_scratch, path, service, store) = setup("repair-proof-established");
    let history = path.with_file_name(kumochi_protocol::constants::STORE_FILE);
    let owner = HistoryOwner::open(&history);
    let mut vault = bound(&store, &service, &path, &owner);
    let (attempt, verified) = bound_replacement(&mut vault, &store).await;
    owner.run(plant_history);
    let pending = vault
        .bind_repair_replacement(&attempt, verified, &FixedClock(NOW))
        .unwrap();
    let (_, tag) = pending.binding();
    assert_eq!(vault.proof(tag), Proof::Unproved, "bound, no cutoff yet");
    let receipt = vault.establish_history_cutoff(pending).unwrap();
    let cut = owner.check(tag).unwrap();
    assert_eq!(
        vault.proof(tag),
        Proof::Unproved,
        "cutoff written, not established"
    );
    vault.establish_repair_context(receipt).unwrap();
    assert_eq!(vault.proof(tag), Proof::Recovered(cut));
    assert_eq!(vault.proof(tag + 1), Proof::Unproved, "another connection");
    raw(&path)
        .execute("UPDATE chatgpt_repair SET recovered_sign_out=1", [])
        .unwrap();
    assert_eq!(vault.proof(tag), Proof::Unproved, "its row is signing out");
    raw(&path)
        .execute("UPDATE chatgpt_repair SET recovered_sign_out=0", [])
        .unwrap();
    assert_eq!(vault.proof(tag), Proof::Recovered(cut));
    let mut marker = vault.authority.marker().unwrap();
    marker.sign_out = Some(SignOut {
        disposition: Disposition::Keep,
        phase: Phase::Revoking,
    });
    vault.authority.set_marker(marker).unwrap();
    assert_eq!(vault.proof(tag), Proof::Unproved, "a sign-out is pending");
    drop(attempt);
    drop(vault);
}
