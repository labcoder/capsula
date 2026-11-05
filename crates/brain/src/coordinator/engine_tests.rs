//! A round's events in the core loop: how its ending ends the run, the draw call's arguments,
//! and events for a run that already ended.

use kumochi_protocol::message::{
    Action, ControlToShell, ErrorKey, MAX_STEP_LABEL, Phase, UserError,
};

use super::{
    R, T, ack, call, commit_in_scene, committed, delta, draw_call, draw_step, end_run, ends_failed,
    engine, run_end, started, state,
};
use crate::connections::model::error::Failure as Stream;
use crate::coordinator::{Effect, Input, Stale};
use crate::engines::{EngineFailure, Finish, Opaque, ToolName, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd};

fn args(delta: &str) -> TurnEvent {
    TurnEvent::ToolArgsDelta {
        call: call(),
        delta: delta.to_owned(),
    }
}

fn ready(args: &str) -> TurnEvent {
    TurnEvent::ToolCallReady {
        call: call(),
        args: args.to_owned(),
    }
}

/// Every event a round can send, for a run that ended.
fn late() -> Vec<TurnEvent> {
    vec![
        TurnEvent::TextDelta("late".to_owned()),
        draw_call(),
        delta("late"),
        ready("late\n"),
        TurnEvent::OutputItem {
            index: 0,
            item: Opaque::new(b"late".to_vec()).expect("small"),
        },
        TurnEvent::Finished(Finish::Completed),
        TurnEvent::Finished(Finish::Truncated),
    ]
}

/// The shell hears run 10 failed, with `error`.
fn failed(key: ErrorKey, action: Action) -> Effect {
    Effect::Control(ControlToShell::RunState {
        run: R,
        thread: T,
        phase: Phase::Failed,
        error: Some(UserError::new(key, Vec::new(), vec![action]).expect("one action")),
        origin: None,
    })
}

/// Each ending fails the run, and the shell hears the stream's or the account's reason; the
/// fixture's provider failure and an interruption carry none.
#[test]
fn a_truncated_failed_or_interrupted_round_fails_its_active_run() {
    for (finish, reason) in [
        (
            Finish::Truncated,
            Some(failed(ErrorKey::AccountOffline, Action::Retry)),
        ),
        (
            Finish::Failed(EngineFailure::Stream(Stream::Protocol)),
            Some(failed(ErrorKey::ResponseUnreadable, Action::Retry)),
        ),
        (
            Finish::Failed(EngineFailure::Access(None)),
            Some(failed(ErrorKey::SignInNeeded, Action::SignIn)),
        ),
        (Finish::Failed(EngineFailure::Provider), None),
        (Finish::Interrupted, None),
    ] {
        let mut core = started();
        let mut ends = ends_failed(Failure::Engine);
        if let Some(reason) = reason {
            ends[1] = reason;
        }
        assert_eq!(
            core.handle(engine(R, TurnEvent::Finished(finish.clone()))),
            Ok(ends),
            "{finish:?}"
        );
        assert_eq!(core.current_run(), None, "{finish:?}");
    }
}

#[test]
fn text_then_a_truncated_round_is_never_done() {
    let mut core = started();
    core.handle(engine(R, TurnEvent::TextDelta("half".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(engine(R, TurnEvent::Finished(Finish::Truncated)))
            .expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            failed(ErrorKey::AccountOffline, Action::Retry),
            run_end(R),
            end_run(Some("half"), None, RunEnd::Failed(Failure::Engine)),
        ]
    );
}

#[test]
fn a_local_stop_is_not_truncation_and_nothing_follows_its_run_end() {
    let mut core = started();
    core.handle(engine(R, TurnEvent::TextDelta("so far".to_owned())))
        .expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(
                Some("so far"),
                Some(Vec::new()),
                RunEnd::Interrupted(Interruption::Stop)
            ),
        ]
    );
    for event in late() {
        assert_eq!(
            core.handle(engine(R, event.clone())),
            Err(Stale),
            "{event:?}"
        );
    }
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
}

#[test]
fn a_completed_round_then_late_events_send_nothing_after_its_run_end() {
    let mut core = started();
    assert_eq!(
        core.handle(engine(R, TurnEvent::Finished(Finish::Completed))),
        Ok(vec![end_run(None, None, RunEnd::Done)])
    );
    for event in late() {
        assert_eq!(
            core.handle(engine(R, event.clone())),
            Err(Stale),
            "{event:?}"
        );
    }
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![state(R, T, Phase::Done), run_end(R)])
    );
}

#[test]
fn a_draw_line_with_no_newline_fails_at_the_label_cap() {
    let mut core = started();
    assert_eq!(core.handle(engine(R, draw_call())), Ok(Vec::new()));
    let quarter = "x".repeat(MAX_STEP_LABEL / 4);
    for _ in 0..4 {
        assert_eq!(
            core.handle(engine(R, args(&quarter))),
            Ok(Vec::new()),
            "the open line fits the cap"
        );
    }
    assert_eq!(
        core.handle(engine(R, args("x"))),
        Ok(ends_failed(Failure::Engine)),
        "one byte past the cap"
    );
}

#[test]
fn a_ready_draw_call_must_match_what_streamed_and_its_open_line_is_a_step() {
    let mut core = started();
    draw_step(&mut core, R, "a").expect("current");
    assert_eq!(core.handle(engine(R, args("b"))), Ok(Vec::new()));
    assert_eq!(
        core.handle(engine(R, ready("a\nb"))),
        Ok(Vec::new()),
        "b waits for a's scene id"
    );
    assert_eq!(
        core.handle(ack(0, "a")),
        Ok(vec![committed(0, "a"), commit_in_scene(None, 1, "b")])
    );
    assert_eq!(
        core.handle(engine(R, args("c\n"))),
        Ok(vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R)
        ]),
        "a ready call takes no more arguments; b's write is still answered first"
    );

    let mut core = started();
    core.handle(engine(R, draw_call())).expect("current");
    core.handle(engine(R, args("a"))).expect("current");
    assert_eq!(
        core.handle(engine(R, ready("b"))),
        Ok(ends_failed(Failure::Engine)),
        "final arguments that differ from the streamed ones"
    );
}

#[test]
fn arguments_for_a_call_never_started_and_other_tools_fail_the_run() {
    let mut core = started();
    assert_eq!(
        core.handle(engine(R, delta("a"))),
        Ok(ends_failed(Failure::Engine))
    );
    let mut core = started();
    let unknown = TurnEvent::ToolCallStarted {
        call: call(),
        name: ToolName::Unknown,
    };
    assert_eq!(
        core.handle(engine(R, unknown)),
        Ok(ends_failed(Failure::Engine))
    );
}
