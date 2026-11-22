use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kumochi_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use crate::store::{Cancel, Copied, Pages, RunEnd, Store, StoreError, Swap};
use crate::test_support::Scratch;

#[test]
fn controlled_slot_and_swap_waits_end_before_the_block_is_released() {
    use crate::recall::{Error, control::Control};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    for swapping in [false, true] {
        for cause in 0..3 {
            let scratch = Scratch::new("recall-side-waits");
            let store = owing(&scratch);
            let side = store.side();
            let held = (!swapping).then(|| side.reader.lock().expect("slot"));
            if swapping {
                side.begin_swap();
            }
            let control = Control::new(Instant::now() + Duration::from_secs(60));
            let (reply, result) = mpsc::channel();
            let worker = {
                let side = Arc::clone(&side);
                let control = control.clone();
                std::thread::spawn(move || {
                    reply
                        .send(side.read_until::<u8, Error>(&control, |_, _| Ok(9)))
                        .expect("reply");
                })
            };
            let end = Instant::now() + Duration::from_secs(10);
            while !control.test.waiting.load(Ordering::SeqCst) {
                assert!(Instant::now() < end, "reader entered the wait");
                std::thread::sleep(Duration::from_millis(1));
            }
            let closer = match cause {
                0 => {
                    control.cancel();
                    None
                }
                1 => {
                    let side = Arc::clone(&side);
                    Some(std::thread::spawn(move || side.close()))
                }
                2 => {
                    *control.test.time.lock().expect("clock") = Some(control.end);
                    None
                }
                _ => unreachable!(),
            };
            assert_eq!(
                result
                    .recv_timeout(Duration::from_secs(10))
                    .expect("wait ends"),
                Err(if cause == 2 {
                    Error::Timeout
                } else {
                    Error::Ended
                })
            );
            drop(held);
            side.end_swap();
            worker.join().expect("reader");
            if let Some(closer) = closer {
                closer.join().expect("closer");
                let retry = Control::new(Instant::now() + Duration::from_secs(60));
                assert_eq!(
                    side.read_until::<u8, Error>(&retry, |_, _| Ok(9)),
                    Err(Error::Ended)
                );
            }
        }
    }
}

#[test]
fn failed_hook_cleanup_discards_the_side_connection() {
    use crate::recall::control::Control;
    use std::time::{Duration, Instant};
    for point in [1, 2, 3] {
        let scratch = Scratch::new("recall-hook-failure");
        let store = owing(&scratch);
        let side = store.side();
        let control = Control::new(Instant::now() + Duration::from_secs(60));
        control.test.fault.store(point, Ordering::SeqCst);
        let result = side.read_until::<i64, StoreError>(&control, |conn, control| {
            conn.busy_timeout(Duration::from_millis(333))?;
            control
                .with_handler(conn, |conn, scope| scope.query(conn, || Ok(7)))
                .map_err(StoreError::from)?
                .map_err(StoreError::from)
        });
        assert!(matches!(result, Err(StoreError::Read)));
        assert_eq!(
            side.read(|conn| Ok(conn.query_row::<i64, _, _>(
                "PRAGMA busy_timeout",
                [],
                |row| row.get(0)
            )?))
            .expect("ordinary page connection"),
            5000
        );
        assert_eq!(live(&store), [ThreadId(2)]);
    }
}

#[test]
fn unwinding_a_controlled_read_discards_its_connection() {
    use crate::recall::control::Control;
    use std::time::{Duration, Instant};
    let scratch = Scratch::new("recall-side-unwind");
    let store = owing(&scratch);
    let side = store.side();
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    control.test.fault.store(2, Ordering::SeqCst);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let result = side.read_until::<(), StoreError>(&control, |conn, control| {
            conn.busy_timeout(Duration::from_millis(333))?;
            control
                .with_handler(conn, |_, _| panic!("fixture unwind"))
                .map_err(StoreError::from)
        });
        panic!("expected unwind, got {result:?}");
    }));
    assert!(panic.is_err());
    assert_eq!(
        side.read(|conn| Ok(
            conn.query_row::<i64, _, _>("PRAGMA busy_timeout", [], |row| row.get(0))?
        ))
        .expect("fresh connection"),
        5000
    );
    assert_eq!(live(&store), [ThreadId(2)]);
}

/// Thread 1 deleted, so a purge is owed, and thread 2 kept.
fn owing(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    for id in [1, 2] {
        store
            .begin_turn(ThreadId(id), Some("t"), RunId(id), "words")
            .expect("turn");
        store
            .end_run(ThreadId(id), RunId(id), &[], RunEnd::Done)
            .expect("ended");
    }
    store.begin_delete(ThreadId(1)).expect("begin");
    store.finish_delete(ThreadId(1)).expect("finish");
    store
}

fn copied(store: &Store) -> Copied {
    store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped")
}

fn live(store: &Store) -> Vec<ThreadId> {
    store
        .side()
        .read(|conn| Pages(conn).search_ids("words"))
        .expect("read")
}

fn autocheckpoint(store: &Store) -> i64 {
    store
        .conn_for_tests()
        .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))
        .expect("pragma")
}

#[test]
fn a_swap_holds_both_side_connections_until_the_store_is_back() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::recall::control::Control;

    let scratch = Scratch::new("side-swap");
    let mut store = owing(&scratch);
    let side = store.side();
    assert_eq!(live(&store), [ThreadId(2)]);
    assert!(side.checkpoint().expect("the helper's connection opens"));
    let copied = copied(&store);
    let limit = Duration::from_secs(10);
    let control = Control::new(Instant::now() + limit);
    let (read, read_result) = mpsc::channel();
    let (checkpoint, checkpoint_result) = mpsc::channel();
    let (waiting, helper_waiting) = mpsc::channel();

    std::thread::scope(|workers| {
        let swapped = store.swap_with(&copied, |db, old| {
            let side = &side;
            let reading = &control;
            workers.spawn(move || {
                read.send(side.read_until::<i64, StoreError>(reading, |conn, _| {
                    Ok(conn.query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))?)
                }))
                .expect("reader result");
            });
            workers.spawn(move || {
                let held = side.hold_helper_after(|| waiting.send(()).expect("helper waits"));
                checkpoint
                    .send(side.checkpoint_held(held))
                    .expect("helper result");
            });
            while !control.test.waiting.load(Ordering::SeqCst) {
                assert!(Instant::now() < control.end, "reader entered the wait");
                std::thread::sleep(Duration::from_millis(1));
            }
            helper_waiting
                .recv_timeout(limit)
                .expect("helper entered the wait");
            assert!(matches!(
                read_result.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(matches!(
                checkpoint_result.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            fs::hard_link(db, old)
        });
        assert_eq!(swapped.expect("swap"), Swap::Done);
        assert_eq!(
            read_result
                .recv_timeout(limit)
                .expect("reader replied")
                .expect("read"),
            0
        );
        assert!(
            checkpoint_result
                .recv_timeout(limit)
                .expect("helper replied")
                .expect("checkpoint")
        );
    });
    store
        .begin_turn(ThreadId(3), Some("t"), RunId(3), "words")
        .expect("a write after the swap");
    assert_eq!(live(&store), [ThreadId(3), ThreadId(2)]);
}

#[test]
fn a_side_connection_in_use_blocks_the_swap_and_closes_nothing() {
    let scratch = Scratch::new("side-busy");
    let mut store = owing(&scratch);
    let side = store.side();
    let copied = copied(&store);
    for slot in [&side.reader, &side.helper] {
        let held = slot.lock().expect("free");
        assert_eq!(store.swap(&copied).expect("swap"), Swap::Blocked);
        drop(held);
    }
    assert!(side.available.load(Ordering::SeqCst));
    assert_eq!(live(&store), [ThreadId(2)]);
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
}

#[test]
fn reads_fail_while_the_serving_connection_cant_reopen() {
    let scratch = Scratch::new("side-unavailable");
    let mut store = owing(&scratch);
    let doomed = (ThreadId(3), RunId(3));
    store
        .begin_turn(doomed.0, Some("old"), doomed.1, "retired sentinel")
        .unwrap();
    let sentinel = b"retired sentinel";
    let has_sentinel = || {
        super::super::tests::raw_bytes(&scratch.db())
            .windows(sentinel.len())
            .any(|bytes| bytes == sentinel)
    };
    assert!(has_sentinel(), "the marker was stored before deletion");
    assert_eq!(
        store.delete(doomed.0).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    let copied = copied(&store);
    let wal = store.swap_failing_reopen(&copied);
    assert!(matches!(
        store.side().read(|_| Ok(())),
        Err(StoreError::Closed)
    ));
    assert!(matches!(store.side().checkpoint(), Err(StoreError::Closed)));
    assert!(store.reconnect().is_err());
    assert!(
        store.high_water().is_err(),
        "an unavailable store cannot return allocator state"
    );
    assert!(
        store
            .begin_turn(ThreadId(4), Some("refused"), RunId(4), "refused write")
            .is_err(),
        "the unavailable writer may not acknowledge a turn"
    );
    fs::remove_dir(&wal).expect("out of the way");
    store.reconnect().expect("reconnect");
    assert_eq!(autocheckpoint(&store), 0, "reopened with the same settings");
    assert_eq!(live(&store), [ThreadId(2)]);
    assert!(!store.purge_pending(), "reopened the clean file");
    assert_eq!(
        store.pages().search_ids("refused write").expect("search"),
        []
    );
    store
        .begin_turn(ThreadId(4), Some("after"), RunId(4), "served again")
        .expect("writable");
    store
        .end_run(ThreadId(4), RunId(4), &[], RunEnd::Done)
        .expect("ended");
    assert_eq!(
        store.pages().search_ids("served again").expect("search"),
        [ThreadId(4)]
    );
    store.leftovers().remove().expect("removed");
    assert!(!store.leftovers().exist());
    assert!(store.side().checkpoint().expect("checkpoint"));
    assert!(
        !has_sentinel(),
        "the deleted marker is gone after the purge"
    );
}

#[test]
fn a_missing_store_fails_its_side_connections_rather_than_creating_one() {
    let scratch = Scratch::new("side-missing");
    let store = Store::open(&scratch.db()).expect("store");
    let side = store.side();
    drop(store);
    let wal = super::super::purge::sidecar(&scratch.db(), "-wal");
    assert!(!wal.exists(), "the last close removed it");
    assert_eq!(
        side.read(|conn| Pages(conn).search_ids("words"))
            .expect("a reader opens without sidecars"),
        []
    );
    drop(side.hold());
    for file in [
        scratch.db(),
        wal,
        super::super::purge::sidecar(&scratch.db(), "-shm"),
    ] {
        if file.exists() {
            fs::remove_file(file).expect("removed");
        }
    }
    assert!(matches!(
        side.read(|_| Ok(())),
        Err(StoreError::Sql(rusqlite::Error::SqliteFailure(error, _)))
            if error.code == rusqlite::ErrorCode::CannotOpen
    ));
    assert!(matches!(
        side.checkpoint(),
        Err(StoreError::Sql(rusqlite::Error::SqliteFailure(error, _)))
            if error.code == rusqlite::ErrorCode::CannotOpen
    ));
    assert!(!scratch.db().exists(), "nothing created it");
}

#[test]
fn the_helper_copies_commits_into_the_file_but_never_past_a_reader() {
    let scratch = Scratch::new("side-checkpoint");
    let mut store = Store::open(&scratch.db()).expect("store");
    assert_eq!(autocheckpoint(&store), 0, "commits never checkpoint");
    let side = store.side();
    let text = "x".repeat(64 * 1024);
    let on_disk = || fs::metadata(scratch.db()).expect("file").len();
    store
        .begin_turn(ThreadId(1), Some("t"), RunId(1), &text)
        .expect("turn");
    let before = on_disk();
    assert!(side.checkpoint().expect("checkpoint"));
    assert!(on_disk() > before + 64 * 1024, "the turn reached the file");

    let reader = Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");
    store
        .begin_turn(ThreadId(2), Some("t"), RunId(2), &text)
        .expect("turn");
    assert!(
        !side.checkpoint().expect("checkpoint"),
        "the reader needs it"
    );
    drop(reader);
    assert!(side.checkpoint().expect("checkpoint"));
}
