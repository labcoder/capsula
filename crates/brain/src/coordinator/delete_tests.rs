//! Deleting threads in the core loop: the current run ends, turns naming the thread are refused
//! with no run id while the delete is pending or fenced, and the answer retires its runs and
//! scenes.

use capsula_protocol::message::{
    ControlToShell, DataToShell, Deleted, Phase, RunId, SceneId, ThreadId,
};

use super::{R, T, end_run, fresh, run_end, started, state, step};
use crate::coordinator::{Coordinator, Effect, Input, Stale, StoreOp};
use crate::store::{Interruption, RunEnd, SceneState};

const U: ThreadId = ThreadId(8);

fn delete(thread: ThreadId, after_turns: u32) -> Input {
    Input::Delete {
        thread,
        after_turns,
        resend: false,
    }
}

fn resend(thread: ThreadId) -> Input {
    Input::Delete {
        thread,
        after_turns: 0,
        resend: true,
    }
}

fn answer(thread: ThreadId, outcome: Deleted, last_run: Option<RunId>) -> Input {
    Input::Deleted {
        thread,
        outcome,
        last_run,
    }
}

fn told(thread: ThreadId, outcome: Deleted, last_run: Option<RunId>) -> Effect {
    Effect::Control(ControlToShell::ThreadDeleted {
        thread,
        outcome,
        last_run,
    })
}

fn turn(thread: Option<ThreadId>) -> Input {
    Input::Turn {
        thread,
        text: "more".to_owned(),
    }
}

fn refused(thread: Option<ThreadId>) -> Vec<Effect> {
    vec![Effect::Control(ControlToShell::TurnRefused { thread })]
}

fn store_delete(thread: ThreadId) -> Effect {
    Effect::Store(StoreOp::Delete { thread })
}

/// The run id the next admitted turn gets.
fn next_run(core: &mut Coordinator) -> RunId {
    let effects = core.handle(turn(None)).expect("a turn is never stale");
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Control(ControlToShell::RunState {
                run,
                phase: Phase::Thinking,
                ..
            }) => Some(*run),
            _ => None,
        })
        .expect("the turn was admitted")
}

#[test]
fn a_delete_ends_the_threads_run_and_its_turns_get_no_run_until_answered() {
    let mut core = started();
    assert_eq!(
        core.handle(delete(T, 0)),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(None, None, RunEnd::Interrupted(Interruption::Deleted)),
            store_delete(T),
        ])
    );
    assert_eq!(core.handle(turn(Some(T))), Ok(refused(Some(T))));
    assert_eq!(core.current_run(), None, "the refusal replaced nothing");
    // The outcome isn't answered yet, so run 10 still names the thread.
    assert_eq!(
        core.handle(answer(T, Deleted::Removed, None)),
        Ok(vec![told(T, Deleted::Removed, Some(R))])
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
    assert_eq!(next_run(&mut core), RunId(11), "a refusal takes no id");
}

#[test]
fn a_blank_turn_is_refused_without_a_run() {
    let mut core = fresh();
    let blank = Input::Turn {
        thread: None,
        text: " \n\t ".to_owned(),
    };
    assert_eq!(core.handle(blank), Ok(refused(None)));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
    assert_eq!(next_run(&mut core), R);
}

#[test]
fn the_fence_refuses_exactly_the_submits_sent_before_the_delete() {
    for outcome in [Deleted::Hidden, Deleted::Removed] {
        let mut core = fresh();
        // A new-thread turn, thread 4, read before the delete, then two more sent before it: one naming
        // the thread, one a new thread.
        next_run(&mut core);
        core.handle(delete(U, 3)).expect("accepted");
        let stored = Some(RunId(7));
        assert_eq!(
            core.handle(answer(U, outcome, stored)),
            Ok(vec![told(U, outcome, stored)])
        );
        assert_eq!(core.handle(turn(Some(U))), Ok(refused(Some(U))));
        assert_eq!(next_run(&mut core), RunId(11), "{outcome:?}");
        assert!(core.fenced.is_empty(), "the third submit retired the fence");
        // A later submit naming the thread can only follow a NotHidden; the store decides it.
        assert!(
            core.handle(turn(Some(U)))
                .expect("not stale")
                .contains(&state(RunId(12), U, Phase::Thinking))
        );
    }
}

#[test]
fn not_hidden_fences_nothing_and_the_thread_takes_turns_again() {
    let mut core = fresh();
    core.handle(delete(T, 2)).expect("accepted");
    assert_eq!(
        core.handle(answer(T, Deleted::NotHidden, None)),
        Ok(vec![told(T, Deleted::NotHidden, None)])
    );
    assert!(core.fenced.is_empty());
    assert!(
        core.handle(turn(Some(T)))
            .expect("not stale")
            .contains(&state(R, T, Phase::Thinking))
    );
}

#[test]
fn a_fence_needs_unread_submits_and_retires_with_the_last_one() {
    let mut core = fresh();
    next_run(&mut core);
    next_run(&mut core);
    // The brain read both submits before the delete: nothing to fence.
    core.handle(delete(T, 2)).expect("accepted");
    core.handle(answer(T, Deleted::Removed, None))
        .expect("answered");
    assert!(core.fenced.is_empty());
}

#[test]
fn a_watermark_belongs_to_the_connection_that_sent_it() {
    let mut core = fresh();
    core.handle(delete(T, 3)).expect("accepted");
    core.begin_session();
    // Answered before the new connection's resend: no fence, and the resend is consumed.
    assert_eq!(
        core.handle(answer(T, Deleted::Removed, None)),
        Ok(vec![told(T, Deleted::Removed, None)])
    );
    assert!(core.fenced.is_empty());
    assert_eq!(core.handle(resend(T)), Ok(Vec::new()));
    assert_eq!(
        core.handle(resend(T)),
        Ok(vec![store_delete(T)]),
        "consumed once"
    );

    // Rebound by the resend before the answer: the fence is the new connection's count.
    let mut core = fresh();
    core.handle(delete(U, 5)).expect("accepted");
    core.begin_session();
    let rebind = Input::Delete {
        thread: U,
        after_turns: 1,
        resend: true,
    };
    assert_eq!(core.handle(rebind), Ok(Vec::new()), "no second op");
    core.handle(answer(U, Deleted::Hidden, None))
        .expect("answered");
    assert_eq!(core.fenced, [(U, 1)]);
}

#[test]
fn a_confirmed_delete_after_a_crossed_answer_runs_again() {
    let mut core = fresh();
    core.handle(delete(T, 0)).expect("accepted");
    core.begin_session();
    core.handle(answer(T, Deleted::NotHidden, None))
        .expect("answered");
    assert_eq!(core.crossed, [T]);
    assert_eq!(core.handle(delete(T, 0)), Ok(vec![store_delete(T)]));
    assert!(core.crossed.is_empty());
    core.begin_session();
    assert!(core.crossed.is_empty(), "cleared at serve");
}

#[test]
fn past_eight_in_flight_a_delete_is_refused_and_the_accepted_ones_stay() {
    let mut core = fresh();
    for thread in 1..=8 {
        assert_eq!(
            core.handle(delete(ThreadId(thread), 0)),
            Ok(vec![store_delete(ThreadId(thread))])
        );
    }
    let ninth = ThreadId(9);
    assert_eq!(
        core.handle(delete(ninth, 0)),
        Ok(vec![told(ninth, Deleted::NotHidden, None)])
    );
    assert_eq!(
        core.handle(delete(ThreadId(3), 0)),
        Ok(Vec::new()),
        "rebinds"
    );
    assert_eq!(core.deleting.len(), 8);
    core.handle(answer(ThreadId(3), Deleted::Removed, None))
        .expect("answered");
    assert_eq!(core.handle(delete(ninth, 0)), Ok(vec![store_delete(ninth)]));
    assert_eq!(core.handle(turn(Some(ninth))), Ok(refused(Some(ninth))));
}

#[test]
fn more_fences_than_the_shell_holds_or_an_overflowing_count_break_the_connection() {
    let mut core = fresh();
    for thread in 1..=5 {
        core.handle(delete(ThreadId(thread), 1)).expect("accepted");
    }
    for thread in 1..=4 {
        core.handle(answer(ThreadId(thread), Deleted::Removed, None))
            .expect("answered");
    }
    assert_eq!(
        core.handle(answer(ThreadId(5), Deleted::Removed, None)),
        Ok(vec![
            Effect::Broken("more deletes fenced than the shell holds"),
            told(ThreadId(5), Deleted::Removed, None),
        ])
    );
    let mut core = fresh();
    core.turns_read = u32::MAX;
    assert_eq!(
        core.handle(turn(None)),
        Ok(vec![Effect::Broken("more turns than a connection counts")])
    );
}

#[test]
fn the_answer_retires_the_threads_scenes_and_tells_the_data_pipe() {
    let scene = |id, thread| SceneState {
        scene: SceneId(id),
        thread,
        run: RunId(1),
        revision: 1,
        kept: vec![step("kept")],
        stopped: false,
    };
    let mut core = Coordinator::new(
        ThreadId(9),
        RunId(9),
        vec![scene(1, T), scene(2, U), scene(3, T)],
    );
    core.handle(delete(T, 0)).expect("accepted");
    let removed = vec![SceneId(1), SceneId(3)];
    assert_eq!(
        core.handle(answer(T, Deleted::Hidden, Some(RunId(1)))),
        Ok(vec![
            told(T, Deleted::Hidden, Some(RunId(1))),
            Effect::RevokeScenes {
                scenes: removed.clone()
            },
            Effect::Data(DataToShell::ScenesRemoved { scenes: removed }),
        ])
    );
    let left: Vec<SceneId> = core.visible().iter().map(|state| state.scene).collect();
    assert_eq!(left, [SceneId(2)]);
    assert_eq!(
        core.handle(answer(T, Deleted::Hidden, None)),
        Err(Stale),
        "answered once"
    );
}
