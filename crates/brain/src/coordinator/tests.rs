use capsula_protocol::message::{
    ControlToShell, DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, Step, ThreadId,
};

use super::{
    Coordinator, Effect, EngineEvent, Input, MAX_DELTA, MAX_TEXT, Stale, StoreOp, WALL_CLOCK,
};
use crate::store::{Failure, Interruption, RunEnd};

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

fn ack(index: u32, label: &str) -> Input {
    Input::StepStored {
        run: R,
        index,
        step: step(label),
    }
}

fn committed(index: u32, label: &str) -> Effect {
    Effect::Data(DataToShell::StepCommit {
        run: R,
        index,
        step: step(label),
    })
}

fn run_end(run: RunId) -> Effect {
    Effect::Data(DataToShell::RunEnd { run })
}

fn text(delta: &str) -> Effect {
    Effect::Data(DataToShell::AssistantText {
        run: R,
        delta: delta.to_owned(),
    })
}

/// Run 10's outcome in thread 4.
fn end_run(text: Option<&str>, revealed: Option<Vec<u32>>, end: RunEnd) -> Effect {
    Effect::Store(StoreOp::EndRun {
        thread: T,
        run: R,
        text: text.map(str::to_owned),
        revealed,
        end,
    })
}

/// The write that replaces an outcome the store refused.
fn failed_store() -> Effect {
    end_run(None, None, RunEnd::Failed(Failure::Store))
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
fn a_blank_turn_is_ignored() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    assert_eq!(
        core.handle(Input::Turn {
            thread: None,
            text: " \n\t ".to_owned(),
        }),
        Ok(Vec::new())
    );
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

#[test]
fn a_step_is_sent_only_once_it_is_committed() {
    let mut core = started();
    assert_eq!(
        core.handle(engine(R, EngineEvent::Text("a cat".to_owned())))
            .expect("current"),
        vec![state(R, T, Phase::Answering), text("a cat")]
    );
    assert_eq!(
        core.handle(engine(R, EngineEvent::Step(step("head"))))
            .expect("current"),
        vec![Effect::Store(StoreOp::CommitStep {
            thread: T,
            run: R,
            text: Some("a cat".to_owned()),
            index: 0,
            step: step("head"),
        })],
        "the text before the step is stored with it, and nothing reaches the shell yet"
    );
    assert_eq!(
        core.handle(ack(0, "head")).expect("current"),
        vec![committed(0, "head")]
    );
}

#[test]
fn text_waits_behind_a_pending_step() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Text("before".to_owned())))
        .expect("current");
    let commit = |text: &str, index: u32, label: &str| {
        vec![Effect::Store(StoreOp::CommitStep {
            thread: T,
            run: R,
            text: Some(text.to_owned()),
            index,
            step: step(label),
        })]
    };
    assert_eq!(
        core.handle(engine(R, EngineEvent::Step(step("a"))))
            .expect("current"),
        commit("before", 0, "a")
    );
    assert_eq!(
        core.handle(engine(R, EngineEvent::Text("after ".to_owned()))),
        Ok(Vec::new()),
        "held behind a"
    );
    core.handle(engine(R, EngineEvent::Text("a".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(engine(R, EngineEvent::Step(step("b"))))
            .expect("current"),
        commit("after a", 1, "b")
    );
    assert_eq!(
        core.handle(engine(R, EngineEvent::Text("after b".to_owned()))),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(ack(0, "a")).expect("current"),
        vec![committed(0, "a"), text("after a")],
        "released up to the next pending step, as one frame"
    );
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("Stop is handled while a step is pending"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ]
    );
    assert_eq!(
        core.handle(ack(1, "b")).expect("still ending"),
        vec![end_run(
            Some("after b"),
            Some(vec![0]),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "the held text is stored but never sent"
    );
}

#[test]
fn stop_tells_the_shell_at_once_and_refuses_everything_late() {
    let mut core = started();
    for event in [
        EngineEvent::Step(step("a")),
        EngineEvent::Step(step("b")),
        EngineEvent::Step(step("c")),
        EngineEvent::Text("tail".to_owned()),
    ] {
        core.handle(engine(R, event)).expect("current");
    }
    core.handle(ack(0, "a")).expect("current");
    core.handle(ack(1, "b")).expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![1, 0, 2, 1, 7],
        })
        .expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ],
        "the outcome waits for the step still in the store"
    );
    let late = [
        engine(R, EngineEvent::Step(step("d"))),
        engine(R, EngineEvent::Text("more".to_owned())),
        engine(R, EngineEvent::Done),
        Input::Stop {
            run: R,
            revealed: Vec::new(),
        },
        Input::Deadline { run: R },
        Input::Stalled { run: R },
        Input::RunStored { run: R },
    ];
    for input in late {
        assert_eq!(core.handle(input.clone()), Err(Stale), "{input:?}");
    }
    assert_eq!(
        core.handle(ack(2, "c")).expect("ending"),
        vec![end_run(
            Some("tail"),
            Some(vec![0, 1]),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "nothing more is sent, and revealed keeps only steps the shell got, once each, in order"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
}

#[test]
fn a_stop_before_the_turn_is_stored_ends_it_after_the_turn_and_the_engine_never_starts() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: Some(ThreadId(2)),
        text: "hi".to_owned(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("current"),
        vec![
            Effect::Revoke { run: R },
            state(R, ThreadId(2), Phase::Interrupted),
            run_end(R)
        ],
        "nothing to cancel"
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("ending"),
        vec![Effect::Store(StoreOp::EndRun {
            thread: ThreadId(2),
            run: R,
            text: None,
            revealed: Some(Vec::new()),
            end: RunEnd::Interrupted(Interruption::Stop),
        })]
    );
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
        next[..6],
        [
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(None, None, RunEnd::Interrupted(Interruption::Replaced)),
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: RunId(11),
                text: "again".to_owned(),
            }),
        ]
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
        vec![Effect::Store(StoreOp::EndRun {
            thread: T,
            run: RunId(11),
            text: None,
            revealed: None,
            end: RunEnd::Done,
        })]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: RunId(11) })
            .expect("ending"),
        vec![state(RunId(11), T, Phase::Done), run_end(RunId(11))]
    );
}

#[test]
fn done_is_told_only_once_it_is_stored_and_needs_no_cancel() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Text("x".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(engine(R, EngineEvent::Done)).expect("current"),
        vec![end_run(Some("x"), None, RunEnd::Done)]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()), "no run left");
}

#[test]
fn done_waits_for_the_last_step_to_be_committed_and_sent() {
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
        core.handle(ack(0, "last")).expect("current"),
        vec![committed(0, "last"), end_run(None, None, RunEnd::Done)]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
}

#[test]
fn a_repeated_step_ack_changes_nothing() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    core.handle(engine(R, EngineEvent::Step(step("b"))))
        .expect("current");
    core.handle(engine(R, EngineEvent::Done)).expect("current");
    assert_eq!(
        core.handle(ack(0, "a")).expect("first ack"),
        vec![committed(0, "a")]
    );
    assert_eq!(core.handle(ack(0, "a")), Err(Stale), "a repeat");
    assert_eq!(core.handle(ack(5, "z")), Err(Stale), "out of range");
    assert_eq!(
        core.handle(ack(1, "b")).expect("second ack"),
        vec![committed(1, "b"), end_run(None, None, RunEnd::Done)],
        "the run ends only with the second distinct ack"
    );

    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(ack(0, "early")),
        Err(Stale),
        "before the turn is stored"
    );
}

#[test]
fn a_failed_ending_is_reported_after_done_after_stop_and_after_a_replacement() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Text("x".to_owned())))
        .expect("current");
    core.handle(engine(R, EngineEvent::Done)).expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![state(R, T, Phase::Failed), run_end(R), failed_store()],
        "after Done, the shell is never told Done"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));

    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: vec![0],
    })
    .expect("current");
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run(
            None,
            Some(Vec::new()),
            RunEnd::Interrupted(Interruption::Stop)
        )]
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![state(R, T, Phase::Failed), failed_store()],
        "after Stop"
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Ok(Vec::new()),
        "the failed(store) write failing too is the last word"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));

    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    core.handle(Input::Turn {
        thread: Some(T),
        text: "again".to_owned(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![
            state(R, T, Phase::Failed),
            failed_store(),
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: RunId(11),
                text: "again".to_owned(),
            }),
        ],
        "the old run's step failed while its replacement waits"
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: RunId(11) })
            .expect("the replacement is untouched"),
        vec![Effect::StartEngine {
            run: RunId(11),
            text: "again".to_owned(),
        }]
    );
}

#[test]
fn a_store_failure_mid_run_records_failed_store() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Err(Stale),
        "nothing was pending"
    );
    core.handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
            failed_store(),
        ]
    );
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
        over[..3],
        [
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R)
        ],
        "the byte past the cap is never sent"
    );
    assert!(matches!(
        &over[3],
        Effect::Store(StoreOp::EndRun {
            text: Some(stored),
            end: RunEnd::Failed(Failure::Budget),
            ..
        }) if stored.len() == MAX_TEXT
    ));

    let mut core = started();
    assert_eq!(
        core.handle(Input::Deadline { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
            end_run(None, None, RunEnd::Failed(Failure::Budget)),
        ]
    );
}

#[test]
fn a_lost_shell_ends_the_run_without_telling_it() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::ShellLost).expect("never stale"),
        vec![
            Effect::CancelEngine { run: R },
            end_run(None, None, RunEnd::Interrupted(Interruption::ShellLost)),
        ]
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![failed_store()],
        "and a failure isn't told either"
    );
}

#[test]
fn a_stalled_view_or_a_refused_turn_ends_the_run() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::Stalled { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(None, None, RunEnd::Interrupted(Interruption::ViewStalled)),
        ]
    );

    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: Some(ThreadId(99)),
        text: "hi".to_owned(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![state(R, ThreadId(99), Phase::Failed), run_end(R)],
        "nothing more is written for a turn the store refused"
    );
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

fn ends_failed(failure: Failure) -> Vec<Effect> {
    vec![
        Effect::CancelEngine { run: R },
        state(R, T, Phase::Failed),
        run_end(R),
        end_run(None, None, RunEnd::Failed(failure)),
    ]
}

#[test]
fn over_budget_steps_and_oversized_labels_commit_and_send_nothing() {
    let mut core = started();
    for index in 0..MAX_STEPS {
        core.handle(engine(R, EngineEvent::Step(step(&index.to_string()))))
            .expect("within the step budget");
        let index = u32::try_from(index).expect("small");
        core.handle(ack(index, &index.to_string())).expect("stored");
    }
    let over = core
        .handle(engine(R, EngineEvent::Step(step("one too many"))))
        .expect("current");
    assert_eq!(over, ends_failed(Failure::Budget));

    let mut core = started();
    let label = "x".repeat(MAX_STEP_LABEL + 1);
    let over = core
        .handle(engine(R, EngineEvent::Step(step(&label))))
        .expect("current");
    assert_eq!(over, ends_failed(Failure::Engine));
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

#[test]
fn a_stop_while_the_last_step_is_stored_interrupts_the_run() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    core.handle(engine(R, EngineEvent::Done)).expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("still current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ]
    );
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run(
            None,
            Some(Vec::new()),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "the shell never got the step"
    );
}

#[test]
fn a_lost_shell_isnt_told_about_runs_still_ending() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Text("x".to_owned())))
        .expect("current");
    core.handle(engine(R, EngineEvent::Done)).expect("current");
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()));
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
}

#[test]
fn the_engine_is_heard_only_once_its_turn_is_stored() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9));
    core.handle(Input::Turn {
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("turn");
    assert_eq!(core.handle(engine(R, EngineEvent::Done)), Err(Stale));
}

#[test]
fn a_deadline_after_done_leaves_the_finished_run_alone() {
    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("last"))))
        .expect("current");
    core.handle(engine(R, EngineEvent::Text("tail".to_owned())))
        .expect("current");
    core.handle(engine(R, EngineEvent::Done)).expect("current");
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Err(Stale),
        "the engine finished within its budget"
    );
    assert_eq!(
        core.handle(ack(0, "last")).expect("current"),
        vec![
            committed(0, "last"),
            text("tail"),
            end_run(Some("tail"), None, RunEnd::Done)
        ]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
}

#[test]
fn a_replacement_is_stored_and_started_only_after_the_runs_before_it() {
    let (b, c) = (RunId(11), RunId(12));
    let turn = |run: RunId, text: &str| {
        Effect::Store(StoreOp::BeginTurn {
            thread: T,
            title: None,
            run,
            text: text.to_owned(),
        })
    };
    let mut core = started();
    core.handle(engine(R, EngineEvent::Step(step("one"))))
        .expect("current");
    core.handle(engine(R, EngineEvent::Text("tail".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(Input::Turn {
            thread: Some(T),
            text: "b".to_owned(),
        })
        .expect("turn"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            state(b, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: b,
                after: WALL_CLOCK
            },
        ],
        "run 10's step is still in the store, so run 11's turn waits"
    );
    assert_eq!(
        core.handle(Input::Stop {
            run: b,
            revealed: Vec::new(),
        })
        .expect("current"),
        vec![
            Effect::Revoke { run: b },
            state(b, T, Phase::Interrupted),
            run_end(b)
        ]
    );
    assert_eq!(core.handle(Input::TurnStored { run: b }), Err(Stale));
    assert_eq!(
        core.handle(Input::Turn {
            thread: Some(T),
            text: "c".to_owned(),
        })
        .expect("turn"),
        vec![
            state(c, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: c,
                after: WALL_CLOCK
            },
        ]
    );
    assert_eq!(
        core.handle(ack(0, "one")).expect("ending"),
        vec![
            end_run(
                Some("tail"),
                None,
                RunEnd::Interrupted(Interruption::Replaced)
            ),
            turn(b, "b"),
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: b }).expect("ending"),
        vec![
            Effect::Store(StoreOp::EndRun {
                thread: T,
                run: b,
                text: None,
                revealed: Some(Vec::new()),
                end: RunEnd::Interrupted(Interruption::Stop),
            }),
            turn(c, "c"),
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: c }).expect("current"),
        vec![Effect::StartEngine {
            run: c,
            text: "c".to_owned(),
        }]
    );
}
