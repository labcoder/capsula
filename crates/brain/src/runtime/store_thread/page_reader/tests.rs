use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::message::{DataToShell, PageOf, RunId, ThreadId};
use tokio::sync::mpsc;

use super::super::tests::{PATIENCE, beside, next, owed, owing, purge_files};
use super::super::{Attempt, Gate, Schedule, StoreThread};
use super::{Barrier, Exit};
use crate::coordinator::{Input, StoreOp};
use crate::runtime::Seen;
use crate::store::{Cancel, Pause, Pauses, Point, RunEnd, Store, StoreError};
use crate::test_support::Scratch;

/// Forwards a tokio channel to one a test can wait on with a timeout.
fn forward<T: Send + 'static>(mut from: mpsc::UnboundedReceiver<T>) -> std_mpsc::Receiver<T> {
    let (to, out) = std_mpsc::channel();
    std::thread::spawn(move || {
        while let Some(item) = from.blocking_recv() {
            if to.send(item).is_err() {
                return;
            }
        }
    });
    out
}

/// The store's thread with its reader and helper, and a test's handles on them.
struct Running {
    thread: StoreThread,
    acks: std_mpsc::Receiver<Input>,
    seen: std_mpsc::Receiver<Seen>,
    gates: std_mpsc::Sender<Gate>,
    read_gates: std_mpsc::Sender<Gate>,
    attempts: std_mpsc::Receiver<Attempt>,
}

/// An owed purge starts at once and isn't retried during the test.
fn run(store: Store, pauses: Option<Pauses>) -> Running {
    run_with(store, pauses, None)
}

fn run_with(
    store: Store,
    pauses: Option<Pauses>,
    checkpoint_gate: Option<super::HelperGate>,
) -> Running {
    let (acks, replies) = mpsc::unbounded_channel();
    let (observed, seen) = mpsc::unbounded_channel();
    let (gates, gates_heard) = std_mpsc::channel();
    let (read_gates, read_gates_heard) = std_mpsc::channel();
    let (heard, attempts) = std_mpsc::channel();
    let schedule = Schedule {
        quiet: Duration::from_millis(1),
        first_retry: Duration::from_secs(600),
        last_retry: Duration::from_secs(600),
        free_space: |_| Ok(u64::MAX),
        attempts: Some(heard),
        observed: Some(observed),
        gates: Some(gates_heard),
        read_gates: Some(read_gates_heard),
        checkpoint_gate,
        pauses,
    };
    Running {
        thread: StoreThread::spawn(store, acks, schedule).expect("thread"),
        acks: forward(replies),
        seen: forward(seen),
        gates,
        read_gates,
        attempts,
    }
}

/// Holds the next op `matches` accepts until the returned sender sends or drops.
fn gate(gates: &std_mpsc::Sender<Gate>, matches: fn(&StoreOp) -> bool) -> std_mpsc::Sender<()> {
    let (release, held) = std_mpsc::channel();
    gates
        .send(Gate {
            matches,
            release: held,
        })
        .expect("the thread runs");
    release
}

impl Running {
    fn ack(&self) -> Input {
        self.acks.recv_timeout(PATIENCE).expect("an answer")
    }

    fn saw(&self, wanted: impl Fn(&Seen) -> bool) {
        while !wanted(&self.seen.recv_timeout(PATIENCE).expect("seen")) {}
    }
}

fn is_turn(op: &StoreOp) -> bool {
    matches!(op, StoreOp::BeginTurn { .. })
}

fn is_read(op: &StoreOp) -> bool {
    matches!(op, StoreOp::ReadPage { .. })
}

fn turn(id: u64) -> StoreOp {
    StoreOp::BeginTurn {
        thread: ThreadId(id),
        title: Some("t".to_owned()),
        run: RunId(id),
        text: "words".to_owned(),
    }
}

fn threads(request: u32) -> StoreOp {
    StoreOp::ReadPage {
        epoch: 1,
        request,
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    }
}

/// The threads a thread-list reply lists.
fn listed(reply: Input) -> Vec<ThreadId> {
    match reply {
        Input::PageRead {
            reply: DataToShell::ThreadList { threads, .. },
            ..
        } => threads.into_iter().map(|row| row.id).collect(),
        other => panic!("not a thread list: {other:?}"),
    }
}

/// A store with thread `id`'s run ended.
fn with_thread(store: &mut Store, id: u64) {
    store
        .begin_turn(ThreadId(id), Some("t"), RunId(id), "words")
        .expect("turn");
    store
        .end_run(ThreadId(id), RunId(id), &[], RunEnd::Done)
        .expect("ended");
}

#[test]
fn a_read_sees_the_ops_queued_before_it() {
    let scratch = Scratch::new("read-barrier");
    let running = run(Store::open(&scratch.db()).expect("store"), None);
    let release = gate(&running.gates, is_turn);
    assert!(running.thread.send(turn(1)));
    running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
    assert!(running.thread.send(threads(7)));
    running.saw(|seen| *seen == Seen::ReadWaiting);
    release.send(()).expect("held");
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(1) });
    assert_eq!(listed(running.ack()), [ThreadId(1)]);
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn failed_and_empty_ops_count_toward_a_reads_wait() {
    let scratch = Scratch::new("read-after-failures");
    let running = run(Store::open(&scratch.db()).expect("store"), None);
    for op in [
        StoreOp::EndRun {
            thread: ThreadId(9),
            run: RunId(9),
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        },
        StoreOp::Delete {
            thread: ThreadId(9),
        },
        StoreOp::ClearScenes { scenes: Vec::new() },
        threads(1),
    ] {
        assert!(running.thread.send(op));
    }
    assert_eq!(running.ack(), Input::StoreFailed { run: RunId(9) });
    assert!(matches!(running.ack(), Input::Deleted { .. }));
    assert!(matches!(
        running.ack(),
        Input::ScenesCleared { ok: true, .. }
    ));
    assert_eq!(listed(running.ack()), []);
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn a_write_commits_while_a_read_holds_a_snapshot() {
    let scratch = Scratch::new("write-beside-read");
    let mut store = Store::open(&scratch.db()).expect("store");
    with_thread(&mut store, 1);
    let running = run(store, None);
    let release = gate(&running.read_gates, is_read);
    assert!(running.thread.send(threads(1)));
    running.saw(|seen| *seen == Seen::ReadHeld);
    assert!(running.thread.send(turn(2)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
    release.send(()).expect("held");
    assert_eq!(listed(running.ack()), [ThreadId(2), ThreadId(1)]);
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn a_ready_swap_waits_for_the_running_read_and_goes_before_the_next() {
    let scratch = Scratch::new("swap-priority");
    let mut store = owing(&scratch);
    with_thread(&mut store, 2);
    let (reached, reaching) = std_mpsc::channel();
    let (copy, release) = std_mpsc::channel();
    let running = run(
        store,
        Some(Pauses {
            at: Mutex::new(vec![Pause {
                at: Point::Clean,
                reached,
                release,
            }]),
            waiting: None,
        }),
    );
    reaching
        .recv_timeout(PATIENCE)
        .expect("the copy let go of the store");
    let first = gate(&running.read_gates, is_read);
    assert!(running.thread.send(threads(1)));
    running.saw(|seen| *seen == Seen::ReadHeld);
    let second = gate(&running.read_gates, is_read);
    copy.send(()).expect("the copy waits");
    running.saw(|seen| *seen == Seen::SwapWaiting);
    assert!(running.thread.send(threads(2)));
    first.send(()).expect("held");
    assert_eq!(listed(running.ack()), [ThreadId(2)]);
    // Were the second read to start first, it would hold its connection until released.
    assert_eq!(next(&running.attempts), Attempt::Done);
    second.send(()).expect("held");
    assert_eq!(listed(running.ack()), [ThreadId(2)]);
    // The reader reopened the new file: it sees a write made after the swap.
    assert!(running.thread.send(turn(5)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(5) });
    assert!(running.thread.send(threads(3)));
    assert_eq!(listed(running.ack()), [ThreadId(5), ThreadId(2)]);
    running.thread.close().blocking_recv().expect("closed");
    assert_eq!(owed(&scratch), 0);
    assert_eq!(purge_files(&scratch), Vec::<std::path::PathBuf>::new());
}

#[test]
fn reads_fail_while_the_store_cant_reopen_and_work_once_it_does() {
    let scratch = Scratch::new("read-unavailable");
    let mut store = owing(&scratch);
    with_thread(&mut store, 2);
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped");
    let wal = store.swap_failing_reopen(&copied);
    let running = run(store, None);
    assert_eq!(next(&running.attempts), Attempt::Failed);
    assert!(running.thread.send(threads(1)));
    assert_eq!(
        running.ack(),
        Input::PageRead {
            epoch: 1,
            request: 1,
            reply: DataToShell::PageFailed { request: 1 },
        }
    );
    std::fs::remove_dir(&wal).expect("out of the way");
    assert!(running.thread.send(turn(3)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(3) });
    assert!(running.thread.send(threads(2)));
    assert_eq!(listed(running.ack()), [ThreadId(3), ThreadId(2)]);
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn close_returns_at_once_and_finishes_the_queued_write_and_its_reads() {
    let scratch = Scratch::new("close-with-read");
    let running = run(Store::open(&scratch.db()).expect("store"), None);
    let release = gate(&running.gates, is_turn);
    assert!(running.thread.send(turn(1)));
    running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
    let reading = gate(&running.read_gates, is_read);
    assert!(running.thread.send(threads(4)));
    assert!(running.thread.send(threads(5)));
    let closed = running.thread.close();
    release.send(()).expect("held");
    assert_eq!(
        running.acks.recv_timeout(PATIENCE),
        Ok(Input::TurnStored { run: RunId(1) })
    );
    while running.seen.recv_timeout(PATIENCE).expect("seen") != Seen::ReadHeld {}
    reading.send(()).expect("held");
    for _ in 0..2 {
        assert_eq!(
            listed(running.acks.recv_timeout(PATIENCE).expect("a read")),
            [ThreadId(1)]
        );
    }
    closed.blocking_recv().expect("closed");
    // Every connection let go of the file.
    std::fs::rename(scratch.db(), beside(&scratch.db(), ".moved")).expect("not held");
}

#[test]
fn a_panicking_store_thread_fails_the_reads_waiting_on_it() {
    let scratch = Scratch::new("store-panic");
    let store = Store::open(&scratch.db()).expect("store");
    let side = store.side();
    let barrier = Arc::new(Barrier::default());
    let exit = Exit {
        side: Arc::clone(&side),
        barrier: Arc::clone(&barrier),
        checkpoints: Arc::default(),
    };
    let (waited, wait) = std_mpsc::channel();
    let waiting = Arc::clone(&barrier);
    std::thread::spawn(move || waited.send(waiting.wait(1)));
    let panicked = std::thread::spawn(move || {
        let _exit = exit;
        panic!("a store op panicked");
    })
    .join();
    assert!(panicked.is_err());
    assert_eq!(wait.recv_timeout(PATIENCE), Ok(false));
    assert!(matches!(side.read(|_| Ok(())), Err(StoreError::Closed)));
    drop(store);
}

#[test]
fn the_helper_keeps_a_raise_made_while_it_ran_and_retries_what_a_reader_held() {
    let scratch = Scratch::new("helper-raises");
    let (checked, done) = std_mpsc::channel();
    let (release, released) = std_mpsc::channel();
    let running = run_with(
        Store::open(&scratch.db()).expect("store"),
        None,
        Some((checked, released)),
    );
    let on_disk = || std::fs::metadata(scratch.db()).expect("file").len();
    let big = |id| StoreOp::BeginTurn {
        thread: ThreadId(id),
        title: Some("t".to_owned()),
        run: RunId(id),
        text: "x".repeat(64 * 1024),
    };
    assert!(running.thread.send(big(1)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(1) });
    assert_eq!(done.recv_timeout(PATIENCE), Ok(true));
    // Raised while that checkpoint is held, after its result.
    let before = on_disk();
    assert!(running.thread.send(big(2)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
    release.send(()).expect("held");
    assert_eq!(done.recv_timeout(PATIENCE), Ok(true), "the raise was kept");
    assert!(on_disk() > before + 64 * 1024, "turn 2 reached the file");

    let reader = rusqlite::Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");
    assert!(running.thread.send(big(3)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(3) });
    release.send(()).expect("held");
    assert_eq!(
        done.recv_timeout(PATIENCE),
        Ok(false),
        "the reader holds turn 3"
    );
    let before = on_disk();
    drop(reader);
    release.send(()).expect("held");
    assert_eq!(
        done.recv_timeout(PATIENCE),
        Ok(true),
        "retried with no new raise"
    );
    assert!(on_disk() > before + 64 * 1024, "turn 3 reached the file");
    drop(release);
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn the_helper_owes_nothing_while_the_store_is_closed() {
    let scratch = Scratch::new("helper-closed");
    let mut store = owing(&scratch);
    with_thread(&mut store, 2);
    let copied = store
        .owed()
        .expect("owed")
        .expect("idle")
        .copy(&Arc::new(Cancel::default()))
        .expect("copy")
        .expect("not stopped");
    let wal = store.swap_failing_reopen(&copied);
    let (checked, done) = std_mpsc::channel();
    let (release, released) = std_mpsc::channel();
    let running = run_with(store, None, Some((checked, released)));
    assert_eq!(next(&running.attempts), Attempt::Failed);
    assert!(running.thread.send(turn(3)));
    assert_eq!(running.ack(), Input::StoreFailed { run: RunId(3) });
    assert_eq!(done.recv_timeout(PATIENCE), Ok(true), "nothing to retry");
    drop(release);
    std::fs::remove_dir(&wal).expect("out of the way");
    running.thread.close().blocking_recv().expect("closed");
}
