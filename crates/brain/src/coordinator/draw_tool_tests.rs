use kumochi_protocol::message::{DataToShell, ThreadId};
use serde_json::{Value, json};

use super::{R, T, fresh};
use crate::context::{Built, Origin};
use crate::coordinator::{BuildKey, ContextBuilt, Coordinator, Effect, Input, StoreOp};
use crate::dispatch::{Caller, Committed, Dispatch, ToolError, ToolOutput};
use crate::engines::{CallId, Finish, TurnEvent};
use crate::store::Store;
use crate::test_support::Scratch;

fn id(name: &str) -> CallId {
    CallId::new(name).expect("ASCII fixture")
}

fn offered(sources: Vec<ThreadId>) -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("turn");
    let effects = core
        .handle(Input::TurnStored { run: R })
        .expect("turn stored");
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("build");
    core.handle(built(key, sources)).expect("offered");
    let crate::coordinator::Availability::Ready(ready) = &mut core.availability else {
        panic!("ready");
    };
    ready.trusted_draw = false;
    core
}

fn built(key: BuildKey, sources: Vec<ThreadId>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: None,
        result: Ok(Built {
            request: crate::context::ModelRequest {
                tools: crate::context::tools(Origin::Task(T)),
                ..super::request()
            },
            sources,
        }),
    })
}

fn args(third: Option<Value>) -> Vec<u8> {
    let mut records = vec![
        json!({"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"}}),
        json!({"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}}),
        json!({"step":{"id":"s2","put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10]}}]}}),
    ];
    if let Some(step) = third {
        records.push(json!({"step":step}));
    }
    records.push(json!({"end":{}}));
    serde_json::to_vec(&json!({"records":records})).expect("fixture")
}

fn store(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store.main_thread().expect("main");
    store.begin_turn(T, Some("test"), R, "turn").expect("turn");
    store
}

fn pending(effects: &[Effect]) -> Option<(u64, crate::drawing::Write)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
        _ => None,
    })
}

fn write(core: &mut Coordinator, store: &mut Store, effects: &[Effect]) -> Vec<Effect> {
    let (serial, write) = pending(effects).expect("pending drawing write");
    let reply = store.drawing_write(&write).expect("durable write");
    core.handle(Input::DrawingWritten {
        serial,
        reply: Ok(reply),
    })
    .expect("acknowledgement")
}

fn call(core: &mut Coordinator, name: &str, args: &[u8]) -> (Dispatch, Vec<Effect>) {
    let mut effects = Vec::new();
    let dispatch = core.tool_call(
        Caller::Model {
            run: R,
            call: id(name),
        },
        crate::engines::ToolName::Draw,
        args,
        &mut effects,
    );
    (dispatch, effects)
}

#[test]
fn unavailable_drawing_returns_a_fixed_durable_refusal_without_failing_the_run() {
    let scratch = Scratch::new("draw-unavailable");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    assert!(
        core.restore_drawings(Err(crate::store::StoreError::Closed))
            .is_empty()
    );
    let (dispatch, effects) = call(&mut core, "unavailable", &args(None));
    assert_eq!(dispatch, Dispatch::Committing);
    let [Effect::Store(StoreOp::CommitCall(result))] = effects.as_slice() else {
        panic!("only the refusal result may be written");
    };
    assert_eq!(result.output, ToolOutput::Error(ToolError::NotHere));
    assert_eq!(result.write, crate::dispatch::LocalWrite::None);
    let committed = store.commit_call(result).expect("durable refusal");
    assert_eq!(committed, Committed::Done);
    core.handle(Input::CallCommitted {
        key: result.key.clone(),
        outcome: committed,
        then: None,
    })
    .expect("ack");
    assert!(
        core.current
            .as_ref()
            .expect("run stays active")
            .ending
            .is_none()
    );
    assert!(core.current.as_ref().expect("run").draw_calls.is_empty());
    core.drawing = crate::drawing::Drawing::default();
    assert_eq!(
        call(&mut core, "unavailable", &args(None)),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::NotHere)),
            Vec::new()
        )
    );
}

#[test]
fn an_offered_draw_uses_real_scene_commits_then_waits_for_its_result_write() {
    let scratch = Scratch::new("draw-tool-order");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let args = String::from_utf8(args(None)).expect("JSON");
    for event in [
        TurnEvent::ToolCallStarted {
            call: id("model"),
            name: crate::engines::ToolName::Draw,
        },
        TurnEvent::ToolArgsDelta {
            call: id("model"),
            delta: args.clone(),
        },
        TurnEvent::ToolCallReady {
            call: id("model"),
            args,
        },
    ] {
        assert_eq!(core.handle(Input::Engine { run: R, event }), Ok(Vec::new()));
    }
    let lookup = core
        .handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("round done");
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    assert_eq!(
        second
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Data(DataToShell::SceneStepCommit { index: 0, .. })
            ))
            .count(),
        1
    );
    assert!(!second.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::CommitCall(_) | StoreOp::RoundItems(_))
    )));
    let last = write(&mut core, &mut store, &second);
    assert_eq!(
        last.iter()
            .filter(|effect| matches!(
                effect,
                Effect::Data(DataToShell::SceneStepCommit { index: 1, .. })
            ))
            .count(),
        1
    );
    let result = last
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call.clone()),
            _ => None,
        })
        .expect("actual result waits for ledger write");
    assert_eq!(result.output, ToolOutput::Text("drawn: [s1, s2]".into()));
    assert!(
        !last
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
    );
    let ack = Input::CallCommitted {
        key: result.key,
        outcome: Committed::Done,
        then: None,
    };
    let settled = core.handle(ack.clone()).expect("ledger acknowledgement");
    let round = settled
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => Some(round),
            _ => None,
        })
        .expect("one answered round");
    assert_eq!(
        round.calls,
        [(id("model"), ToolOutput::Text("drawn: [s1, s2]".into()))]
    );
    assert_eq!(core.handle(ack), Ok(Vec::new()));
    assert_eq!(core.current.as_ref().expect("run").steps, 0);
}

#[test]
fn a_typo_in_the_third_step_has_zero_durable_scene_writes_and_a_typed_refusal() {
    preflight_failure(
        json!({"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[0,0,10,10],"typo":true}}]}),
        "scene.UnknownField",
    );
}

#[test]
fn every_third_step_preflight_failure_has_zero_durable_scene_writes() {
    for (third, code) in [
        (
            json!({"id":"s3","put":[{"view3d":{"id":"v"}}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","put":[{"solid":{"in":"v","id":"c","shape":"sphere","r":1}}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","put":[{"anim":{"target":"a","track":"pos3","keys":[]}}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","put":[{"anim":{"target":"a","track":"rot3","keys":[]}}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","put":[{"anim":{"target":"a","track":"pos","keys":[[0,[1,2,3]]]}}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","set":[{"target":"a","info":"solid"}]}),
            "scene.Unsupported",
        ),
        (
            json!({"id":"s3","set":[{"target":"a","info":null}]}),
            "scene.Unsupported",
        ),
    ] {
        preflight_failure(third, code);
    }
}

fn preflight_failure(third: Value, expected_error: &str) {
    let scratch = Scratch::new("draw-tool-preflight");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let (dispatch, mut effects) = call(&mut core, "bad", &args(Some(third)));
    assert_eq!(dispatch, Dispatch::Committing);
    let mut commits = 0;
    while pending(&effects).is_some() {
        effects = write(&mut core, &mut store, &effects);
        commits += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Data(DataToShell::SceneStepCommit { .. })))
            .count();
    }
    assert_eq!(commits, 0);
    assert_eq!(store.drawing_scenes().expect("no scene").scenes.len(), 0);
    let run = core.current.as_ref().expect("current");
    assert_eq!(run.answered.len(), 1);
    let ToolOutput::Text(text) = &run.answered[0].1 else {
        panic!("typed refusal, not bare Invalid");
    };
    assert!(text.starts_with("drawn: []; stopped: step s3: "));
    assert!(text.ends_with(expected_error));
    let expected = ToolOutput::Text(text.clone());
    assert_eq!(
        call(&mut core, "bad", &args(None)).0,
        Dispatch::Answered(expected)
    );
}

#[test]
fn a_preflight_refusal_spends_the_existing_repair_budget() {
    let mut core = offered(vec![T]);
    let bad = args(Some(json!({"id":"s3","typo":true})));
    for name in ["first", "repair", "spent"] {
        assert_eq!(
            call(&mut core, name, &bad),
            (Dispatch::Committing, Vec::new())
        );
    }
    let answers = &core.current.as_ref().expect("run").answered;
    assert_eq!(answers.len(), 3);
    assert_eq!(
        answers[2].1,
        ToolOutput::Text("drawn: []; stopped: drawing.BudgetSpent".into())
    );
}

#[test]
fn retries_join_then_replay_and_changed_arguments_never_redraw() {
    let scratch = Scratch::new("draw-tool-retry");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let args = args(None);
    let (_, lookup) = call(&mut core, "same", &args);
    assert_eq!(
        call(&mut core, "same", &args),
        (Dispatch::Joined, Vec::new())
    );
    assert_eq!(
        call(&mut core, "same", br#"{"records":[]}"#),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::Reused)),
            Vec::new()
        )
    );
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    let last = write(&mut core, &mut store, &second);
    let result = last
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call),
            _ => None,
        })
        .expect("ledger write");
    core.handle(Input::CallCommitted {
        key: result.key.clone(),
        outcome: Committed::Done,
        then: None,
    })
    .expect("ack");
    assert_eq!(
        call(&mut core, "same", &args),
        (
            Dispatch::Answered(ToolOutput::Text("drawn: [s1, s2]".into())),
            Vec::new()
        )
    );
    assert_eq!(core.current.as_ref().expect("run").answered.len(), 2);
    assert_eq!(
        store.drawing_scenes().expect("one scene").scenes[0]
            .log
            .len(),
        2
    );
}

#[test]
fn model_and_mcp_ids_and_two_launches_never_share_a_reducer_call() {
    let mut core = offered(vec![T]);
    assert_eq!(call(&mut core, "same", &args(None)).0, Dispatch::Committing);
    for launch in [crate::bridge::LaunchId(1), crate::bridge::LaunchId(2)] {
        core.current.as_mut().expect("run").mcp = Some(launch);
        let mut effects = Vec::new();
        assert_eq!(
            core.tool_call(
                Caller::Mcp {
                    run: R,
                    launch,
                    key: crate::bridge::CallKey("same".into())
                },
                crate::engines::ToolName::Draw,
                &args(None),
                &mut effects
            ),
            Dispatch::Committing
        );
    }
    let calls = &core.current.as_ref().expect("run").draw_calls;
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .map(|(call, _)| call.as_str())
            .collect::<Vec<_>>(),
        ["draw-0", "draw-1", "draw-2"]
    );
    assert_eq!(
        calls.iter().map(|(_, key)| key.route).collect::<Vec<_>>(),
        [
            crate::dispatch::Route::Model,
            crate::dispatch::Route::Mcp(crate::bridge::LaunchId(1)),
            crate::dispatch::Route::Mcp(crate::bridge::LaunchId(2)),
        ]
    );
}

#[test]
fn source_deletion_rebuilds_an_items_only_round_held_by_drawing_without_finishing_it() {
    let scratch = Scratch::new("draw-tool-voided-finishing");
    let mut store = store(&scratch);
    let source = ThreadId(2);
    let mut core = offered(vec![T, source]);
    let lookup = core
        .handle(Input::Drawing(crate::drawing::Input::Call {
            thread: T,
            run: R,
            call: id("fixture"),
            args: args(None),
        }))
        .expect("pending drawing");
    core.handle(Input::Engine {
        run: R,
        event: TurnEvent::OutputItem {
            index: 0,
            item: crate::engines::Opaque::new(br#"{"type":"reasoning","id":"old"}"#.to_vec())
                .expect("small item"),
        },
    })
    .expect("native item");
    assert_eq!(
        core.handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed)
        }),
        Ok(Vec::new())
    );
    assert!(core.current.as_ref().expect("run").finishing);
    core.handle(Input::Delete {
        thread: source,
        after_turns: 1,
        resend: false,
    })
    .expect("delete accepted");
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    let last = write(&mut core, &mut store, &second);
    assert!(!core.current.as_ref().expect("run").finishing);
    assert!(
        !last
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
    );
    let rebuild = core
        .handle(Input::Deleted {
            thread: source,
            outcome: kumochi_protocol::message::Deleted::Hidden,
            last_run: None,
        })
        .expect("deleted");
    let key = rebuild
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("replacement build");
    let crate::coordinator::Availability::Ready(ready) = &mut core.availability else {
        panic!("ready");
    };
    ready.trusted_draw = true;
    core.handle(built(key, vec![T]))
        .expect("replacement admitted");
    assert_eq!(
        core.handle(Input::Engine {
            run: R,
            event: TurnEvent::TextDelta("replacement".into())
        }),
        Ok(vec![
            super::state(R, T, kumochi_protocol::message::Phase::Answering),
            super::text("replacement")
        ])
    );
    let completed = core
        .handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("replacement finishes");
    assert_eq!(
        completed
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Store(StoreOp::EndRun {
                    end: crate::store::RunEnd::Done,
                    ..
                })
            ))
            .count(),
        1
    );
}

#[test]
fn a_late_drawing_result_cannot_answer_a_replacement_round_twice() {
    let scratch = Scratch::new("draw-tool-replacement");
    let mut store = store(&scratch);
    let source = ThreadId(2);
    let mut core = offered(vec![T, source]);
    for event in [
        TurnEvent::ToolCallStarted {
            call: id("old"),
            name: crate::engines::ToolName::Draw,
        },
        TurnEvent::ToolCallReady {
            call: id("old"),
            args: String::from_utf8(args(None)).expect("JSON"),
        },
    ] {
        core.handle(Input::Engine { run: R, event })
            .expect("call event");
    }
    let lookup = core
        .handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("pending drawing");
    core.handle(Input::Delete {
        thread: source,
        after_turns: 1,
        resend: false,
    })
    .expect("delete accepted");
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    let last = write(&mut core, &mut store, &second);
    let old = last
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call),
            _ => None,
        })
        .expect("old result write");
    let ack = Input::CallCommitted {
        key: old.key.clone(),
        outcome: Committed::Done,
        then: None,
    };
    let dropped = core.handle(ack.clone()).expect("old result acknowledged");
    assert!(
        !dropped
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
    );
    assert!(!core.current.as_ref().expect("run").finishing);
    let rebuild = core
        .handle(Input::Deleted {
            thread: source,
            outcome: kumochi_protocol::message::Deleted::Hidden,
            last_run: None,
        })
        .expect("deleted");
    let key = rebuild
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("replacement build");
    let crate::coordinator::Availability::Ready(ready) = &mut core.availability else {
        panic!("ready");
    };
    ready.trusted_draw = true;
    core.handle(built(key, vec![T]))
        .expect("replacement admitted");
    assert_eq!(core.handle(ack.clone()), Ok(Vec::new()));
    core.handle(Input::Engine {
        run: R,
        event: TurnEvent::TextDelta("replacement".into()),
    })
    .expect("replacement accepts text");
    assert_eq!(core.handle(ack), Ok(Vec::new()));
    let completed = core
        .handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("replacement completes");
    assert_eq!(
        completed
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Store(StoreOp::EndRun {
                    end: crate::store::RunEnd::Done,
                    ..
                })
            ))
            .count(),
        1
    );
}

#[test]
fn stop_fences_the_pending_draw_result_and_a_store_failure_never_redraws() {
    for stop in [true, false] {
        let mut core = offered(vec![T]);
        let (_, lookup) = call(&mut core, "draw", &args(None));
        let (serial, _) = pending(&lookup).expect("lookup pending");
        if stop {
            core.handle(Input::Stop {
                run: R,
                revealed: Vec::new(),
            })
            .expect("stop");
        }
        let effects = core
            .handle(Input::DrawingWritten {
                serial,
                reply: Err(()),
            })
            .expect("failed lookup");
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::CommitCall(_) | StoreOp::Drawing { .. })
        )));
        if !stop {
            assert_eq!(
                effects
                    .iter()
                    .filter(|effect| matches!(
                        effect,
                        Effect::Store(StoreOp::EndRun {
                            end: crate::store::RunEnd::Failed(crate::store::Failure::Store),
                            ..
                        })
                    ))
                    .count(),
                1
            );
        }
        assert_eq!(
            call(&mut core, "draw", &args(None)),
            (
                Dispatch::Answered(ToolOutput::Error(ToolError::Ended)),
                Vec::new()
            )
        );
    }
}

#[test]
fn semantic_admission_returns_only_the_durable_prefix_and_exact_error() {
    let scratch = Scratch::new("draw-tool-partial");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let (_, lookup) = call(
        &mut core,
        "partial",
        &args(Some(json!({"id":"s3","remove":["missing"]}))),
    );
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    let last = write(&mut core, &mut store, &second);
    let result = last
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call),
            _ => None,
        })
        .expect("honest partial result");
    assert_eq!(
        result.output,
        ToolOutput::Text("drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef".into())
    );
    assert_eq!(
        store.drawing_scenes().expect("committed prefix").scenes[0]
            .log
            .len(),
        2
    );
}

#[test]
fn the_complete_draw_cap_is_sixteen_kib_and_one_over_stays_refused() {
    let mut core = offered(vec![T]);
    let mut exact = args(None);
    exact.resize(16 * 1024, b' ');
    let (dispatch, effects) = call(&mut core, "exact", &exact);
    assert_eq!(dispatch, Dispatch::Committing);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. })))
            .count(),
        1
    );
    exact.push(b' ');
    assert_eq!(
        call(&mut core, "over", &exact),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::TooLarge)),
            Vec::new()
        )
    );
    assert_eq!(
        call(&mut core, "over", &args(None)),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::TooLarge)),
            Vec::new()
        )
    );
}
