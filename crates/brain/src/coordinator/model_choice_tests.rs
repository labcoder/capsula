//! Model-choice reads and writes in the core loop: answers reach only the pair that asked, the
//! read table stays bounded through cancellation, and a read of a new chat waits for its row.

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ControlToShell, Effort, MAX_PENDING_MODEL_READS, ModelChoice, ModelSlug,
    SavedChoice, ThreadId, ThreadOp,
};

use super::{R, T, ack, draw_step, fresh, started};
use crate::coordinator::{Effect, Input, ModelRead, StoreOp};
use crate::recall::control::READ_TIME;

const MAIN: ThreadId = ThreadId(1);

fn picked() -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new("model-result-sentinel").expect("valid"),
        effort: Effort::XHigh,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

fn read(request: u32, thread: ThreadId) -> Input {
    Input::ReadModel { request, thread }
}

fn refused(request: u32, thread: ThreadId) -> Effect {
    Effect::Control(ControlToShell::ThreadModelRead {
        request,
        thread,
        result: ChoiceRead::Failed,
    })
}

/// The reads handed to the reader, in order.
fn sent(effects: &[Effect]) -> Vec<ModelRead> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::ReadModel(read)) => Some(read.clone()),
            _ => None,
        })
        .collect()
}

/// Set and Clear are one write each, on main as on a task, stamped with the asking session; an
/// answer reaches only that session.
#[test]
fn a_write_is_stamped_and_its_answer_reaches_only_the_session_that_asked() {
    let mut core = fresh();
    let session = core.model_session();
    for (thread, choice) in [(MAIN, Some(picked())), (T, None)] {
        assert_eq!(
            core.handle(Input::ThreadOp(ThreadOp::SetModel {
                request: 7,
                thread,
                choice: choice.clone(),
            })),
            Ok(vec![Effect::Store(StoreOp::SetModel {
                session,
                request: 7,
                thread,
                choice,
            })])
        );
    }
    let found = ChoiceRead::Found {
        choice: SavedChoice::Chosen(picked()),
        revision: ChoiceRev(3),
    };
    core.begin_session();
    assert_eq!(
        core.handle(Input::ModelSet {
            session,
            request: 7,
            thread: T,
            result: found,
        }),
        Ok(Vec::new()),
        "an earlier pair's answer with the same request and thread isn't delivered"
    );
}

#[test]
fn reads_hold_their_slots_until_answered_even_across_a_new_session() {
    let mut core = fresh();
    let old = core.model_session();
    let mut admitted = Vec::new();
    for request in 1..=u32::try_from(MAX_PENDING_MODEL_READS).expect("small") {
        let effects = core.handle(read(request, T)).expect("admitted");
        admitted.extend(sent(&effects));
    }
    assert_eq!(
        admitted
            .iter()
            .map(|read| (read.session, read.request, read.thread))
            .collect::<Vec<_>>(),
        [(old, 1, T), (old, 2, T)]
    );
    assert_eq!(
        core.handle(read(3, MAIN)),
        Ok(vec![refused(3, MAIN)]),
        "the table is full"
    );
    core.begin_session();
    for read in &admitted {
        assert!(
            read.control.check().is_err(),
            "the lost pair's read is cancelled"
        );
    }
    assert_eq!(
        core.handle(read(1, T)),
        Ok(vec![refused(1, T)]),
        "a cancelled read keeps its slot until the reader answers"
    );
    let found = ChoiceRead::Found {
        choice: SavedChoice::Inherit,
        revision: ChoiceRev(0),
    };
    for read in admitted {
        assert_eq!(
            core.handle(Input::ModelRead {
                read,
                result: found.clone(),
            }),
            Ok(Vec::new()),
            "an earlier pair's answer isn't delivered"
        );
    }
    let effects = core.handle(read(1, T)).expect("capacity is back");
    let reads = sent(&effects);
    assert_eq!(reads.len(), 1);
    assert_eq!(
        core.handle(Input::ModelRead {
            read: reads[0].clone(),
            result: found.clone(),
        }),
        Ok(vec![Effect::Control(ControlToShell::ThreadModelRead {
            request: 1,
            thread: T,
            result: found,
        })])
    );
}

#[test]
fn a_repeated_request_is_refused_without_taking_a_slot() {
    let mut core = fresh();
    assert_eq!(sent(&core.handle(read(1, T)).expect("admitted")).len(), 1);
    assert_eq!(core.handle(read(1, MAIN)), Ok(vec![refused(1, MAIN)]));
    assert_eq!(
        sent(&core.handle(read(2, MAIN)).expect("admitted")).len(),
        1
    );
}

/// A New chat's id arrives before its row: the read waits for the turn write that creates the
/// row and goes to the store after it, so it can't answer Missing for a chat about to exist.
#[test]
fn a_read_of_a_new_chat_waits_for_the_write_that_creates_its_row() {
    let mut core = started();
    draw_step(&mut core, R, "one").expect("current");
    let new = ThreadId(T.0 + 1);
    let effects = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "b".into(),
        })
        .expect("turn");
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::BeginTurn { .. }))),
        "run 10's step is still in the store, so the new chat's row waits"
    );
    let effects = core.handle(read(1, new)).expect("parked");
    assert!(
        matches!(effects.as_slice(), [Effect::ArmModelReads { .. }]),
        "parked, with its deadline armed: {effects:?}"
    );
    let effects = core.handle(ack(0, "one")).expect("ending");
    let begin = effects.iter().position(|effect| {
        matches!(effect, Effect::Store(StoreOp::BeginTurn { thread, title: Some(_), .. }) if *thread == new)
    });
    let read = effects.iter().position(
        |effect| matches!(effect, Effect::Store(StoreOp::ReadModel(read)) if read.thread == new),
    );
    assert!(
        begin.zip(read).is_some_and(|(begin, read)| begin < read),
        "the read follows the row's write: {begin:?} then {read:?}"
    );
}

/// An existing chat never waits, even while a turn of its own is held.
#[test]
fn a_read_of_a_stored_chat_goes_at_once() {
    let mut core = started();
    draw_step(&mut core, R, "one").expect("current");
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "b".into(),
    })
    .expect("turn");
    assert_eq!(sent(&core.handle(read(1, T)).expect("sent")).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_parked_read_fails_at_its_deadline_and_frees_its_slot() {
    let mut core = started();
    draw_step(&mut core, R, "one").expect("current");
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "b".into(),
    })
    .expect("turn");
    let new = ThreadId(T.0 + 1);
    core.handle(read(1, new)).expect("parked");
    core.handle(read(2, new)).expect("parked");
    tokio::time::advance(READ_TIME / 2).await;
    let early = core.handle(Input::ModelReadsDue).expect("early");
    assert!(
        matches!(early.as_slice(), [Effect::ArmModelReads { .. }]),
        "nothing expires early and the timer is armed again: {early:?}"
    );
    tokio::time::advance(READ_TIME).await;
    assert_eq!(
        core.handle(Input::ModelReadsDue),
        Ok(vec![refused(1, new), refused(2, new)])
    );
    assert_eq!(
        sent(&core.handle(read(3, T)).expect("a slot is free")).len(),
        1
    );
}

/// A read the reader holds past its deadline answers `Failed` once and is cancelled; it keeps
/// its slot until the reader lets it go, and the late answer is dropped.
#[tokio::test(start_paused = true)]
async fn a_handed_out_read_fails_at_its_deadline_and_holds_its_slot_until_retired() {
    let mut core = fresh();
    let effects = core.handle(read(1, T)).expect("sent");
    let reads = sent(&effects);
    assert_eq!(reads.len(), 1);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ArmModelReads { .. })),
        "its deadline is armed: {effects:?}"
    );
    tokio::time::advance(READ_TIME).await;
    assert_eq!(core.handle(Input::ModelReadsDue), Ok(vec![refused(1, T)]));
    assert!(
        reads[0].control.check().is_err(),
        "the reader is told to stop"
    );
    assert_eq!(
        core.handle(Input::ModelReadsDue),
        Ok(Vec::new()),
        "answered once"
    );
    assert_eq!(sent(&core.handle(read(2, T)).expect("admitted")).len(), 1);
    assert_eq!(
        core.handle(read(3, T)),
        Ok(vec![refused(3, T)]),
        "the expired read still holds its slot"
    );
    let found = ChoiceRead::Found {
        choice: SavedChoice::Inherit,
        revision: ChoiceRev(0),
    };
    assert_eq!(
        core.handle(Input::ModelRead {
            read: reads[0].clone(),
            result: found,
        }),
        Ok(Vec::new()),
        "the late answer is dropped"
    );
    assert_eq!(
        sent(&core.handle(read(3, T)).expect("a slot is free")).len(),
        1
    );
}

/// Losing the pair cancels its reads at once, before any new pair starts.
#[test]
fn losing_the_shell_cancels_its_reads() {
    let mut core = fresh();
    let reads = sent(&core.handle(read(1, T)).expect("sent"));
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()));
    assert!(reads[0].control.check().is_err(), "cancelled on loss");
    assert_eq!(
        core.handle(Input::ModelRead {
            read: reads[0].clone(),
            result: ChoiceRead::Missing,
        }),
        Ok(Vec::new()),
        "nobody is left to tell"
    );
}

/// A read that finished in time but is taken after its deadline, before the timer fires, still
/// answers the one `Failed` its deadline promised.
#[tokio::test(start_paused = true)]
async fn an_answer_taken_after_the_deadline_is_the_promised_failure() {
    let mut core = fresh();
    let reads = sent(&core.handle(read(1, T)).expect("sent"));
    tokio::time::advance(READ_TIME).await;
    let found = ChoiceRead::Found {
        choice: SavedChoice::Inherit,
        revision: ChoiceRev(0),
    };
    assert_eq!(
        core.handle(Input::ModelRead {
            read: reads[0].clone(),
            result: found,
        }),
        Ok(vec![refused(1, T)])
    );
    assert_eq!(
        core.handle(Input::ModelReadsDue),
        Ok(Vec::new()),
        "answered once"
    );
}
