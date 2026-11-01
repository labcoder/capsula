use capsula_protocol::message::{
    ControlToShell, DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, Step, ThreadId,
};

use super::{
    Coordinator, Effect, EngineEvent, Input, MAX_DELTA, MAX_TEXT, Stale, StoreOp, WALL_CLOCK,
};
use crate::store::{EventKind, Failure, Interruption, RunEnd};

const T: ThreadId = ThreadId(4);
const R: RunId = RunId(10);

fn step(label: &str) -> Step {
    Step {
        label: label.to_owned(),
    }
}

fn state(run: RunId, thread: ThreadId, phase: Phase) -> Effect {
    Effect::Control(ControlToShell::RunState { run, thread, phase })
}

fn engine(run: RunId, event: EngineEvent) -> Input {
    Input::Engine { run, event }
}

/// A coordinator whose last ids were thread 3 and run 9, with run 10 started in a new thread 4
/// and its turn stored.
fn started() -> Coordinator {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core
}

#[test]
fn a_turn_is_stored_before_the_engine_starts() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    let turn = core
        .handle(Input::Turn {
            thread: None,
            text: "  \n  Draw a cat\nplease".to_owned(),
        })
        .expect("turn");
    assert_eq!(
        turn,
        vec![
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: Some("Draw a cat".to_owned()),
                run: R,
                text: "  \n  Draw a cat\nplease".to_owned(),
            }),
            state(R, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: R,
                after: WALL_CLOCK
            },
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("current"),
        vec![Effect::StartEngine {
            run: R,
            text: "  \n  Draw a cat\nplease".to_owned(),
        }]
    );
}

#[test]
fn a_step_is_sent_only_once_it_is_committed() {
    let mut core = started();
    let streamed = core
        .handle(engine(R, EngineEvent::Text("a cat".to_owned())))
        .expect("current");
    assert_eq!(
        streamed,
        vec![
            state(R, T, Phase::Answering),
            Effect::Data(DataToShell::AssistantText {
                run: R,
                delta: "a cat".to_owned()
            }),
        ]
    );
    let drawn = core
        .handle(engine(R, EngineEvent::Step(step("head"))))
        .expect("current");
    assert_eq!(
        drawn,
        vec![
            Effect::Store(StoreOp::Append {
                thread: T,
                run: R,
                kind: EventKind::AssistantText,
                text: Some("a cat".to_owned()),
                body: None,
            }),
            Effect::Store(StoreOp::CommitStep {
                thread: T,
                run: R,
                index: 0,
                step: step("head"),
            }),
        ],
        "text before the step is stored first, and nothing reaches the shell yet"
    );
    assert_eq!(
        core.handle(Input::StepStored {
            run: R,
            index: 0,
            step: step("head")
        })
        .expect("current"),
        vec![Effect::Data(DataToShell::StepCommit {
            run: R,
            index: 0,
            step: step("head"),
        })]
    );
}

#[test]
fn stop_cancels_first_and_refuses_everything_late() {
    let mut core = started();
    for event in [
        EngineEvent::Step(step("a")),
        EngineEvent::Step(step("b")),
        EngineEvent::Text("tail".to_owned()),
    ] {
        core.handle(engine(R, event)).expect("current");
    }
    let stopped = core
        .handle(Input::Stop {
            run: R,
            revealed: vec![1, 0, 1, 7],
        })
        .expect("current");
    assert_eq!(
        stopped,
        vec![
            Effect::CancelEngine { run: R },
            Effect::Store(StoreOp::Append {
                thread: T,
                run: R,
                kind: EventKind::AssistantText,
                text: Some("tail".to_owned()),
                body: None,
            }),
            Effect::Store(StoreOp::Append {
                thread: T,
                run: R,
                kind: EventKind::Revealed,
                text: None,
                body: Some("[0,1]".to_owned()),
            }),
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Interrupted(Interruption::Stop),
            }),
            state(R, T, Phase::Interrupted),
        ],
        "revealed keeps only committed steps, once each, in order"
    );
    let late = [
        engine(R, EngineEvent::Step(step("c"))),
        engine(R, EngineEvent::Text("more".to_owned())),
        engine(R, EngineEvent::Done),
        Input::StepStored {
            run: R,
            index: 1,
            step: step("b"),
        },
        Input::Stop {
            run: R,
            revealed: Vec::new(),
        },
        Input::Deadline { run: R },
    ];
    for input in late {
        assert_eq!(core.handle(input.clone()), Err(Stale), "{input:?}");
    }
}

#[test]
fn a_stop_before_the_turn_is_stored_still_ends_it_and_the_engine_never_starts() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: Some(ThreadId(2)),
        text: "hi".to_owned(),
    })
    .expect("turn");
    let stopped = core
        .handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("current");
    assert_eq!(
        stopped,
        vec![
            Effect::Store(StoreOp::Append {
                thread: ThreadId(2),
                run: R,
                kind: EventKind::Revealed,
                text: None,
                body: Some("[]".to_owned()),
            }),
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Interrupted(Interruption::Stop),
            }),
            state(R, ThreadId(2), Phase::Interrupted),
        ],
        "nothing to cancel, and the store ends the run after the queued turn"
    );
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

#[test]
fn a_new_turn_replaces_the_current_run_with_a_higher_id() {
    let mut core = started();
    let next = core
        .handle(Input::Turn {
            thread: Some(T),
            text: "again".to_owned(),
        })
        .expect("turn");
    assert_eq!(
        next[..3],
        [
            Effect::CancelEngine { run: R },
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Interrupted(Interruption::Replaced),
            }),
            state(R, T, Phase::Interrupted),
        ]
    );
    assert_eq!(
        next[3],
        Effect::Store(StoreOp::BeginTurn {
            thread: T,
            title: None,
            run: RunId(11),
            text: "again".to_owned(),
        })
    );
    assert_eq!(
        core.handle(engine(R, EngineEvent::Text("old".to_owned()))),
        Err(Stale)
    );
    core.handle(Input::TurnStored { run: RunId(11) })
        .expect("the new run is current");
    assert_eq!(
        core.handle(engine(RunId(11), EngineEvent::Done))
            .expect("current"),
        vec![
            Effect::Store(StoreOp::EndRun {
                run: RunId(11),
                end: RunEnd::Done
            }),
            state(RunId(11), T, Phase::Done),
        ]
    );
}

#[test]
fn done_stores_the_text_and_needs_no_cancel() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Text("x".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(engine(R, EngineEvent::Done)).expect("current"),
        vec![
            Effect::Store(StoreOp::Append {
                thread: T,
                run: R,
                kind: EventKind::AssistantText,
                text: Some("x".to_owned()),
                body: None,
            }),
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Done
            }),
            state(R, T, Phase::Done),
        ]
    );
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()), "no run left");
}

#[test]
fn budgets_are_absolute_for_the_run() {
    let mut core = started();
    let half = "x".repeat(MAX_TEXT / 2);
    core.handle(engine(R, EngineEvent::Text(half.clone())))
        .expect("first half");
    core.handle(engine(R, EngineEvent::Text(half)))
        .expect("exactly the cap");
    let over = core
        .handle(engine(R, EngineEvent::Text("y".to_owned())))
        .expect("current");
    assert_eq!(
        over.first(),
        Some(&Effect::CancelEngine { run: R }),
        "the byte past the cap is never sent"
    );
    assert!(!over.iter().any(|effect| matches!(effect, Effect::Data(_))));
    assert_eq!(
        over[over.len() - 2],
        Effect::Store(StoreOp::EndRun {
            run: R,
            end: RunEnd::Failed(Failure::Budget)
        })
    );

    let mut core = started();
    for index in 0..MAX_STEPS {
        core.handle(engine(R, EngineEvent::Step(step(&index.to_string()))))
            .expect("within the step budget");
    }
    let over = core
        .handle(engine(R, EngineEvent::Step(step("one too many"))))
        .expect("current");
    assert!(over.contains(&Effect::Store(StoreOp::EndRun {
        run: R,
        end: RunEnd::Failed(Failure::Budget)
    })));
    assert!(
        !over
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitStep { .. })))
    );

    let mut core = started();
    let over = core
        .handle(engine(
            R,
            EngineEvent::Step(step(&"x".repeat(MAX_STEP_LABEL + 1))),
        ))
        .expect("current");
    assert!(over.contains(&Effect::Store(StoreOp::EndRun {
        run: R,
        end: RunEnd::Failed(Failure::Engine)
    })));

    let mut core = started();
    assert!(
        core.handle(Input::Deadline { run: R })
            .expect("current")
            .contains(&state(R, T, Phase::Failed))
    );
}

#[test]
fn a_lost_shell_ends_the_run_without_telling_it() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::ShellLost).expect("never stale"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Interrupted(Interruption::ShellLost),
            }),
        ]
    );
}

#[test]
fn a_stalled_view_or_a_refused_turn_ends_the_run() {
    let mut core = started();
    assert!(
        core.handle(Input::Stalled { run: R })
            .expect("current")
            .contains(&Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Interrupted(Interruption::ViewStalled),
            }))
    );

    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: Some(ThreadId(99)),
        text: "hi".to_owned(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![state(R, ThreadId(99), Phase::Failed)],
        "nothing more is written for a turn the store refused"
    );
}

fn ends_failed(effects: &[Effect], failure: Failure) -> bool {
    !effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Data(_) | Effect::Store(StoreOp::CommitStep { .. })
        )
    }) && effects.contains(&Effect::Store(StoreOp::EndRun {
        run: R,
        end: RunEnd::Failed(failure),
    }))
}

#[test]
fn over_budget_steps_and_oversized_labels_commit_and_send_nothing() {
    let mut core = started();
    for index in 0..MAX_STEPS {
        core.handle(engine(R, EngineEvent::Step(step(&index.to_string()))))
            .expect("within the step budget");
    }
    let over = core
        .handle(engine(R, EngineEvent::Step(step("one too many"))))
        .expect("current");
    assert!(ends_failed(&over, Failure::Budget), "{over:?}");

    let mut core = started();
    let label = "x".repeat(MAX_STEP_LABEL + 1);
    let over = core
        .handle(engine(R, EngineEvent::Step(step(&label))))
        .expect("current");
    assert!(ends_failed(&over, Failure::Engine), "{over:?}");
}

#[test]
fn done_waits_for_the_last_step_to_be_committed_and_shown() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("last"))))
        .expect("current");
    assert_eq!(
        core.handle(engine(R, EngineEvent::Done)).expect("current"),
        Vec::new(),
        "nothing ends while a step is in the store"
    );
    assert_eq!(
        core.handle(engine(R, EngineEvent::Text("after done".to_owned()))),
        Err(Stale)
    );
    assert_eq!(
        core.handle(Input::StepStored {
            run: R,
            index: 0,
            step: step("last")
        })
        .expect("current"),
        vec![
            Effect::Data(DataToShell::StepCommit {
                run: R,
                index: 0,
                step: step("last"),
            }),
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Done
            }),
            state(R, T, Phase::Done),
        ]
    );
}

#[test]
fn a_store_failure_mid_run_still_records_the_failure() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Store(StoreOp::EndRun {
                run: R,
                end: RunEnd::Failed(Failure::Store),
            }),
            state(R, T, Phase::Failed),
        ]
    );
}

#[test]
fn a_repeated_turn_ack_never_starts_a_second_engine() {
    let mut core = started();
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

#[test]
fn a_long_delta_goes_out_in_frames_that_fit_the_data_pipe() {
    let mut core = started();
    // Three-byte characters, so a plain byte split would land inside one.
    let delta = "€".repeat(MAX_DELTA / 3 * 2 + 5);
    let sent = core
        .handle(engine(R, EngineEvent::Text(delta.clone())))
        .expect("current");
    let pieces: Vec<&str> = sent
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::AssistantText { delta, .. }) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(pieces.len(), 3);
    assert!(pieces.iter().all(|piece| piece.len() <= MAX_DELTA));
    assert_eq!(pieces.concat(), delta);
}
