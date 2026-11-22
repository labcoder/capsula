use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::message::{
    AssistantName, DataToShell, Effort, ModelChoice, ModelSlug, PageOf, RunId, ThreadId,
};
use tokio::sync::mpsc;

use super::super::tests::{PATIENCE, beside, next, owed, owing, purge_files};
use super::super::{Attempt, Gate, Schedule, StoreThread};
use super::{Barrier, Exit};
use crate::context::{Built, ConnectionTag, InputItem, Origin, Role, RunSnapshot};
use crate::coordinator::{BuildContext, BuildKey, ContextBuilt, Input, StoreOp, Unbuilt};
use crate::runtime::Seen;
use crate::store::{Cancel, Pause, Pauses, Point, RunEnd, Store, StoreError};
use crate::test_support::Scratch;

mod history;
mod hold;

#[test]
fn card_reads_keep_the_original_cancel_deadline_and_fixed_failure() {
    for deadline in [false, true] {
        let scratch = Scratch::new("card-read-ended");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let running = run(store, None);
        let key = crate::coordinator::PageKey::for_tests(1, 7);
        if deadline {
            *key.control.test.time.lock().unwrap() = Some(key.control.end);
        } else {
            key.control.cancel();
        }
        let cards = Arc::new(crate::coordinator::Cards::new(
            key,
            &kumochi_scene::text::Bounded::default(),
        ));
        let end = cards.key.control.end;
        assert!(running.thread.send(StoreOp::ReadCards {
            cards: Arc::clone(&cards)
        }));
        assert_eq!(
            running.ack(),
            Input::CardsRead {
                cards: Arc::clone(&cards),
                reply: Err(())
            }
        );
        assert_eq!(cards.key.control.end, end);
        running.thread.close().blocking_recv().unwrap();
    }
}

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
        after_close: None,
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
        title: Some("t".into()),
        run: RunId(id),
        text: "words".into(),
        screen: None,
    }
}

fn threads(request: u32) -> StoreOp {
    StoreOp::ReadPage {
        key: crate::coordinator::PageKey::for_tests(1, request),
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    }
}

#[test]
fn admitted_pages_keep_their_end_while_queued_and_at_the_writer_barrier() {
    for queued in [false, true] {
        for deadline in [false, true] {
            let scratch = Scratch::new("page-admission-end");
            let mut store = Store::open(&scratch.db()).unwrap();
            store.recover().unwrap();
            let running = run(store, None);
            let release = if queued {
                let release = gate(&running.read_gates, is_read);
                assert!(running.thread.send(threads(1)));
                running.saw(|seen| *seen == Seen::ReadHeld);
                release
            } else {
                let release = gate(&running.gates, is_turn);
                assert!(running.thread.send(turn(2)));
                running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
                release
            };
            let key = crate::coordinator::PageKey::for_tests(1, 2);
            let end = key.control.end;
            assert!(running.thread.send(StoreOp::ReadPage {
                key: key.clone(),
                page: PageOf::Threads {
                    search: None,
                    after: None
                },
            }));
            if !queued {
                running.saw(|seen| *seen == Seen::ReadWaiting);
            }
            if deadline {
                *key.control.test.time.lock().unwrap() = Some(end);
            } else {
                key.control.cancel();
            }
            if queued {
                release.send(()).unwrap();
                assert_eq!(listed(running.ack()), [ThreadId(1)]);
            }
            let failed = running.ack();
            assert_eq!(
                failed,
                Input::PageRead {
                    key: key.clone(),
                    reply: DataToShell::PageFailed { request: 2 }
                }
            );
            let Input::PageRead { key: completed, .. } = failed else {
                unreachable!()
            };
            assert_eq!(completed.control.end, end);
            if !queued {
                release.send(()).unwrap();
                assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
            }
            running.thread.close().blocking_recv().unwrap();
        }
    }
}

#[test]
fn admitted_page_sql_uses_the_existing_scoped_handler() {
    for deadline in [false, true] {
        let scratch = Scratch::new("page-sql-end");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        for id in 2..=101 {
            with_thread(&mut store, id);
        }
        let key = crate::coordinator::PageKey::for_tests(1, 7);
        let (entered, heard) = std_mpsc::channel();
        let (release, held) = std_mpsc::channel();
        *key.control.test.gate.lock().unwrap() = Some(crate::recall::control::tests::Gate {
            entered,
            release: held,
        });
        let running = run(store, None);
        assert!(running.thread.send(StoreOp::ReadPage {
            key: key.clone(),
            page: PageOf::Threads {
                search: None,
                after: None
            },
        }));
        heard.recv_timeout(PATIENCE).unwrap();
        if deadline {
            *key.control.test.time.lock().unwrap() = Some(key.control.end);
        } else {
            key.control.cancel();
        }
        release.send(()).unwrap();
        assert_eq!(
            running.ack(),
            Input::PageRead {
                key,
                reply: DataToShell::PageFailed { request: 7 }
            }
        );
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn page_handler_install_and_cleanup_failures_never_return_partial_success() {
    for fault in 1..=3 {
        let scratch = Scratch::new("page-handler-failure");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let running = run(store, None);
        let key = crate::coordinator::PageKey::for_tests(1, 7);
        key.control
            .test
            .fault
            .store(fault, std::sync::atomic::Ordering::SeqCst);
        assert!(running.thread.send(StoreOp::ReadPage {
            key: key.clone(),
            page: PageOf::Threads {
                search: None,
                after: None
            },
        }));
        assert_eq!(
            running.ack(),
            Input::PageRead {
                key,
                reply: DataToShell::PageFailed { request: 7 }
            }
        );
        assert!(running.thread.send(threads(8)));
        assert_eq!(listed(running.ack()), [ThreadId(1)]);
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn admitted_page_side_slot_and_swap_waits_observe_cancel_eof_and_end() {
    for swapping in [false, true] {
        for cause in 0..3 {
            let scratch = Scratch::new("page-side-end");
            let mut store = Store::open(&scratch.db()).unwrap();
            store.recover().unwrap();
            let side = store.side();
            let (entered, heard) = std_mpsc::channel();
            let (release, held) = std_mpsc::channel();
            let holding = if swapping {
                side.begin_swap();
                None
            } else {
                let side = Arc::clone(&side);
                let worker = std::thread::spawn(move || {
                    side.read(|_| {
                        entered.send(()).unwrap();
                        held.recv_timeout(PATIENCE * 2).unwrap();
                        Ok(())
                    })
                });
                heard.recv_timeout(PATIENCE).unwrap();
                Some(worker)
            };
            let running = run(store, None);
            let key = crate::coordinator::PageKey::for_tests(1, 7);
            if cause == 0 {
                *key.control.test.time.lock().unwrap() = Some(std::time::Instant::now());
            }
            assert!(running.thread.send(StoreOp::ReadPage {
                key: key.clone(),
                page: PageOf::Threads {
                    search: None,
                    after: None
                },
            }));
            let until = std::time::Instant::now() + PATIENCE;
            while !key
                .control
                .test
                .waiting
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                assert!(std::time::Instant::now() < until);
                std::thread::sleep(Duration::from_millis(1));
            }
            eprintln!("page Side wait reached: swapping={swapping}, cause={cause}");
            let closing = match cause {
                0 => {
                    key.control.cancel();
                    eprintln!("page Side cancellation issued: swapping={swapping}");
                    None
                }
                1 => {
                    let side = Arc::clone(&side);
                    Some(std::thread::spawn(move || side.close()))
                }
                2 => {
                    *key.control.test.time.lock().unwrap() = Some(key.control.end);
                    None
                }
                _ => unreachable!(),
            };
            assert_eq!(
                running.ack(),
                Input::PageRead {
                    key,
                    reply: DataToShell::PageFailed { request: 7 }
                }
            );
            if swapping {
                side.end_swap();
            } else {
                release.send(()).unwrap();
            }
            if let Some(worker) = holding {
                worker.join().unwrap().unwrap();
            }
            if let Some(worker) = closing {
                worker.join().unwrap();
            }
            running.thread.close().blocking_recv().unwrap();
        }
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

/// A run's first build in task `thread`, run `thread`.
fn build(thread: u64, attempt: u64, choice: Option<ModelChoice>) -> StoreOp {
    StoreOp::BuildContext(BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: BuildKey {
            run: RunId(thread),
            round: 1,
            attempt,
        },
        thread: ThreadId(thread),
        origin: Origin::Task(ThreadId(thread)),
        snapshot: Some(RunSnapshot {
            origin: Origin::Task(ThreadId(thread)),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice,
            connection: ConnectionTag(0),
        }),
        first: true,
        arrived: crate::coordinator::Arrived::default(),
        drawings: crate::drawing::Digest::default(),
    })
}

fn choice(slug: &str) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(slug).expect("slug"),
        effort: Effort::Low,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

/// A choice saved on another connection reaches a run's build and a job's read as saved.
#[test]
fn a_saved_route_reaches_the_build_and_the_jobs_read() {
    use crate::coordinator::{JobBuilt, JobKey, JobKind, ReadJob};
    use kumochi_protocol::message::ConnectionKind;

    for connection in [ConnectionKind::ClaudeCode, ConnectionKind::AnthropicKey] {
        let scratch = Scratch::new(&format!("route-read-{connection:?}"));
        let mut store = Store::open(&scratch.db()).expect("store");
        store.recover().expect("recover");
        // Ten runs in task 12, the build's the last, so compaction finds older ones than the
        // eight it keeps raw.
        for run in 2..=12 {
            let title = (run == 2).then_some("t");
            store
                .begin_turn(ThreadId(12), title, RunId(run), "words")
                .expect("turn");
            store
                .end_run(ThreadId(12), RunId(run), &[], RunEnd::Done)
                .expect("ended");
        }
        let picked = ModelChoice {
            connection,
            ..choice("same-slug")
        };
        store.set_model(ThreadId(12), &picked).expect("save");
        let running = run(store, None);
        let request = build(12, 1, Some(choice("chatgpt-default")));
        let StoreOp::BuildContext(context) = &request else {
            panic!("build")
        };
        let snapshot = context.snapshot.clone();
        assert!(running.thread.send(request));
        let (built, read) = made(running.ack());
        assert_eq!(read.as_ref(), Some(&picked), "{connection:?}");
        assert_eq!(built.request.choice.as_ref(), Some(&picked));
        for kind in [JobKind::Title, JobKind::Compact] {
            assert!(running.thread.send(StoreOp::ReadJob(ReadJob {
                control: crate::recall::control::Control::new(
                    std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
                ),
                key: JobKey {
                    thread: ThreadId(12),
                    serial: 1
                },
                kind,
                origin: Origin::Task(ThreadId(12)),
                snapshot: snapshot.clone(),
                compact_at: 1,
            })));
            let Input::JobBuilt(JobBuilt {
                result: Ok(Some(ask)),
                ..
            }) = running.ack()
            else {
                panic!("a job read: {kind:?}");
            };
            assert_eq!(ask.built.request.choice.as_ref(), Some(&picked), "{kind:?}");
        }
        running.thread.close().blocking_recv().expect("closed");
    }
}

fn made(reply: Input) -> (Built, Option<ModelChoice>) {
    match reply {
        Input::ContextBuilt(ContextBuilt {
            snapshot: Some(snapshot),
            result: Ok(built),
            ..
        }) => (built, snapshot.choice),
        other => panic!("not a build: {other:?}"),
    }
}

#[test]
fn a_build_reads_after_the_ops_queued_before_it_and_takes_the_default_model() {
    let scratch = Scratch::new("build-barrier");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    let running = run(store, None);
    let release = gate(&running.gates, is_turn);
    assert!(running.thread.send(turn(2)));
    running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
    assert!(
        running
            .thread
            .send(build(2, 1, Some(choice("default-model"))))
    );
    running.saw(|seen| *seen == Seen::ReadWaiting);
    release.send(()).expect("held");
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
    let (built, model) = made(running.ack());
    assert_eq!(built.sources, [ThreadId(2)]);
    let [
        InputItem::Message {
            role: Role::User,
            text,
        },
    ] = built.request.input.as_slice()
    else {
        panic!("one user message: {:?}", built.request.input);
    };
    let time = text
        .strip_prefix("words\n\nLocal time: ")
        .expect("the turn, then the time");
    let (time, drawings) = time
        .split_once("\nDrawings on screen: ")
        .expect("drawing context follows time");
    assert_eq!(drawings, "[]");
    assert!(
        time.len() == 16
            && time.chars().enumerate().all(|(at, c)| match at {
                4 | 7 => c == '-',
                10 => c == ' ',
                13 => c == ':',
                _ => c.is_ascii_digit(),
            }),
        "a local time to the minute, not {time:?}"
    );
    assert_eq!(
        model,
        Some(choice("default-model")),
        "the thread chose none"
    );
    running.thread.close().blocking_recv().expect("closed");
}

#[test]
fn a_first_build_takes_the_threads_model_and_a_delete_sent_before_a_build_fails_it() {
    let scratch = Scratch::new("build-model");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    with_thread(&mut store, 2);
    store
        .begin_turn(ThreadId(2), None, RunId(3), "more words")
        .expect("turn");
    let running = run(store, None);
    let picked = StoreOp::SetModel {
        thread: ThreadId(2),
        choice: choice("thread-model"),
    };
    assert!(running.thread.send(picked));
    assert_eq!(
        running.ack(),
        Input::ModelSet {
            thread: ThreadId(2),
            choice: choice("thread-model"),
            applied: true,
        }
    );
    let mut first = build(2, 1, Some(choice("default-model")));
    let mut later = build(2, 2, None);
    for (op, is_first) in [(&mut first, true), (&mut later, false)] {
        if let StoreOp::BuildContext(build) = op {
            build.key.run = RunId(3);
            build.first = is_first;
        }
    }
    assert!(running.thread.send(first));
    assert!(running.thread.send(later));
    assert_eq!(made(running.ack()).1, Some(choice("thread-model")));
    assert_eq!(
        made(running.ack()).1,
        None,
        "a later build keeps what the run read"
    );
    assert!(running.thread.send(StoreOp::Delete {
        thread: ThreadId(2)
    }));
    assert!(running.thread.send(build(2, 3, None)));
    assert!(matches!(running.ack(), Input::Deleted { .. }));
    assert!(matches!(
        running.ack(),
        Input::ContextBuilt(ContextBuilt {
            result: Err(Unbuilt::Failed),
            ..
        })
    ));
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
            key: crate::coordinator::PageKey::for_tests(1, 1),
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
    use std::time::Instant;

    use crate::recall::{Error, control::Control};

    let scratch = Scratch::new("store-panic");
    let store = Store::open(&scratch.db()).expect("store");
    let side = store.side();
    let (observed, seen) = mpsc::unbounded_channel();
    let seen = forward(seen);
    let barrier = Arc::new(Barrier::new(Some(observed)));
    let exit = Exit {
        side: Arc::clone(&side),
        barrier: Arc::clone(&barrier),
        checkpoints: Arc::default(),
    };
    let (waited, wait) = std_mpsc::channel();
    let waiting = Arc::clone(&barrier);
    let reader = std::thread::spawn(move || {
        let control = Control::new(Instant::now() + PATIENCE);
        waited
            .send(waiting.wait_until(1, &control))
            .expect("read result");
    });
    assert!(matches!(
        seen.recv_timeout(PATIENCE).expect("reader is waiting"),
        Seen::ReadWaiting
    ));
    let panicked = std::thread::spawn(move || {
        let _exit = exit;
        panic!("a store op panicked");
    })
    .join();
    assert!(panicked.is_err());
    assert_eq!(wait.recv_timeout(PATIENCE), Ok(Err(Error::Ended)));
    reader.join().expect("reader joined");
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
        title: Some("t".into()),
        run: RunId(id),
        text: "x".repeat(64 * 1024).into(),
        screen: None,
    };
    assert!(running.thread.send(big(1)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(1) });
    running.saw(|seen| *seen == Seen::CheckpointOwed);
    assert_eq!(done.recv_timeout(PATIENCE), Ok(true));
    // Raised while that checkpoint is held, after its result.
    let before = on_disk();
    assert!(running.thread.send(big(2)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
    running.saw(|seen| *seen == Seen::CheckpointOwed);
    release.send(()).expect("held");
    assert_eq!(done.recv_timeout(PATIENCE), Ok(true), "the raise was kept");
    assert!(on_disk() > before + 64 * 1024, "turn 2 reached the file");

    let reader = rusqlite::Connection::open(scratch.db()).expect("reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM events;")
        .expect("snapshot");
    assert!(running.thread.send(big(3)));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(3) });
    running.saw(|seen| *seen == Seen::CheckpointOwed);
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
