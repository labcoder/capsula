//! The visible set in the core loop: eviction, kept steps, clears and steps waiting for their
//! scene's id.

use std::time::Duration;

use kumochi_protocol::message::{Phase, RunId, SceneId, Step, ThreadId};

use super::{
    R, S, T, ack, commit_in_scene, committed, delta, draw_call, draw_step, end_run,
    end_run_clearing, engine, run_end, started, state, step,
};
use crate::coordinator::{Coordinator, Effect, Input, StoreOp};
use crate::engines::{Finish, TurnEvent};
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
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(scenes));
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    super::admit(&mut core, R);
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

fn arm(millis: u64) -> Effect {
    Effect::ArmRetry {
        after: Duration::from_millis(millis),
    }
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
        draw_step(&mut core, R, "a").expect("current"),
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
    let effects = draw_step(&mut core, R, "a").expect("current");
    assert_eq!(effects[1], first_write(None, "a"), "three leave room");
}

#[test]
fn a_clear_leaves_the_set_at_once_and_counts_only_once_durable() {
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1, 2]));
    assert!(core.settled());
    let clear = |scenes: &[u64]| Input::Clear {
        scenes: scenes.iter().copied().map(SceneId).collect(),
    };
    let revoke = |scenes: &[u64]| Effect::RevokeScenes {
        scenes: scenes.iter().copied().map(SceneId).collect(),
    };
    assert_eq!(
        core.handle(clear(&[1, 7])),
        Ok(vec![revoke(&[1, 7]), clear_scenes(&[1])]),
        "an unknown id writes nothing"
    );
    assert_eq!(ids(&core), [2]);
    assert!(!core.settled(), "the clear isn't durable yet");
    assert_eq!(
        core.handle(clear(&[1])),
        Ok(vec![revoke(&[1])]),
        "already clearing"
    );
    let failed = Input::ScenesCleared {
        scenes: vec![SceneId(1)],
        ok: false,
    };
    assert_eq!(core.handle(failed), Ok(vec![arm(500)]));
    assert!(!core.settled(), "a failed clear never turns into a no-op");
    assert!(!core.clears_in_flight());
    let turn = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("turn");
    assert!(
        turn.ends_with(&[clear_scenes(&[1]), Effect::CancelRetry]),
        "retried with the next store operation, so its timer goes"
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
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1, 2, 3]));
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
        Ok(vec![clear_scenes(&[3, 1, 2]), Effect::CancelRetry]),
        "one write for the visible set and what's still clearing"
    );
    assert!(core.visible().is_empty());
}

#[test]
fn late_acks_after_a_stop_count_in_the_revision_but_never_come_back() {
    let mut core = started();
    for label in ["a", "b", "c", "d"] {
        draw_step(&mut core, R, label).expect("current");
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
        draw_step(&mut core, R, label).expect("current");
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
        effects.extend(draw_step(&mut core, R, label).expect("current"));
    }
    assert_eq!(written(&effects), ["a"], "b and c wait for a's ack");
    (core, effects)
}

/// A later run in thread 4 begins its own scene from index 0, with nothing of run 10's.
fn next_run_starts_clean(core: &mut Coordinator, effects: &mut Vec<Effect>) {
    let next = RunId(11);
    let turn = Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "next".into(),
    };
    effects.extend(core.handle(turn).expect("a turn is never stale"));
    effects.extend(super::admit(core, next));
    let key = super::engine_key(core, next);
    for input in [engine(key, draw_call()), engine(key, delta("z"))] {
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
            turn: 1,
            thread: Some(T),
            text: "next".into(),
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
    effects.extend(super::admit(&mut core, RunId(11)));
    effects.extend(draw_step(&mut core, RunId(11), "z").expect("current"));
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

#[test]
fn a_failed_owed_write_is_retried_on_a_timer_that_backs_off_and_resets() {
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1, 2]));
    let failed = |id: u64| Input::ScenesCleared {
        scenes: vec![SceneId(id)],
        ok: false,
    };
    core.handle(Input::Clear {
        scenes: vec![SceneId(1)],
    })
    .expect("a clear is never stale");
    for millis in [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000] {
        assert_eq!(core.handle(failed(1)), Ok(vec![arm(millis)]));
        assert_eq!(
            core.handle(Input::Retry),
            Ok(vec![clear_scenes(&[1])]),
            "written again when the timer fires"
        );
    }
    let committed = Input::ScenesCleared {
        scenes: vec![SceneId(1)],
        ok: true,
    };
    assert_eq!(core.handle(committed), Ok(Vec::new()));
    assert!(core.settled());
    core.handle(Input::Clear {
        scenes: vec![SceneId(2)],
    })
    .expect("a clear is never stale");
    assert_eq!(
        core.handle(failed(2)),
        Ok(vec![arm(500)]),
        "a commit resets the wait"
    );
    assert_eq!(
        core.handle(failed(2)),
        Ok(Vec::new()),
        "one timer at a time"
    );
}

#[test]
fn a_retry_deadline_leaves_with_the_debt_and_a_new_failure_waits_the_first_delay() {
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1, 2, 3]));
    let failed = |id: u64| Input::ScenesCleared {
        scenes: vec![SceneId(id)],
        ok: false,
    };
    let committed = |id: u64| Input::ScenesCleared {
        scenes: vec![SceneId(id)],
        ok: true,
    };
    let clear = |id: u64| Input::Clear {
        scenes: vec![SceneId(id)],
    };
    let revoke = |id: u64| Effect::RevokeScenes {
        scenes: vec![SceneId(id)],
    };
    core.handle(clear(1)).expect("a clear is never stale");
    for millis in [500, 1000, 2000, 4000, 8000, 16_000, 30_000] {
        assert_eq!(core.handle(failed(1)), Ok(vec![arm(millis)]));
        assert_eq!(core.handle(Input::Retry), Ok(vec![clear_scenes(&[1])]));
    }
    assert_eq!(core.handle(failed(1)), Ok(vec![arm(30_000)]));
    assert_eq!(
        core.handle(clear(2)),
        Ok(vec![
            revoke(2),
            clear_scenes(&[2]),
            clear_scenes(&[1]),
            Effect::CancelRetry
        ]),
        "the deadline goes once the owed write rides another op"
    );
    assert_eq!(core.handle(committed(1)), Ok(Vec::new()));
    assert_eq!(core.handle(committed(2)), Ok(Vec::new()));
    assert_eq!(
        core.handle(clear(3)),
        Ok(vec![revoke(3), clear_scenes(&[3])])
    );
    assert_eq!(
        core.handle(failed(3)),
        Ok(vec![arm(500)]),
        "a new failure waits the first delay at once"
    );
}

#[test]
fn tuned_scene_clear_retries_start_and_cap_at_the_configured_delays() {
    let mut core = Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1]));
    core.configure_drawing(crate::config::tuning::DrawingTuning {
        first_retry: Duration::from_millis(750),
        last_retry: Duration::from_millis(1200),
        ..Default::default()
    });
    core.handle(Input::Clear {
        scenes: vec![SceneId(1)],
    })
    .unwrap();
    for millis in [750, 1200, 1200] {
        assert_eq!(
            core.handle(Input::ScenesCleared {
                scenes: vec![SceneId(1)],
                ok: false
            }),
            Ok(vec![arm(millis)])
        );
        assert_eq!(core.handle(Input::Retry), Ok(vec![clear_scenes(&[1])]));
    }
}

#[test]
fn a_partial_commit_resets_the_wait_and_replaces_a_longer_deadline() {
    let mut core =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), visible(&[1, 2]));
    let cleared = |id: u64, ok: bool| Input::ScenesCleared {
        scenes: vec![SceneId(id)],
        ok,
    };
    core.handle(Input::Clear {
        scenes: vec![SceneId(1)],
    })
    .expect("a clear is never stale");
    assert_eq!(core.handle(cleared(1, false)), Ok(vec![arm(500)]));
    assert_eq!(core.handle(Input::Retry), Ok(vec![clear_scenes(&[1])]));
    // A separate clear while A's retry is in flight goes on its own.
    assert_eq!(
        core.handle(Input::Clear {
            scenes: vec![SceneId(2)],
        }),
        Ok(vec![
            Effect::RevokeScenes {
                scenes: vec![SceneId(2)],
            },
            clear_scenes(&[2]),
        ])
    );
    assert_eq!(core.handle(cleared(1, false)), Ok(vec![arm(1000)]));
    assert_eq!(
        core.handle(cleared(2, true)),
        Ok(vec![arm(500)]),
        "a commit resets the wait, and the 1 s deadline is replaced"
    );
    assert_eq!(core.debt(), 1);
}

#[test]
fn a_scene_both_kept_and_cleared_counts_once_and_both_writes_retire() {
    let mut core = started_over(&[]);
    for (index, label) in [(0, "a"), (1, "b")] {
        draw_step(&mut core, R, label).expect("current");
        core.handle(ack(index, label)).expect("current");
    }
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    core.handle(Input::RunStored { run: R }).expect("ending");
    let stop = Input::Stop {
        run: R,
        revealed: vec![0],
    };
    assert_eq!(
        core.handle(stop),
        Ok(vec![Effect::Store(StoreOp::KeepScene {
            scene: S,
            revealed: vec![0],
        })])
    );
    let clear = Input::Clear { scenes: vec![S] };
    assert_eq!(
        core.handle(clear),
        Ok(vec![
            Effect::RevokeScenes { scenes: vec![S] },
            clear_scenes(&[S.0])
        ]),
        "cleared while its selection is still being written"
    );
    assert_eq!(core.debt(), 1, "one scene, owed twice");
    assert!(core.clears_in_flight());
    let kept = Input::SceneKept { scene: S, ok: true };
    assert_eq!(core.handle(kept), Ok(Vec::new()));
    assert_eq!(core.debt(), 1, "its clear is still owed");
    assert!(!core.settled());
    let cleared = Input::ScenesCleared {
        scenes: vec![S],
        ok: true,
    };
    assert_eq!(core.handle(cleared), Ok(Vec::new()));
    assert_eq!(core.debt(), 0);
    assert!(core.settled());
}
