use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kumochi_protocol::message::{RunId, ThreadId};
use rusqlite::Connection;

use crate::store::{Cancel, Copied, Pages, RunEnd, Store, StoreError, Swap};
use crate::test_support::Scratch;

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
    let scratch = Scratch::new("side-swap");
    let mut store = owing(&scratch);
    let side = store.side();
    assert_eq!(live(&store), [ThreadId(2)]);
    side.checkpoint().expect("the helper's connection opens");
    let copied = copied(&store);
    let watched = Arc::clone(&side);
    let swapped = store.swap_with(&copied, |db, old| {
        assert!(
            watched.reader.try_lock().is_err(),
            "the reader's slot is held"
        );
        assert!(
            watched.helper.try_lock().is_err(),
            "the helper's slot is held"
        );
        assert!(!watched.available.load(Ordering::SeqCst));
        fs::hard_link(db, old)
    });
    assert_eq!(swapped.expect("swap"), Swap::Done);
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
    let copied = copied(&store);
    let wal = store.swap_failing_reopen(&copied);
    assert!(matches!(
        store.side().read(|_| Ok(())),
        Err(StoreError::Closed)
    ));
    assert!(matches!(store.side().checkpoint(), Err(StoreError::Closed)));
    fs::remove_dir(&wal).expect("out of the way");
    store.reconnect().expect("reconnect");
    assert_eq!(autocheckpoint(&store), 0, "reopened with the same settings");
    assert_eq!(live(&store), [ThreadId(2)]);
    assert!(store.side().checkpoint().expect("checkpoint"));
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
    assert!(side.read(|_| Ok(())).is_err());
    assert!(side.checkpoint().is_err());
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
