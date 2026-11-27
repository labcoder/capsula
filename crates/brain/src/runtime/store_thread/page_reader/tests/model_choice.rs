use super::*;

use kumochi_protocol::message::{ChoiceRead, ChoiceRev, SavedChoice};

use crate::coordinator::ModelRead;
use crate::recall::control::{Control, READ_TIME};

const TASK: ThreadId = ThreadId(2);

fn read(request: u32) -> StoreOp {
    StoreOp::ReadModel(ModelRead {
        session: 1,
        request,
        thread: TASK,
        control: Control::new(std::time::Instant::now() + READ_TIME),
    })
}

fn answered(reply: Input) -> (u32, ChoiceRead) {
    let Input::ModelRead { read, result } = reply else {
        panic!("a model read's answer: {reply:?}")
    };
    (read.request, result)
}

/// A read sent while an earlier Set is still writing waits for it and answers the committed
/// choice with its revision.
#[test]
fn a_read_waits_for_an_earlier_write_and_answers_its_revision() {
    let scratch = Scratch::new("model-read-barrier");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
        .begin_turn(TASK, Some("task"), RunId(1), "prior")
        .expect("turn");
    store
        .end_run(TASK, RunId(1), &[], RunEnd::Done)
        .expect("end");
    let running = run(store, None);
    assert!(running.thread.send(read(1)));
    assert_eq!(
        answered(running.ack()),
        (
            1,
            ChoiceRead::Found {
                choice: SavedChoice::Inherit,
                revision: ChoiceRev(0)
            }
        )
    );
    let picked = ModelChoice {
        model: ModelSlug::new("picked").expect("valid"),
        effort: Effort::High,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    };
    let release = gate(&running.gates, |op| matches!(op, StoreOp::SetModel { .. }));
    assert!(running.thread.send(StoreOp::SetModel {
        session: 1,
        request: 2,
        thread: TASK,
        choice: Some(picked.clone()),
    }));
    running.saw(|seen| matches!(seen, Seen::StoreOpStarted(StoreOp::SetModel { .. })));
    assert!(running.thread.send(read(3)));
    running.saw(|seen| *seen == Seen::ReadWaiting);
    release.send(()).expect("released");
    let committed = ChoiceRead::Found {
        choice: SavedChoice::Chosen(picked),
        revision: ChoiceRev(1),
    };
    assert_eq!(
        running.ack(),
        Input::ModelSet {
            session: 1,
            request: 2,
            thread: TASK,
            result: committed.clone(),
        }
    );
    assert_eq!(answered(running.ack()), (3, committed));
}

/// A read whose pair is gone or whose deadline passed answers Failed, never a guess.
#[test]
fn a_cancelled_or_late_read_answers_failed() {
    let scratch = Scratch::new("model-read-cancelled");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let running = run(store, None);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
    for control in [
        Control::new(std::time::Instant::now() + READ_TIME).observe(cancel),
        Control::new(std::time::Instant::now()),
    ] {
        assert!(running.thread.send(StoreOp::ReadModel(ModelRead {
            session: 1,
            request: 1,
            thread: ThreadId(1),
            control,
        })));
        assert_eq!(answered(running.ack()), (1, ChoiceRead::Failed));
    }
}
