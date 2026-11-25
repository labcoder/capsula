use std::collections::BTreeMap;
#[cfg(windows)]
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::{DataToShell, Deleted, PageOf, RunId, Step, ThreadId};
use rusqlite::Connection;
use tokio::sync::mpsc;

use super::{Attempt, Purger, Schedule, StoreThread};
use crate::coordinator::{Input, StoreOp};
use crate::platform::{SessionLock, SessionLockError};
use crate::store::{Cancel, Pause, Pauses, Point, RunEnd, SceneTarget, Store, Swap, UnsentScreen};
use crate::test_support::Scratch;

/// A store owing the purge of its deleted thread 1.
pub(super) fn owing(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store
        .begin_turn(ThreadId(1), Some("t"), RunId(1), "doomed")
        .expect("turn");
    store.begin_delete(ThreadId(1)).expect("begin");
    store.finish_delete(ThreadId(1)).expect("finish");
    store
}

/// The store's thread with short waits, and every purge attempt it makes.
fn spawn(store: Store) -> (StoreThread, std_mpsc::Receiver<Attempt>) {
    spawn_with_room(store, |_| Ok(u64::MAX))
}

fn spawn_with_room(
    store: Store,
    free_space: fn(&Path) -> std::io::Result<u64>,
) -> (StoreThread, std_mpsc::Receiver<Attempt>) {
    let (acks, _replies) = mpsc::unbounded_channel();
    let (heard, attempts) = std_mpsc::channel();
    let schedule = Schedule {
        context: crate::config::tuning::ContextTuning::default(),
        quiet: Duration::from_millis(1),
        first_retry: Duration::from_millis(20),
        last_retry: Duration::from_millis(20),
        free_space,
        attempts: Some(heard),
        observed: None,
        gates: None,
        read_gates: None,
        checkpoint_gate: None,
        after_close: None,
        pauses: None,
    };
    let custody = crate::test_support::custody(&store);
    let thread = StoreThread::spawn(store, custody, acks, schedule).expect("thread");
    (thread, attempts)
}

pub(super) fn next(attempts: &std_mpsc::Receiver<Attempt>) -> Attempt {
    attempts
        .recv_timeout(PATIENCE)
        .expect("the store's thread tried the purge")
}

/// Waits out repeats a retry already under way can add, then expects the purge to finish.
fn done_after(attempts: &std_mpsc::Receiver<Attempt>, repeats: &[Attempt]) {
    let mut next_one = next(attempts);
    while repeats.contains(&next_one) {
        next_one = next(attempts);
    }
    assert_eq!(next_one, Attempt::Done);
}

pub(super) fn owed(scratch: &Scratch) -> i64 {
    Connection::open(scratch.db())
        .expect("watcher")
        .query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))
        .expect("count")
}

#[test]
fn an_owed_purge_waits_for_the_active_run_and_then_runs_in_session() {
    let scratch = Scratch::new("idle-purge");
    let mut store = owing(&scratch);
    store
        .begin_turn(ThreadId(2), Some("kept"), RunId(2), "busy")
        .expect("an active run");
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Busy);
    assert_eq!(owed(&scratch), 1);
    assert!(thread.send(StoreOp::EndRun {
        screen: None,
        thread: ThreadId(2),
        run: RunId(2),
        text: None,
        revealed: None,
        clear: None,
        end: RunEnd::Done,
    }));
    assert_eq!(next(&attempts), Attempt::Done);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
}

#[test]
fn a_purge_a_reader_held_off_is_retried_in_session() {
    let scratch = Scratch::new("held-off-purge");
    let mut store = owing(&scratch);
    let reader = Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");
    // A write past the reader's snapshot, so the WAL can't be emptied under it.
    store
        .begin_turn(ThreadId(2), Some("later"), RunId(2), "later")
        .expect("turn");
    store
        .end_run(ThreadId(2), RunId(2), &[], RunEnd::Done)
        .expect("ended");
    let (thread, attempts) = spawn(store);
    // The drain can't copy the WAL past the reader, so the copy stops.
    assert_eq!(next(&attempts), Attempt::Stale);
    drop(reader);
    // A retry racing the reader's close can find its file still held.
    done_after(&attempts, &[Attempt::Stale, Attempt::Failed]);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
}

#[test]
fn a_failed_purge_is_retried_in_session() {
    let scratch = Scratch::new("failed-purge");
    let store = owing(&scratch);
    // The copy carries the trigger, so clearing its owed row fails.
    store
        .conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER keep_owed BEFORE DELETE ON purge_pending BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trigger");
    let (reached, reaching) = std_mpsc::channel();
    let (releasing, release) = std_mpsc::channel();
    let (heard, attempts) = std_mpsc::channel();
    let (acks, _replies) = mpsc::unbounded_channel();
    let schedule = Schedule {
        quiet: Duration::from_millis(1),
        first_retry: Duration::from_millis(20),
        last_retry: Duration::from_millis(20),
        free_space: |_| Ok(u64::MAX),
        attempts: Some(heard),
        pauses: Some(Pauses {
            at: std::sync::Mutex::new(vec![Pause {
                at: Point::Clean,
                reached,
                release,
            }]),
            waiting: None,
        }),
        ..Schedule::default()
    };
    let custody = crate::test_support::custody(&store);
    let thread = StoreThread::spawn(store, custody, acks, schedule).expect("thread");
    // Fixed while the first copy holds its snapshot, so the retry's copy never meets the fixer.
    reaching
        .recv_timeout(PATIENCE)
        .expect("the first copy took its snapshot");
    Connection::open(scratch.db())
        .expect("fixer")
        .execute_batch("DROP TRIGGER keep_owed")
        .expect("drop");
    releasing.send(()).expect("released");
    assert_eq!(next(&attempts), Attempt::Failed);
    assert_eq!(next(&attempts), Attempt::Done);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
}

/// `db` with `suffix` appended.
pub(super) fn beside(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// The purge's files left in the store's folder.
pub(super) fn purge_files(scratch: &Scratch) -> Vec<PathBuf> {
    std::fs::read_dir(scratch.db().parent().expect("a folder"))
        .expect("folder")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.to_string_lossy().contains(".purge-"))
        .collect()
}

/// A store whose swap finished, with the old store still beside it, as a crash right after
/// the swap leaves it.
fn swapped_with_old_left(scratch: &Scratch) -> Store {
    let mut store = owing(scratch);
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped");
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
    assert_eq!(
        purge_files(scratch),
        vec![beside(&scratch.db(), ".purge-old")]
    );
    store
}

/// Opens `path` so it can't be deleted until the handle drops.
#[cfg(windows)]
fn hold(path: &Path) -> File {
    const FILE_SHARE_READ: u32 = 1;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .expect("held")
}

#[test]
fn an_old_store_left_by_a_crash_is_removed_at_start() {
    let scratch = Scratch::new("old-left");
    drop(swapped_with_old_left(&scratch));
    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(
        !store.purge_pending(),
        "the swap finished; only the file is owed"
    );
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Done);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
}

#[test]
#[cfg(windows)]
fn an_old_store_that_cant_be_removed_is_retried_in_session_and_at_the_next_start() {
    let scratch = Scratch::new("old-held");
    let store = swapped_with_old_left(&scratch);
    let holder = hold(&beside(&scratch.db(), ".purge-old"));
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Failed);
    assert_eq!(next(&attempts), Attempt::Failed, "retried in session");
    thread.close().blocking_recv().expect("closed");
    assert_eq!(
        purge_files(&scratch),
        vec![beside(&scratch.db(), ".purge-old")]
    );

    // The next start owes the removal even though no purge row is left.
    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(!store.purge_pending());
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Failed);
    drop(holder);
    done_after(&attempts, &[Attempt::Failed]);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
}

#[test]
#[cfg(windows)]
fn an_old_store_held_with_delete_sharing_is_finished_only_once_its_name_is_gone() {
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 1 | 2 | 4;
    let scratch = Scratch::new("old-shared");
    let store = swapped_with_old_left(&scratch);
    let old = beside(&scratch.db(), ".purge-old");
    let holder = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .open(&old)
        .expect("held");
    let (thread, attempts) = spawn(store);
    let first = next(&attempts);
    let files = purge_files(&scratch);
    // Some Windows volumes drop the name at once; others retain it until the handle closes.
    let unlinked = files.is_empty();
    if !unlinked {
        assert_eq!(files, [old]);
    }
    assert_eq!(
        first,
        if unlinked {
            Attempt::Done
        } else {
            Attempt::Failed
        }
    );
    drop(holder);
    if !unlinked {
        done_after(&attempts, &[Attempt::Failed]);
    }
    thread.close().blocking_recv().expect("closed");
    assert!(purge_files(&scratch).is_empty(), "the old name is gone");
}

async fn reply(replies: &mut mpsc::UnboundedReceiver<Input>) -> Input {
    tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .expect("a reply in time")
        .expect("the thread is alive")
}

#[tokio::test]
async fn a_held_model_write_answers_only_after_release() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

    let scratch = Scratch::new("model-held");
    let mut store = Store::open(&scratch.db()).expect("store");
    let id = ThreadId(1);
    store
        .begin_turn(id, Some("task"), RunId(1), "ask")
        .expect("turn");
    let choice = ModelChoice {
        model: ModelSlug::new("held-model").expect("valid"),
        effort: Effort::Max,
        connection: ConnectionKind::ChatGpt,
    };
    let (acks, mut replies) = mpsc::unbounded_channel();
    let (observed, mut seen) = mpsc::unbounded_channel();
    let (gates, held) = std_mpsc::channel();
    let (release, wait) = std_mpsc::channel();
    gates
        .send(super::Gate {
            matches: |op| matches!(op, StoreOp::SetModel { .. }),
            release: wait,
        })
        .expect("gate");
    let custody = crate::test_support::custody(&store);
    let worker = StoreThread::spawn(
        store,
        custody,
        acks,
        Schedule {
            observed: Some(observed),
            gates: Some(held),
            ..Schedule::default()
        },
    )
    .expect("worker");
    let op = StoreOp::SetModel {
        thread: id,
        choice: choice.clone(),
    };
    assert!(worker.send(op.clone()));
    assert_eq!(
        tokio::time::timeout(PATIENCE, seen.recv())
            .await
            .expect("started in time"),
        Some(crate::runtime::Seen::StoreOpStarted(op))
    );
    assert_eq!(replies.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    release.send(()).expect("released");
    assert_eq!(
        reply(&mut replies).await,
        Input::ModelSet {
            thread: id,
            choice: choice.clone(),
            applied: true,
        }
    );
    worker.close().await.expect("closed");
    let store = Store::open(&scratch.db()).expect("reopen");
    let (rows, _) = store.pages().thread_page(None, 50).expect("page");
    assert_eq!(
        rows.iter()
            .map(|row| (row.id, row.choice.clone()))
            .collect::<Vec<_>>(),
        [(id, Some(choice))]
    );
    assert_eq!(
        replies.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    );
}

#[tokio::test]
async fn close_releases_the_reply_sender_before_post_close_cleanup_finishes() {
    let scratch = Scratch::new("close-replies");
    let store = Store::open(&scratch.db()).expect("store");
    let (acks, mut replies) = mpsc::unbounded_channel();
    let (reached, closed) = std_mpsc::channel();
    let (release, wait) = std_mpsc::channel();
    let custody = crate::test_support::custody(&store);
    let worker = StoreThread::spawn(
        store,
        custody,
        acks,
        Schedule {
            after_close: Some((reached, wait)),
            ..Schedule::default()
        },
    )
    .expect("worker");
    tokio::time::timeout(PATIENCE, worker.close())
        .await
        .expect("close completes in time")
        .expect("closed");
    closed
        .recv_timeout(PATIENCE)
        .expect("worker holds after close");
    assert_eq!(
        replies.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    );
    release.send(()).expect("release post-close cleanup");
}

#[tokio::test]
async fn the_worker_keeps_the_writer_lock_until_post_close_cleanup_ends() {
    let scratch = Scratch::new("close-custody");
    let store = owing(&scratch);
    let lock = scratch.dir().join(SESSION_LOCK);
    let custody = Arc::new(SessionLock::try_acquire(&lock).expect("lock"));
    let (acks, _replies) = mpsc::unbounded_channel();
    let (reached, closed) = std_mpsc::channel();
    let (release, wait) = std_mpsc::channel();
    let worker = StoreThread::spawn(
        store,
        custody,
        acks,
        Schedule {
            after_close: Some((reached, wait)),
            ..Schedule::default()
        },
    )
    .expect("worker");
    tokio::time::timeout(PATIENCE, worker.close())
        .await
        .expect("close completes in time")
        .expect("closed");
    closed
        .recv_timeout(PATIENCE)
        .expect("worker holds after close");
    assert!(
        matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
        "the lock outlives the caller and close's acknowledgement while the worker cleans up"
    );
    release.send(()).expect("release post-close cleanup");
    let deadline = Instant::now() + PATIENCE;
    loop {
        match SessionLock::try_acquire(&lock) {
            Ok(_) => break,
            Err(SessionLockError::Busy) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => panic!("the worker kept the lock after its cleanup: {error}"),
        }
    }
}

pub(super) fn after_close(gate: Option<(std_mpsc::Sender<()>, std_mpsc::Receiver<()>)>) {
    if let Some((reached, release)) = gate
        && reached.send(()).is_ok()
    {
        assert!(
            matches!(
                release.recv_timeout(PATIENCE),
                Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected)
            ),
            "post-close cleanup is released before the test deadline"
        );
    }
}

#[tokio::test]
async fn a_failed_model_write_is_not_rescued_by_a_later_success() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

    let scratch = Scratch::new("model-failed");
    let mut store = Store::open(&scratch.db()).expect("store");
    for id in 1..=2 {
        store
            .begin_turn(ThreadId(id), Some("task"), RunId(id), "ask")
            .expect("turn");
    }
    store
        .conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER refuse_model BEFORE UPDATE OF model ON threads WHEN old.id = 1
         BEGIN SELECT RAISE(ABORT, 'MODEL_ERROR_SENTINEL'); END;",
        )
        .expect("trigger");
    let (acks, mut replies) = mpsc::unbounded_channel();
    let captured = crate::test_support::Captured::default();
    let _logs = captured.install();
    let mut purger = Purger::new(Schedule::default(), &store);
    let failed = ModelChoice {
        model: ModelSlug::new("failed-model").expect("valid"),
        effort: Effort::Low,
        connection: ConnectionKind::ChatGpt,
    };
    let later = ModelChoice {
        model: ModelSlug::new("later-model").expect("valid"),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    for (id, choice) in [(1, failed.clone()), (2, later.clone())] {
        super::serve_op(
            &mut store,
            &mut purger,
            &acks,
            &mut BTreeMap::new(),
            StoreOp::SetModel {
                thread: ThreadId(id),
                choice,
            },
        );
    }
    assert_eq!(
        reply(&mut replies).await,
        Input::ModelSet {
            thread: ThreadId(1),
            choice: failed,
            applied: false,
        }
    );
    assert_eq!(
        reply(&mut replies).await,
        Input::ModelSet {
            thread: ThreadId(2),
            choice: later.clone(),
            applied: true,
        }
    );
    let logs = captured.text();
    assert!(logs.contains("a model choice wasn't stored"));
    assert!(!logs.contains("MODEL_ERROR_SENTINEL"));
    assert!(!logs.contains(&format!("{:?}", b"MODEL_ERROR_SENTINEL")));
    drop(acks);
    drop(purger);
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let (rows, _) = store.pages().thread_page(None, 50).expect("page");
    assert_eq!(
        rows.iter()
            .find(|row| row.id == ThreadId(1))
            .expect("failed row")
            .choice,
        None
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.id == ThreadId(2))
            .expect("later row")
            .choice,
        Some(later)
    );
    assert_eq!(
        replies.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    );
}

#[tokio::test]
async fn an_op_fails_while_the_store_cant_reopen_and_the_next_is_stored_once_it_can() {
    let scratch = Scratch::new("unavailable");
    let mut store = owing(&scratch);
    let (kept, run) = (ThreadId(2), RunId(2));
    store
        .begin_turn(kept, Some("kept"), run, "kept words")
        .expect("turn");
    store.end_run(kept, run, &[], RunEnd::Done).expect("ended");
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped");
    let wal = store.swap_failing_reopen(&copied);

    let (acks, mut replies) = mpsc::unbounded_channel();
    let (heard, attempts) = std_mpsc::channel();
    // The purge's retry is far off, so only an op can open the store again.
    let schedule = Schedule {
        first_retry: Duration::from_secs(600),
        last_retry: Duration::from_secs(600),
        attempts: Some(heard),
        observed: None,
        ..Schedule::default()
    };
    let custody = crate::test_support::custody(&store);
    let thread = StoreThread::spawn(store, custody, acks, schedule).expect("thread");
    assert_eq!(
        next(&attempts),
        Attempt::Failed,
        "the purge can't reopen it"
    );
    let turn = |run, text: &str| StoreOp::BeginTurn {
        thread: ThreadId(3),
        title: Some("new".into()),
        run,
        text: text.into(),
        screen: None,
    };
    assert!(thread.send(turn(RunId(3), "refused words")));
    assert_eq!(
        reply(&mut replies).await,
        Input::StoreFailed { run: RunId(3) }
    );
    assert!(wal.is_dir(), "nothing took the obstruction's place");

    std::fs::remove_dir(&wal).expect("out of the way");
    assert!(thread.send(turn(RunId(4), "stored words")));
    assert_eq!(
        reply(&mut replies).await,
        Input::TurnStored { run: RunId(4) }
    );
    thread.close().await.expect("closed");

    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store.pages().search_ids("stored").expect("search"),
        vec![ThreadId(3)]
    );
    assert_eq!(
        store.pages().search_ids("kept").expect("search"),
        vec![kept],
        "the same file, not an empty one"
    );
    assert_eq!(
        store.pages().search_ids("refused").expect("search"),
        Vec::new()
    );
}

#[test]
fn a_crash_right_after_the_link_keeps_the_store_and_the_purge_finishes() {
    let scratch = Scratch::new("crash-linked");
    let store = owing(&scratch);
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped");
    drop((copied, store));
    // The brain dies between the link and the move: two names for the old store.
    std::fs::hard_link(scratch.db(), beside(&scratch.db(), ".purge-old")).expect("linked");

    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(store.purge_pending());
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Done);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
    assert_eq!(owed(&scratch), 0);
}

static ROOM: AtomicU64 = AtomicU64::new(0);

#[test]
fn a_copy_waits_until_the_volume_has_room_for_it_and_the_foreground() {
    let scratch = Scratch::new("no-room");
    let store = owing(&scratch);
    let bytes: i64 = store
        .conn_for_tests()
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )
        .expect("size");
    let needs = 2 * u64::try_from(bytes).expect("a size") + super::RESERVE;
    ROOM.store(needs - 1, Ordering::SeqCst);
    let (thread, attempts) = spawn_with_room(store, |_| Ok(ROOM.load(Ordering::SeqCst)));
    assert_eq!(next(&attempts), Attempt::NoRoom);
    assert_eq!(
        purge_files(&scratch),
        Vec::<PathBuf>::new(),
        "nothing was written"
    );
    assert_eq!(owed(&scratch), 1);
    ROOM.store(needs, Ordering::SeqCst);
    done_after(&attempts, &[Attempt::NoRoom]);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
}

#[test]
fn copies_made_stale_back_off_until_one_finishes() {
    let scratch = Scratch::new("stale-backoff");
    let mut store = owing(&scratch);
    let (heard, attempts) = std_mpsc::channel();
    let schedule = Schedule {
        first_retry: Duration::from_secs(60),
        last_retry: Duration::from_secs(240),
        free_space: |_| Ok(u64::MAX),
        attempts: Some(heard),
        ..Schedule::default()
    };
    let mut purger = Purger::new(schedule, &store);
    for seconds in [60, 120, 240, 240] {
        let before = Instant::now();
        purger.settle(Attempt::Stale, &store);
        assert_eq!(next(&attempts), Attempt::Stale);
        let wait = Duration::from_secs(seconds);
        let timeout = purger.timeout().expect("stale copies schedule a retry");
        assert!((wait.saturating_sub(before.elapsed())..=wait).contains(&timeout));
        purger.tick(&mut store);
        assert_eq!(attempts.try_recv(), Err(std_mpsc::TryRecvError::Empty));
    }

    purger.next = Some(Instant::now());
    let end = Instant::now() + PATIENCE;
    let result = loop {
        purger.tick(&mut store);
        match attempts.try_recv() {
            Ok(attempt) => break attempt,
            Err(std_mpsc::TryRecvError::Empty) => {
                assert!(Instant::now() < end, "the due retry finishes");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(std_mpsc::TryRecvError::Disconnected) => panic!("no purge receipts"),
        }
    };
    assert_eq!(result, Attempt::Done);
    assert!(!store.purge_pending());
    assert!(purge_files(&scratch).is_empty());
    assert_eq!(
        purger.timeout(),
        None,
        "a completed cleanup leaves no retry"
    );

    let before = Instant::now();
    purger.settle(Attempt::Stale, &store);
    assert_eq!(next(&attempts), Attempt::Stale);
    let first = Duration::from_secs(60);
    let timeout = purger.timeout().expect("fresh retry after completion");
    assert!((first.saturating_sub(before.elapsed())..=first).contains(&timeout));
}

#[test]
fn every_operation_gets_one_reply_in_order() {
    let scratch = Scratch::new("replies");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    for op in [
        StoreOp::BeginTurn {
            thread,
            title: Some("t".into()),
            run,
            text: "hi".into(),
            screen: None,
        },
        StoreOp::CommitStep {
            thread: ThreadId(2),
            run,
            text: None,
            scene: SceneTarget::New { evict: None },
            index: 0,
            step: Step {
                label: "s".to_owned(),
            },
        },
        StoreOp::EndRun {
            screen: None,
            thread,
            run,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        },
        StoreOp::ReadPage {
            key: crate::coordinator::PageKey::for_tests(4, 9),
            page: PageOf::Events {
                thread: ThreadId(999),
                before: None,
            },
        },
    ] {
        assert!(store.send(op));
    }
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(
        got,
        [
            Input::TurnStored { run },
            Input::StoreFailed { run },
            Input::RunStored { run },
            Input::PageRead {
                key: crate::coordinator::PageKey::for_tests(4, 9),
                reply: DataToShell::Page {
                    request: 9,
                    thread: ThreadId(999),
                    events: Vec::new(),
                    next: None
                },
            },
        ]
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the same store worker handles each failure and the next turn"
)]
fn a_round_the_reader_would_refuse_fails_its_run_and_the_thread_goes_on() {
    use crate::coordinator::RoundItems;
    use crate::dispatch::ToolOutput;
    use crate::engines::{CallId, Opaque};
    let scratch = Scratch::new("refused-round");
    let store = Store::open(&scratch.db()).expect("store");
    let thread = ThreadId(1);
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    let round = |run, item: &str, output| {
        StoreOp::RoundItems(RoundItems {
            thread,
            run,
            round: 1,
            connection: crate::context::Route::chatgpt(7),
            text: None,
            items: vec![Opaque::new(item.as_bytes().to_vec()).expect("small")],
            key_calls: Vec::new(),
            pictured_key_model: None,
            calls: vec![(CallId::new("c1").expect("short"), output)],
        })
    };
    let item = r#"{"type":"message"}"#;
    // `1e5` reads back as `100000.0`, so this legal item grows past 256 KiB.
    let growing = format!(r#"{{"n":[{}1e5]}}"#, "1e5,".repeat(59_999));
    let ok = || ToolOutput::Text("ok".into());
    let end = |run, end| StoreOp::EndRun {
        screen: None,
        thread,
        run,
        text: None,
        revealed: None,
        clear: None,
        end,
    };
    let over = ToolOutput::Text("x".repeat(65 * 1024));
    for op in [
        StoreOp::BeginTurn {
            thread,
            title: Some("t".into()),
            run: RunId(1),
            text: "hi".into(),
            screen: None,
        },
        round(RunId(1), item, over),
        end(RunId(1), RunEnd::Failed(crate::store::Failure::Store)),
        StoreOp::BeginTurn {
            thread,
            title: None,
            run: RunId(2),
            text: "again".into(),
            screen: None,
        },
        round(RunId(2), &growing, ok()),
        end(RunId(2), RunEnd::Failed(crate::store::Failure::Store)),
        StoreOp::BeginTurn {
            thread,
            title: None,
            run: RunId(3),
            text: "once more".into(),
            screen: None,
        },
        round(RunId(3), item, ok()),
        end(RunId(3), RunEnd::Done),
    ] {
        assert!(store.send(op));
    }
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(
        got,
        [
            Input::TurnStored { run: RunId(1) },
            Input::StoreFailed { run: RunId(1) },
            Input::RunStored { run: RunId(1) },
            Input::TurnStored { run: RunId(2) },
            Input::StoreFailed { run: RunId(2) },
            Input::RunStored { run: RunId(2) },
            Input::TurnStored { run: RunId(3) },
            Input::RoundStored {
                run: RunId(3),
                round: 1
            },
            Input::RunStored { run: RunId(3) },
        ]
    );
    let conn = Connection::open(scratch.db()).expect("open");
    let rows: Vec<(String, i64)> = conn
        .prepare(
            "SELECT kind, run FROM events WHERE kind IN ('output_item', 'tool_result') ORDER BY id",
        )
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(
        rows,
        [("output_item".to_owned(), 3), ("tool_result".to_owned(), 3)],
        "only the third run's round was written"
    );
}

#[test]
fn a_heard_point_is_stored_and_answers_nothing() {
    let scratch = Scratch::new("heard-op");
    let store = Store::open(&scratch.db()).expect("store");
    store
        .conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER refuse_heard BEFORE UPDATE OF heard_segment ON runs WHEN OLD.id = 2
             BEGIN SELECT RAISE(ABORT, 'kept'); END;",
        )
        .expect("trigger");
    let (thread, run, refused) = (ThreadId(1), RunId(1), RunId(2));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    let heard = crate::voice::reply::Heard {
        segment: kumochi_protocol::message::SegmentId(2),
        sample: 480,
        text: 17,
    };
    for op in [
        StoreOp::BeginTurn {
            thread,
            title: Some("t".into()),
            run,
            text: "hi".into(),
            screen: None,
        },
        StoreOp::EndRun {
            screen: None,
            thread,
            run,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        },
        StoreOp::BeginTurn {
            thread,
            title: None,
            run: refused,
            text: "again".into(),
            screen: None,
        },
        StoreOp::EndRun {
            screen: None,
            thread,
            run: refused,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        },
        StoreOp::Heard {
            run: refused,
            heard,
        },
        StoreOp::Heard { run, heard },
        StoreOp::Heard {
            run: RunId(u64::MAX),
            heard,
        },
    ] {
        assert!(store.send(op));
    }
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(
        got,
        [
            Input::TurnStored { run },
            Input::RunStored { run },
            Input::TurnStored { run: refused },
            Input::RunStored { run: refused },
        ],
        "a heard point SQLite refuses, or on a run id it can't hold, answers nothing"
    );
    let conn = rusqlite::Connection::open(scratch.db()).expect("raw open");
    let row = |id: i64| -> (String, Option<i64>, Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT state, heard_segment, heard_sample, heard_text FROM runs WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("the run's row")
    };
    assert_eq!(row(1), ("done".into(), Some(2), Some(480), Some(17)));
    assert_eq!(
        row(2),
        ("done".into(), None, None, None),
        "the refused point leaves the run's outcome as it was"
    );
}

#[test]
fn a_held_turns_screen_is_stored_with_its_turn() {
    use kumochi_protocol::message::{HeldScreen, ScreenClass};
    let scratch = Scratch::new("held-op");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    assert!(store.send(StoreOp::BeginTurn {
        thread,
        title: Some("t".into()),
        run,
        text: "hi".into(),
        screen: Some(HeldScreen::NotIncluded(ScreenClass::TooLarge)),
    }));
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(got, [Input::TurnStored { run }]);
    let conn = rusqlite::Connection::open(scratch.db()).expect("raw open");
    let stored: Option<String> = conn
        .query_row("SELECT screen FROM runs WHERE id = 1", [], |row| row.get(0))
        .expect("the run's row");
    assert_eq!(stored.as_deref(), Some("too_large"));
}

#[test]
fn a_page_read_neither_stops_a_copy_nor_waits_for_it() {
    let scratch = Scratch::new("read-during-copy");
    let paused = armed_and_paused(&scratch, &[Point::Snapshot]);
    reached(&paused, 0);
    assert!(paused.thread.send(StoreOp::ReadPage {
        key: crate::coordinator::PageKey::for_tests(1, 3),
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    }));
    let reply = paused
        .acks
        .recv_timeout(PATIENCE)
        .expect("the read didn't wait");
    let Input::PageRead {
        key: crate::coordinator::PageKey { request: 3, .. },
        reply: DataToShell::ThreadList { threads, .. },
        ..
    } = reply
    else {
        panic!("not the thread list: {reply:?}");
    };
    let ids: Vec<ThreadId> = threads.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, [ThreadId(3), ThreadId(2)]);
    release(&paused, 0);
    assert_eq!(
        next(&paused.attempts),
        Attempt::Done,
        "the read didn't stop it"
    );
    paused.thread.close().blocking_recv().expect("closed");
}

#[test]
fn a_delete_in_session_purges_its_thread() {
    let scratch = Scratch::new("delete-purge");
    let mut store = Store::open(&scratch.db()).expect("store");
    for thread in [1, 2] {
        store
            .begin_turn(ThreadId(thread), Some("t"), RunId(thread), "text")
            .expect("turn");
        store
            .end_run(ThreadId(thread), RunId(thread), &[], RunEnd::Done)
            .expect("ended");
    }
    assert_eq!(owed(&scratch), 0, "no purge is owed before the delete");
    let (thread, attempts) = spawn(store);
    assert!(thread.send(StoreOp::Delete {
        thread: ThreadId(1)
    }));
    assert_eq!(next(&attempts), Attempt::Done);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(!store.leftovers().exist(), "no copy is left");
    let threads: Vec<ThreadId> = store
        .pages()
        .thread_page(None, 50)
        .map(|(rows, _)| rows.into_iter().map(|row| row.id).collect())
        .expect("threads");
    assert_eq!(threads, [ThreadId(2)]);
}

/// A purge copy paused at each of `points`, with how a test reaches and releases each.
struct Paused {
    thread: StoreThread,
    side: Arc<crate::store::Side>,
    attempts: std_mpsc::Receiver<Attempt>,
    acks: std_mpsc::Receiver<Input>,
    /// Hears that a write waits for the copy to let go of the live store.
    waiting: std_mpsc::Receiver<()>,
    points: Vec<(std_mpsc::Receiver<()>, std_mpsc::Sender<()>)>,
}

/// Threads 1 to 3 with an ended run each, the store's thread pausing its first copy at
/// `points`, and thread 1 deleted to arm that copy.
fn armed_and_paused(scratch: &Scratch, points: &[Point]) -> Paused {
    let mut store = Store::open(&scratch.db()).expect("store");
    for thread in 1..=3 {
        store
            .begin_turn(ThreadId(thread), Some("t"), RunId(thread), "text")
            .expect("turn");
        store
            .end_run(ThreadId(thread), RunId(thread), &[], RunEnd::Done)
            .expect("ended");
    }
    let (heard, attempts) = std_mpsc::channel();
    let (waits, waiting) = std_mpsc::channel();
    let mut holds = Vec::new();
    let mut handles = Vec::new();
    for &at in points {
        let (reached, reaching) = std_mpsc::channel();
        let (releasing, release) = std_mpsc::channel();
        holds.push(Pause {
            at,
            reached,
            release,
        });
        handles.push((reaching, releasing));
    }
    let schedule = Schedule {
        quiet: Duration::from_millis(1),
        first_retry: Duration::from_millis(20),
        last_retry: Duration::from_millis(20),
        free_space: |_| Ok(u64::MAX),
        attempts: Some(heard),
        pauses: Some(Pauses {
            at: std::sync::Mutex::new(holds),
            waiting: Some(waits),
        }),
        ..Schedule::default()
    };
    let (acks_tx, mut replies) = mpsc::unbounded_channel();
    let (forward, acks) = std_mpsc::channel();
    std::thread::spawn(move || {
        while let Some(input) = replies.blocking_recv() {
            if forward.send(input).is_err() {
                return;
            }
        }
    });
    let side = store.side();
    let custody = crate::test_support::custody(&store);
    let thread = StoreThread::spawn(store, custody, acks_tx, schedule).expect("thread");
    let paused = Paused {
        thread,
        side,
        attempts,
        acks,
        waiting,
        points: handles,
    };
    assert!(paused.thread.send(StoreOp::Delete {
        thread: ThreadId(1)
    }));
    assert_removed(&paused, 1);
    paused
}

/// How long a test waits for the store's thread: far above a slow CI runner's worst case.
pub(super) const PATIENCE: Duration = Duration::from_secs(60);

fn assert_removed(paused: &Paused, thread: u64) {
    let ack = paused.acks.recv_timeout(PATIENCE).expect("acknowledged");
    assert!(
        matches!(
            ack,
            Input::Deleted {
                thread: acked,
                outcome: Deleted::Removed,
                ..
            } if acked == ThreadId(thread)
        ),
        "{ack:?}"
    );
}

fn reached(paused: &Paused, point: usize) {
    paused.points[point]
        .0
        .recv_timeout(PATIENCE)
        .expect("the copy got there");
}

fn release(paused: &Paused, point: usize) {
    paused.points[point].1.send(()).expect("the copy waits");
}

fn live(scratch: &Scratch, thread: i64) -> bool {
    Connection::open(scratch.db())
        .expect("watcher")
        .query_row(
            "SELECT state = 'live' FROM threads WHERE id = ?1",
            [thread],
            |row| row.get(0),
        )
        .expect("the thread's row")
}

/// The purge retries and finishes, and the next op works.
fn then_purged(paused: Paused) {
    done_after(&paused.attempts, &[Attempt::Stale]);
    assert!(paused.thread.send(StoreOp::Delete {
        thread: ThreadId(3)
    }));
    assert_removed(&paused, 3);
    paused.thread.close().blocking_recv().expect("closed");
}

#[test]
fn a_write_waits_for_a_copy_reading_the_live_store_but_not_for_its_cleanup() {
    for at in [Point::Drain, Point::Snapshot] {
        let scratch = Scratch::new("source-held");
        let paused = armed_and_paused(&scratch, &[at, Point::Cleanup]);
        reached(&paused, 0);
        assert!(paused.thread.send(StoreOp::Delete {
            thread: ThreadId(2)
        }));
        paused
            .waiting
            .recv_timeout(PATIENCE)
            .expect("the write waits");
        assert!(
            paused.acks.try_recv().is_err(),
            "{at:?}: no write before the release"
        );
        assert!(live(&scratch, 2), "{at:?}");
        release(&paused, 0);
        reached(&paused, 1);
        assert_removed(&paused, 2);
        release(&paused, 1);
        then_purged(paused);
    }
}

#[test]
fn a_write_never_waits_for_the_helper_behind_a_drain() {
    let scratch = Scratch::new("drain-beside-helper");
    let paused = armed_and_paused(&scratch, &[Point::Source, Point::Helper]);
    reached(&paused, 0);
    let helper = paused.side.hold_helper();
    release(&paused, 0);
    reached(&paused, 1);
    assert!(paused.thread.send(StoreOp::Delete {
        thread: ThreadId(2)
    }));
    assert_removed(&paused, 2);
    release(&paused, 1);
    drop(helper);
    then_purged(paused);
}

#[test]
fn a_write_after_the_copy_let_go_of_the_live_store_never_waits() {
    for at in [Point::Source, Point::Clean] {
        let scratch = Scratch::new("source-free");
        let paused = armed_and_paused(&scratch, &[at]);
        reached(&paused, 0);
        assert!(paused.thread.send(StoreOp::Delete {
            thread: ThreadId(2)
        }));
        assert_removed(&paused, 2);
        release(&paused, 0);
        then_purged(paused);
    }
}

/// Rez measurement for the pull request, not a check: a delete that arrives while the real
/// purge's copy holds the live store, and the turn queued right behind it. Each sample holds the
/// copy, sends the delete, waits until it has stopped the copy and waits for it to let go, queues
/// the turn, and only then releases the copy, so both are timed from the release: the copy's own
/// stop and close, not the hold. In its drain the copy is held just before its checkpoint, which
/// can't be stopped, so the delete waits for all of it. In its snapshot it's held inside
/// `VACUUM INTO`, from the progress handler, once some of the snapshot is on disk. The delete that
/// armed the purge, with nothing running, is the baseline. Five samples in each. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_delete_during_a_purge`;
/// `MEASURE_ONLY` names another store than the largest.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
#[expect(
    clippy::too_many_lines,
    reason = "one sample reads best as one sequence"
)]
fn measure_delete_during_a_purge() {
    use crate::store::tests::{MEASURED_STORES, measured_store};
    const SAMPLES: u64 = 5;
    const LONG: Duration = Duration::from_secs(60);
    let only = std::env::var("MEASURE_ONLY").unwrap_or_else(|_| MEASURED_STORES[3].0.to_owned());
    let (name, shape) = MEASURED_STORES
        .into_iter()
        .find(|(name, _)| *name == only)
        .expect("a measured store");
    let scratch = Scratch::new("measure-purge-delete");
    let mut store = Some(measured_store(&scratch, shape));
    let size = std::fs::metadata(scratch.db()).expect("size").len() >> 20;
    let snapshot = PathBuf::from(format!("{}.purge-snapshot", scratch.db().display()));
    let worst = |took: &[Duration]| took.iter().max().copied().unwrap_or_default();
    let (mut armed, mut during, mut behind) = (Vec::new(), Vec::new(), Vec::new());
    let mut sample = 0;
    for at in [Point::Drain, Point::InSnapshot] {
        for _ in 0..SAMPLES {
            let (reached, reaching) = std_mpsc::channel();
            let (releasing, release) = std_mpsc::channel();
            let (heard, attempts) = std_mpsc::channel();
            let (waits, waiting) = std_mpsc::channel();
            let schedule = Schedule {
                first_retry: Duration::from_millis(20),
                last_retry: Duration::from_millis(20),
                attempts: Some(heard),
                pauses: Some(Pauses {
                    at: std::sync::Mutex::new(vec![Pause {
                        at,
                        reached,
                        release,
                    }]),
                    waiting: Some(waits),
                }),
                ..Schedule::default()
            };
            let opened = store
                .take()
                .unwrap_or_else(|| Store::open(&scratch.db()).expect("reopen"));
            let (acks, mut replies) = mpsc::unbounded_channel();
            let custody = crate::test_support::custody(&opened);
            let thread = StoreThread::spawn(opened, custody, acks, schedule).expect("thread");
            let mut ack = || replies.blocking_recv().expect("acknowledged");
            let started = Instant::now();
            assert!(thread.send(StoreOp::Delete {
                thread: ThreadId(1 + 2 * sample),
            }));
            assert!(matches!(
                ack(),
                Input::Deleted {
                    outcome: Deleted::Removed,
                    ..
                }
            ));
            armed.push(started.elapsed());
            reaching
                .recv_timeout(LONG)
                .expect("the copy holds the store");
            let written = std::fs::metadata(&snapshot).map_or(0, |meta| meta.len()) >> 10;
            assert!(thread.send(StoreOp::Delete {
                thread: ThreadId(2 + 2 * sample),
            }));
            waiting
                .recv_timeout(LONG)
                .expect("the delete stopped the copy");
            let turn = (ThreadId(100_000 + sample), RunId(100_000 + sample));
            assert!(thread.send(StoreOp::BeginTurn {
                thread: turn.0,
                title: Some("t".into()),
                run: turn.1,
                text: "a measured turn".into(),
                screen: None,
            }));
            let released = Instant::now();
            releasing.send(()).expect("the copy waits");
            assert!(matches!(
                ack(),
                Input::Deleted {
                    outcome: Deleted::Removed,
                    ..
                }
            ));
            during.push(released.elapsed());
            assert_eq!(ack(), Input::TurnStored { run: turn.1 });
            behind.push(released.elapsed());
            assert!(thread.send(StoreOp::EndRun {
                screen: None,
                thread: turn.0,
                run: turn.1,
                text: None,
                revealed: None,
                clear: None,
                end: RunEnd::Done,
            }));
            let mut ended = Vec::new();
            while ended.last() != Some(&Attempt::Done) {
                ended.push(attempts.recv().expect("the purge goes on"));
            }
            assert_eq!(ended[0], Attempt::Stale, "the delete stopped the held copy");
            thread.close().blocking_recv().expect("closed");
            eprintln!(
                "{name} ({size} MiB), copy held in its {at:?} with {written} KiB of snapshot written: \
                 armed {:.1?}, delete {:.1?} and the turn behind it {:.1?} after the release",
                armed[armed.len() - 1],
                during[during.len() - 1],
                behind[behind.len() - 1],
            );
            sample += 1;
        }
    }
    eprintln!(
        "{name} ({size} MiB): worst delete with nothing running {:.1?}; after a held copy's \
         release, worst delete {:.1?} and worst turn behind it {:.1?}",
        worst(&armed),
        worst(&during),
        worst(&behind),
    );
}

#[test]
fn a_withdrawn_screen_is_stored_after_its_turn() {
    use kumochi_protocol::message::HeldScreen;
    let scratch = Scratch::new("held-withdraw-op");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    assert!(store.send(StoreOp::BeginTurn {
        thread,
        title: Some("t".into()),
        run,
        text: "hi".into(),
        screen: Some(HeldScreen::Attached),
    }));
    assert!(store.send(StoreOp::WithdrawScreen { run }));
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(
        got,
        [Input::TurnStored { run }],
        "nothing answers the withdrawal"
    );
    let conn = rusqlite::Connection::open(scratch.db()).expect("raw open");
    let stored: Option<String> = conn
        .query_row("SELECT screen FROM runs WHERE id = 1", [], |row| row.get(0))
        .expect("the run's row");
    assert_eq!(stored.as_deref(), Some("withdrawn"));
}

#[test]
fn an_unsent_screen_is_cleared_after_its_turn() {
    use kumochi_protocol::message::HeldScreen;
    let scratch = Scratch::new("held-forget-op");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let custody = crate::test_support::custody(&store);
    let store = StoreThread::spawn(store, custody, acks, Schedule::default()).expect("thread");
    assert!(store.send(StoreOp::BeginTurn {
        thread,
        title: Some("t".into()),
        run,
        text: "hi".into(),
        screen: Some(HeldScreen::Screen),
    }));
    let other = RunId(2);
    assert!(store.send(StoreOp::BeginTurn {
        thread: ThreadId(2),
        title: Some("u".into()),
        run: other,
        text: "hey".into(),
        screen: Some(HeldScreen::Attached),
    }));
    assert!(store.send(StoreOp::RepairScreen {
        run,
        correction: UnsentScreen::Clear
    }));
    store.close().blocking_recv().expect("closed");
    let mut got = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        got.push(reply);
    }
    assert_eq!(
        got,
        [Input::TurnStored { run }, Input::TurnStored { run: other }],
        "nothing answers the clear"
    );
    let conn = rusqlite::Connection::open(scratch.db()).expect("raw open");
    let stored = |id: i64| -> Option<String> {
        conn.query_row("SELECT screen FROM runs WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .expect("the run's row")
    };
    assert_eq!(stored(1), None);
    assert_eq!(stored(2).as_deref(), Some("pending_attached"));
}

#[test]
fn an_owed_picture_correction_refuses_new_authority_until_its_row_is_gone() {
    use kumochi_protocol::message::HeldScreen;

    use crate::coordinator::{BuildKey, LeaseId, ScreenGrantRefusal, SendKey};

    let scratch = Scratch::new("bounded-screen-repair");
    let mut store = Store::open(&scratch.db()).unwrap();
    for id in [1, 2] {
        store
            .begin_held_turn(
                ThreadId(id),
                Some("t"),
                RunId(id),
                "words",
                Some(HeldScreen::Screen),
            )
            .unwrap();
    }
    assert!(store.grant_screen(RunId(1)).unwrap());
    store
        .end_run(ThreadId(1), RunId(1), &[], RunEnd::Done)
        .unwrap();
    store
        .conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER refuse_late_clear BEFORE UPDATE OF screen ON runs
         WHEN old.id = 1 AND new.screen IS NULL BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
        )
        .unwrap();
    let (acks, mut replies) = mpsc::unbounded_channel();
    let mut repairs = BTreeMap::new();
    let mut purger = Purger::new(Schedule::default(), &store);
    let lease = LeaseId {
        key: SendKey::Build(BuildKey {
            run: RunId(2),
            round: 1,
            attempt: 1,
        }),
        send: 2,
    };
    super::serve_op(
        &mut store,
        &mut purger,
        &acks,
        &mut repairs,
        StoreOp::RepairScreen {
            run: RunId(1),
            correction: UnsentScreen::Clear,
        },
    );
    super::serve_op(
        &mut store,
        &mut purger,
        &acks,
        &mut repairs,
        StoreOp::GrantScreen { lease },
    );
    assert_eq!(
        replies.try_recv().unwrap(),
        Input::ScreenGrantStored {
            lease,
            result: Err(ScreenGrantRefusal::Store)
        }
    );
    assert_eq!(store.delete(ThreadId(1)).outcome, Deleted::Removed);
    let lease = LeaseId { send: 3, ..lease };
    super::serve_op(
        &mut store,
        &mut purger,
        &acks,
        &mut repairs,
        StoreOp::GrantScreen { lease },
    );
    assert_eq!(
        replies.try_recv().unwrap(),
        Input::ScreenGrantStored {
            lease,
            result: Ok(())
        }
    );
}
