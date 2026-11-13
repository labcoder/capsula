use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::coordinator::{Coordinator, Effect, HoldKey, HoldSnapshot};
use crate::store::Generation;
use kumochi_protocol::message::GestureId;

const KEY: HoldKey = HoldKey {
    epoch: 3,
    gesture: GestureId(8),
};
const MAIN: ThreadId = ThreadId(1);
const TASK: ThreadId = ThreadId(2);

fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(model).unwrap(),
        effort,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

fn prepare(core: &Coordinator, target: Option<ThreadId>) -> StoreOp {
    core.prepare_hold(
        KEY,
        target,
        42,
        RunSnapshot {
            origin: Origin::Kumo,
            assistant: AssistantName::new("Kumo").unwrap(),
            user: None,
            choice: Some(choice("default", Effort::Low)),
            connection: ConnectionTag(99),
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}

fn snapshot(reply: Input) -> HoldSnapshot {
    let Input::HoldPrepared {
        key: KEY,
        result: Ok(snapshot),
    } = reply
    else {
        panic!("snapshot reply: {reply:?}")
    };
    snapshot
}

#[test]
fn new_hold_stores_exact_words_once_only_after_submit() {
    let scratch = Scratch::new("new-held-admission");
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    let running = run(store, None);
    let mut core = Coordinator::ready_for_tests(MAIN, MAIN, RunId(9), Vec::new());
    assert!(running.thread.send(prepare(&core, None)));
    let snapshot = snapshot(running.ack());
    assert_eq!(snapshot.thread, None);
    assert_eq!(snapshot.generation, None);
    let reader = rusqlite::Connection::open(scratch.db()).unwrap();
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM threads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let effects = core
        .submit_hold(snapshot, "recognized-history-sentinel".into(), None)
        .unwrap();
    let writes = effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Store(op) => Some(op),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        vec![StoreOp::BeginTurn {
            thread: TASK,
            title: Some("recognized-history-sentinel".into()),
            run: RunId(10),
            text: "recognized-history-sentinel".into(),
        }]
    );
    assert!(running.thread.send(writes.into_iter().next().unwrap()));
    assert_eq!(running.ack(), Input::TurnStored { run: RunId(10) });
    running.thread.close().blocking_recv().unwrap();
    let events = reader
        .prepare("SELECT thread, run, kind, text FROM events ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        events,
        vec![(2, 10, "user".into(), "recognized-history-sentinel".into())]
    );
}

#[test]
fn recognition_reads_choice_and_live_generation_after_the_prior_write_barrier() {
    let scratch = Scratch::new("held-choice-barrier");
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    store
        .begin_turn(TASK, Some("task"), RunId(1), "prior")
        .unwrap();
    store.end_run(TASK, RunId(1), &[], RunEnd::Done).unwrap();
    let running = run(store, None);
    let mut core = Coordinator::ready_for_tests(MAIN, TASK, RunId(9), Vec::new());
    let captured = choice("captured", Effort::High);
    let release = gate(&running.gates, |op| matches!(op, StoreOp::SetModel { .. }));
    assert!(running.thread.send(StoreOp::SetModel {
        thread: TASK,
        choice: captured.clone()
    }));
    running.saw(|seen| matches!(seen, Seen::StoreOpStarted(StoreOp::SetModel { .. })));
    assert!(running.thread.send(prepare(&core, Some(TASK))));
    running.saw(|seen| *seen == Seen::ReadWaiting);
    release.send(()).unwrap();
    assert_eq!(
        running.ack(),
        Input::ModelSet {
            thread: TASK,
            choice: captured.clone(),
            applied: true
        }
    );
    let snapshot = snapshot(running.ack());
    assert_eq!(snapshot.generation, Some(Generation(0)));
    assert_eq!(snapshot.selection, 42);
    assert_eq!(snapshot.defaults.origin, Origin::Task(TASK));
    assert_eq!(snapshot.defaults.choice, Some(captured.clone()));
    assert_eq!(snapshot.defaults.connection, ConnectionTag(0));
    for effect in core.submit_hold(snapshot, "next".into(), None).unwrap() {
        if let Effect::Store(op) = effect {
            assert!(running.thread.send(op));
        }
    }
    let effects = core.handle(running.ack()).unwrap();
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("build")
    };
    assert!(!build.first);
    let mut build = build.clone();
    let mut later = build.snapshot.clone().unwrap();
    later.choice = Some(choice("later-default", Effort::Low));
    build.snapshot.get_or_insert(later);
    assert!(running.thread.send(StoreOp::BuildContext(build)));
    let Input::ContextBuilt(done) = running.ack() else {
        panic!("built")
    };
    assert_eq!(done.snapshot.unwrap().choice, Some(captured.clone()));
    assert_eq!(done.result.unwrap().request.choice, Some(captured));
    running.thread.close().blocking_recv().unwrap();
}

#[test]
fn a_deleted_or_missing_target_returns_only_fixed_failure() {
    for deleted in [false, true] {
        let scratch = Scratch::new("held-target-gone");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        if deleted {
            store
                .begin_turn(
                    TASK,
                    Some("private-target-marker"),
                    RunId(1),
                    "private-text-marker",
                )
                .unwrap();
            store.end_run(TASK, RunId(1), &[], RunEnd::Done).unwrap();
            assert_eq!(
                store.delete(TASK).outcome,
                kumochi_protocol::message::Deleted::Removed
            );
        }
        let running = run(store, None);
        let core = Coordinator::ready_for_tests(MAIN, TASK, RunId(9), Vec::new());
        assert!(running.thread.send(prepare(&core, Some(TASK))));
        assert_eq!(
            running.ack(),
            Input::HoldPrepared {
                key: KEY,
                result: Err(())
            }
        );
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn held_snapshot_read_ends_on_cancel_or_deadline_at_the_write_barrier() {
    for deadline in [false, true] {
        let scratch = Scratch::new("held-read-ending");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let running = run(store, None);
        let core = Coordinator::ready_for_tests(MAIN, MAIN, RunId(9), Vec::new());
        let StoreOp::ReadHold(read) = prepare(&core, None) else {
            unreachable!()
        };
        let control = read.control.clone();
        let cancel = Arc::clone(&read.snapshot.cancel);
        let release = gate(&running.gates, is_turn);
        assert!(running.thread.send(turn(2)));
        running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
        assert!(running.thread.send(StoreOp::ReadHold(read)));
        running.saw(|seen| *seen == Seen::ReadWaiting);
        if deadline {
            *control.test.time.lock().unwrap() = Some(control.end);
        } else {
            cancel.store(true, Ordering::SeqCst);
        }
        assert_eq!(
            running.ack(),
            Input::HoldPrepared {
                key: KEY,
                result: Err(())
            }
        );
        release.send(()).unwrap();
        assert_eq!(running.ack(), Input::TurnStored { run: RunId(2) });
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn cancel_or_deadline_with_a_snapshot_open_cannot_publish_an_admission() {
    for deadline in [false, true] {
        let scratch = Scratch::new("held-snapshot-ending");
        let mut store = Store::open(&scratch.db()).unwrap();
        store.recover().unwrap();
        let running = run(store, None);
        let core = Coordinator::ready_for_tests(MAIN, MAIN, RunId(9), Vec::new());
        let StoreOp::ReadHold(read) = prepare(&core, Some(MAIN)) else {
            unreachable!()
        };
        let control = read.control.clone();
        let cancel = Arc::clone(&read.snapshot.cancel);
        let release = gate(&running.read_gates, |op| matches!(op, StoreOp::ReadHold(_)));
        assert!(running.thread.send(StoreOp::ReadHold(read)));
        running.saw(|seen| *seen == Seen::ReadHeld);
        if deadline {
            *control.test.time.lock().unwrap() = Some(control.end);
        } else {
            cancel.store(true, Ordering::SeqCst);
        }
        release.send(()).unwrap();
        assert_eq!(
            running.ack(),
            Input::HoldPrepared {
                key: KEY,
                result: Err(())
            }
        );
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn deletion_accepted_after_recognition_fences_snapshot_even_after_its_reply() {
    let scratch = Scratch::new("held-delete-order");
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    store
        .begin_turn(TASK, Some("task"), RunId(1), "prior")
        .unwrap();
    store.end_run(TASK, RunId(1), &[], RunEnd::Done).unwrap();
    let running = run(store, None);
    let mut core = Coordinator::ready_for_tests(MAIN, TASK, RunId(9), Vec::new());
    assert!(running.thread.send(prepare(&core, Some(TASK))));
    let snapshot = snapshot(running.ack());
    assert_eq!(snapshot.generation, Some(Generation(0)));
    snapshot.cancel.store(true, Ordering::SeqCst);
    for effect in core
        .handle(Input::Delete {
            thread: TASK,
            after_turns: 0,
            resend: false,
        })
        .unwrap()
    {
        if let Effect::Store(op) = effect {
            assert!(running.thread.send(op));
        }
    }
    core.handle(running.ack()).unwrap();
    assert!(!core.deletes_pending());
    assert_eq!(
        core.submit_hold(snapshot, "late words".into(), None),
        Err(())
    );
    assert_eq!(core.current_run(), None);
    running.thread.close().blocking_recv().unwrap();
}
