//! Inline compaction (B5): a run too large only because of its summary rewrites it under the run,
//! once per admission episode, fenced by deletes, Stop and replacements like any build.

use kumochi_protocol::message::{Deleted, EventId, ThreadId};

use super::{R, T, build_context, ends_failed, engine, fresh, request};
use crate::context::{Built, Origin};
use crate::coordinator::{
    BuildContext, BuildKey, CompactBuilt, CompactContext, ContextBuilt, Coordinator, Effect, Input,
    JobAsk, LeaseId, SEND_DEADLINE, SendKey, Stale, StoreOp, SummaryText, Unbuilt,
};
use crate::engines::{Finish, RunRequest, TurnEvent};
use crate::store::{Failure, Generation};

const G: Generation = Generation(5);
const B: ThreadId = ThreadId(2);
const COVERS: (EventId, EventId) = (EventId(20), EventId(31));
const ROOM: usize = 500;

#[test]
fn lifecycle_inline_reader_is_owned_by_the_original_run() {
    let mut core = compacting();
    let read = core
        .current
        .as_ref()
        .unwrap()
        .build
        .as_ref()
        .unwrap()
        .control
        .0
        .clone();
    assert_eq!(read.end, core.current.as_ref().unwrap().control.0.end);
    assert_eq!(read.check(), Ok(()));
    core.handle(Input::Stop {
        run: R,
        revealed: vec![],
    })
    .unwrap();
    assert_eq!(read.check(), Err(crate::recall::Error::Ended));
}

fn key(attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt,
    }
}

fn too_large(attempt: u64, room: Option<usize>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key: key(attempt),
        snapshot: None,
        result: Err(Unbuilt::TooLarge { room }),
    })
}

fn compact_context(attempt: u64) -> Effect {
    Effect::Store(StoreOp::CompactContext(CompactContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: key(attempt),
        thread: T,
        snapshot: None,
        room: ROOM,
    }))
}

fn compact_built(attempt: u64, sources: &[ThreadId]) -> Input {
    Input::CompactBuilt(CompactBuilt {
        key: key(attempt),
        result: Ok(Some((
            JobAsk {
                generation: G,
                built: Built {
                    request: request(),
                    sources: sources.to_vec(),
                },
                covers: Some(COVERS),
            },
            100,
        ))),
    })
}

fn start(attempt: u64) -> Effect {
    Effect::StartEngine {
        request: RunRequest {
            key: SendKey::Build(key(attempt)),
            run: R,
            thread: T,
            origin: Origin::Task(T),
            request: request(),
        },
    }
}

fn ready(attempt: u64) -> Input {
    Input::SendReady {
        key: SendKey::Build(key(attempt)),
        hold: None,
    }
}

fn granted(attempt: u64) -> Vec<Effect> {
    let lease = LeaseId {
        key: SendKey::Build(key(attempt)),
        send: 1,
    };
    vec![
        Effect::Grant(lease),
        Effect::ArmSendDeadline {
            lease,
            after: SEND_DEADLINE,
        },
    ]
}

fn stored(attempt: u64) -> Input {
    Input::SummaryStored {
        owner: SendKey::Build(key(attempt)),
        applied: true,
    }
}

fn store_summary(attempt: u64, text: &str) -> Effect {
    Effect::Store(StoreOp::StoreSummary {
        owner: SendKey::Build(key(attempt)),
        thread: T,
        generation: G,
        covers: COVERS,
        text: SummaryText(text.to_owned()),
        model: None,
    })
}

/// A later attempt of the run's first round, after its first build read the snapshot.
fn rebuilt(attempt: u64) -> Effect {
    Effect::Store(StoreOp::BuildContext(BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: key(attempt),
        thread: T,
        origin: Origin::Task(T),
        snapshot: None,
        first: true,
        drawings: crate::drawing::Digest::default(),
    }))
}

/// Run 10 in new task 4, whose first build was too large with its summary: inline compaction 2
/// read and was admitted, and its request started.
fn compacting() -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    assert_eq!(
        core.handle(Input::TurnStored { run: R }),
        Ok(vec![build_context(R, T, 1)])
    );
    assert_eq!(
        core.handle(too_large(1, Some(ROOM))),
        Ok(vec![compact_context(2)])
    );
    assert_eq!(core.handle(compact_built(2, &[T])), Ok(vec![start(2)]));
    core
}

#[test]
fn a_summary_too_large_for_the_turn_is_rewritten_under_the_run_then_built_again() {
    let mut core = compacting();
    assert_eq!(core.handle(ready(2)), Ok(granted(2)));
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::TextDelta(" Short".to_owned())
        )),
        Ok(Vec::new()),
        "the summary's text never reaches the shell"
    );
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        )),
        Ok(vec![store_summary(2, "Short")])
    );
    assert_eq!(core.handle(stored(2)), Ok(vec![rebuilt(3)]));
    assert_eq!(core.handle(stored(2)), Err(Stale), "acknowledged once");
    assert_eq!(
        core.handle(too_large(3, Some(ROOM))),
        Ok(ends_failed(Failure::Budget)),
        "a second too-large build in the episode fails on its budget"
    );
}

#[test]
fn an_escaped_answer_is_cut_to_the_bytes_its_item_may_add() {
    let mut core = compacting();
    assert_eq!(core.handle(ready(2)), Ok(granted(2)));
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::TextDelta("\"".repeat(60))
        )),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        )),
        Ok(vec![store_summary(2, &"\"".repeat(50))]),
        "60 quotes escape to 120 bytes; 50 take the whole allowance of 100"
    );
}

#[test]
fn formatting_the_run_never_shows_the_summary_being_collected() {
    const SENTINEL: &str = "sentinel: the rewritten summary of task four";
    let mut core = compacting();
    assert_eq!(core.handle(ready(2)), Ok(granted(2)));
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::TextDelta(SENTINEL.to_owned())
        )),
        Ok(Vec::new())
    );
    let shown = format!("{:?}", core.current);
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    assert!(!shown.contains("sentinel"), "the summary text leaked");
    assert!(
        !shown.contains(byte_list.trim_matches(['[', ']'])),
        "the summary bytes leaked"
    );
    assert!(
        shown.contains(&format!("text_bytes: {}", SENTINEL.len())),
        "the length is missing"
    );
}

#[test]
fn an_empty_summary_or_nothing_to_rewrite_fails_the_run_on_its_budget() {
    let mut core = compacting();
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        )),
        Ok(ends_failed(Failure::Budget))
    );
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(too_large(1, Some(ROOM))).expect("current");
    assert_eq!(
        core.handle(Input::CompactBuilt(CompactBuilt {
            key: key(2),
            result: Ok(None),
        })),
        Ok(ends_failed(Failure::Budget))
    );
}

#[test]
fn a_delete_between_the_compact_read_and_the_grant_rebuilds_and_never_compacts_twice() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(too_large(1, Some(ROOM))).expect("current");
    assert_eq!(core.handle(compact_built(2, &[T, B])), Ok(vec![start(2)]));
    assert_eq!(
        core.handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false,
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: B })])
    );
    assert_eq!(
        core.handle(ready(2)),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Rebuild(SendKey::Build(key(2))),
        ]),
        "the rebuild waits for the delete's answer"
    );
    let answered = core
        .handle(Input::Deleted {
            thread: B,
            outcome: Deleted::Hidden,
            last_run: None,
        })
        .expect("answered");
    assert!(answered.contains(&rebuilt(3)), "{answered:?}");
    assert_eq!(
        core.handle(too_large(3, Some(ROOM))),
        Ok(ends_failed(Failure::Budget)),
        "one inline compaction per admission episode, even across a delete's rebuild"
    );
}

#[test]
fn stop_or_a_replacement_leaves_a_held_summary_ack_stale() {
    for replace in [false, true] {
        let mut core = compacting();
        assert_eq!(core.handle(ready(2)), Ok(granted(2)));
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::TextDelta("Short".to_owned()),
        ))
        .expect("current");
        assert_eq!(
            core.handle(engine(
                super::engine_key(&core, R),
                TurnEvent::Finished(Finish::Completed)
            )),
            Ok(vec![store_summary(2, "Short")])
        );
        if replace {
            core.handle(Input::Turn {
                turn: 1,
                thread: Some(T),
                text: "again".into(),
            })
            .expect("a turn is never stale");
        } else {
            core.handle(Input::Stop {
                run: R,
                revealed: Vec::new(),
            })
            .expect("current");
        }
        assert_eq!(core.handle(stored(2)), Err(Stale));
        assert_eq!(core.handle(compact_built(2, &[T])), Err(Stale));
    }
}

#[test]
fn a_compaction_read_never_answers_a_normal_build_or_the_reverse() {
    let mut core = compacting();
    assert_eq!(core.handle(too_large(2, Some(ROOM))), Err(Stale));
    assert_eq!(core.handle(compact_built(1, &[T])), Err(Stale));
}
