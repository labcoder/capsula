//! The drawing system's order and durability rules, driven through its inputs with a store
//! double that persists steps and cuts as JSON, the way the store does.

use std::sync::Arc;

use kumochi_protocol::message::{
    DataToShell, MAX_DRAW_STEPS_PER_RUN, RunId, SceneCut, SceneCuts, SceneFailure, SceneHeader,
    SceneId, ThreadId,
};
use kumochi_scene::text::Bounded;
use kumochi_scene::{
    Budget, ErrorCode, Handle, Record, SceneEvent, SceneState, Step, StepError, StopCut,
    TokenError, WireId, parse_record,
};

use super::call::MAX_CALL_RECORDS;
use super::{
    CallEnd, CallError, Drawing, Held, Input, Loss, MAX_QUEUED_JOBS, Output, Remaining, Reply,
    StoredScene, Target, Write,
};
use crate::coordinator::{Effect, Stale, StoreOp};
use crate::engines::CallId;

mod paint;
mod ready;
mod screen;
mod timing;

const THREAD: ThreadId = ThreadId(7);

#[test]
fn stop_cuts_every_visible_scene_and_acknowledges_three_no_ops_without_logging_them() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "first", args("one", &[text("a", "x"), text("b", "y")])),
    );
    for scene in 2..=4 {
        store.run(
            &mut drawing,
            call(
                1,
                &format!("call{scene}"),
                args(&format!("scene{scene}"), &[text("a", "x")]),
            ),
        );
    }
    let cuts = (1..=4)
        .map(|scene| SceneCut {
            scene: SceneId(scene),
            cut: cut(1, vec![handle(&drawing, scene, "x")]),
        })
        .collect();
    let out = store.run(
        &mut drawing,
        Input::StopScenes {
            run: RunId(1),
            cuts: SceneCuts::new(cuts).expect("four scenes"),
        },
    );
    assert_eq!(
        trace(&out),
        [
            "write cut 1@2:1",
            "cut 1@2:1",
            "cut 2@1:1",
            "cut 3@1:1",
            "cut 4@1:1",
        ]
    );
    assert_eq!(
        store
            .scenes
            .iter()
            .map(|scene| scene.log.len())
            .collect::<Vec<_>>(),
        [3, 1, 1, 1]
    );
    assert_eq!(drawing.snapshot().scenes.len(), 4);
}

#[test]
fn a_covering_stop_durably_cuts_older_held_scenes_before_a_later_continuation() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(4, "a4", args("a", &[text("a1", "x"), text("a2", "y")])),
    );
    store.run(&mut drawing, closed(4));
    store.run(&mut drawing, call(5, "b5", args("b", &[text("b1", "b")])));
    store.run(&mut drawing, closed(5));
    store.run(&mut drawing, call(6, "c6", args("c", &[text("c1", "c")])));
    store.run(&mut drawing, closed(6));
    let cuts = SceneCuts::new(vec![
        SceneCut {
            scene: SceneId(1),
            cut: cut(1, vec![Handle::new(1, 0).expect("literal x")]),
        },
        SceneCut {
            scene: SceneId(2),
            cut: cut(0, vec![]),
        },
        SceneCut {
            scene: SceneId(3),
            cut: cut(0, vec![]),
        },
    ])
    .expect("three held scenes");
    let mut out = drawing
        .handle(Input::StopScenes {
            run: RunId(6),
            cuts,
        })
        .expect("covering Stop");
    assert_eq!(trace(&out), ["write cut 1@2:1"]);
    let later = drawing
        .handle(call(7, "a7", args("a", &[text("a3", "z")])))
        .expect("later run");
    assert_eq!(trace(&later), Vec::<String>::new());
    merge(&mut out, later);
    let out = store.settle(&mut drawing, out);
    assert_eq!(
        trace(&out),
        [
            "write cut 1@2:1",
            "cut 1@2:1",
            "write cut 2@1:0",
            "cut 2@1:0",
            "clear 2",
            "write cut 3@1:0",
            "cut 3@1:0",
            "clear 3",
            "write step 3",
            "step 1#2",
            "end 1 true",
        ]
    );
    assert_eq!(store.cleared, [SceneId(2), SceneId(3)]);
    assert_eq!(
        store
            .scenes
            .iter()
            .map(|scene| scene
                .log
                .iter()
                .map(|event| match event {
                    SceneEvent::Step { revision, .. } => (*revision, None),
                    SceneEvent::Stop(cut) => (0, Some(cut.activated)),
                })
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            vec![(1, None), (2, None), (0, Some(1)), (3, None)],
            vec![(1, None), (0, Some(0))],
            vec![(1, None), (0, Some(0))],
        ]
    );
    let state = &drawing.scenes[0].state;
    assert_eq!(state.revision(), 3);
    assert_eq!(state.committed(), 2);
    assert_eq!(state.handle(&wire("y")), None);
    assert_eq!(state.step(2).map(|step| &step.id), Some(&wire("a3")));
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert_eq!(skipped, Vec::<SceneId>::new());
    assert_eq!(
        recovered
            .snapshot()
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(1)]
    );
}

#[test]
fn a_second_clear_supersedes_pending_covering_cuts_without_cutting_a_later_scene() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(5, "b5", args("b", &[text("b1", "b")])));
    store.run(&mut drawing, call(6, "c6", args("c", &[text("c1", "c")])));
    let cuts = SceneCuts::new(vec![
        SceneCut {
            scene: SceneId(1),
            cut: cut(0, vec![]),
        },
        SceneCut {
            scene: SceneId(2),
            cut: cut(0, vec![]),
        },
    ])
    .expect("two held scenes");
    let out = drawing
        .handle(Input::StopScenes {
            run: RunId(6),
            cuts,
        })
        .expect("Stop");
    assert_eq!(trace(&out), ["write cut 1@1:0"]);
    let clear = store.run(
        &mut drawing,
        Input::Clear {
            scenes: vec![SceneId(1), SceneId(2)],
        },
    );
    assert_eq!(trace(&clear), ["revoke 1,2", "clear 1,2"]);
    let out = store.settle(&mut drawing, out);
    assert_eq!(trace(&out), ["write cut 1@1:0"]);
    assert_eq!(store.cleared, [SceneId(1), SceneId(2)]);
    let again = store.run(
        &mut drawing,
        Input::Clear {
            scenes: vec![SceneId(1), SceneId(2)],
        },
    );
    assert_eq!(trace(&again), Vec::<String>::new());
    assert_eq!(store.cleared, [SceneId(1), SceneId(2)]);
    let out = store.run(&mut drawing, call(7, "d7", args("d", &[text("d1", "d")])));
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "begin 3",
            "step 3#0",
            "end 3 true"
        ]
    );
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert_eq!(skipped, Vec::<SceneId>::new());
    assert_eq!(
        recovered
            .snapshot()
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(3)]
    );
}

fn wire(id: &str) -> WireId {
    id.to_owned().try_into().expect("a valid id")
}

/// A step putting one text, at coordinates JSON can't carry exactly in fewer digits.
fn text(step: &str, object: &str) -> String {
    format!(
        r#"{{"id":"{step}","put":[{{"text":{{"id":"{object}","text":"","at":[12345.679,-0.1]}}}}]}}"#
    )
}

fn begin(scene: &str, alt: &str) -> String {
    format!(
        r#"{{"begin":{{"v":1,"scene":"{scene}","surface":"overlay","alt":"{alt}","others":"keep"}}}}"#
    )
}

fn sized(scene: &str) -> String {
    format!(
        r#"{{"begin":{{"v":1,"scene":"{scene}","surface":"overlay","alt":"","size":[600,400],"others":"keep"}}}}"#
    )
}

fn envelope(begin: &str, steps: &[String]) -> Vec<u8> {
    let mut records = vec![begin.to_owned()];
    records.extend(steps.iter().map(|step| format!(r#"{{"step":{step}}}"#)));
    records.push(r#"{"end":{}}"#.to_owned());
    format!(r#"{{"records":[{}]}}"#, records.join(",")).into_bytes()
}

fn args(scene: &str, steps: &[String]) -> Vec<u8> {
    envelope(&begin(scene, ""), steps)
}

fn replacement_args(scene: &str, steps: &[String]) -> Vec<u8> {
    envelope(
        &format!(r#"{{"begin":{{"v":1,"scene":"{scene}","surface":"overlay","alt":""}}}}"#),
        steps,
    )
}

#[test]
fn a_scene_refusal_never_cancels_an_unrelated_pending_replacement() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let mut other = call(1, "other", args("other", &[text("other", "other")]));
    if let Input::Call { thread, .. } = &mut other {
        *thread = ThreadId(8);
    }
    store.run(&mut drawing, other);
    store.run(
        &mut drawing,
        call(1, "old", args("old", &[text("old", "old")])),
    );
    let lookup = drawing
        .handle(call(
            2,
            "new",
            replacement_args("new", &[text("next", "next")]),
        ))
        .expect("lookup");
    let answer = store.answer(&sent(&lookup).expect("lookup").1);
    let pending = reply(&mut drawing, &lookup, Ok(answer));
    let write = sent(&pending).expect("first-step write").1;
    let Write::Step { clear, .. } = &write else {
        panic!("first-step write");
    };
    assert_eq!(clear, &[SceneId(2)]);

    let refused = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(1) });
    assert_eq!(trace(&refused), ["revoke 1", "clear 1"]);
    assert_eq!(refused.calls, []);
    for scene in [SceneId(1), SceneId(99), SceneId(1)] {
        let stale = drawing
            .handle(Input::SceneFailed { scene })
            .expect("stale refusal");
        assert!(stale.effects.is_empty());
        assert_eq!(stale.calls, []);
    }
    let answer = store.answer(&write);
    let committed = reply(&mut drawing, &pending, Ok(answer));
    let committed = store.settle(&mut drawing, committed);
    assert_eq!(
        trace(&committed),
        [
            "revoke 2",
            "replaced 2 by 3",
            "begin 3",
            "step 3#0",
            "end 3 true"
        ]
    );
    assert_eq!(committed.calls.len(), 1);
    assert_eq!(committed.calls[0].run, RunId(2));
    assert_eq!(committed.calls[0].scene, Some(SceneId(3)));
    assert_eq!(committed.calls[0].drawn, [wire("next")]);
    assert_eq!(committed.calls[0].error, None);
    assert_eq!(store.cleared, [SceneId(1), SceneId(2)]);
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert_eq!(skipped, []);
    assert_eq!(
        recovered
            .snapshot()
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(3)]
    );
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether this fixed regression prefix has a validated paint"
)]
fn committed_replacement_prefix(painted: bool) -> (Drawing, Store, u32) {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "old", args("old", &[text("old", "old")])),
    );
    store.run(
        &mut drawing,
        streamed(
            "new",
            0,
            r#"{"begin":{"v":1,"scene":"new","surface":"overlay","alt":""}}"#,
        ),
    );
    store.run(
        &mut drawing,
        streamed("new", 1, r#"{"step":{"id":"empty","start":"now"}}"#),
    );
    let next = if painted {
        store.run(
            &mut drawing,
            streamed(
                "new",
                2,
                &format!(r#"{{"step":{}}}"#, text("visible", "visible")),
            ),
        );
        store.run(
            &mut drawing,
            Input::StepShown {
                run: RunId(1),
                scene: SceneId(2),
                index: 1,
            },
        );
        3
    } else {
        2
    };
    (drawing, store, next)
}

#[test]
fn a_brain_error_after_a_committed_prefix_reports_loss_once_without_hiding_its_cause() {
    use kumochi_protocol::message::ControlToShell;
    for painted in [false, true] {
        for store_failure in [false, true] {
            let (mut drawing, mut store, next) = committed_replacement_prefix(painted);
            let failed = if store_failure {
                let pending = drawing
                    .handle(streamed(
                        "new",
                        next,
                        &format!(r#"{{"step":{}}}"#, text("failed", "failed")),
                    ))
                    .expect("submitted step");
                assert_eq!(
                    trace(&pending),
                    [if painted {
                        "write step 3"
                    } else {
                        "write step 2"
                    }]
                );
                reply(&mut drawing, &pending, Err(()))
            } else {
                store.run(
                    &mut drawing,
                    streamed("new", next, r#"{"step":{"id":"bad","put":[],"colour":1}}"#),
                )
            };
            assert_eq!(trace(&failed), ["end 2 false"]);
            assert_eq!(failed.calls.len(), 1);
            let result = &failed.calls[0];
            assert_eq!(result.run, RunId(1));
            assert_eq!(result.scene, Some(SceneId(2)));
            assert_eq!(
                result.drawn,
                if painted {
                    vec![wire("empty"), wire("visible")]
                } else {
                    vec![wire("empty")]
                }
            );
            if store_failure {
                assert_eq!(result.error, Some(CallError::Store));
                assert_eq!(
                    result.tool_text(),
                    if painted {
                        "drawn: [empty, visible]; stopped: drawing.StoreFailed"
                    } else {
                        "drawn: [empty]; stopped: drawing.StoreFailed"
                    }
                );
            } else {
                assert_eq!(error(result), ErrorCode::UnknownField);
                assert!(result.tool_text().contains("stopped: step bad:"));
            }
            assert!(!result.tool_text().contains("PreviousRemoved"));
            let notices: Vec<_> = failed
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Control(ControlToShell::DrawingReplacementFailed { thread, run }) => {
                        Some((*thread, *run))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(notices, [], "the run can still repair the loss");
            assert_eq!(store.cleared, [SceneId(1)]);
            assert_eq!(
                store
                    .visible()
                    .iter()
                    .map(|scene| scene.scene)
                    .collect::<Vec<_>>(),
                [SceneId(2)]
            );
            let duplicate = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(2) });
            assert_eq!(duplicate.calls, []);
            assert_eq!(notices_in(&duplicate), []);
            let ended = store.run(&mut drawing, closed(1));
            assert_eq!(
                notices_in(&ended),
                if painted {
                    vec![]
                } else {
                    vec![(THREAD, RunId(1))]
                }
            );
            assert!(no_loss(&drawing));
        }
    }
}

#[test]
fn an_interrupt_never_takes_text_after_the_records_array_as_the_call_s_ending() {
    use kumochi_protocol::message::ControlToShell;
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "old", args("old", &[text("old", "old")])),
    );
    let records = format!(
        r#"{{"records":[{{"begin":{{"v":1,"scene":"new","surface":"overlay","alt":""}}}},{},{}]}} x"#,
        step(r#"{"id":"s1","start":"now"}"#),
        step(r#"{"id":"s2","start":"now"}"#)
    );
    let mut held = drawing
        .handle(call(1, "new", records.into_bytes()))
        .expect("a fresh call");
    while let Some((serial, write)) =
        sent(&held).filter(|(_, write)| !matches!(write, Write::Step { .. }))
    {
        let reply = Ok(store.answer(&write));
        held = drawing
            .handle(Input::Written { serial, reply })
            .expect("the pending lookup");
    }
    let first = sent(&held)
        .expect("the first step's write is in the store")
        .1;
    let mut out = drawing
        .handle(Input::CallFinished {
            thread: THREAD,
            run: RunId(1),
            call: CallId::new("new").expect("call id"),
            error: Some(CallError::Interrupted),
        })
        .expect("the interrupt");
    let answer = store.answer(&first);
    merge(&mut out, reply(&mut drawing, &held, Ok(answer)));
    let out = store.settle(&mut drawing, out);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].drawn, [wire("s1")]);
    assert_eq!(out.calls[0].error, Some(CallError::Interrupted));
    let ends: Vec<bool> = out
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::SceneEnd { complete, .. }) => Some(*complete),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [false]);
    assert!(!out.effects.iter().any(|effect| matches!(
        effect,
        Effect::Control(ControlToShell::DrawingReplacementFailed { .. })
    )));
}

#[test]
fn text_after_the_records_meets_the_spent_budget_an_end_record_would() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let forward = r#"{"id":"a","start":{"after":"later","ms":0}}"#.to_owned();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", std::slice::from_ref(&forward))),
    );
    store.run(&mut drawing, streamed("open", 0, &begin("s2", "")));
    store.run(&mut drawing, streamed("open", 1, &step(&text("a", "x"))));
    let spent = store.run(&mut drawing, call(1, "c3", args("s3", &[forward])));
    assert_eq!(error(&spent.calls[0]), ErrorCode::ForwardRef);
    let out = store.run(
        &mut drawing,
        Input::CallFinished {
            thread: THREAD,
            run: RunId(1),
            call: CallId::new("open").expect("call id"),
            error: Some(CallError::Token(TokenError::AfterRecords)),
        },
    );
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].drawn, [wire("a")]);
    assert_eq!(out.calls[0].error, Some(CallError::Exhausted));
}

#[test]
fn a_stop_for_a_cleared_scene_never_says_its_drawn_steps_are_on_screen() {
    let end = CallEnd {
        run: RunId(1),
        call: CallId::new("c").expect("call id"),
        scene: Some(SceneId(1)),
        drawn: vec![wire("a")],
        error: Some(CallError::Step(StepError::new(ErrorCode::SceneCleared))),
        remaining: Remaining::Steps,
        after_records: false,
        picture: None,
    };
    assert_eq!(
        end.tool_text(),
        format!("drawn: [a]; stopped: $: scene.SceneCleared{RETRY}")
    );
}

#[test]
fn cancelling_an_empty_committed_replacement_does_not_report_a_drawing_failure() {
    use kumochi_protocol::message::ControlToShell;
    let (mut drawing, mut store, _) = committed_replacement_prefix(false);
    let stopped = store.run(
        &mut drawing,
        Input::CallFinished {
            thread: THREAD,
            run: RunId(1),
            call: CallId::new("new").expect("call id"),
            error: Some(CallError::Interrupted),
        },
    );
    assert_eq!(stopped.calls.len(), 1);
    assert_eq!(stopped.calls[0].error, Some(CallError::Interrupted));
    assert!(!stopped.effects.iter().any(|effect| matches!(
        effect,
        Effect::Control(ControlToShell::DrawingReplacementFailed { .. })
    )));
    assert_eq!(store.cleared, [SceneId(1)]);
}

#[test]
fn replacement_failure_reports_loss_once_after_the_call_and_original_run_ended() {
    use kumochi_protocol::message::ControlToShell;
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "old", args("old", &[text("old", "old")])),
    );
    let ended = store.run(
        &mut drawing,
        call(1, "new", replacement_args("new", &[text("new", "new")])),
    );
    assert_eq!(ended.calls.len(), 1);
    assert_eq!(ended.calls[0].error, None);
    store.run(&mut drawing, closed(1));
    let mut another = call(2, "other", args("other", &[text("other", "other")]));
    if let Input::Call { thread, .. } = &mut another {
        *thread = ThreadId(8);
    }
    store.run(&mut drawing, another);
    let failed = store.run(
        &mut drawing,
        Input::StepFailed {
            run: RunId(1),
            scene: SceneId(2),
            index: 0,
            reason: SceneFailure::Timeout,
            cut: cut(0, vec![]),
        },
    );
    assert_eq!(failed.calls, []);
    let notices: Vec<_> = failed
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Control(ControlToShell::DrawingReplacementFailed { thread, run }) => {
                Some((*thread, *run))
            }
            _ => None,
        })
        .collect();
    assert_eq!(notices, [(THREAD, RunId(1))]);
    let repeated = store.run(
        &mut drawing,
        Input::StepFailed {
            run: RunId(1),
            scene: SceneId(2),
            index: 0,
            reason: SceneFailure::Timeout,
            cut: cut(0, vec![]),
        },
    );
    assert!(!repeated.effects.iter().any(|effect| matches!(
        effect,
        Effect::Control(ControlToShell::DrawingReplacementFailed { .. })
    )));
}

#[test]
fn a_whole_scene_refusal_reports_late_loss_but_an_explicit_clear_does_not() {
    use kumochi_protocol::message::ControlToShell;
    for failed in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        store.run(
            &mut drawing,
            call(1, "old", args("old", &[text("old", "old")])),
        );
        store.run(
            &mut drawing,
            call(1, "new", replacement_args("new", &[text("new", "new")])),
        );
        let out = store.run(
            &mut drawing,
            if failed {
                Input::SceneFailed { scene: SceneId(2) }
            } else {
                Input::Clear {
                    scenes: vec![SceneId(2)],
                }
            },
        );
        assert_eq!(
            out.effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::Control(ControlToShell::DrawingReplacementFailed {
                        thread: THREAD,
                        run: RunId(1)
                    })
                ))
                .count(),
            0
        );
        assert_eq!(drawing.scenes.len(), 0);
        assert_eq!(store.cleared, [SceneId(1), SceneId(2)]);
        let ended = store.run(&mut drawing, closed(1));
        assert_eq!(notices_in(&ended).len(), usize::from(failed));
        assert!(no_loss(&drawing));
    }
}

#[test]
fn an_open_replacement_call_reports_loss_without_overwriting_its_failure_code() {
    use kumochi_protocol::message::ControlToShell;
    for painted in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        store.run(
            &mut drawing,
            call(1, "old", args("old", &[text("old", "old")])),
        );
        store.run(
            &mut drawing,
            streamed(
                "new",
                0,
                r#"{"begin":{"v":1,"scene":"new","surface":"overlay","alt":""}}"#,
            ),
        );
        store.run(
            &mut drawing,
            streamed("new", 1, &format!(r#"{{"step":{}}}"#, text("new", "new"))),
        );
        let cut = if painted {
            store.run(
                &mut drawing,
                Input::StepShown {
                    run: RunId(1),
                    scene: SceneId(2),
                    index: 0,
                },
            );
            cut(1, vec![handle(&drawing, 2, "new")])
        } else {
            cut(0, vec![])
        };
        let failed = store.run(
            &mut drawing,
            Input::StepFailed {
                run: RunId(1),
                scene: SceneId(2),
                index: 0,
                reason: SceneFailure::Timeout,
                cut,
            },
        );
        assert_eq!(failed.calls.len(), 1);
        assert_eq!(failed.calls[0].error, Some(CallError::Failed));
        assert_eq!(
            failed
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::Control(ControlToShell::DrawingReplacementFailed { .. })
                ))
                .count(),
            0
        );
        assert_eq!(
            failed.calls[0].tool_text(),
            "drawn: [new]; stopped: drawing.PreparationFailed"
        );
        let ended = store.run(&mut drawing, closed(1));
        assert_eq!(notices_in(&ended).len(), usize::from(!painted));
        assert!(no_loss(&drawing));
    }
}

/// Run 1 drew "old", then replaced it with "new", scene 2, in `steps` steps.
fn replaced(drawing: &mut Drawing, store: &mut Store, steps: usize) {
    store.run(drawing, call(1, "old", args("old", &[text("old", "old")])));
    let mut drawn = vec![text("new", "new")];
    drawn.extend(numbered("n", steps - 1));
    store.run(drawing, call(1, "new", replacement_args("new", &drawn)));
}

/// The shell couldn't show the replacement's first step.
fn first_step_failed() -> Input {
    Input::StepFailed {
        run: RunId(1),
        scene: SceneId(2),
        index: 0,
        reason: SceneFailure::Timeout,
        cut: cut(0, vec![]),
    }
}

/// A one-step replacement whose first step failed: that failure's output.
fn lost_replacement(drawing: &mut Drawing, store: &mut Store) -> Output {
    replaced(drawing, store, 1);
    store.run(drawing, first_step_failed())
}

/// `count` steps that paint nothing, `{prefix}0` onwards.
fn numbered(prefix: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|n| format!(r#"{{"id":"{prefix}{n}","start":"now"}}"#))
        .collect()
}

fn notices_in(out: &Output) -> Vec<(ThreadId, RunId)> {
    use kumochi_protocol::message::ControlToShell;
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Control(ControlToShell::DrawingReplacementFailed { thread, run }) => {
                Some((*thread, *run))
            }
            _ => None,
        })
        .collect()
}

fn no_loss(drawing: &Drawing) -> bool {
    drawing.runs.iter().all(|ledger| ledger.loss.is_none())
}

#[test]
fn a_lost_replacement_is_told_once_when_its_run_ends_on_its_own() {
    for whole in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        replaced(&mut drawing, &mut store, 1);
        let mut failed = Output::default();
        if whole {
            let refused = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(2) });
            merge(&mut failed, refused);
        }
        merge(&mut failed, store.run(&mut drawing, first_step_failed()));
        assert_eq!(notices_in(&failed), [], "a corrected call may still paint");
        let again = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(2) });
        assert_eq!(notices_in(&again), []);
        let ended = store.run(&mut drawing, closed(1));
        assert_eq!(notices_in(&ended), [(THREAD, RunId(1))]);
        assert!(no_loss(&drawing));
    }
}

#[test]
fn a_frame_the_run_paints_afterwards_drops_its_loss_silently() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    lost_replacement(&mut drawing, &mut store);
    store.run(
        &mut drawing,
        call(1, "fix", args("fix", &[text("fix", "fix")])),
    );
    let shown = store.run(
        &mut drawing,
        Input::StepShown {
            run: RunId(1),
            scene: SceneId(3),
            index: 0,
        },
    );
    assert_eq!(notices_in(&shown), []);
    let ended = store.run(&mut drawing, closed(1));
    assert_eq!(notices_in(&ended), []);
    assert!(no_loss(&drawing));
}

#[test]
fn a_frame_another_run_paints_or_a_commit_never_shown_keeps_the_loss() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    lost_replacement(&mut drawing, &mut store);
    store.run(
        &mut drawing,
        call(
            1,
            "blank",
            args("blank", &[r#"{"id":"blank","start":"now"}"#.to_owned()]),
        ),
    );
    let mut other = call(2, "other", args("other", &[text("other", "other")]));
    if let Input::Call { thread, .. } = &mut other {
        *thread = ThreadId(8);
    }
    store.run(&mut drawing, other);
    store.run(
        &mut drawing,
        Input::StepShown {
            run: RunId(2),
            scene: SceneId(4),
            index: 0,
        },
    );
    let ended = store.run(&mut drawing, closed(1));
    assert_eq!(notices_in(&ended), [(THREAD, RunId(1))]);
    assert!(no_loss(&drawing));
}

#[test]
fn a_run_the_user_ended_drops_its_loss_and_later_failures_stay_silent() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    lost_replacement(&mut drawing, &mut store);
    let ended = store.run(
        &mut drawing,
        Input::RunClosed {
            run: RunId(1),
            loss: Loss::Drop,
        },
    );
    assert_eq!(notices_in(&ended), []);
    assert!(no_loss(&drawing));
    let late = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(2) });
    assert_eq!(notices_in(&late), []);
}

#[test]
fn a_run_with_no_step_left_is_told_at_once_and_only_once() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    replaced(&mut drawing, &mut store, MAX_DRAW_STEPS_PER_RUN - 1);
    assert_eq!(drawing.remaining(RunId(1)), Remaining::Calls);
    let failed = store.run(&mut drawing, first_step_failed());
    assert_eq!(notices_in(&failed), [(THREAD, RunId(1))]);
    let again = store.run(&mut drawing, Input::SceneFailed { scene: SceneId(2) });
    assert_eq!(notices_in(&again), []);
    let ended = store.run(&mut drawing, closed(1));
    assert_eq!(notices_in(&ended), []);
    assert!(no_loss(&drawing));
}

#[test]
fn a_call_that_spends_the_run_s_last_step_tells_the_loss() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    lost_replacement(&mut drawing, &mut store);
    let rest = MAX_DRAW_STEPS_PER_RUN - 1;
    let spent = store.run(
        &mut drawing,
        call(1, "more", args("more", &numbered("m", rest))),
    );
    assert_eq!(spent.calls.len(), 1);
    assert_eq!(spent.calls[0].remaining, Remaining::Calls);
    assert_eq!(notices_in(&spent), [(THREAD, RunId(1))]);
    let ended = store.run(&mut drawing, closed(1));
    assert_eq!(notices_in(&ended), []);
    assert!(no_loss(&drawing));
}

#[test]
fn a_replacement_saved_after_the_user_ended_its_run_reports_no_loss() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "old", args("old", &[text("old", "old")])),
    );
    let mut held = drawing
        .handle(call(1, "new", replacement_args("new", &numbered("n", 1))))
        .expect("a fresh call");
    while let Some((serial, write)) =
        sent(&held).filter(|(_, write)| !matches!(write, Write::Step { .. }))
    {
        let reply = Ok(store.answer(&write));
        held = drawing
            .handle(Input::Written { serial, reply })
            .expect("the pending lookup");
    }
    let first = sent(&held)
        .expect("the first step's write is in the store")
        .1;
    let mut out = drawing
        .handle(Input::RunClosed {
            run: RunId(1),
            loss: Loss::Drop,
        })
        .expect("the user ended the run");
    let answer = store.answer(&first);
    merge(&mut out, reply(&mut drawing, &held, Ok(answer)));
    let out = store.settle(&mut drawing, out);
    assert_eq!(
        drawing.scenes.len(),
        1,
        "the replacement's scene was installed"
    );
    let failed = store.run(&mut drawing, first_step_failed());
    assert_eq!(notices_in(&out), []);
    assert_eq!(notices_in(&failed), []);
    assert!(no_loss(&drawing));
}

#[test]
fn deleting_the_thread_drops_its_loss_silently() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    lost_replacement(&mut drawing, &mut store);
    let deleted = store.run(&mut drawing, Input::ThreadDeleted { thread: THREAD });
    assert_eq!(notices_in(&deleted), []);
    let ended = store.run(&mut drawing, closed(1));
    assert_eq!(notices_in(&ended), []);
    assert!(no_loss(&drawing));
}

#[test]
fn a_step_too_large_to_paint_ends_its_call_asking_for_fewer_shapes() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        streamed(
            "big",
            0,
            r#"{"begin":{"v":1,"scene":"big","surface":"overlay","alt":""}}"#,
        ),
    );
    store.run(&mut drawing, streamed("big", 1, &step(&text("big", "big"))));
    let failed = store.run(
        &mut drawing,
        Input::StepFailed {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
            reason: SceneFailure::PaintWork,
            cut: cut(0, vec![]),
        },
    );
    assert_eq!(failed.calls.len(), 1);
    assert_eq!(
        failed.calls[0].error,
        Some(CallError::TooMuchToPaint { cleared: true })
    );
    assert_eq!(
        failed.calls[0].tool_text(),
        "drawn: [big]; stopped: drawing.TooMuchToPaint; nothing of this scene was shown, so it was removed; everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows; begin a new scene with a new name and fewer or simpler shapes"
    );
    assert!(drawing.drew_nothing(RunId(1)));
}

#[test]
fn a_later_step_too_large_to_paint_keeps_the_shown_picture() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        streamed(
            "big",
            0,
            r#"{"begin":{"v":1,"scene":"big","surface":"overlay","alt":""}}"#,
        ),
    );
    store.run(&mut drawing, streamed("big", 1, &step(&text("one", "one"))));
    store.run(
        &mut drawing,
        Input::StepShown {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
        },
    );
    store.run(&mut drawing, streamed("big", 2, &step(&text("two", "two"))));
    let shown = cut(1, vec![handle(&drawing, 1, "one")]);
    let failed = store.run(
        &mut drawing,
        Input::StepFailed {
            run: RunId(1),
            scene: SceneId(1),
            index: 1,
            reason: SceneFailure::PaintWork,
            cut: shown,
        },
    );
    assert_eq!(failed.calls.len(), 1);
    assert_eq!(
        failed.calls[0].tool_text(),
        "drawn: [one, two]; stopped: drawing.TooMuchToPaint; the picture stays at its last shown step; everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows; use fewer or simpler shapes and call draw again"
    );
    assert!(!drawing.drew_nothing(RunId(1)));
}

/// The budget the refusal names is the prompt's, and its numbers hold at the paint caps, with
/// diagram boxes charged unshrunk whatever their fit: each stated drawing fits, one with a
/// quarter more circles or boxes does not, and the true edges are 42 circles and 36 boxes.
#[test]
fn the_paint_budget_matches_the_prompt_and_the_paint_caps() {
    use kumochi_scene::display::PaintWork;
    use serde_json::json;

    const CIRCLES: usize = 40;
    const BOXES: usize = 30;
    let words = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    assert!(
        words(include_str!("../../prompts/draw.md")).contains(&words(super::call::PAINT_BUDGET)),
        "the prompt states the refusal's budget"
    );
    assert!(super::call::PAINT_BUDGET.contains(&format!("about {CIRCLES} filled circles")));
    assert!(super::call::PAINT_BUDGET.contains(&format!(
        "a diagram of about {BOXES} boxes with labelled arrows"
    )));
    let circles = |count: usize| -> Vec<serde_json::Value> {
        (0..count)
            .map(|index| {
                json!({"shape":{"id":format!("c{index}"),"kind":"ellipse","box":[index * 30, 100, 24, 24],"fill":"blue"}})
            })
            .collect()
    };
    let diagram = |boxes: usize| {
        let mut put = vec![json!({"diagram":{"id":"d","dir":"down"}})];
        put.extend(
            (0..boxes)
                .map(|index| json!({"node":{"id":format!("n{index}"),"in":"d","label":"Step"}})),
        );
        put.extend((1..boxes).map(|index| {
            json!({"edge":{"id":format!("e{index}"),"from":format!("n{}", index - 1),"to":format!("n{index}"),"label":"then"}})
        }));
        put
    };
    let circle_work = |count: usize| PaintWork {
        fill_vertices: 48 * count,
        largest_fill: 48,
        ..PaintWork::default()
    };
    // Unshrunk, a box costs 21 fill and 21 outline points, and a labelled arrow 24 fill. A
    // chain of 30 or more overflows the default canvas, so these are also shrunk-to-fit layouts.
    let diagram_work = |boxes: usize| PaintWork {
        fill_vertices: 21 * boxes + 24 * (boxes - 1),
        largest_fill: 21,
        outline_points: 21 * boxes,
        largest_outline: 21,
        ..PaintWork::default()
    };
    for (count, fits) in [
        (CIRCLES, true),
        (42, true),
        (43, false),
        (CIRCLES + CIRCLES / 4, false),
    ] {
        let work = paint_work(&circles(count));
        assert_eq!(work, circle_work(count), "{count} circles");
        assert_eq!(work.within_limits(), fits, "{count} circles");
    }
    for (boxes, fits) in [(BOXES, true), (36, true), (37, false)] {
        let work = paint_work(&diagram(boxes));
        assert_eq!(work, diagram_work(boxes), "{boxes} boxes");
        assert_eq!(work.within_limits(), fits, "{boxes} boxes");
    }
    assert_eq!(diagram_work(36).outline_points, 756);
    assert_eq!(diagram_work(37).outline_points, 777);
}

/// The paint work of one step of `put` on a fresh overlay scene laid out on the shell's default
/// canvas, as production prepares it, without a cap.
fn paint_work(put: &[serde_json::Value]) -> kumochi_scene::display::PaintWork {
    use kumochi_scene::display::{MeasureError, TextMeasure, TextStyle};
    use kumochi_scene::layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare};

    struct Measure;
    impl TextMeasure for Measure {
        fn measure(
            &mut self,
            _: &str,
            _: TextStyle,
            _: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok([40.0, 20.0])
        }
    }
    let record = |value: serde_json::Value| {
        parse_record(&serde_json::to_vec(&value).expect("JSON")).expect("a record")
    };
    let Record::Begin(begin) =
        record(serde_json::json!({"begin":{"v":1,"scene":"budget","surface":"overlay","alt":""}}))
    else {
        panic!("a begin record");
    };
    let Record::Step(step) = record(serde_json::json!({"step":{"id":"one","put":put}})) else {
        panic!("a step record");
    };
    let mut scene = SceneState::begin(begin).expect("scene");
    let mut budget = kumochi_scene::GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let kumochi_scene::Admission::New(admitted) = scene.admit(&step, &budget).expect("admitted")
    else {
        panic!("a new step");
    };
    let ticket = scene
        .commit(admitted, &mut budget)
        .expect("committed")
        .preparation;
    let input = PrepareInput::new(
        ticket,
        before,
        SurfaceGeometry::Overlay {
            default_canvas: kumochi_scene::layout::DEFAULT_CANVAS,
        },
        Arc::new(kumochi_scene::theme::Theme::default()),
    )
    .map_err(|(error, _)| error)
    .expect("the first step follows the empty layout");
    prepare(
        input,
        &mut Measure,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .expect("prepared")
    .after
    .paint_work()
}

/// One record of run `run`'s streamed call `call`.
fn record_of(run: u64, call: &str, index: u32, record: &str) -> Input {
    Input::Record {
        thread: THREAD,
        run: RunId(run),
        call: CallId::new(call).expect("a short ASCII id"),
        index,
        bytes: record.as_bytes().to_vec(),
    }
}

/// A second failed cut on one scene counts only steps the first didn't, so a run whose later
/// steps are all refused still drew nothing, and one that showed a step still drew something.
#[test]
fn a_second_failed_cut_counts_only_steps_the_first_did_not() {
    let begin = begin("big", "");
    for showing in [1, 2] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        let first = store.run(
            &mut drawing,
            call(1, "first", args("big", &[text("one", "one")])),
        );
        assert_eq!(first.calls[0].error, None);
        store.run(
            &mut drawing,
            Input::StepShown {
                run: RunId(1),
                scene: SceneId(1),
                index: 0,
            },
        );
        if showing == 2 {
            store.run(&mut drawing, closed(1));
        }
        let shown = cut(1, vec![handle(&drawing, 1, "one")]);
        for (index, call, id) in [(1, "second", "two"), (2, "third", "three")] {
            store.run(&mut drawing, record_of(showing, call, 0, &begin));
            store.run(
                &mut drawing,
                record_of(showing, call, 1, &step(&text(id, id))),
            );
            let failed = store.run(
                &mut drawing,
                Input::StepFailed {
                    run: RunId(showing),
                    scene: SceneId(1),
                    index,
                    reason: SceneFailure::PaintWork,
                    cut: shown.clone(),
                },
            );
            assert_eq!(failed.calls.len(), 1);
        }
        assert_eq!(drawing.drew_nothing(RunId(showing)), showing == 2);
    }
}

/// Run `run` closed on its own, so a drawing it lost is told.
fn closed(run: u64) -> Input {
    Input::RunClosed {
        run: RunId(run),
        loss: Loss::Tell,
    }
}

fn call(run: u64, call: &str, args: Vec<u8>) -> Input {
    Input::Call {
        thread: THREAD,
        run: RunId(run),
        call: CallId::new(call).expect("a short ASCII id"),
        args,
    }
}

/// One closed record of the streamed call `call` of run 1.
fn streamed(call: &str, index: u32, record: &str) -> Input {
    Input::Record {
        thread: THREAD,
        run: RunId(1),
        call: CallId::new(call).expect("a short ASCII id"),
        index,
        bytes: record.as_bytes().to_vec(),
    }
}

fn step(step: &str) -> String {
    format!(r#"{{"step":{step}}}"#)
}

fn cut(activated: u32, revealed: Vec<Handle>) -> StopCut {
    StopCut {
        activated,
        revealed: Bounded::new(revealed).expect("few handles"),
    }
}

fn stop(run: u64, scene: u64, cut: StopCut) -> Input {
    let cuts = vec![SceneCut {
        scene: SceneId(scene),
        cut,
    }];
    Input::StopScenes {
        run: RunId(run),
        cuts: SceneCuts::new(cuts).expect("one scene"),
    }
}

fn handle(drawing: &Drawing, scene: u64, object: &str) -> Handle {
    let held = drawing
        .scenes
        .iter()
        .find(|held| held.scene == SceneId(scene))
        .expect("a visible scene");
    held.state
        .handle(&wire(object))
        .expect("a surviving object")
}

fn ids(scenes: &[SceneId]) -> String {
    let ids: Vec<String> = scenes.iter().map(|scene| scene.0.to_string()).collect();
    ids.join(",")
}

/// What the store and the shell would see, in order.
fn trace(out: &Output) -> Vec<String> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::Drawing { write, .. }) => Some(match write {
                Write::Lookup { .. } => "lookup".to_owned(),
                Write::Step { revision, .. } => format!("write step {revision}"),
                Write::Cut {
                    scene,
                    revision,
                    cut,
                } => format!("write cut {}@{revision}:{}", scene.0, cut.activated),
            }),
            Effect::Store(StoreOp::ClearScenes { scenes }) => {
                Some(format!("clear {}", ids(scenes)))
            }
            Effect::RevokeScenes { scenes } => Some(format!("revoke {}", ids(scenes))),
            Effect::Data(DataToShell::ScenesRemoved { scenes }) => {
                Some(format!("removed {}", ids(scenes)))
            }
            Effect::Data(DataToShell::ScenesReplaced { scenes, successor }) => {
                Some(format!("replaced {} by {}", ids(scenes), successor.0))
            }
            Effect::Data(DataToShell::SceneBegin { scene, .. }) => {
                Some(format!("begin {}", scene.0))
            }
            Effect::Data(DataToShell::SceneStepCommit { scene, index, .. }) => {
                Some(format!("step {}#{index}", scene.0))
            }
            Effect::Data(DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            }) => Some(format!("cut {}@{revision}:{}", scene.0, cut.activated)),
            Effect::Data(DataToShell::SceneEnd {
                scene, complete, ..
            }) => Some(format!("end {} {complete}", scene.0)),
            Effect::Broken(_) => Some("broken".to_owned()),
            _ => None,
        })
        .collect()
}

/// The one drawing write an output sends, if any.
fn sent(out: &Output) -> Option<(u64, Write)> {
    let writes: Vec<_> = out
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
            _ => None,
        })
        .collect();
    assert!(
        writes.len() <= 1,
        "one drawing write is in the store at a time"
    );
    writes.into_iter().next()
}

fn serial(out: &Output) -> u64 {
    sent(out).expect("a drawing write").0
}

fn reply(drawing: &mut Drawing, out: &Output, reply: Result<Reply, ()>) -> Output {
    let serial = serial(out);
    drawing
        .handle(Input::Written { serial, reply })
        .expect("the pending write")
}

fn merge(into: &mut Output, more: Output) {
    into.effects.extend(more.effects);
    into.calls.extend(more.calls);
}

/// A store double: scene ids from 1, names per thread, and logs persisted as JSON.
#[derive(Default)]
struct Store {
    scenes: Vec<StoredScene>,
    cleared: Vec<SceneId>,
    /// The shell double never prepares a step, so finished calls wait for it.
    unprepared: bool,
}

impl Store {
    fn answer(&mut self, write: &Write) -> Reply {
        match write {
            Write::Lookup { thread, name } => Reply::Lookup(
                self.scenes
                    .iter()
                    .find(|stored| stored.thread == *thread && stored.header.begin.scene == *name)
                    .map(|stored| stored.scene),
            ),
            Write::Step {
                thread,
                target,
                clear,
                revision,
                step,
                ..
            } => {
                let scene = match target {
                    Target::New(header) => {
                        let count = u64::try_from(self.scenes.len()).expect("few scenes");
                        let scene = SceneId(count + 1);
                        self.scenes.push(StoredScene {
                            scene,
                            thread: *thread,
                            header: json(header),
                            log: Vec::new(),
                        });
                        scene
                    }
                    Target::Existing(scene) => *scene,
                };
                let step: Step = json(&**step);
                self.log(scene).push(SceneEvent::Step {
                    revision: *revision,
                    step: Arc::new(step),
                });
                self.cleared.extend(clear.iter().copied());
                Reply::Step(scene)
            }
            Write::Cut { scene, cut, .. } => {
                self.log(*scene).push(SceneEvent::Stop(json(cut)));
                Reply::Cut
            }
        }
    }

    fn log(&mut self, scene: SceneId) -> &mut Vec<SceneEvent> {
        &mut self
            .scenes
            .iter_mut()
            .find(|stored| stored.scene == scene)
            .expect("a begun scene")
            .log
    }

    /// The newest four scenes without a clear.
    fn visible(&self) -> Vec<StoredScene> {
        let mut visible: Vec<_> = self
            .scenes
            .iter()
            .filter(|stored| !self.cleared.contains(&stored.scene))
            .cloned()
            .collect();
        let excess = visible.len().saturating_sub(4);
        visible.drain(..excess);
        visible
    }

    /// Answers every drawing write and clear until none is pending, as the store thread and the
    /// coordinator's clearing ledger would.
    fn settle(&mut self, drawing: &mut Drawing, mut out: Output) -> Output {
        let mut seen = 0;
        loop {
            let fresh = Output {
                effects: out.effects[seen..].to_vec(),
                calls: Vec::new(),
            };
            seen = out.effects.len();
            if fresh.effects.is_empty() {
                return out;
            }
            let mut cleared = false;
            for effect in &fresh.effects {
                if let Effect::Store(StoreOp::ClearScenes { scenes }) = effect {
                    self.cleared.extend(scenes);
                    cleared = true;
                }
            }
            if cleared {
                merge(&mut out, drawing.clears_pending(0));
            }
            for effect in &fresh.effects {
                if let Effect::PrepareDrawing(job) = effect {
                    let more = drawing
                        .handle(job.clone().run())
                        .expect("the pending preparation");
                    merge(&mut out, more);
                }
            }
            for effect in &fresh.effects {
                if let Effect::Data(DataToShell::SceneStepCommit {
                    run, scene, index, ..
                }) = effect
                    && !self.unprepared
                {
                    let ready = Input::StepReady {
                        run: *run,
                        scene: *scene,
                        index: *index,
                    };
                    merge(&mut out, drawing.handle(ready).expect("a committed step"));
                }
            }
            if let Some((serial, write)) = sent(&fresh) {
                let reply = Ok(self.answer(&write));
                let more = drawing
                    .handle(Input::Written { serial, reply })
                    .expect("the pending write");
                merge(&mut out, more);
            }
        }
    }

    fn run(&mut self, drawing: &mut Drawing, input: Input) -> Output {
        let out = drawing.handle(input).expect("a fresh input");
        self.settle(drawing, out)
    }
}

/// Stored data goes through JSON, so recovery sees what the store would give back.
fn json<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    let encoded = serde_json::to_string(value).expect("typed data encodes");
    serde_json::from_str(&encoded).expect("typed data decodes")
}

/// Four visible scenes, s1 to s4, one step each.
fn four(store: &mut Store, drawing: &mut Drawing) {
    for n in 1..=4 {
        let input = call(
            1,
            &format!("c{n}"),
            args(&format!("s{n}"), &[text("a", "x")]),
        );
        store.run(drawing, input);
    }
}

fn error(end: &CallEnd) -> ErrorCode {
    match &end.error {
        Some(CallError::Step(error)) => error.code,
        other => panic!("a rejected record, not {other:?}"),
    }
}

#[test]
fn unsaved_default_replacements_preserve_existing_drawings() {
    let forward = r#"{"id":"a","start":{"after":"later","ms":0}}"#.to_owned();
    for (args, expected, trace_expected) in [
        (
            replacement_args("new", &[forward]),
            Some(ErrorCode::ForwardRef),
            vec!["lookup"],
        ),
        (
            br#"{"records":[]}"#.to_vec(),
            Some(ErrorCode::BeforeBegin),
            vec![],
        ),
        (
            br#"{"records":[{"begin":{"v":1,"scene":"new","surface":"overlay","alt":""}}]}"#
                .to_vec(),
            Some(ErrorCode::MissingField),
            vec!["lookup"],
        ),
        (replacement_args("new", &[]), None, vec!["lookup"]),
    ] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        four(&mut store, &mut drawing);
        let before = drawing.snapshot();
        let stored = store.visible();
        let budget = (drawing.global.usage(), drawing.global.generation());
        assert_eq!(
            before
                .scenes
                .iter()
                .map(|scene| scene.scene)
                .collect::<Vec<_>>(),
            [SceneId(1), SceneId(2), SceneId(3), SceneId(4)]
        );
        let out = store.run(&mut drawing, call(1, "new", args));
        assert_eq!(trace(&out), trace_expected);
        assert!(
            !out.effects
                .iter()
                .any(|effect| matches!(effect, Effect::Data(_) | Effect::RevokeScenes { .. }))
        );
        assert_eq!(out.calls.len(), 1);
        assert_eq!(
            out.calls[0].error.as_ref().map(|_| error(&out.calls[0])),
            expected
        );
        assert_eq!(out.calls[0].drawn, []);
        assert_eq!(out.calls[0].scene, None);
        let after = drawing.snapshot();
        assert_eq!(after.scenes, before.scenes);
        assert_eq!(after.chunks, before.chunks);
        assert_eq!(after.skipped, []);
        assert_eq!(store.visible(), stored);
        assert_eq!(store.scenes, stored);
        assert_eq!(store.cleared, []);
        assert_eq!(
            (drawing.global.usage(), drawing.global.generation()),
            budget
        );
        assert_eq!(
            (
                drawing.global.usage().scenes,
                drawing.global.usage().objects
            ),
            (4, 4)
        );
    }
}

#[test]
fn default_replacement_clears_once_but_continuing_its_header_preserves_a_kept_scene() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    four(&mut store, &mut drawing);
    let out = store.run(
        &mut drawing,
        call(
            1,
            "new",
            replacement_args("new", &[text("a", "x"), text("b", "y")]),
        ),
    );
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "revoke 1,2,3,4",
            "replaced 1,2,3,4 by 5",
            "begin 5",
            "step 5#0",
            "write step 2",
            "step 5#1",
            "end 5 true"
        ]
    );
    assert_eq!(
        store.cleared,
        [SceneId(1), SceneId(2), SceneId(3), SceneId(4)]
    );
    assert_eq!(
        (
            drawing.global.usage().scenes,
            drawing.global.usage().objects
        ),
        (1, 2)
    );
    store.run(
        &mut drawing,
        call(1, "compare", args("compare", &[text("a", "z")])),
    );
    let continued = store.run(
        &mut drawing,
        call(1, "continue", replacement_args("new", &[text("c", "v")])),
    );
    assert_eq!(
        trace(&continued),
        ["write step 3", "step 5#2", "end 5 true"]
    );
    assert_eq!(
        drawing
            .scenes
            .iter()
            .map(|held| held.scene.0)
            .collect::<Vec<_>>(),
        [5, 6]
    );
    assert_eq!(drawing.global.usage().objects, 4);
}

#[test]
fn stop_or_clear_before_replacement_submission_never_writes_the_first_step() {
    for clear in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        four(&mut store, &mut drawing);
        let lookup = drawing
            .handle(call(1, "new", replacement_args("new", &[text("a", "x")])))
            .expect("lookup");
        let interrupt = if clear {
            Input::Clear {
                scenes: (1..=4).map(SceneId).collect(),
            }
        } else {
            Input::CallFinished {
                thread: THREAD,
                run: RunId(1),
                call: CallId::new("new").expect("id"),
                error: Some(CallError::Interrupted),
            }
        };
        let out = store.run(&mut drawing, interrupt);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].error, Some(CallError::Interrupted));
        let answer = store.answer(&sent(&lookup).expect("lookup write").1);
        let out = reply(&mut drawing, &lookup, Ok(answer));
        assert_eq!(trace(&out), Vec::<String>::new());
        assert_eq!(store.scenes.len(), 4);
        assert_eq!(drawing.scenes.len(), if clear { 0 } else { 4 });
        assert_eq!(drawing.global.usage().scenes, if clear { 0 } else { 4 });
    }
}

#[test]
fn clear_or_stop_after_submission_settles_replacement_even_before_reply_delivery() {
    for clear in [false, true] {
        for committed in [false, true] {
            let mut drawing = Drawing::default();
            let mut store = Store::default();
            four(&mut store, &mut drawing);
            let lookup = drawing
                .handle(call(1, "new", replacement_args("new", &[text("a", "x")])))
                .expect("lookup");
            let answer = store.answer(&sent(&lookup).expect("lookup").1);
            let pending = reply(&mut drawing, &lookup, Ok(answer));
            let write = sent(&pending).expect("first-step write").1;
            let saved = committed.then(|| store.answer(&write));
            if clear {
                store.run(
                    &mut drawing,
                    Input::Clear {
                        scenes: (1..=4).map(SceneId).collect(),
                    },
                );
            } else {
                store.run(
                    &mut drawing,
                    Input::CallFinished {
                        thread: THREAD,
                        run: RunId(1),
                        call: CallId::new("new").expect("id"),
                        error: Some(CallError::Interrupted),
                    },
                );
                store.run(
                    &mut drawing,
                    Input::StopScenes {
                        run: RunId(1),
                        cuts: SceneCuts::default(),
                    },
                );
            }
            let answer = saved.unwrap_or_else(|| store.answer(&write));
            let out = reply(&mut drawing, &pending, Ok(answer));
            if clear {
                assert!(
                    !trace(&out)
                        .iter()
                        .any(|item| item.starts_with("begin ") || item.starts_with("step "))
                );
            }
            store.settle(&mut drawing, out);
            assert_eq!(
                drawing.scenes.len(),
                0,
                "clear={clear} committed={committed}"
            );
            assert_eq!(
                drawing.global.usage(),
                kumochi_scene::budget::Usage::default()
            );
            assert_eq!(store.visible(), []);
            for id in 1..=5 {
                assert!(store.cleared.contains(&SceneId(id)));
            }
        }
    }
}

#[test]
fn deleted_replacement_settles_a_cross_thread_cap_victim_only_when_the_write_committed() {
    for committed in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = Store::default();
        for n in 1..=4 {
            let mut input = call(
                1,
                &format!("c{n}"),
                args(&format!("s{n}"), &[text("a", "x")]),
            );
            let Input::Call { thread, .. } = &mut input else {
                panic!("call")
            };
            *thread = ThreadId(8);
            store.run(&mut drawing, input);
        }
        store.run(&mut drawing, closed(1));
        let lookup = drawing
            .handle(call(2, "new", replacement_args("new", &[text("a", "x")])))
            .expect("lookup");
        let answer = store.answer(&sent(&lookup).expect("lookup").1);
        let pending = reply(&mut drawing, &lookup, Ok(answer));
        let answer = if committed {
            Ok(store.answer(&sent(&pending).expect("step").1))
        } else {
            Err(())
        };
        store.run(&mut drawing, Input::ThreadDeleted { thread: THREAD });
        let out = reply(&mut drawing, &pending, answer);
        assert_eq!(
            trace(&out),
            if committed {
                vec!["revoke 1", "removed 1"]
            } else {
                vec![]
            }
        );
        assert_eq!(
            drawing
                .scenes
                .iter()
                .map(|held| held.scene.0)
                .collect::<Vec<_>>(),
            if committed {
                vec![2, 3, 4]
            } else {
                vec![1, 2, 3, 4]
            }
        );
        assert_eq!(drawing.global.usage().scenes, if committed { 3 } else { 4 });
        assert_eq!(
            drawing.global.usage().objects,
            if committed { 3 } else { 4 }
        );
    }
}

#[test]
fn a_failed_step_write_commits_nothing_and_the_name_stays_free() {
    let mut drawing = Drawing::default();
    let out = drawing
        .handle(call(1, "c1", args("s", &[text("a", "x")])))
        .expect("a fresh call");
    assert_eq!(trace(&out), ["lookup"]);
    let out = reply(&mut drawing, &out, Ok(Reply::Lookup(None)));
    assert_eq!(trace(&out), ["write step 1"]);
    let out = reply(&mut drawing, &out, Err(()));
    assert!(trace(&out).is_empty(), "nothing reaches the shell");
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].error, Some(CallError::Store));
    assert!(out.calls[0].drawn.is_empty(), "nothing was drawn");
    assert!(drawing.scenes.is_empty(), "no scene was committed");
    assert_eq!(drawing.global.usage().objects, 0, "nothing stays charged");

    let mut store = Store::default();
    let out = store.run(&mut drawing, call(1, "c2", args("s", &[text("a", "x")])));
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "begin 1",
            "step 1#0",
            "end 1 true"
        ]
    );
}

#[test]
fn a_closed_run_s_queued_records_never_reach_the_store_but_its_written_step_commits() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let out = drawing
        .handle(call(1, "c", args("s", &[text("a", "x"), text("b", "y")])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    let write = sent(&out).expect("the first step").1;

    let closed = drawing.handle(closed(1)).expect("a close");
    assert!(closed.effects.is_empty(), "a close sends nothing");
    assert!(
        drawing.pending_for(RunId(1)),
        "its first step is in the store"
    );
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert_eq!(trace(&out), ["begin 1", "step 1#0", "end 1 false"]);
    assert!(drawing.settled(), "step b never went out");
    assert!(!drawing.pending_for(RunId(1)), "nothing of the run is left");

    let late = call(1, "late", args("s", &[text("c", "z")]));
    assert_eq!(drawing.handle(late).err(), Some(Stale));
}

#[test]
fn a_repeated_call_record_or_finish_changes_nothing() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let whole = args("s", &[text("a", "x")]);
    store.run(&mut drawing, call(1, "c", whole.clone()));
    assert_eq!(drawing.handle(call(1, "c", whole)).err(), Some(Stale));

    let streamed = CallId::new("streamed").expect("a short ASCII id");
    let record = |index: u32, json: String| Input::Record {
        thread: THREAD,
        run: RunId(1),
        call: streamed.clone(),
        index,
        bytes: json.into_bytes(),
    };
    let finished = Input::CallFinished {
        thread: THREAD,
        run: RunId(1),
        call: streamed.clone(),
        error: None,
    };
    store.run(&mut drawing, record(0, begin("s", "")));
    let out = store.run(
        &mut drawing,
        record(1, format!(r#"{{"step":{}}}"#, text("b", "y"))),
    );
    assert_eq!(trace(&out), ["write step 2", "step 1#1"]);
    let repeat = record(1, format!(r#"{{"step":{}}}"#, text("b", "y")));
    assert_eq!(drawing.handle(repeat).err(), Some(Stale));
    store.run(&mut drawing, record(2, r#"{"end":{}}"#.to_owned()));
    let out = store.run(&mut drawing, finished.clone());
    assert_eq!(trace(&out), ["end 1 true"]);
    assert!(out.calls[0].complete(), "the streamed call completed");
    assert_eq!(drawing.handle(finished).err(), Some(Stale));
}

#[test]
fn a_complete_call_commits_its_valid_prefix_one_write_at_a_time() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let bad = r#"{"id":"c","put":[],"colour":1}"#.to_owned();
    let out = store.run(
        &mut drawing,
        call(1, "c", args("s", &[text("a", "x"), text("b", "y"), bad])),
    );
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "begin 1",
            "step 1#0",
            "write step 2",
            "step 1#1",
            "end 1 false",
        ]
    );
    let end = &out.calls[0];
    assert_eq!(end.drawn, [wire("a"), wire("b")]);
    assert_eq!(error(end), ErrorCode::UnknownField);
    assert!(
        end.tool_text()
            .starts_with("drawn: [a, b]; stopped: step c: "),
        "the model hears what it drew and the step that failed: {}",
        end.tool_text()
    );
}

#[test]
fn malformed_arguments_run_nothing() {
    let mut drawing = Drawing::default();
    let mut whole = args("s", &[text("a", "x")]);
    whole.truncate(whole.len() - 2);
    let out = drawing.handle(call(1, "c", whole)).expect("a fresh call");
    assert!(out.effects.is_empty(), "not even a lookup");
    assert_eq!(
        out.calls[0].error,
        Some(CallError::Token(TokenError::Truncated))
    );
}

#[test]
fn a_continuing_call_repeats_the_whole_header() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let other = envelope(&sized("s"), &[text("b", "y")]);
    let out = store.run(&mut drawing, call(1, "c2", other));
    assert!(trace(&out).is_empty(), "nothing is written");
    assert_eq!(error(&out.calls[0]), ErrorCode::HeaderMismatch);
    let out = store.run(&mut drawing, call(2, "c3", args("s", &[text("b", "y")])));
    assert_eq!(trace(&out), ["write step 2", "step 1#1", "end 1 true"]);
}

#[test]
fn a_draw_call_puts_an_object_behind_an_earlier_one_and_refuses_a_cycle() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s", &[text("a", "x"), text("b", "y")])),
    );
    let behind = r#"{"id":"c","put":[{"text":{"id":"z","text":"","at":[0,0]}}],"set":[{"target":"z","behind":"x"}]}"#;
    let out = store.run(&mut drawing, call(2, "c2", args("s", &[behind.to_owned()])));
    assert_eq!(trace(&out), ["write step 3", "step 1#2", "end 1 true"]);
    let cycle = r#"{"id":"d","set":[{"target":"x","behind":"z"}]}"#;
    let out = store.run(&mut drawing, call(3, "c3", args("s", &[cycle.to_owned()])));
    assert_eq!(
        trace(&out),
        ["end 1 false"],
        "the refused step writes nothing"
    );
    assert_eq!(error(&out.calls[0]), ErrorCode::ReferenceCycle);
}

#[test]
fn a_continuing_call_may_restate_the_title_and_alt() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let restated = r#"{"begin":{"v":1,"scene":"s","surface":"overlay","title":"Rain","alt":"now with rain","others":"keep"}}"#;
    let out = store.run(
        &mut drawing,
        call(2, "c2", envelope(restated, &[text("b", "y")])),
    );
    assert_eq!(trace(&out), ["write step 2", "step 1#1", "end 1 true"]);
    assert_eq!(
        drawing.scenes[0].state.header().alt.as_str(),
        "",
        "the stored header keeps its first text"
    );
}

#[test]
fn recovery_rebuilds_the_live_log_handles_and_reserved_ids() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s", &[text("a", "x"), text("b", "y")])),
    );
    let x = handle(&drawing, 1, "x");
    let out = store.run(&mut drawing, stop(1, 1, cut(1, vec![x])));
    assert_eq!(trace(&out), ["write cut 1@2:1", "cut 1@2:1"]);
    store.run(
        &mut drawing,
        call(2, "c2", args("s", &[text("c", "z"), text("d", "w")])),
    );
    let z = handle(&drawing, 1, "z");
    let out = store.run(&mut drawing, stop(1, 1, cut(3, vec![x, z])));
    assert_eq!(trace(&out), ["write cut 1@4:3", "cut 1@4:3"]);
    let out = store.run(&mut drawing, call(3, "c3", args("s", &[text("e", "y")])));
    assert_eq!(error(&out.calls[0]), ErrorCode::IdReused);

    let (recovered, skipped) = Drawing::recover(store.visible());
    assert!(skipped.is_empty(), "the log replays");
    let (live, again) = (&drawing.scenes[0].state, &recovered.scenes[0].state);
    assert_eq!(
        again.recovery_log(),
        live.recovery_log(),
        "JSON kept every value"
    );
    assert_eq!(again.revision(), 4);
    assert_eq!(again.committed(), 2);
    assert_eq!(handle(&recovered, 1, "x"), x);
    assert_eq!(handle(&recovered, 1, "z"), z);
    assert_eq!(recovered.global.usage(), drawing.global.usage());
    let Record::Step(reuse) =
        parse_record(format!(r#"{{"step":{}}}"#, text("e", "y")).as_bytes()).expect("a step")
    else {
        panic!("a step record");
    };
    let refused = again
        .admit(&reuse, &recovered.global)
        .expect_err("a reserved id");
    assert_eq!(refused.code, ErrorCode::IdReused);
}

#[test]
fn a_cut_to_nothing_is_durable_before_the_scene_leaves() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c", args("s", &[text("a", "x")])));
    let out = store.run(&mut drawing, stop(1, 1, cut(0, Vec::new())));
    assert_eq!(trace(&out), ["write cut 1@1:0", "cut 1@1:0", "clear 1"]);
    assert!(drawing.live(SceneId(1)).is_none(), "the scene left");
    assert_eq!(drawing.global.usage().scenes, 0);
    assert!(
        matches!(store.scenes[0].log.last(), Some(SceneEvent::Stop(stored)) if stored.activated == 0),
        "the zero cut is in the log"
    );
}

#[test]
fn recovery_leaves_out_a_scene_whose_clear_never_landed() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c", args("s", &[text("a", "x")])));
    let out = drawing
        .handle(stop(1, 1, cut(0, Vec::new())))
        .expect("a stop");
    // The cut is durable, then the brain dies before its clear is written.
    store.answer(&sent(&out).expect("the cut").1);
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert!(
        recovered.scenes.is_empty(),
        "a scene cut to nothing isn't visible"
    );
    assert_eq!(skipped, [SceneId(1)], "its clear is owed again");
    let snapshot = recovered.snapshot();
    assert!(snapshot.scenes.is_empty() && snapshot.chunks.is_empty());
}

#[test]
fn a_cleared_scene_name_is_refused() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = store.run(
        &mut drawing,
        Input::Clear {
            scenes: vec![SceneId(1), SceneId(9)],
        },
    );
    assert_eq!(trace(&out), ["revoke 1", "clear 1"]);
    assert_eq!(
        drawing.global.usage().objects,
        0,
        "its charges are released"
    );
    let out = store.run(&mut drawing, call(1, "c2", args("s", &[text("b", "y")])));
    assert_eq!(trace(&out), ["lookup"]);
    assert_eq!(error(&out.calls[0]), ErrorCode::SceneCleared);
}

#[test]
fn a_stop_during_a_step_write_sends_the_step_before_the_cut() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let x = handle(&drawing, 1, "x");
    let out = drawing
        .handle(call(1, "c2", args("s", &[text("b", "y")])))
        .expect("a fresh call");
    assert_eq!(trace(&out), ["write step 2"]);
    let held = drawing.handle(stop(1, 1, cut(1, vec![x]))).expect("a stop");
    let closed = drawing.handle(closed(1)).expect("a close");
    assert!(
        held.effects.is_empty() && closed.effects.is_empty(),
        "the cut waits"
    );
    let write = sent(&out).expect("the step").1;
    let answered = reply(&mut drawing, &out, Ok(store.answer(&write)));
    let out = store.settle(&mut drawing, answered);
    assert_eq!(
        trace(&out),
        ["step 1#1", "end 1 false", "write cut 1@2:1", "cut 1@2:1"]
    );
    assert_eq!(drawing.scenes[0].state.committed(), 1);
}

#[test]
fn a_stop_before_a_new_scene_reached_the_shell_cuts_it_to_nothing() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let out = drawing
        .handle(call(1, "c", args("s", &[text("a", "x")])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    let unseen = Input::StopScenes {
        run: RunId(1),
        cuts: SceneCuts::default(),
    };
    assert!(drawing.handle(unseen).expect("a stop").effects.is_empty());
    drawing.handle(closed(1)).expect("a close");
    let write = sent(&out).expect("the step").1;
    let answered = reply(&mut drawing, &out, Ok(store.answer(&write)));
    let out = store.settle(&mut drawing, answered);
    assert_eq!(
        trace(&out),
        [
            "begin 1",
            "step 1#0",
            "end 1 false",
            "write cut 1@1:0",
            "cut 1@1:0",
            "clear 1"
        ]
    );
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert!(recovered.scenes.is_empty(), "the Esc survives a restart");
    assert!(skipped.is_empty(), "its clear landed");
}

#[test]
fn a_shown_step_waits_for_the_write_it_would_make_stale() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let removal = r#"{"id":"b","remove":["x"]}"#.to_owned();
    let out = drawing
        .handle(call(1, "c2", args("s", &[removal])))
        .expect("a fresh call");
    let shown = Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index: 0,
    };
    assert!(
        drawing
            .handle(shown)
            .expect("a shown step")
            .effects
            .is_empty()
    );
    let write = sent(&out).expect("the step").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert_eq!(trace(&out), ["step 1#1"]);
    let ready = Input::StepReady {
        run: RunId(1),
        scene: SceneId(1),
        index: 1,
    };
    let out = drawing.handle(ready).expect("a prepared step");
    assert_eq!(trace(&out), ["end 1 true"]);
    assert_eq!(
        drawing.scenes[0].state.activated(),
        1,
        "step 0 retired after the commit"
    );

    for (run, index) in [(1, 2), (2, 1)] {
        let wrong = Input::StepShown {
            run: RunId(run),
            scene: SceneId(1),
            index,
        };
        let out = drawing.handle(wrong).expect("an answer");
        assert_eq!(
            trace(&out),
            ["broken"],
            "run {run} never committed step {index}"
        );
    }
    let unknown = Input::StepShown {
        run: RunId(1),
        scene: SceneId(9),
        index: 0,
    };
    assert!(drawing.handle(unknown).expect("stale").effects.is_empty());
}

#[test]
fn a_clear_during_a_step_write_drops_the_step_s_frames() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = drawing
        .handle(call(1, "c2", args("s", &[text("b", "y")])))
        .expect("a fresh call");
    let clear = drawing
        .handle(Input::Clear {
            scenes: vec![SceneId(1)],
        })
        .expect("a clear");
    assert_eq!(trace(&clear), ["revoke 1", "clear 1"]);
    let write = sent(&out).expect("the step").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert!(
        trace(&out).is_empty(),
        "nothing of the cleared scene goes out"
    );
    assert_eq!(error(&out.calls[0]), ErrorCode::SceneCleared);
    assert!(drawing.scenes.is_empty(), "the scene left");
    assert_eq!(drawing.global.usage().objects, 0);
}

#[test]
fn a_scene_names_its_thread_until_it_leaves() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    assert_eq!(drawing.thread(SceneId(1)), Some(THREAD));
    assert_eq!(drawing.thread(SceneId(9)), None);
    let out = drawing
        .handle(call(1, "c2", args("s", &[text("b", "y")])))
        .expect("a fresh call");
    drawing
        .handle(Input::ThreadDeleted { thread: THREAD })
        .expect("a deletion");
    assert!(drawing.live(SceneId(1)).is_none());
    assert_eq!(
        drawing.thread(SceneId(1)),
        Some(THREAD),
        "kept while its step is pending"
    );
    let write = sent(&out).expect("the step").1;
    reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert_eq!(drawing.thread(SceneId(1)), None);
}

#[test]
fn stops_past_the_queue_ceiling_behind_a_lookup_break_the_session() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let lookup = drawing
        .handle(call(1, "c", args("s", &[text("a", "x")])))
        .expect("a fresh call");
    let stop = || Input::StopScenes {
        run: RunId(1),
        cuts: SceneCuts::default(),
    };
    while drawing.queue.len() < MAX_QUEUED_JOBS {
        let out = drawing.handle(stop()).expect("a stop");
        assert!(
            out.effects.is_empty(),
            "under the ceiling a stop only queues"
        );
    }
    let out = drawing.handle(stop()).expect("a stop");
    assert_eq!(trace(&out), ["broken"]);
    assert_eq!(
        drawing.queue.len(),
        MAX_QUEUED_JOBS,
        "nothing past the ceiling is kept"
    );
    let write = sent(&lookup).expect("the lookup").1;
    let out = reply(&mut drawing, &lookup, Ok(store.answer(&write)));
    let out = store.settle(&mut drawing, out);
    let trace = trace(&out);
    let cuts = trace
        .iter()
        .filter(|line| line.starts_with("write cut"))
        .count();
    assert_eq!(
        cuts, 1,
        "the first stop cuts the unseen scene once, the rest find it gone"
    );
    assert!(trace.contains(&"clear 1".to_owned()));
    assert!(drawing.settled() && drawing.scenes.is_empty());
}

#[test]
fn shown_steps_and_a_stop_past_the_queue_ceiling_behind_a_step_break_the_session() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = drawing
        .handle(call(1, "c2", args("s", &[text("b", "y")])))
        .expect("a fresh call");
    assert!(matches!(sent(&out), Some((_, Write::Step { .. }))));
    let shown = || Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index: 0,
    };
    while drawing.queue.len() < MAX_QUEUED_JOBS - 1 {
        let out = drawing.handle(shown()).expect("a shown step");
        assert!(
            out.effects.is_empty(),
            "under the ceiling a shown step only queues"
        );
    }
    let out = drawing
        .handle(stop(1, 1, cut(0, Vec::new())))
        .expect("a stop");
    assert_eq!(
        trace(&out),
        ["broken"],
        "its cut fits, its unseen pass doesn't"
    );
    assert_eq!(drawing.queue.len(), MAX_QUEUED_JOBS);
    let out = drawing.handle(shown()).expect("a shown step");
    assert_eq!(trace(&out), ["broken"]);
    assert_eq!(drawing.queue.len(), MAX_QUEUED_JOBS);
}

#[test]
fn a_complete_call_past_its_record_cap_drains_its_prefix_and_alone_is_exhausted() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let steps = vec![text("a", "x"); MAX_CALL_RECORDS];
    let out = store.run(&mut drawing, call(1, "c1", args("s", &steps)));
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "begin 1",
            "step 1#0",
            "end 1 false"
        ],
        "the begin and every step before the cap ran; the repeats changed nothing"
    );
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].drawn, [wire("a")]);
    assert_eq!(out.calls[0].error, Some(CallError::Exhausted));
    assert!(drawing.settled());
    let out = store.run(&mut drawing, call(1, "c2", args("s", &[text("b", "y")])));
    assert!(out.calls[0].complete(), "the run's next call still draws");
    assert_eq!(trace(&out), ["write step 2", "step 1#1", "end 1 true"]);
}

#[test]
fn a_streamed_call_past_its_record_cap_is_exhausted_and_its_later_input_is_stale() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let mut out = store.run(&mut drawing, streamed("c", 0, &begin("s", "")));
    let last = u32::try_from(MAX_CALL_RECORDS).expect("few records");
    for index in 1..=last {
        let more = store.run(&mut drawing, streamed("c", index, &step(&text("a", "x"))));
        merge(&mut out, more);
    }
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "begin 1",
            "step 1#0",
            "end 1 false"
        ],
        "the record past the cap ends the call after the ones before it"
    );
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].drawn, [wire("a")]);
    assert_eq!(out.calls[0].error, Some(CallError::Exhausted));
    let end = streamed("c", last + 1, r#"{"end":{}}"#);
    assert_eq!(drawing.handle(end).err(), Some(Stale));
    let finished = Input::CallFinished {
        thread: THREAD,
        run: RunId(1),
        call: CallId::new("c").expect("a short ASCII id"),
        error: None,
    };
    assert_eq!(drawing.handle(finished).err(), Some(Stale));
    assert!(drawing.settled());
}

#[test]
fn a_streamed_call_s_repeated_finish_or_later_record_behind_a_write_is_stale() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let lookup = drawing
        .handle(streamed("c", 0, &begin("s", "")))
        .expect("a begin");
    drawing
        .handle(streamed("c", 1, r#"{"end":{}}"#))
        .expect("an end");
    let finished = || Input::CallFinished {
        thread: THREAD,
        run: RunId(1),
        call: CallId::new("c").expect("a short ASCII id"),
        error: None,
    };
    drawing.handle(finished()).expect("the first finish");
    let queued = drawing.queue.len();
    assert_eq!(drawing.handle(finished()).err(), Some(Stale));
    assert_eq!(
        drawing
            .handle(streamed("c", 2, &step(&text("a", "x"))))
            .err(),
        Some(Stale)
    );
    assert_eq!(drawing.queue.len(), queued, "neither was queued");
    let write = sent(&lookup).expect("the lookup").1;
    let out = reply(&mut drawing, &lookup, Ok(store.answer(&write)));
    assert_eq!(out.calls.len(), 1, "the call ends once");
    assert!(out.calls[0].complete());
    assert!(drawing.settled());
    assert_eq!(drawing.handle(finished()).err(), Some(Stale));
}

#[test]
fn two_streamed_calls_beginning_one_new_name_draw_in_one_scene() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let first = drawing
        .handle(streamed("c1", 0, &begin("s", "")))
        .expect("a begin");
    let second = drawing
        .handle(streamed("c2", 0, &begin("s", "")))
        .expect("a begin");
    assert!(sent(&second).is_none(), "one write at a time");
    let write = sent(&first).expect("c1's lookup").1;
    let out = reply(&mut drawing, &first, Ok(store.answer(&write)));
    let write = sent(&out).expect("c2's lookup").1;
    assert!(
        matches!(write, Write::Lookup { .. }),
        "the name isn't stored yet"
    );
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert!(sent(&out).is_none());
    let out = store.run(&mut drawing, streamed("c1", 1, &step(&text("a", "x"))));
    assert_eq!(trace(&out), ["write step 1", "begin 1", "step 1#0"]);
    let out = drawing
        .handle(streamed("c2", 1, &step(&text("b", "y"))))
        .expect("a step");
    let write = sent(&out).expect("c2's step").1;
    assert!(
        matches!(
            write,
            Write::Step {
                target: Target::Existing(SceneId(1)),
                revision: 2,
                ..
            }
        ),
        "c2 continues the scene c1 began"
    );
    store.settle(&mut drawing, out);
    assert_eq!(drawing.scenes.len(), 1);
    assert_eq!(store.scenes.len(), 1);
}

#[test]
fn a_streamed_call_beginning_a_new_name_with_another_header_is_refused_once_it_exists() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, streamed("c1", 0, &begin("s", "")));
    store.run(&mut drawing, streamed("c2", 0, &sized("s")));
    let out = store.run(&mut drawing, streamed("c1", 1, &step(&text("a", "x"))));
    assert_eq!(out.calls.len(), 1);
    assert_eq!(
        out.calls[0].call,
        CallId::new("c2").expect("a short ASCII id")
    );
    assert_eq!(error(&out.calls[0]), ErrorCode::HeaderMismatch);
    assert_eq!(
        drawing
            .handle(streamed("c2", 1, &step(&text("b", "y"))))
            .err(),
        Some(Stale)
    );
}

#[test]
fn a_deletion_during_a_new_scene_write_keeps_nothing_of_it() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let out = drawing
        .handle(call(1, "c", args("s", &[text("a", "x")])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    drawing.handle(closed(1)).expect("a close");
    let deleted = drawing
        .handle(Input::ThreadDeleted { thread: THREAD })
        .expect("a deletion");
    assert!(
        deleted.effects.is_empty(),
        "nothing of the thread was visible yet"
    );
    let write = sent(&out).expect("the step").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert!(trace(&out).is_empty(), "the late answer sends nothing");
    assert!(drawing.scenes.is_empty() && drawing.settled());
    assert_eq!(drawing.global.usage().scenes, 0);
}

#[test]
fn a_deletion_removes_the_thread_s_scenes_at_once() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    four(&mut store, &mut drawing);
    let out = store.run(&mut drawing, Input::ThreadDeleted { thread: THREAD });
    assert_eq!(trace(&out), ["revoke 1,2,3,4", "removed 1,2,3,4"]);
    assert!(drawing.scenes.is_empty());
}

#[test]
fn a_refused_fifth_scene_preserves_every_old_drawing() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    four(&mut store, &mut drawing);
    let forward = r#"{"id":"a","start":{"after":"later","ms":0}}"#.to_owned();
    let out = store.run(&mut drawing, call(1, "c5", args("s5", &[forward])));
    assert_eq!(trace(&out), ["lookup"]);
    assert_eq!(error(&out.calls[0]), ErrorCode::ForwardRef);
    let visible: Vec<u64> = drawing.scenes.iter().map(|held| held.scene.0).collect();
    assert_eq!(
        visible,
        [1, 2, 3, 4],
        "refusal never applies the selected eviction"
    );
    assert_eq!(store.cleared, []);
}

#[test]
fn three_dimensional_steps_wait_for_prepared_views() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let view = r#"{"id":"a","put":[{"view3d":{"id":"v"}}]}"#.to_owned();
    let out = store.run(&mut drawing, call(1, "c1", args("s1", &[view])));
    assert_eq!(error(&out.calls[0]), ErrorCode::Unsupported);
    assert!(drawing.scenes.is_empty());
    assert_eq!(drawing.global.usage().scenes, 0);
}

#[test]
fn a_failed_fifth_scene_write_preserves_its_victim_and_a_new_call_can_replace_it() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    four(&mut store, &mut drawing);
    let out = drawing
        .handle(call(1, "c5", args("s5", &[text("a", "x")])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    assert_eq!(
        trace(&out),
        ["write step 1"],
        "eviction and the new first step share one write"
    );
    let out = reply(&mut drawing, &out, Err(()));
    assert_eq!(out.calls[0].error, Some(CallError::Store));
    assert_eq!(
        drawing.scenes.len(),
        4,
        "a failed transaction changes no victim"
    );
    assert_eq!(store.cleared, []);
    let out = store.run(&mut drawing, call(1, "c6", args("s5", &[text("a", "x")])));
    assert_eq!(
        trace(&out),
        [
            "lookup",
            "write step 1",
            "revoke 1",
            "replaced 1 by 5",
            "begin 5",
            "step 5#0",
            "end 5 true"
        ]
    );
}

#[test]
fn unavailable_recovery_refuses_drawing_calls_without_writing_beside_unknown_scenes() {
    let mut drawing = Drawing::unavailable();
    let out = drawing
        .handle(call(1, "c", args("s", &[text("a", "x")])))
        .expect("call outcome");
    assert!(out.effects.is_empty());
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].error, Some(CallError::Store));
    assert!(!out.calls[0].complete());
    assert!(!drawing.available());
    assert!(drawing.settled());
}

#[test]
fn a_late_stop_cuts_steps_another_run_committed_to_its_scene() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let x = handle(&drawing, 1, "x");
    store.run(&mut drawing, closed(1));
    store.run(&mut drawing, call(2, "c2", args("s", &[text("b", "y")])));
    let out = store.run(&mut drawing, stop(1, 1, cut(1, vec![x])));
    assert_eq!(trace(&out), ["write cut 1@2:1", "cut 1@2:1"]);
    let state = &drawing.scenes[0].state;
    assert_eq!(state.committed(), 1);
    assert_eq!(state.handle(&wire("y")), None, "run 2's step was dropped");
}

#[test]
fn cuts_for_scenes_the_brain_no_longer_holds_are_stale() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    four(&mut store, &mut drawing);
    let x = handle(&drawing, 1, "x");
    store.run(&mut drawing, call(1, "c5", args("s5", &[text("a", "x")])));
    store.run(&mut drawing, closed(1));
    let out = store.run(&mut drawing, stop(2, 1, cut(1, vec![x])));
    assert!(
        out.effects.is_empty(),
        "an Esc racing the eviction ends nothing"
    );
}

#[test]
fn held_cuts_always_pass_the_scene_s_checks() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c", args("s", &[text("a", "x"), text("b", "y")])),
    );
    let (x, y) = (handle(&drawing, 1, "x"), handle(&drawing, 1, "y"));
    let out = store.run(&mut drawing, stop(1, 1, cut(9, Vec::new())));
    assert_eq!(trace(&out), ["broken"], "past the revision");
    let out = store.run(&mut drawing, stop(1, 1, cut(2, vec![x, y])));
    assert_eq!(
        trace(&out),
        ["cut 1@2:2"],
        "a cut that changes nothing is acknowledged"
    );
    let out = store.run(&mut drawing, stop(1, 1, cut(2, vec![x])));
    assert_eq!(trace(&out), ["write cut 1@2:2", "cut 1@2:2"]);
    let out = store.run(&mut drawing, stop(1, 1, cut(2, Vec::new())));
    assert_eq!(
        trace(&out),
        ["broken"],
        "a second effective cut at one revision"
    );
}

/// A cut to nothing of scene 1 in the store, then `leave` takes the scene, then the write fails.
fn a_failed_cut_of_a_scene_that_left(leave: Input) {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = drawing
        .handle(stop(1, 1, cut(0, Vec::new())))
        .expect("a stop");
    assert!(matches!(sent(&out), Some((_, Write::Cut { .. }))));
    drawing.handle(leave).expect("the scene leaves");
    let failed = reply(&mut drawing, &out, Err(()));
    assert!(failed.effects.is_empty(), "nothing of the scene goes out");
    assert!(
        !drawing.needs_retry(),
        "the scene's clear supersedes its cut"
    );
    assert!(drawing.settled() && drawing.scenes.is_empty());
    let out = drawing
        .handle(call(1, "c2", args("t", &[text("b", "y")])))
        .expect("a fresh call");
    assert!(
        matches!(sent(&out), Some((_, Write::Lookup { .. }))),
        "the next call writes"
    );
}

#[test]
fn a_failed_cut_of_a_deleted_scene_is_not_owed() {
    a_failed_cut_of_a_scene_that_left(Input::ThreadDeleted { thread: THREAD });
}

#[test]
fn a_failed_cut_of_a_cleared_scene_is_not_owed() {
    a_failed_cut_of_a_scene_that_left(Input::Clear {
        scenes: vec![SceneId(1)],
    });
}

#[test]
fn a_step_stored_into_a_scene_cleared_meanwhile_spends_the_run_s_step_budget() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let steps: Vec<String> = (1..MAX_DRAW_STEPS_PER_RUN)
        .map(|n| text(&format!("a{n}"), &format!("x{n}")))
        .collect();
    store.run(&mut drawing, call(1, "c1", args("s", &steps)));
    let out = drawing
        .handle(call(1, "c2", args("s", &[text("b", "y")])))
        .expect("a fresh call");
    let clearing = drawing
        .handle(Input::Clear {
            scenes: vec![SceneId(1)],
        })
        .expect("a clear");
    store.settle(&mut drawing, clearing);
    let write = sent(&out).expect("the last step the run may draw").1;
    reply(&mut drawing, &out, Ok(store.answer(&write)));
    let out = store.run(&mut drawing, call(1, "c3", args("t", &[text("c", "z")])));
    assert_eq!(error(&out.calls[0]), ErrorCode::OverBudget(Budget::Steps));
    assert_eq!(store.scenes.len(), 1, "the run drew its whole budget");
}

/// A streamed record that doesn't parse, so it's a rejected record.
const UNPARSED: &str = r#"{"step":{"id":"c","put":[],"colour":1}}"#;

/// Runs each streamed record through the store and gathers the calls that ended.
fn ends(store: &mut Store, drawing: &mut Drawing, records: &[(&str, u32, &str)]) -> Vec<CallEnd> {
    records
        .iter()
        .flat_map(|(call, index, record)| store.run(drawing, streamed(call, *index, record)).calls)
        .collect()
}

#[test]
fn rejections_in_calls_opened_before_any_failed_share_the_run_s_eight() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let names: Vec<(String, String)> = (1..=9)
        .map(|n| (format!("c{n}"), format!("s{n}")))
        .collect();
    let begins: Vec<String> = names.iter().map(|(_, scene)| begin(scene, "")).collect();
    let mut records: Vec<(&str, u32, &str)> = names
        .iter()
        .zip(&begins)
        .map(|((call, _), begin)| (call.as_str(), 0, begin.as_str()))
        .collect();
    records.extend(
        names[..8]
            .iter()
            .map(|(call, _)| (call.as_str(), 1, UNPARSED)),
    );
    let valid = step(&text("a", "x"));
    records.push(("c9", 1, valid.as_str()));
    let ends = ends(&mut store, &mut drawing, &records);
    let errors: Vec<Option<CallError>> = ends.iter().map(|end| end.error.clone()).collect();
    assert_eq!(errors.len(), 9, "every call ended");
    assert!(
        errors[..8]
            .iter()
            .all(|error| matches!(error, Some(CallError::Step(_)))),
        "eight rejected records"
    );
    assert_eq!(errors[8], Some(CallError::Exhausted));
    assert!(store.scenes.is_empty(), "the ninth call drew nothing");
}

#[test]
fn a_failure_in_one_open_repair_call_ends_the_other_s_round() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let begins = [begin("s1", ""), begin("s2", ""), begin("s3", "")];
    let valid = step(&text("a", "x"));
    let records = [
        ("c1", 0, begins[0].as_str()),
        ("c1", 1, UNPARSED),
        ("c2", 0, begins[1].as_str()),
        ("c3", 0, begins[2].as_str()),
        ("c2", 1, UNPARSED),
        ("c3", 1, valid.as_str()),
    ];
    let ends = ends(&mut store, &mut drawing, &records);
    let errors: Vec<Option<CallError>> = ends.iter().map(|end| end.error.clone()).collect();
    assert_eq!(errors.len(), 3, "every call ended");
    assert!(
        matches!(errors[0], Some(CallError::Step(_))),
        "the first failure"
    );
    assert!(
        matches!(errors[1], Some(CallError::Step(_))),
        "the repair round's failure"
    );
    assert_eq!(errors[2], Some(CallError::Exhausted));
    assert!(
        store.scenes.is_empty(),
        "the other repair call drew nothing"
    );
}

#[test]
fn clearing_everything_supersedes_a_failed_cut() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    store.run(&mut drawing, call(1, "c2", args("t", &[text("b", "y")])));
    let out = drawing
        .handle(stop(1, 1, cut(0, Vec::new())))
        .expect("a stop");
    reply(&mut drawing, &out, Err(()));
    assert!(drawing.needs_retry() && drawing.cut_pending() && !drawing.in_flight());
    let out = drawing.handle(Input::ClearVisible).expect("a quit");
    assert_eq!(
        trace(&out),
        ["clear 1,2"],
        "nothing waits behind the owed cut"
    );
    assert!(
        !drawing.needs_retry() && !drawing.cut_pending(),
        "the clear supersedes the cut"
    );
    assert!(drawing.settled() && drawing.scenes.is_empty());
}

#[test]
fn clearing_everything_during_a_first_step_write_clears_its_scene_once_it_lands() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = drawing
        .handle(call(1, "c2", args("t", &[text("b", "y")])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    let quit = drawing.handle(Input::ClearVisible).expect("a quit");
    assert_eq!(
        trace(&quit),
        ["clear 1"],
        "the visible scene clears at once"
    );
    let write = sent(&out).expect("the first step").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert_eq!(
        trace(&out),
        ["clear 2"],
        "the latched clear suppresses late painting and clears the allocated scene"
    );
    assert!(drawing.settled() && drawing.scenes.is_empty());
}

#[test]
fn a_scene_whose_log_doesn_t_replay_is_left_out_and_the_others_recover() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s1", &[text("a", "x")])));
    store.run(&mut drawing, call(1, "c2", args("s2", &[text("a", "x")])));
    let mut stored = store.visible();
    let repeated = stored[0].log[0].clone();
    stored[0].log.push(repeated);
    let (recovered, skipped) = Drawing::recover(stored);
    assert_eq!(skipped, [SceneId(1)]);
    let visible: Vec<SceneId> = recovered.scenes.iter().map(|held| held.scene).collect();
    assert_eq!(visible, [SceneId(2)]);
    assert_eq!(
        recovered.global.usage(),
        recovered.scenes[0].state.usage(),
        "the budget holds only the scene that recovered"
    );
}

/// A stored scene of 600 objects, its own cap, put by two steps of 300 texts.
fn wide(scene: u64) -> StoredScene {
    let Ok(Record::Begin(begin)) = parse_record(begin(&format!("s{scene}"), "").as_bytes()) else {
        panic!("a valid begin");
    };
    let log = (0..2_u32)
        .map(|revision| {
            let puts: Vec<String> = (0..300)
                .map(|n| format!(r#"{{"text":{{"id":"o{revision}_{n}","text":"","at":[0,0]}}}}"#))
                .collect();
            let record = format!(
                r#"{{"step":{{"id":"w{revision}","put":[{}]}}}}"#,
                puts.join(",")
            );
            let Ok(Record::Step(step)) = parse_record(record.as_bytes()) else {
                panic!("a valid step");
            };
            SceneEvent::Step {
                revision: revision + 1,
                step: Arc::new(step),
            }
        })
        .collect();
    StoredScene {
        scene: SceneId(scene),
        thread: THREAD,
        header: SceneHeader {
            begin,
            layout_version: 2,
        },
        log,
    }
}

#[test]
fn scenes_that_fit_alone_but_not_together_lose_the_oldest() {
    let stored: Vec<StoredScene> = (1..=3).map(wide).collect();
    let (recovered, skipped) = Drawing::recover(stored.clone());
    assert_eq!(skipped, [SceneId(1)], "1,800 objects pass the global 1,500");
    let visible: Vec<SceneId> = recovered.scenes.iter().map(|held| held.scene).collect();
    assert_eq!(visible, [SceneId(2), SceneId(3)]);
    assert_eq!(
        recovered.global.usage().objects,
        1_200,
        "one budget holds both"
    );
    for (held, stored) in recovered.scenes.iter().zip(&stored[1..]) {
        assert_eq!(
            held.state.recovery_log(),
            stored.log.as_slice(),
            "the log is kept whole"
        );
    }
}

#[test]
fn a_snapshot_leaves_out_a_scene_the_shell_would_refuse_and_keeps_the_rest() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s1", &[text("a", "x")])));
    let begin = drawing.scenes[0].state.header().clone();
    // A producer bug: a visible scene with no committed step has no valid descriptor.
    drawing.scenes.push(Held {
        scene: SceneId(9),
        thread: THREAD,
        layout_version: 2,
        state: SceneState::begin(begin).expect("a valid header"),
        committers: Vec::new(),
        refused_through: 0,
        gone: false,
        replaced: false,
        paint: None,
    });
    let snapshot = drawing.snapshot();
    assert_eq!(snapshot.skipped, [SceneId(9)]);
    let described: Vec<SceneId> = snapshot.scenes.iter().map(|scene| scene.scene).collect();
    assert_eq!(described, [SceneId(1)]);
    assert!(
        snapshot
            .chunks
            .iter()
            .all(|chunk| chunk.scene == SceneId(1)),
        "nothing of the refused scene is sent"
    );
    assert_eq!(snapshot.chunks.len(), 1);
    for id in 2..=5 {
        let mut begin = drawing.scenes[0].state.header().clone();
        begin.scene = wire(&format!("s{id}"));
        let state = SceneState::replay(begin, drawing.scenes[0].state.recovery_log())
            .expect("a valid scene");
        drawing.scenes.push(Held {
            scene: SceneId(id),
            thread: THREAD,
            layout_version: 2,
            state,
            committers: vec![None],
            refused_through: 0,
            gone: false,
            replaced: false,
            paint: None,
        });
    }
    let snapshot = drawing.snapshot();
    assert_eq!(
        snapshot
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(1), SceneId(2), SceneId(3), SceneId(4)]
    );
    assert_eq!(snapshot.skipped, [SceneId(9), SceneId(5)]);
    assert_eq!(snapshot.chunks.len(), 4);
}

#[test]
fn a_failed_cut_write_is_owed_and_holds_back_later_writes() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s", &[text("a", "x"), text("b", "y")])),
    );
    let x = handle(&drawing, 1, "x");
    let out = drawing.handle(stop(1, 1, cut(1, vec![x]))).expect("a stop");
    let failed = reply(&mut drawing, &out, Err(()));
    assert!(failed.effects.is_empty(), "the shell keeps its hold");
    assert!(drawing.needs_retry());
    let later = drawing
        .handle(call(2, "c2", args("s", &[text("c", "z")])))
        .expect("a fresh call");
    assert!(later.effects.is_empty(), "nothing overtakes the owed cut");

    let retried = Output {
        effects: drawing.retry(),
        calls: Vec::new(),
    };
    assert_eq!(trace(&retried), ["write cut 1@2:1"]);
    assert!(!drawing.needs_retry());
    let out = store.settle(&mut drawing, retried);
    assert_eq!(
        trace(&out),
        [
            "write cut 1@2:1",
            "cut 1@2:1",
            "write step 3",
            "step 1#2",
            "end 1 true"
        ]
    );
}

#[test]
fn a_failed_repair_round_spends_the_run_s_drawing_budget() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let forward = || r#"{"id":"a","start":{"after":"later","ms":0}}"#.to_owned();
    let first = store.run(&mut drawing, call(1, "c1", args("s1", &[forward()])));
    assert_eq!(error(&first.calls[0]), ErrorCode::ForwardRef);
    let repair = store.run(&mut drawing, call(1, "c2", args("s2", &[forward()])));
    assert_eq!(error(&repair.calls[0]), ErrorCode::ForwardRef);
    let spent = store.run(&mut drawing, call(1, "c3", args("s3", &[text("a", "x")])));
    assert!(spent.effects.is_empty(), "nothing runs");
    assert_eq!(spent.calls[0].error, Some(CallError::Exhausted));
    let next = store.run(&mut drawing, call(2, "c4", args("s3", &[text("a", "x")])));
    assert!(next.calls[0].complete(), "the next run has its own budget");
}

/// The closing phrases of a stopped result.
const RETRY: &str = "; drawn lists what this call added, and earlier steps stay as they were; correct this and call draw again";
const FULL: &str = "; no further step can be drawn in this turn";
const SPENT: &str = "; no further draw call will be accepted in this turn";

#[test]
fn a_refusal_says_whether_a_repair_follows_and_the_repair_reuses_the_unsaved_scene_name() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let refused =
        || r#"{"id":"a","put":[{"text":{"id":"x","text":"","at":[0,0],"size":0}}]}"#.to_owned();
    let stopped = "drawn: []; stopped: step a: put[0].text.size: scene.OutOfRange";
    let first = store.run(&mut drawing, call(1, "c1", args("sky", &[refused()])));
    assert_eq!(first.calls[0].tool_text(), format!("{stopped}{RETRY}"));
    assert!(store.scenes.is_empty(), "the refused call saved no scene");
    let repaired = store.run(&mut drawing, call(1, "c2", args("sky", &[text("a", "x")])));
    assert_eq!(repaired.calls[0].tool_text(), "drawn: [a]");
    assert_eq!(store.scenes.len(), 1);
    store.run(&mut drawing, call(2, "c3", args("sea", &[refused()])));
    let repair = store.run(&mut drawing, call(2, "c4", args("sea", &[refused()])));
    assert_eq!(repair.calls[0].tool_text(), format!("{stopped}{SPENT}"));
    let late = store.run(&mut drawing, call(2, "c5", args("sea", &[text("a", "x")])));
    assert_eq!(
        late.calls[0].tool_text(),
        format!("drawn: []; stopped: drawing.BudgetSpent{SPENT}")
    );
}

#[test]
fn a_replayed_step_before_a_refusal_stays_without_being_listed_as_drawn() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let refused = r#"{"id":"b","put":[{"text":{"id":"y","text":"","at":[0,0],"size":0}}]}"#;
    let out = store.run(
        &mut drawing,
        call(1, "c2", args("s", &[text("a", "x"), refused.to_owned()])),
    );
    assert_eq!(
        out.calls[0].tool_text(),
        format!("drawn: []; stopped: step b: put[0].text.size: scene.OutOfRange{RETRY}")
    );
    assert_eq!(drawing.scenes[0].state.committed(), 1, "a stays as it was");
}

#[test]
fn a_refusal_after_the_run_s_last_step_says_no_further_step_while_replays_still_run() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let steps = |prefix: &str, count: usize| -> Vec<String> {
        (1..=count)
            .map(|n| text(&format!("{prefix}{n}"), &format!("{prefix}x{n}")))
            .collect()
    };
    let refused =
        r#"{"id":"r","put":[{"text":{"id":"rx","text":"","at":[0,0],"size":0}}]}"#.to_owned();
    let below = MAX_DRAW_STEPS_PER_RUN - 1;
    store.run(&mut drawing, call(1, "c1", args("s", &steps("a", below))));
    let short = store.run(&mut drawing, call(1, "c2", args("s", &[refused])));
    assert_eq!(
        short.calls[0].tool_text(),
        format!("drawn: []; stopped: step r: put[0].text.size: scene.OutOfRange{RETRY}")
    );
    // Two scenes of 32, so only the run's budget is full, not either scene's own.
    let half = MAX_DRAW_STEPS_PER_RUN / 2;
    store.run(&mut drawing, call(2, "c3", args("t", &steps("b", half))));
    store.run(&mut drawing, call(2, "c4", args("u", &steps("c", half))));
    let full = store.run(&mut drawing, call(2, "c5", args("t", &[text("d", "dx")])));
    assert_eq!(error(&full.calls[0]), ErrorCode::OverBudget(Budget::Steps));
    assert_eq!(
        full.calls[0].tool_text(),
        format!("drawn: []; stopped: step d: $: scene.OverBudget{FULL}")
    );
    let replay = store.run(&mut drawing, call(2, "c6", args("t", &steps("b", 1))));
    assert_eq!(
        replay.calls[0].tool_text(),
        "drawn: []",
        "a replay still runs"
    );
}

#[test]
fn a_refusal_queued_behind_a_step_write_says_no_further_call_once_the_run_closes() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let unparsed = r#"{"id":"c","put":[],"colour":1}"#.to_owned();
    let out = drawing
        .handle(call(1, "c", args("s", &[text("a", "x"), unparsed])))
        .expect("a fresh call");
    let lookup = sent(&out).expect("a lookup").1;
    let out = reply(&mut drawing, &out, Ok(store.answer(&lookup)));
    let write = sent(&out).expect("the first step").1;
    let closed = drawing.handle(closed(1)).expect("a close");
    assert!(closed.calls.is_empty(), "the call waits for its write");
    let out = reply(&mut drawing, &out, Ok(store.answer(&write)));
    assert_eq!(
        out.calls[0].tool_text(),
        format!("drawn: [a]; stopped: step c: $: scene.UnknownField{SPENT}")
    );
}

#[test]
fn snapshots_carry_whole_logs_in_checked_chunks() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let long = "a".repeat(400);
    let steps: Vec<String> = (0..10)
        .map(|step| {
            let puts: Vec<String> = (0..10)
                .map(|object| {
                    format!(r#"{{"text":{{"id":"o{step}_{object}","text":"{long}","at":[0,0]}}}}"#)
                })
                .collect();
            format!(r#"{{"id":"s{step}","put":[{}]}}"#, puts.join(","))
        })
        .collect();
    store.run(&mut drawing, call(1, "c", args("s", &steps)));
    let snapshot = drawing.snapshot();
    assert!(snapshot.skipped.is_empty());
    assert_eq!(snapshot.scenes.len(), 1);
    assert_eq!(
        (
            snapshot.scenes[0].revision,
            snapshot.scenes[0].committed,
            snapshot.scenes[0].log_entries
        ),
        (10, 10, 10)
    );
    let shape: Vec<(u16, bool, bool)> = snapshot
        .chunks
        .iter()
        .map(|chunk| (chunk.ordinal, chunk.header.is_some(), chunk.last))
        .collect();
    assert_eq!(
        shape,
        [(0, true, false), (1, false, true)],
        "two 32 KiB chunks"
    );
    let header = snapshot.chunks[0]
        .header
        .as_ref()
        .expect("chunk 0's header");
    assert_eq!(header.begin, *drawing.scenes[0].state.header());
    let events: Vec<SceneEvent> = snapshot
        .chunks
        .iter()
        .flat_map(|chunk| chunk.events.iter().cloned())
        .collect();
    assert_eq!(events, drawing.scenes[0].state.recovery_log());
}

#[test]
fn debug_and_display_never_carry_drawing_content() {
    const SECRET: &str = "sentinel";
    let Record::Step(step) = parse_record(
        format!(r#"{{"step":{{"id":"{SECRET}","put":[{{"text":{{"id":"{SECRET}","text":"{SECRET}","at":[0,0]}}}}]}}}}"#)
            .as_bytes(),
    )
    .expect("a step") else {
        panic!("a step record");
    };
    let Record::Begin(begin) = parse_record(begin(SECRET, SECRET).as_bytes()).expect("a header")
    else {
        panic!("a begin record");
    };
    let header = SceneHeader {
        begin,
        layout_version: 2,
    };
    let writes = [
        Write::Lookup {
            thread: THREAD,
            name: wire(SECRET),
        },
        Write::Step {
            thread: THREAD,
            run: RunId(1),
            clear: Vec::new(),
            target: Target::New(header.clone()),
            revision: 1,
            layout_version: 2,
            step: Arc::new(step),
        },
    ];
    let end = CallEnd {
        run: RunId(1),
        call: CallId::new("c").expect("a short ASCII id"),
        scene: None,
        drawn: vec![wire(SECRET)],
        error: Some(CallError::Step(kumochi_scene::StepError {
            step: Some(wire(SECRET)),
            path: kumochi_scene::FieldPath::default(),
            code: ErrorCode::IdReused,
        })),
        remaining: Remaining::Steps,
        after_records: false,
        picture: None,
    };
    let stored = StoredScene {
        scene: SceneId(1),
        thread: THREAD,
        header,
        log: Vec::new(),
    };
    let mut deferred = Drawing {
        defer_preparation: true,
        ..Drawing::default()
    };
    let content = format!(
        r#"{{"id":"{SECRET}","put":[{{"text":{{"id":"{SECRET}","text":"{SECRET}","at":[0,0]}}}}]}}"#
    );
    let mut store = Store::default();
    store.run(&mut deferred, call(1, "b", args(SECRET, &[text("a", "x")])));
    let admitted = deferred
        .handle(call(1, "c", args(SECRET, &[content])))
        .expect("a call");
    let job = admitted
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PrepareDrawing(job) => Some(job.clone()),
            _ => None,
        })
        .expect("a step to lay out");
    let prepared = job.clone().run();
    assert!(matches!(
        prepared,
        Input::Prepared {
            outcome: crate::drawing::Prepared::Layout { .. },
            ..
        }
    ));
    let printed = [
        format!("{job:?}"),
        format!("{prepared:?}"),
        format!("{:?}", admitted.effects),
        format!("{writes:?}"),
        format!("{end:?}"),
        format!("{stored:?}"),
        format!("{:?}", call(1, "c", SECRET.as_bytes().to_vec())),
        end.error.as_ref().expect("an error").to_string(),
    ];
    for text in printed {
        assert!(!text.contains(SECRET), "leaked: {text}");
    }
    assert!(
        end.tool_text().contains(SECRET),
        "the model hears its own ids"
    );
}

#[test]
fn a_repeated_step_in_a_continuing_call_draws_nothing() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("s", &[text("a", "x")])));
    let out = store.run(&mut drawing, call(1, "c2", args("s", &[text("a", "x")])));
    assert_eq!(trace(&out), ["end 1 true"]);
    assert!(
        out.calls[0].drawn.is_empty(),
        "the duplicate committed nothing"
    );
    assert!(out.calls[0].complete(), "a retransmission isn't an error");
}

/// The digest line of `thread`, parsed.
fn digest(drawing: &Drawing, thread: ThreadId) -> serde_json::Value {
    serde_json::from_str(drawing.digest(thread).as_str()).expect("a JSON line")
}

/// `count` texts `{prefix}000` onwards, one step each 256.
fn texts(prefix: &str, from: usize, count: usize) -> Vec<String> {
    let ids: Vec<_> = (from..from + count)
        .map(|n| format!("{prefix}{n:03}"))
        .collect();
    ids.chunks(256)
        .map(|chunk| {
            let put: Vec<_> = chunk
                .iter()
                .map(|id| format!(r#"{{"text":{{"id":"{id}","text":"","at":[1,1]}}}}"#))
                .collect();
            format!(r#"{{"id":"{}","put":[{}]}}"#, chunk[0], put.join(","))
        })
        .collect()
}

#[test]
fn the_digest_lists_a_thread_s_scenes_with_their_objects_in_put_order() {
    let mut drawing = Drawing::default();
    assert_eq!(drawing.digest(THREAD).as_str(), "[]");
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("one", &[text("a", "z"), text("b", "m")])),
    );
    store.run(
        &mut drawing,
        Input::Call {
            thread: ThreadId(8),
            run: RunId(1),
            call: CallId::new("c2").expect("id"),
            args: args("theirs", &[text("a", "q")]),
        },
    );
    store.run(&mut drawing, call(1, "c3", args("two", &[text("a", "k")])));
    let begin = |scene: &str| serde_json::json!({"v":1,"scene":scene,"surface":"overlay","size":null,"title":null,"alt":"","others":"keep"});
    assert_eq!(
        digest(&drawing, THREAD),
        serde_json::json!([
            {"begin":begin("one"),"objects":[["z","text"],["m","text"]]},
            {"begin":begin("two"),"objects":[["k","text"]]},
        ])
    );
    assert_eq!(
        digest(&drawing, ThreadId(8)),
        serde_json::json!([{"begin":begin("theirs"),"objects":[["q","text"]]}])
    );
    assert_eq!(drawing.digest(ThreadId(9)).as_str(), "[]");
}

#[test]
fn the_digest_cap_lists_the_newest_objects_and_counts_the_rest() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    // Each pair is `["o000","text"]`, 15 bytes and a comma: 512 fill the cap exactly.
    store.run(
        &mut drawing,
        call(1, "c1", args("big", &texts("o", 0, 512))),
    );
    let line = digest(&drawing, THREAD);
    assert_eq!(line[0]["objects"].as_array().expect("pairs").len(), 512);
    assert_eq!(line[0]["objects"][0], serde_json::json!(["o000", "text"]));
    assert_eq!(line[0].get("more"), None);
    store.run(
        &mut drawing,
        call(1, "c2", args("big", &texts("o", 512, 1))),
    );
    let line = digest(&drawing, THREAD);
    assert_eq!(line[0]["objects"].as_array().expect("pairs").len(), 512);
    assert_eq!(line[0]["objects"][0], serde_json::json!(["o001", "text"]));
    assert_eq!(line[0]["objects"][511], serde_json::json!(["o512", "text"]));
    assert_eq!(line[0]["more"], 1);
    store.run(
        &mut drawing,
        call(1, "c3", args("small", &[text("a", "s")])),
    );
    let line = digest(&drawing, THREAD);
    assert_eq!(line[1]["objects"], serde_json::json!([["s", "text"]]));
    assert_eq!(line[1].get("more"), None);
    assert_eq!(line[0]["objects"].as_array().expect("pairs").len(), 511);
    assert_eq!(line[0]["objects"][0], serde_json::json!(["o002", "text"]));
    assert_eq!(line[0]["more"], 2);
}

#[test]
fn the_digest_takes_a_step_once_it_is_written_and_drops_a_cleared_scene_at_once() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(&mut drawing, call(1, "c1", args("one", &[text("a", "x")])));
    let before = drawing.digest(THREAD);
    let pending = drawing
        .handle(call(1, "c2", args("one", &[text("b", "y")])))
        .expect("a continuing call");
    let write = sent(&pending).expect("step write").1;
    assert_eq!(
        drawing.digest(THREAD),
        before,
        "a step in the store isn't listed"
    );
    let written = store.answer(&write);
    let out = reply(&mut drawing, &pending, Ok(written));
    store.settle(&mut drawing, out);
    assert_eq!(
        digest(&drawing, THREAD)[0]["objects"],
        serde_json::json!([["x", "text"], ["y", "text"]])
    );
    let pending = drawing
        .handle(call(1, "c3", args("one", &[text("c", "w")])))
        .expect("a continuing call");
    let write = sent(&pending).expect("step write").1;
    drawing
        .handle(Input::Clear {
            scenes: vec![SceneId(1)],
        })
        .expect("clear");
    assert_eq!(drawing.digest(THREAD).as_str(), "[]");
    let written = store.answer(&write);
    let out = reply(&mut drawing, &pending, Ok(written));
    store.settle(&mut drawing, out);
    assert_eq!(drawing.digest(THREAD).as_str(), "[]");
}

#[test]
fn the_digest_debug_shows_no_header_text() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let input = call(
        1,
        "c1",
        envelope(&begin("one", "PRIVATE_ALT_MARKER"), &[text("a", "x")]),
    );
    store.run(&mut drawing, input);
    let digest = drawing.digest(THREAD);
    assert!(digest.as_str().contains("PRIVATE_ALT_MARKER"));
    let debug = format!("{digest:?}");
    assert_eq!(
        debug,
        format!("Digest {{ bytes: {} }}", digest.as_str().len())
    );
    assert!(!debug.contains(&format!("{:?}", b"PRIVATE_ALT_MARKER")));
}

#[test]
fn the_digest_follows_a_remove_a_stop_cut_and_a_deleted_thread() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let removal = r#"{"id":"c","remove":["x"]}"#.to_owned();
    store.run(
        &mut drawing,
        call(
            1,
            "c1",
            args("one", &[text("a", "x"), text("b", "y"), removal]),
        ),
    );
    store.run(
        &mut drawing,
        call(1, "c2", args("two", &[text("a", "p"), text("b", "q")])),
    );
    // Scene one is cut too, or the Stop would clear it as never shown.
    let cuts = vec![
        SceneCut {
            scene: SceneId(1),
            cut: cut(3, vec![handle(&drawing, 1, "y")]),
        },
        SceneCut {
            scene: SceneId(2),
            cut: cut(1, vec![handle(&drawing, 2, "p")]),
        },
    ];
    let cuts = SceneCuts::new(cuts).expect("two scenes");
    store.run(
        &mut drawing,
        Input::StopScenes {
            run: RunId(1),
            cuts,
        },
    );
    let begin = |scene: &str| serde_json::json!({"v":1,"scene":scene,"surface":"overlay","size":null,"title":null,"alt":"","others":"keep"});
    assert_eq!(
        digest(&drawing, THREAD),
        serde_json::json!([
            {"begin":begin("one"),"objects":[["y","text"]]},
            {"begin":begin("two"),"objects":[["p","text"]]},
        ])
    );
    store.run(&mut drawing, Input::ThreadDeleted { thread: THREAD });
    assert_eq!(drawing.digest(THREAD).as_str(), "[]");
}
