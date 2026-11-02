use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use capsula_protocol::message::{DataToShell, PageOf, RunId, Step, ThreadId};
use rusqlite::Connection;
use tokio::sync::mpsc;

use super::{Attempt, Job, Purger, Schedule, StoreThread, serve_op};
use crate::coordinator::{Input, StoreOp};
use crate::store::{RunEnd, SceneTarget, Store, Swap};
use crate::test_support::Scratch;

/// A store owing the purge of its deleted thread 1.
fn owing(scratch: &Scratch) -> Store {
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
    };
    let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
    (thread, attempts)
}

fn next(attempts: &std_mpsc::Receiver<Attempt>) -> Attempt {
    attempts
        .recv_timeout(Duration::from_secs(10))
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

fn owed(scratch: &Scratch) -> i64 {
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
    let (thread, attempts) = spawn(store);
    assert_eq!(next(&attempts), Attempt::Failed);
    Connection::open(scratch.db())
        .expect("fixer")
        .execute_batch("DROP TRIGGER keep_owed")
        .expect("drop");
    done_after(&attempts, &[Attempt::Failed]);
    thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
}

/// `db` with `suffix` appended.
fn beside(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// The purge's files left in the store's folder.
fn purge_files(scratch: &Scratch) -> Vec<PathBuf> {
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
        .copy(&Arc::new(AtomicBool::new(false)))
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
        .copy(&Arc::new(AtomicBool::new(false)))
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
        title: Some("new".to_owned()),
        run,
        text: text.to_owned(),
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
        store.search_ids("stored").expect("search"),
        vec![ThreadId(3)]
    );
    assert_eq!(
        store.search_ids("kept").expect("search"),
        vec![kept],
        "the same file, not an empty one"
    );
    assert_eq!(store.search_ids("refused").expect("search"), Vec::new());
}

#[test]
fn a_crash_right_after_the_link_keeps_the_store_and_the_purge_finishes() {
    let scratch = Scratch::new("crash-linked");
    let store = owing(&scratch);
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(AtomicBool::new(false)))
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
            title: Some("t".to_owned()),
            run,
            text: "hi".to_owned(),
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
fn page_reads_neither_postpone_a_purge_nor_stop_its_copy() {
    let scratch = Scratch::new("read-during-copy");
    let mut store = owing(&scratch);
    let (acks, mut replies) = mpsc::unbounded_channel();
    let (heard, attempts) = std_mpsc::channel();
    let mut purger = Purger::new(
        Schedule {
            quiet: Duration::from_secs(600),
            attempts: Some(heard),
            observed: None,
            ..Schedule::default()
        },
        &store,
    );
    purger.next = Some(Instant::now() + Duration::from_secs(60));
    let next = purger.next;
    let read = || StoreOp::ReadPage {
        epoch: 1,
        request: 3,
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    };
    let expected = Input::PageRead {
        epoch: 1,
        request: 3,
        reply: DataToShell::ThreadList {
            request: 3,
            threads: Vec::new(),
            next: None,
        },
    };
    serve_op(&mut store, &mut purger, &acks, read());
    assert_eq!(replies.try_recv().expect("reply"), expected);
    assert_eq!(
        purger.next, next,
        "a read doesn't postpone the next attempt"
    );

    let copying = store.owed().expect("owed").expect("idle");
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (started, running) = std_mpsc::channel();
    let (resume, paused) = std_mpsc::channel();
    let helper = std::thread::spawn(move || {
        started.send(()).expect("running");
        paused.recv().expect("resume the real copy");
        copying.copy(&stopping)
    });
    purger.job = Some(Job::Copy {
        stop: Arc::clone(&stop),
        helper,
    });
    running
        .recv_timeout(Duration::from_secs(10))
        .expect("helper running");
    serve_op(&mut store, &mut purger, &acks, read());
    assert_eq!(replies.try_recv().expect("reply"), expected);
    assert!(
        !stop.load(Ordering::Relaxed),
        "the read didn't cancel the copy"
    );
    assert_eq!(purger.next, next);
    resume.send(()).expect("resume");
    let copy = purger.job.take().expect("copy running");
    purger.finish(&mut store, copy);
    let remove = purger.job.take().expect("removing the old store");
    purger.finish(&mut store, remove);
    done_after(&attempts, &[]);
    assert_eq!(owed(&scratch), 0);
}

#[test]
fn a_page_read_starts_an_already_due_purge_without_waiting_for_quiet() {
    let scratch = Scratch::new("read-due-purge");
    let mut store = owing(&scratch);
    let (acks, mut replies) = mpsc::unbounded_channel();
    let (heard, attempts) = std_mpsc::channel();
    let mut purger = Purger::new(
        Schedule {
            quiet: Duration::from_secs(600),
            free_space: |_| Ok(u64::MAX),
            attempts: Some(heard),
            observed: None,
            ..Schedule::default()
        },
        &store,
    );
    serve_op(
        &mut store,
        &mut purger,
        &acks,
        StoreOp::ReadPage {
            epoch: 1,
            request: 1,
            page: PageOf::Threads {
                search: None,
                after: None,
            },
        },
    );
    assert!(matches!(
        replies.try_recv(),
        Ok(Input::PageRead {
            reply: DataToShell::ThreadList { .. },
            ..
        })
    ));
    let copy = purger
        .job
        .take()
        .expect("the due purge started despite the read");
    assert!(matches!(copy, Job::Copy { .. }));
    purger.finish(&mut store, copy);
    let remove = purger.job.take().expect("removing the old store");
    purger.finish(&mut store, remove);
    done_after(&attempts, &[]);
    assert_eq!(owed(&scratch), 0);
}

/// Rez measurement for the pull request, not a check: how long the store's thread takes to answer
/// a page read, and a write queued right behind it, on the large measured stores with a purge
/// owed. Pages are a thread's 50 newest events at near the frame budget each, and a search for a
/// phrase every event holds. The write alone, with no read ahead of it, is the before. Run it
/// with `cargo test --release -p capsula-brain -- --ignored --nocapture measure_page_reads`;
/// set `MEASURE_ONLY` to one store's name to run just that one.
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
        // Just under a turn's 256 KiB cap, so an events page reads about 12.8 MB.
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
        store.begin_delete(ThreadId(2)).expect("begin");
        store.finish_delete(ThreadId(2)).expect("finish");
        let (acks, mut replies) = mpsc::unbounded_channel();
        let thread = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
        let write = |thread: &StoreThread, at: u64| {
            assert!(thread.send(StoreOp::BeginTurn {
                thread: history,
                title: None,
                run: RunId(2_000_000 + at),
                text: "a measured turn".to_owned(),
            }));
        };
        let mut alone = Vec::new();
        for at in 0..ROUNDS {
            let started = Instant::now();
            write(&thread, at);
            assert!(matches!(
                replies.blocking_recv(),
                Some(Input::TurnStored { .. })
            ));
            alone.push(started.elapsed());
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
                "events page, 50 turns near 256 KiB",
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
                let started = Instant::now();
                assert!(thread.send(StoreOp::ReadPage {
                    epoch: 1,
                    request: 0,
                    page: page.clone(),
                }));
                write(&thread, round * ROUNDS + at);
                assert!(matches!(
                    replies.blocking_recv(),
                    Some(Input::PageRead {
                        reply: DataToShell::ThreadList { .. } | DataToShell::Page { .. },
                        ..
                    })
                ));
                reads.push(started.elapsed());
                assert!(matches!(
                    replies.blocking_recv(),
                    Some(Input::TurnStored { .. })
                ));
                behind.push(started.elapsed());
            }
            lines.push(format!(
                "{what}: read {}; the BeginTurn behind it {}",
                summary(reads),
                summary(behind)
            ));
        }
        thread.close().blocking_recv().expect("closed");
        eprintln!("{}", lines.join("\n  "));
    }
}
