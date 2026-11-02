//! The visible set in the core loop: eviction, kept steps, clears and steps waiting for their
//! scene's id.

use capsula_protocol::message::{Phase, RunId, SceneId, Step, ThreadId};

use super::{
    R, S, T, ack, commit_in_scene, committed, end_run, end_run_clearing, engine, run_end, started,
    state, step,
};
use crate::coordinator::{Coordinator, Effect, EngineEvent, Input, StoreOp};
use crate::store::{Failure, Interruption, RunEnd, SceneState, SceneTarget};

fn visible(ids: &[u64]) -> Vec<SceneState> {
    ids.iter()
        .map(|&id| SceneState {
            scene: SceneId(id),
            thread: ThreadId(1),
            run: RunId(0),
            stopped: false,
            revision: 1,
            kept: vec![step("old")],
        })
        .collect()
}

fn ids(core: &Coordinator) -> Vec<u64> {
    core.visible().iter().map(|state| state.scene.0).collect()
}

/// Run 10 started in a new thread 4, over `scenes`.
fn started_over(scenes: &[u64]) -> Coordinator {
    let mut core = Coordinator::new(ThreadId(3), RunId(9), visible(scenes));
    core.handle(Input::Turn {
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core
}

fn first_write(evict: Option<u64>, label: &str) -> Effect {
    Effect::Store(StoreOp::CommitStep {
        thread: T,
        run: R,
        text: None,
        scene: SceneTarget::New {
            evict: evict.map(SceneId),
        },
        index: 0,
        step: step(label),
    })
}

fn clear_scenes(ids: &[u64]) -> Effect {
    Effect::Store(StoreOp::ClearScenes {
        scenes: ids.iter().copied().map(SceneId).collect(),
    })
}

#[test]
fn a_fifth_scene_evicts_the_oldest_in_its_first_steps_write() {
    let mut core = started_over(&[1, 2, 3, 4]);
    assert_eq!(
        core.handle(engine(R, EngineEvent::Step(step("a"))))
            .expect("current"),
        vec![state(R, T, Phase::Answering), first_write(Some(1), "a")]
    );
    assert_eq!(ids(&core), [1, 2, 3, 4], "nothing moves before the commit");
    core.handle(ack(0, "a")).expect("current");
    assert_eq!(
        ids(&core),
        [2, 3, 4, S.0],
        "the committed transaction's set"
    );

    let mut core = started_over(&[1, 2, 3]);
    let effects = core
        .handle(engine(R, EngineEvent::Step(step("a"))))
        .expect("current");
    assert_eq!(effects[1], first_write(None, "a"), "three leave room");
}

#[test]
fn a_clear_leaves_the_set_at_once_and_counts_only_once_durable() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9), visible(&[1, 2]));
    assert!(core.settled());
    let clear = |scenes: &[u64]| Input::Clear {
        scenes: scenes.iter().copied().map(SceneId).collect(),
    };
    assert_eq!(
        core.handle(clear(&[1, 7])),
        Ok(vec![clear_scenes(&[1])]),
        "an unknown id is a no-op"
    );
    assert_eq!(ids(&core), [2]);
    assert!(!core.settled(), "the clear isn't durable yet");
    assert_eq!(core.handle(clear(&[1])), Ok(Vec::new()), "already clearing");
    let failed = Input::ScenesCleared {
        scenes: vec![SceneId(1)],
        ok: false,
    };
    assert_eq!(core.handle(failed), Ok(Vec::new()));
    assert!(!core.settled(), "a failed clear never turns into a no-op");
    assert!(!core.clears_in_flight());
    let turn = core
        .handle(Input::Turn {
            thread: None,
            text: "hi".to_owned(),
        })
        .expect("turn");
    assert_eq!(
        turn.last(),
        Some(&clear_scenes(&[1])),
        "retried with the next store operation"
    );
    assert_eq!(
        core.handle(Input::ScenesCleared {
            scenes: vec![SceneId(1)],
            ok: true,
        }),
        Ok(Vec::new())
    );
    assert!(core.settled());
    assert_eq!(core.retry_clears(), Vec::new());
}

#[test]
fn a_failed_clear_is_retried_at_the_barrier_and_quit_clears_it_with_the_visible_set() {
    let mut core = Coordinator::new(ThreadId(3), RunId(9), visible(&[1, 2, 3]));
    core.handle(Input::Clear {
        scenes: vec![SceneId(3)],
    })
    .expect("never stale");
    core.handle(Input::ScenesCleared {
        scenes: vec![SceneId(3)],
        ok: false,
    })
    .expect("never stale");
    assert_eq!(core.retry_clears(), vec![clear_scenes(&[3])]);
    assert!(core.clears_in_flight());
    core.handle(Input::ScenesCleared {
        scenes: vec![SceneId(3)],
        ok: false,
    })
    .expect("never stale");
    assert_eq!(
        core.handle(Input::ClearVisible),
        Ok(vec![clear_scenes(&[3, 1, 2])]),
        "one write for the visible set and what's still clearing"
    );
    assert!(core.visible().is_empty());
}

#[test]
fn late_acks_after_a_stop_count_in_the_revision_but_never_come_back() {
    let mut core = started();
    for label in ["a", "b", "c", "d"] {
        core.handle(engine(R, EngineEvent::Step(step(label))))
            .expect("current");
    }
    assert_eq!(
        core.handle(ack(0, "a")).expect("current"),
        vec![
            committed(0, "a"),
            commit_in_scene(None, 1, "b"),
            commit_in_scene(None, 2, "c"),
            commit_in_scene(None, 3, "d"),
        ]
    );
    core.handle(ack(1, "b")).expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: vec![0, 1],
    })
    .expect("current");
    core.handle(ack(2, "c")).expect("ending");
    assert_eq!(
        core.handle(ack(3, "d")).expect("ending"),
        vec![end_run(
            None,
            Some(vec![0, 1]),
            RunEnd::Interrupted(Interruption::Stop)
        )]
    );
    let scene = &core.visible()[0];
    assert_eq!(scene.revision, 4);
    assert_eq!(scene.kept, [step("a"), step("b")]);
}

#[test]
fn a_stop_that_kept_a_gap_keeps_only_the_revealed_steps() {
    let mut core = started();
    for label in ["a", "b", "c"] {
        core.handle(engine(R, EngineEvent::Step(step(label))))
            .expect("current");
    }
    for (index, label) in ["a", "b", "c"].into_iter().enumerate() {
        core.handle(ack(u32::try_from(index).expect("small"), label))
            .expect("current");
    }
    core.handle(Input::Stop {
        run: R,
        revealed: vec![2, 0],
    })
    .expect("current");
    assert_eq!(core.visible()[0].kept, [step("a"), step("c")]);
}

/// Labels of every step write in `effects`.
fn written(effects: &[Effect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::CommitStep { step, .. }) => Some(step.label.as_str()),
            _ => None,
        })
        .collect()
}

/// Run 10 with step a in the store and b and c waiting for its scene's id.
fn waiting() -> (Coordinator, Vec<Effect>) {
    let mut core = started();
    let mut effects = Vec::new();
    for label in ["a", "b", "c"] {
        effects.extend(
            core.handle(engine(R, EngineEvent::Step(step(label))))
                .expect("current"),
        );
    }
    assert_eq!(written(&effects), ["a"], "b and c wait for a's ack");
    (core, effects)
}

/// A later run in thread 4 begins its own scene from index 0, with nothing of run 10's.
fn next_run_starts_clean(core: &mut Coordinator, effects: &mut Vec<Effect>) {
    let next = RunId(11);
    for input in [
        Input::Turn {
            thread: Some(T),
            text: "next".to_owned(),
        },
        Input::TurnStored { run: next },
        engine(next, EngineEvent::Step(step("z"))),
    ] {
        effects.extend(core.handle(input).expect("the new run is current"));
    }
    assert_eq!(written(effects), ["a", "z"], "b and c never leak");
    assert!(effects.contains(&Effect::Store(StoreOp::CommitStep {
        thread: T,
        run: next,
        text: None,
        scene: SceneTarget::New { evict: None },
        index: 0,
        step: step("z"),
    })));
}

#[test]
fn a_stop_drops_steps_waiting_for_the_scene_and_the_run_settles() {
    let (mut core, mut effects) = waiting();
    effects.extend(
        core.handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("current"),
    );
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run_clearing(
            Vec::new(),
            RunEnd::Interrupted(Interruption::Stop)
        )]
    );
    core.handle(Input::RunStored { run: R }).expect("ending");
    assert_eq!(core.ending_runs(), 0);
    next_run_starts_clean(&mut core, &mut effects);
}

#[test]
fn a_replacement_drops_steps_waiting_for_the_scene_and_the_run_settles() {
    let (mut core, mut effects) = waiting();
    let next = core
        .handle(Input::Turn {
            thread: Some(T),
            text: "next".to_owned(),
        })
        .expect("turn");
    assert_eq!(written(&next), Vec::<&str>::new());
    let acked = core.handle(ack(0, "a")).expect("ending");
    assert_eq!(
        acked[0],
        end_run(None, None, RunEnd::Interrupted(Interruption::Replaced))
    );
    assert_eq!(written(&acked), Vec::<&str>::new());
    core.handle(Input::RunStored { run: R }).expect("ending");
    assert_eq!(core.ending_runs(), 0);
    effects.extend(
        core.handle(Input::TurnStored { run: RunId(11) })
            .expect("current"),
    );
    effects.extend(
        core.handle(engine(RunId(11), EngineEvent::Step(step("z"))))
            .expect("current"),
    );
    assert_eq!(written(&effects), ["a", "z"], "b and c never leak");
}

#[test]
fn a_failed_first_write_drops_the_waiting_steps_sends_no_scene_and_settles() {
    let (mut core, mut effects) = waiting();
    let failed = core.handle(Input::StoreFailed { run: R }).expect("current");
    assert_eq!(
        failed,
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
            end_run(None, None, RunEnd::Failed(Failure::Store)),
        ],
        "no StepCommit, so no scene id reaches the shell"
    );
    effects.extend(failed);
    core.handle(Input::RunStored { run: R }).expect("ending");
    assert_eq!(core.ending_runs(), 0);
    assert!(core.visible().is_empty());
    next_run_starts_clean(&mut core, &mut effects);
}

#[test]
fn a_lost_shell_drops_steps_waiting_for_the_scene_and_the_run_settles() {
    let (mut core, mut effects) = waiting();
    effects.extend(core.handle(Input::ShellLost).expect("never stale"));
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run(
            None,
            None,
            RunEnd::Interrupted(Interruption::ShellLost)
        )]
    );
    core.handle(Input::RunStored { run: R }).expect("ending");
    assert_eq!(core.ending_runs(), 0);
    assert_eq!(
        core.visible()[0].kept,
        [step("a")],
        "the committed step stays"
    );
    next_run_starts_clean(&mut core, &mut effects);
}

#[test]
fn a_scene_state_s_debug_leaves_its_labels_out() {
    let state = SceneState {
        scene: SceneId(1),
        thread: ThreadId(2),
        run: RunId(4),
        stopped: false,
        revision: 3,
        kept: vec![Step {
            label: "zqxsentinel".to_owned(),
        }],
    };
    let debug = format!("{state:?}");
    assert!(!debug.contains("zqxsentinel"), "{debug}");
    assert!(debug.contains("revision: 3"), "{debug}");
}
