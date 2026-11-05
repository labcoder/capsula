use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{DataToShell, Deleted, PageOf, RunId, Step, ThreadId};
use rusqlite::Connection;
use tokio::sync::mpsc;

use super::{Attempt, Purger, Schedule, StoreThread};
use crate::coordinator::{Input, StoreOp};
use crate::store::{Cancel, Pause, Pauses, Point, RunEnd, SceneTarget, Store, Swap};
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
        quiet: Duration::from_millis(1),
        first_retry: Duration::from_millis(20),
        last_retry: Duration::from_millis(20),
        free_space,
        attempts: Some(heard),
        observed: None,
        gates: None,
        read_gates: None,
        checkpoint_gate: None,
        pauses: None,
    };
    let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
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
    let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
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
fn an_old_store_held_with_delete_sharing_is_finished_only_once_its_name_is_gone() {
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 1 | 2 | 4;
    let scratch = Scratch::new("old-shared");
    let store = swapped_with_old_left(&scratch);
    let mut holder = Some(
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE_DELETE)
            .open(beside(&scratch.db(), ".purge-old"))
            .expect("held"),
    );
    let (thread, attempts) = spawn(store);
    match next(&attempts) {
        // NTFS with POSIX deletion drops the name at once, while the file is still open.
        Attempt::Done => eprintln!("the old store's name went while it was held"),
        attempt => {
            eprintln!("the old store's name stayed while it was held");
            assert_eq!(attempt, Attempt::Failed);
            assert_eq!(
                purge_files(&scratch),
                vec![beside(&scratch.db(), ".purge-old")]
            );
            drop(holder.take());
            done_after(&attempts, &[Attempt::Failed]);
        }
    }
    assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
    drop(holder);
    thread.close().blocking_recv().expect("closed");
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
    let worker = StoreThread::spawn(
        store,
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
    let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
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
fn a_purge_found_finished_resets_the_backoff() {
    let scratch = Scratch::new("found-done");
    let mut store = Store::open(&scratch.db()).expect("store");
    let schedule = Schedule {
        first_retry: Duration::from_secs(60),
        last_retry: Duration::from_secs(240),
        ..Schedule::default()
    };
    let mut purger = Purger::new(schedule, &store);
    purger.retry = Duration::from_secs(240);
    purger.start(&mut store);
    assert_eq!(purger.retry, Duration::from_secs(60));
    assert!(purger.next.is_none());
}

#[test]
fn copies_made_stale_back_off_until_one_finishes() {
    let scratch = Scratch::new("stale-backoff");
    let store = owing(&scratch);
    let schedule = Schedule {
        first_retry: Duration::from_secs(60),
        last_retry: Duration::from_secs(240),
        ..Schedule::default()
    };
    let mut purger = Purger::new(schedule, &store);
    for wait in [60, 120, 240, 240] {
        let before = Instant::now();
        purger.settle(Attempt::Stale, &store);
        let at = purger.next.expect("a retry");
        let wait = Duration::from_secs(wait);
        assert!(at >= before + wait && at <= Instant::now() + wait);
    }
    purger.settle(Attempt::Done, &store);
    assert_eq!(purger.retry, Duration::from_secs(60));
}

#[test]
fn every_operation_gets_one_reply_in_order() {
    let scratch = Scratch::new("replies");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let store = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
    for op in [
        StoreOp::BeginTurn {
            thread,
            title: Some("t".into()),
            run,
            text: "hi".into(),
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
            thread,
            run,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        },
        StoreOp::ReadPage {
            epoch: 4,
            request: 9,
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
                epoch: 4,
                request: 9,
                reply: DataToShell::Page {
                    request: 9,
                    thread: ThreadId(999),
                    events: Vec::new(),
                    before: None
                },
            },
        ]
    );
}

#[test]
fn a_heard_point_is_stored_and_answers_nothing() {
    let scratch = Scratch::new("heard-op");
    let store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    let (acks, mut replies) = mpsc::unbounded_channel();
    let store = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
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
        },
        StoreOp::EndRun {
            thread,
            run,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
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
        [Input::TurnStored { run }, Input::RunStored { run }],
        "a heard point that fails, here on a run id SQLite can't hold, answers nothing either"
    );
    let stored: (i64, i64, i64) = rusqlite::Connection::open(scratch.db())
        .expect("raw open")
        .query_row(
            "SELECT heard_segment, heard_sample, heard_text FROM runs WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the run's point");
    assert_eq!(stored, (2, 480, 17));
}

#[test]
fn a_page_read_neither_stops_a_copy_nor_waits_for_it() {
    let scratch = Scratch::new("read-during-copy");
    let paused = armed_and_paused(&scratch, &[Point::Snapshot]);
    reached(&paused, 0);
    assert!(paused.thread.send(StoreOp::ReadPage {
        epoch: 1,
        request: 3,
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
        request: 3,
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

/// Rez measurement for the pull request, not a check: how long a page read takes, and a write
/// sent right behind it, on the large measured stores with a purge owed. The events page reads a
/// thread's 51 newest events, 25 turns just under the 256 KiB cap and 26 short answers, about
/// 6.5 MB, and the search is for a phrase every event holds. The
/// writes go to a thread the pages don't read, and each one's run is ended (untimed), so no run
/// stays active to hold off the owed purge. Each reply is timed as it arrives, in either order.
/// The write alone, with no read ahead of it, is the before. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_page_reads`; set
/// `MEASURE_ONLY` to one store's name to run just that one.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
#[expect(clippy::too_many_lines, reason = "one measurement, read top to bottom")]
fn measure_page_reads() {
    use crate::store::tests::{MEASURED_STORES, measured_store};
    use crate::store::{EventKind, NewEvent};
    const ROUNDS: u64 = 30;
    let only = std::env::var("MEASURE_ONLY").ok();
    let summary = |mut took: Vec<Duration>| {
        took.sort_unstable();
        format!(
            "median {:.1?}, worst {:.1?}",
            took[took.len() / 2],
            took[took.len() - 1]
        )
    };
    for (name, shape) in MEASURED_STORES {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let scratch = Scratch::new("measure-pages");
        let mut store = measured_store(&scratch, shape);
        let size = std::fs::metadata(scratch.db()).expect("size").len() >> 20;
        let history = ThreadId(1_000_000);
        // Just under a turn's 256 KiB cap.
        let long = "near the frame budget ".repeat(11_900);
        for turn in 0..50_u64 {
            let run = RunId(1_000_000 + turn);
            let title = (turn == 0).then_some("t");
            store.begin_turn(history, title, run, &long).expect("turn");
            store
                .append(
                    history,
                    run,
                    &[NewEvent {
                        kind: EventKind::AssistantText,
                        text: Some("a short answer"),
                        body: None,
                    }],
                )
                .expect("answer");
            store
                .end_run(history, run, &[], RunEnd::Done)
                .expect("ended");
        }
        let writes = ThreadId(2_000_000);
        store
            .begin_turn(writes, Some("w"), RunId(2_000_000), "the writes' thread")
            .expect("turn");
        store
            .end_run(writes, RunId(2_000_000), &[], RunEnd::Done)
            .expect("ended");
        store.begin_delete(ThreadId(2)).expect("begin");
        store.finish_delete(ThreadId(2)).expect("finish");
        let (acks, mut replies) = mpsc::unbounded_channel();
        let thread = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
        let begin = |thread: &StoreThread, at: u64| {
            assert!(thread.send(StoreOp::BeginTurn {
                thread: writes,
                title: None,
                run: RunId(2_000_001 + at),
                text: "a measured turn".into(),
            }));
        };
        let settle = |thread: &StoreThread, replies: &mut mpsc::UnboundedReceiver<Input>, at| {
            let run = RunId(2_000_001 + at);
            assert!(thread.send(StoreOp::EndRun {
                thread: writes,
                run,
                text: None,
                revealed: None,
                clear: None,
                end: RunEnd::Done,
            }));
            assert_eq!(replies.blocking_recv(), Some(Input::RunStored { run }));
        };
        let mut alone = Vec::new();
        for at in 0..ROUNDS {
            let started = Instant::now();
            begin(&thread, at);
            assert_eq!(
                replies.blocking_recv(),
                Some(Input::TurnStored {
                    run: RunId(2_000_001 + at)
                })
            );
            alone.push(started.elapsed());
            settle(&thread, &mut replies, at);
        }
        let common = |after| PageOf::Threads {
            search: Some("lorem".to_owned()),
            after,
        };
        assert!(thread.send(StoreOp::ReadPage {
            epoch: 1,
            request: 0,
            page: common(None),
        }));
        let Some(Input::PageRead {
            reply: DataToShell::ThreadList { threads, next, .. },
            ..
        }) = replies.blocking_recv()
        else {
            panic!("a search page")
        };
        assert_eq!(threads.len(), 50, "every thread matches");
        let pages = [
            (
                "events page, 51 rows read, 25 near 256 KiB",
                PageOf::Events {
                    thread: history,
                    before: None,
                },
            ),
            ("search for a common word", common(None)),
            (
                "its continuation",
                common(Some(next.expect("more matches"))),
            ),
        ];
        let mut lines = vec![format!(
            "{name} ({size} MiB): a BeginTurn alone {}",
            summary(alone)
        )];
        for (round, (what, page)) in (1..).zip(pages) {
            let (mut reads, mut behind) = (Vec::new(), Vec::new());
            for at in 0..ROUNDS {
                let at = round * ROUNDS + at;
                let request = u32::try_from(at).expect("small");
                let started = Instant::now();
                assert!(thread.send(StoreOp::ReadPage {
                    epoch: 1,
                    request,
                    page: page.clone(),
                }));
                begin(&thread, at);
                let (mut read, mut stored) = (None, None);
                while read.is_none() || stored.is_none() {
                    match replies.blocking_recv().expect("the store answers") {
                        Input::PageRead {
                            request: answered,
                            reply,
                            ..
                        } if answered == request => {
                            read = Some(started.elapsed());
                            match reply {
                                DataToShell::ThreadList { threads, .. } => {
                                    assert_eq!(threads.len(), 50);
                                }
                                DataToShell::Page { events, .. } => {
                                    let shape: Vec<(usize, bool)> = events
                                        .iter()
                                        .map(|event| (event.text.len(), event.cut))
                                        .collect();
                                    // The newest answer; the near-cap turn behind it doesn't fit.
                                    assert_eq!(shape, [("a short answer".len(), false)]);
                                }
                                other => panic!("unexpected {other:?}"),
                            }
                        }
                        Input::TurnStored { run } if run == RunId(2_000_001 + at) => {
                            stored = Some(started.elapsed());
                        }
                        other => panic!("unexpected {other:?}"),
                    }
                }
                reads.extend(read);
                behind.extend(stored);
                settle(&thread, &mut replies, at);
            }
            lines.push(format!(
                "{what}: read {}; the BeginTurn sent behind it {}",
                summary(reads),
                summary(behind)
            ));
        }
        thread.close().blocking_recv().expect("closed");
        eprintln!("{}", lines.join("\n  "));
    }
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
    let thread = StoreThread::spawn(store, acks_tx, schedule).expect("thread");
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
            let thread = StoreThread::spawn(opened, acks, schedule).expect("thread");
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
