use kumochi_protocol::message::{AssistantName, DataToShell, ThreadId};
use serde_json::{Value, json};

use super::{R, T, fresh};
use crate::context::{Built, ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::{BuildKey, ContextBuilt, Coordinator, Effect, Input, StoreOp};
use crate::dispatch::{Caller, Committed, Dispatch, ToolError, ToolOutput};
use crate::engines::{CallId, Finish, TurnEvent};
use crate::store::Store;
use crate::test_support::Scratch;

#[path = "provider_draw_tests.rs"]
mod provider_stream;

const RETRY: &str = "; drawn lists what this call added, and earlier steps stay as they were; correct this and call draw again";
const DRAWN_RETRY: &str = "; the steps in drawn are on screen, and earlier steps stay as they were; correct this by drawing only what is missing, with new step and object ids, and call draw again";
const FULL: &str = "; no further step can be drawn in this turn";
const SPENT: &str = "; no further draw call will be accepted in this turn";

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
    ready.images = vec![kumochi_protocol::message::ModelSlug::new("gpt-fixture").expect("slug")];
    core
}

fn built(key: BuildKey, sources: Vec<ThreadId>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: None,
            connection: ConnectionTag(0),
        }),
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
    let effects = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    super::prepared(core, effects)
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

fn admitted_request(
    core: &mut Coordinator,
    effects: &[Effect],
    sources: Vec<ThreadId>,
) -> (BuildKey, crate::engines::RunRequest) {
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("an actual context build");
    let effects = core.handle(built(key, sources)).expect("admitted");
    let [Effect::StartEngine { request }] = effects.as_slice() else {
        panic!("admission starts one engine");
    };
    (key, request.clone())
}

async fn pumped(request: crate::engines::RunRequest, key: BuildKey) -> Vec<Input> {
    let fixture = crate::engines::fixture::Fixture::parse("0 text before\n0 text after")
        .expect("text-only fixture");
    let (inputs, mut received) = tokio::sync::mpsc::channel(4);
    let gate = crate::engines::SendGate::closed(request.key);
    let _cancel = crate::engines::start(&fixture, request, inputs, gate);
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(input) = received.recv().await {
            events.push(input);
        }
        events
    })
    .await
    .expect("the fixture pump closes");
    assert_eq!(
        events,
        [
            Input::Engine {
                key,
                event: TurnEvent::TextDelta("before".into()),
            },
            Input::Engine {
                key,
                event: TurnEvent::TextDelta("after".into()),
            },
            Input::Engine {
                key,
                event: TurnEvent::Finished(Finish::Completed),
            },
        ]
    );
    events
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one generation race across the replaced and current builds"
)]
async fn replaced_build_events_cannot_change_or_finish_the_current_round() {
    let scratch = Scratch::new("engine-build-admission");
    let mut store = store(&scratch);
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("turn");
    let building = core.handle(Input::TurnStored { run: R }).expect("stored");
    let source = ThreadId(8);
    let (first, first_request) = admitted_request(&mut core, &building, vec![T, source]);
    for event in [
        TurnEvent::ToolCallStarted {
            call: id("advance"),
            name: crate::engines::ToolName::Unknown,
        },
        TurnEvent::ToolCallReady {
            call: id("advance"),
            args: "{}".into(),
        },
    ] {
        assert_eq!(
            core.handle(Input::Engine { key: first, event }),
            Ok(Vec::new())
        );
    }
    let effects = core
        .handle(Input::Engine {
            key: first,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("the first round completes its call");
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("one round write");
    };
    store
        .append_round(
            (round.thread, round.run),
            round.connection,
            round.text.as_deref(),
            &round.items,
            &round.calls,
        )
        .expect("durable round");
    let building = core
        .handle(Input::RoundStored {
            run: R,
            round: round.round,
        })
        .expect("the stored call starts another round");
    let (old, old_request) = admitted_request(&mut core, &building, vec![T, source]);
    assert_eq!(old.run, first.run);
    assert_eq!(old.round, first.round + 1);
    for input in pumped(first_request, first).await {
        assert_eq!(core.handle(input), Err(super::Stale));
    }
    let mut queued = pumped(old_request, old).await.into_iter();
    let effects = core
        .handle(queued.next().expect("first text"))
        .expect("current");
    let text: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::AssistantText { run, delta }) => Some((*run, delta.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(text, [(R, "before")]);
    let effects = core
        .handle(Input::Delete {
            thread: source,
            after_turns: 1,
            resend: false,
        })
        .expect("delete accepted");
    assert!(effects.contains(&Effect::Store(StoreOp::Delete { thread: source })));
    let effects = core
        .handle(Input::SendReady {
            key: crate::coordinator::SendKey::Build(old),
            hold: None,
        })
        .expect("the exposed build waits for deletion");
    assert!(effects.contains(&Effect::Rebuild(crate::coordinator::SendKey::Build(old))));
    assert_eq!(super::engine_key(&core, R), old);
    assert!(!core.current.as_ref().expect("retained run").started());
    let before = format!("{:?}", core.current);
    for input in queued {
        assert_eq!(core.handle(input), Err(super::Stale));
        assert_eq!(format!("{:?}", core.current), before);
    }
    let building = core
        .handle(Input::Deleted {
            thread: source,
            outcome: kumochi_protocol::message::Deleted::Hidden,
            last_run: None,
        })
        .expect("deletion releases the replacement build");
    let (new, new_request) = admitted_request(&mut core, &building, vec![T]);
    assert_eq!(new.run, old.run);
    assert_eq!(new.round, old.round);
    assert_ne!(new.attempt, old.attempt);
    for key in [first, old] {
        for event in [
            TurnEvent::TextDelta("stale-prefix".into()),
            TurnEvent::ToolCallStarted {
                call: id("stale"),
                name: crate::engines::ToolName::Draw,
            },
            TurnEvent::Finished(Finish::Completed),
            TurnEvent::Finished(Finish::Truncated),
            TurnEvent::Finished(Finish::Failed(crate::engines::EngineFailure::Provider)),
        ] {
            assert_eq!(core.handle(Input::Engine { key, event }), Err(super::Stale));
            assert_eq!(super::engine_key(&core, R), new);
            assert!(
                core.current
                    .as_ref()
                    .expect("replacement")
                    .draw_calls
                    .is_empty()
            );
        }
    }
    let mut queued = pumped(new_request, new).await.into_iter();
    for expected in ["before", "after"] {
        let effects = core
            .handle(queued.next().expect("current text"))
            .expect("the actual replacement key is accepted");
        let text: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Data(DataToShell::AssistantText { run, delta }) => {
                    Some((*run, delta.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(text, [(R, expected)]);
    }
    core.handle(queued.next().expect("current finish"))
        .expect("the actual replacement can finish");
    assert_eq!(core.current_run(), None);
    assert_eq!(core.ending_runs(), 1);
}

#[test]
fn the_default_draw_owner_cap_refuses_a_seventeenth_call_without_a_durable_result() {
    assert_draw_owner_boundary(offered(vec![T]), 16);
}

#[test]
fn a_lowered_draw_owner_cap_refuses_a_fourth_call_without_a_durable_result() {
    let mut core = offered(vec![T]);
    core.tuning.calls = 3;
    assert_draw_owner_boundary(core, 3);
}

fn assert_draw_owner_boundary(mut core: Coordinator, allowed: u32) {
    use crate::dispatch::{CallInput, DrawAction};
    use crate::engines::ToolName;

    let scratch = Scratch::new("draw-owner-cap");
    let mut store = store(&scratch);
    let start = |core: &mut Coordinator, name: &str| {
        let caller = Caller::Model {
            run: R,
            call: id(name),
        };
        let mut effects = Vec::new();
        let status = core.tool_input(
            caller,
            ToolName::Draw,
            CallInput::Draw(DrawAction::Start),
            &mut effects,
        );
        assert_eq!(effects, []);
        status
    };
    for index in 1..allowed {
        assert_eq!(
            start(&mut core, &format!("stream-{index}")),
            Dispatch::Committing
        );
    }
    let valid = args(None);
    let (dispatch, drawing) = call(&mut core, "last-allowed", &valid);
    assert_eq!(dispatch, Dispatch::Committing);
    let drawn = settle(&mut core, &mut store, drawing);
    assert_eq!(
        answer(&mut core, &mut store, &drawn),
        ToolOutput::Text("drawn: [s1, s2]".into())
    );

    let (dispatch, drawing) = call(&mut core, "over-cap", &valid);
    assert_eq!(dispatch, Dispatch::Committing);
    assert_eq!(drawing, []);
    let refused = Dispatch::Answered(ToolOutput::Error(ToolError::TooLarge));
    assert_eq!(
        call(&mut core, "over-cap", b""),
        (refused.clone(), Vec::new())
    );
    assert_eq!(start(&mut core, "over-cap-stream"), refused);
    assert_eq!(
        call(&mut core, "over-cap-stream", &valid),
        (refused, Vec::new())
    );
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
#[expect(
    clippy::too_many_lines,
    reason = "the scene commits and the result write share one schedule"
)]
fn an_offered_draw_uses_real_scene_commits_then_waits_for_its_result_write() {
    let scratch = Scratch::new("draw-tool-order");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let args = String::from_utf8(args(None)).expect("JSON");
    let key = super::engine_key(&core, R);
    assert_eq!(
        core.handle(Input::Engine {
            key,
            event: TurnEvent::ToolCallStarted {
                call: id("model"),
                name: crate::engines::ToolName::Draw,
            }
        }),
        Ok(Vec::new())
    );
    let lookup = core
        .handle(Input::Engine {
            key,
            event: TurnEvent::ToolArgsDelta {
                call: id("model"),
                delta: args.clone(),
            },
        })
        .expect("a closed record starts intake before completion");
    assert_eq!(
        core.handle(Input::Engine {
            key,
            event: TurnEvent::ToolCallReady {
                call: id("model"),
                args,
            }
        }),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(Input::Engine {
            key,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("round done"),
        Vec::new()
    );
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
    assert_eq!(
        store.commit_call(&result).expect("durable result row"),
        Committed::Done
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
    assert!(text.ends_with(&format!("{expected_error}{RETRY}")));
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
        ToolOutput::Text(format!("drawn: []; stopped: drawing.BudgetSpent{SPENT}"))
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
            .map(|call| call.call.as_str())
            .collect::<Vec<_>>(),
        ["draw-0", "draw-1", "draw-2"]
    );
    assert_eq!(
        calls.iter().map(|call| call.key.route).collect::<Vec<_>>(),
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
        key: super::engine_key(&core, R),
        event: TurnEvent::OutputItem {
            index: 0,
            item: crate::engines::Opaque::new(br#"{"type":"reasoning","id":"old"}"#.to_vec())
                .expect("small item"),
        },
    })
    .expect("native item");
    assert_eq!(
        core.handle(Input::Engine {
            key: super::engine_key(&core, R),
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
    core.handle(built(key, vec![T]))
        .expect("replacement admitted");
    assert_eq!(
        core.handle(Input::Engine {
            key: super::engine_key(&core, R),
            event: TurnEvent::TextDelta("replacement".into())
        }),
        Ok(vec![
            super::state(R, T, kumochi_protocol::message::Phase::Answering),
            super::text("replacement")
        ])
    );
    let completed = core
        .handle(Input::Engine {
            key: super::engine_key(&core, R),
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
#[expect(
    clippy::too_many_lines,
    reason = "one replacement race spans both rounds and their acknowledgements"
)]
fn a_late_drawing_result_cannot_answer_a_replacement_round_twice() {
    let scratch = Scratch::new("draw-tool-replacement");
    let mut store = store(&scratch);
    let source = ThreadId(2);
    let mut core = offered(vec![T, source]);
    let mut lookup = Vec::new();
    for event in [
        TurnEvent::ToolCallStarted {
            call: id("old"),
            name: crate::engines::ToolName::Draw,
        },
        TurnEvent::ToolArgsDelta {
            call: id("old"),
            delta: String::from_utf8(args(None)).expect("JSON"),
        },
        TurnEvent::ToolCallReady {
            call: id("old"),
            args: String::from_utf8(args(None)).expect("JSON"),
        },
    ] {
        lookup.extend(
            core.handle(Input::Engine {
                key: super::engine_key(&core, R),
                event,
            })
            .expect("call event"),
        );
    }
    assert_eq!(
        core.handle(Input::Engine {
            key: super::engine_key(&core, R),
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("pending drawing"),
        Vec::new()
    );
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
    core.handle(Input::Delete {
        thread: source,
        after_turns: 1,
        resend: false,
    })
    .expect("delete accepted while the actual result is pending");
    assert_eq!(
        store.commit_call(old).expect("old result INSERT"),
        Committed::Done
    );
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
    core.handle(built(key, vec![T]))
        .expect("replacement admitted");
    assert_eq!(core.handle(ack.clone()), Ok(Vec::new()));
    core.handle(Input::Engine {
        key: super::engine_key(&core, R),
        event: TurnEvent::TextDelta("replacement".into()),
    })
    .expect("replacement accepts text");
    assert_eq!(core.handle(ack), Ok(Vec::new()));
    let completed = core
        .handle(Input::Engine {
            key: super::engine_key(&core, R),
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
        ToolOutput::Text(format!(
            "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}"
        ))
    );
    assert_eq!(
        store.drawing_scenes().expect("committed prefix").scenes[0]
            .log
            .len(),
        2
    );
}

#[test]
fn a_complete_draw_without_a_canvas_refuses_a_box_after_its_durable_prefix() {
    let scratch = Scratch::new("draw-tool-box-without-canvas");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    core.screen_marks = crate::drawing::ScreenMarks::Off;
    let boxed = args(Some(json!({
        "id": "s3",
        "put": [{"highlight":{"id":"c","target":{"box":[20,30,40,50]},"style":"box"}}]
    })));
    let (_, lookup) = call(&mut core, "boxed", &boxed);
    let drawn = settle(&mut core, &mut store, lookup);
    assert_eq!(
        answer(&mut core, &mut store, &drawn),
        ToolOutput::Text(format!(
            "drawn: [s1, s2]; stopped: step s3: put[0].target: scene.Unsupported{DRAWN_RETRY}"
        ))
    );
    assert_eq!(
        store.drawing_scenes().expect("committed prefix").scenes[0]
            .log
            .len(),
        2
    );
}

#[test]
fn a_complete_draw_ignores_a_trailer_without_changing_its_retry_identity() {
    let scratch = Scratch::new("draw-tool-trailer");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let closed = args(None);
    let mut trailing = closed[..closed.len() - 1].to_vec();
    trailing.extend_from_slice(br#", "records": null}"#);
    let (_, lookup) = call(&mut core, "trailing", &trailing);
    let drawn = settle(&mut core, &mut store, lookup);
    let expected =
        ToolOutput::Text("drawn: [s1, s2]; text after the records array was ignored".into());
    assert_eq!(answer(&mut core, &mut store, &drawn), expected);
    assert_eq!(
        call(&mut core, "trailing", &closed),
        (Dispatch::Answered(expected), Vec::new())
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

#[test]
fn sixty_six_records_complete_and_the_sixty_seventh_keeps_the_durable_prefix() {
    for extra in [false, true] {
        let scratch = Scratch::new("draw-record-bound");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let mut records =
            vec![json!({"begin":{"v":1,"scene":"bounded","surface":"overlay","alt":"bounded"}})];
        for index in 0..64 {
            records.push(json!({"step":{"id":format!("s{index}")}}));
        }
        records.push(json!({"end":{}}));
        if extra {
            records.push(json!({"end":{}}));
        }
        assert_eq!(records.len(), if extra { 67 } else { 66 });
        let bytes = serde_json::to_vec(&json!({"records":records})).expect("fixture");
        let (dispatch, mut effects) = call(&mut core, "bounded", &bytes);
        assert_eq!(dispatch, Dispatch::Committing);
        let mut steps = 0;
        let mut ends = Vec::new();
        loop {
            for effect in &effects {
                match effect {
                    Effect::Data(DataToShell::SceneStepCommit { .. }) => steps += 1,
                    Effect::Data(DataToShell::SceneEnd { complete, .. }) => ends.push(*complete),
                    _ => {}
                }
            }
            if pending(&effects).is_none() {
                break;
            }
            effects = write(&mut core, &mut store, &effects);
        }
        assert_eq!(steps, 64);
        assert_eq!(ends, [!extra]);
        assert_eq!(
            store.drawing_scenes().expect("durable scene").scenes[0]
                .log
                .len(),
            64
        );
        let result = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::CommitCall(result)) => Some(result),
                _ => None,
            })
            .expect("result waits for durable acknowledgement");
        let ids = (0..64)
            .map(|index| format!("s{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let expected = ToolOutput::Text(format!(
            "drawn: [{ids}]{}",
            if extra {
                "; stopped: drawing.BudgetSpent"
            } else {
                ""
            }
        ));
        assert_eq!(result.output, expected);
        assert!(core.current.as_ref().expect("run").answered.is_empty());
        let outcome = store.commit_call(result).expect("durable result");
        assert_eq!(outcome, Committed::Done);
        core.handle(Input::CallCommitted {
            key: result.key.clone(),
            outcome,
            then: None,
        })
        .expect("ack");
        assert_eq!(
            call(&mut core, "bounded", &bytes),
            (Dispatch::Answered(expected), Vec::new())
        );
    }
}

#[test]
fn an_inline_begin_is_refused_before_scene_writes_on_model_and_mcp_routes() {
    let launch = crate::bridge::LaunchId(1);
    let inline = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":"diagram","surface":"inline","alt":"diagram"}},
        {"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("fixture");
    let expected = ToolOutput::Text(format!(
        "drawn: []; stopped: surface: scene.Unsupported{RETRY}"
    ));
    let repaired = ToolOutput::Text("drawn: [s1, s2]".into());
    let caller = |is_mcp, name: &str| {
        if is_mcp {
            Caller::Mcp {
                run: R,
                launch,
                key: crate::bridge::CallKey(name.into()),
            }
        } else {
            Caller::Model {
                run: R,
                call: id(name),
            }
        }
    };
    let invoke = |core: &mut Coordinator, caller: Caller, bytes: &[u8]| {
        let mut effects = Vec::new();
        (
            core.tool_call(caller, crate::engines::ToolName::Draw, bytes, &mut effects),
            effects,
        )
    };
    for (label, is_mcp, route) in [
        ("model", false, crate::dispatch::Route::Model),
        ("mcp", true, crate::dispatch::Route::Mcp(launch)),
    ] {
        let refused_caller = caller(is_mcp, "inline");
        let repair_caller = caller(is_mcp, "overlay");
        let scratch = Scratch::new(&format!("draw-tool-inline-{label}"));
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        core.current.as_mut().expect("run").mcp = Some(launch);
        let (dispatch, effects) = invoke(&mut core, refused_caller.clone(), &inline);
        assert_eq!(dispatch, Dispatch::Committing);
        assert_eq!(effects, Vec::new());
        assert!(pending(&effects).is_none());
        assert!(store.drawing_scenes().expect("no scene").scenes.is_empty());
        assert!(core.drawing.snapshot().scenes.is_empty());
        assert_eq!(
            core.current.as_ref().expect("run").answered,
            vec![(refused_caller.clone(), expected.clone())]
        );
        assert_eq!(
            invoke(&mut core, refused_caller.clone(), &args(None)),
            (Dispatch::Answered(expected.clone()), Vec::new())
        );
        let (dispatch, lookup) = invoke(&mut core, repair_caller.clone(), &args(None));
        assert_eq!(dispatch, Dispatch::Committing);
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
        assert_eq!(result.key.route, route);
        assert_eq!(result.output, repaired);
        assert_eq!(
            store.commit_call(result).expect("durable repair result"),
            Committed::Done
        );
        assert_eq!(
            core.handle(Input::CallCommitted {
                key: result.key.clone(),
                outcome: Committed::Done,
                then: None,
            }),
            Ok(Vec::new())
        );
        assert_eq!(
            core.current.as_ref().expect("run").answered,
            vec![
                (refused_caller, expected.clone()),
                (repair_caller, repaired.clone()),
            ]
        );
        assert_eq!(store.drawing_scenes().expect("one scene").scenes.len(), 1);
    }
}

/// The run's hold sent a picture of the screen on a `ChatGPT` model.
fn sent_picture(core: &mut Coordinator) {
    let current = core.current.as_mut().expect("run");
    current.pictured = true;
    current.screen = Some(crate::context::Screen::Picture(
        crate::context::Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xD9]).expect("a JPEG"),
        None,
        crate::context::Geometry::FIXTURE,
    ));
    current.snapshot.as_mut().expect("snapshot").choice =
        Some(kumochi_protocol::message::ModelChoice {
            model: kumochi_protocol::message::ModelSlug::new("gpt-fixture").expect("slug"),
            effort: kumochi_protocol::message::Effort::Low,
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        });
}

#[test]
fn a_screen_scene_s_canvas_is_asked_of_the_current_run_only_while_marks_are_on() {
    let mut core = offered(vec![T]);
    assert_eq!(
        core.canvas(),
        Some(Err(crate::context::NoCanvas::NoPicture))
    );
    core.screen_marks = crate::drawing::ScreenMarks::Off;
    assert_eq!(core.canvas(), None);
    core.screen_marks = crate::drawing::ScreenMarks::On;
    assert_eq!(
        core.canvas(),
        Some(Err(crate::context::NoCanvas::NoPicture))
    );
    sent_picture(&mut core);
    assert_eq!(core.canvas(), Some(Ok(crate::context::Geometry::FIXTURE)));
    core.screen_marks = crate::drawing::ScreenMarks::Off;
    assert_eq!(core.canvas(), None);
}

#[test]
fn a_screen_call_s_native_item_is_stored_without_its_box_only_while_marks_are_on() {
    let args = r#"{"records":[{"begin":{"scene":"marks","surface":"screen","alt":"marks"}},{"step":{"id":"s1","put":[{"highlight":{"id":"d","target":{"box":[186,598,450,78]},"style":"box"}}]}},{"end":{}}]}"#;
    let native =
        json!({"type":"function_call","id":"fc1","call_id":"model","name":"draw","arguments":args});
    let item =
        crate::engines::Opaque::new(serde_json::to_vec(&native).expect("item")).expect("small");
    for (marks, target) in [
        (
            crate::drawing::ScreenMarks::On,
            json!({"box":[0.0, 0.0, 1.0, 1.0]}),
        ),
        (
            crate::drawing::ScreenMarks::Off,
            json!({"box":[186, 598, 450, 78]}),
        ),
    ] {
        let mut core = offered(vec![T]);
        core.screen_marks = marks;
        sent_picture(&mut core);
        let key = super::engine_key(&core, R);
        let mut event = |event| {
            core.handle(Input::Engine { key, event })
                .expect("engine event")
        };
        event(TurnEvent::OutputItem {
            index: 0,
            item: item.clone(),
        });
        let round = event(TurnEvent::Finished(Finish::Completed))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .expect("the round");
        let stored: Value = serde_json::from_slice(round.items[0].bytes()).expect("item");
        let records: Value =
            serde_json::from_str(stored["arguments"].as_str().expect("arguments")).expect("JSON");
        assert_eq!(
            records["records"][1]["step"]["put"][0]["highlight"]["target"], target,
            "{marks:?}"
        );
        assert_eq!(stored["call_id"], "model");
    }
}

/// The native items of a screen call, whole and cut off inside a box.
const MARKED: &str = r#"{"records":[{"begin":{"scene":"marks","surface":"screen","alt":"marks"}},{"step":{"id":"s1","put":[{"highlight":{"id":"d","target":{"box":[186,598,450,78]},"style":"box"}}]}},{"end":{}}]}"#;
const CUT: &str = r#"{"records":[{"begin":{"scene":"marks","surface":"screen","alt":"marks"}},{"step":{"id":"s1","put":[{"highlight":{"id":"d","target":{"box":[186,598,"#;

/// Permission to draw can go after the boxes exist; the duty to store them without boxes can't.
#[test]
fn a_run_that_took_up_a_screen_picture_stores_its_items_without_boxes_whatever_withdraws_it() {
    let item = |args: &str| {
        let native = json!({"type":"function_call","id":"fc1","call_id":"model","name":"draw","arguments":args});
        crate::engines::Opaque::new(serde_json::to_vec(&native).expect("item")).expect("small")
    };
    let other = crate::context::Geometry {
        display: kumochi_scene::record::ScreenDisplay {
            origin: [2560, 0],
            size: [2560, 1440],
            dpi: 96,
        },
        picture: [1280, 720],
    };
    for (case, target, cut) in [
        (
            "sharing off",
            json!({"box":[0.0, 0.0, 1.0, 1.0]}),
            r#"{"records":[]}"#,
        ),
        (
            "another display",
            json!({"box":[0.0, 0.0, 1.0, 1.0]}),
            r#"{"records":[]}"#,
        ),
        ("a typed turn", json!({"box":[186, 598, 450, 78]}), CUT),
    ] {
        let mut core = offered(vec![T]);
        core.screen_marks = crate::drawing::ScreenMarks::On;
        if case != "a typed turn" {
            sent_picture(&mut core);
        }
        if case == "another display" {
            let current = core.current.as_mut().expect("run");
            let Some(crate::context::Screen::Picture(_, _, geometry)) = &mut current.screen else {
                panic!("the picture");
            };
            *geometry = other;
        }
        let key = super::engine_key(&core, R);
        for (index, args) in [(0, MARKED), (1, CUT)] {
            core.handle(Input::Engine {
                key,
                event: TurnEvent::OutputItem {
                    index,
                    item: item(args),
                },
            })
            .expect("engine event");
        }
        if case == "sharing off" {
            core.handle(Input::ShowScreenOff).expect("the setting");
            assert_eq!(core.canvas(), Some(Err(crate::context::NoCanvas::Off)));
        }
        let round = core
            .handle(Input::Engine {
                key,
                event: TurnEvent::Finished(Finish::Completed),
            })
            .expect("engine event")
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .expect("the round");
        let stored =
            |at: usize| -> Value { serde_json::from_slice(round.items[at].bytes()).expect("item") };
        let records: Value =
            serde_json::from_str(stored(0)["arguments"].as_str().expect("arguments"))
                .expect("JSON");
        assert_eq!(
            records["records"][1]["step"]["put"][0]["highlight"]["target"], target,
            "{case}"
        );
        assert_eq!(stored(1)["arguments"], cut, "{case}");
    }
}

/// An ordinary call with a box target is stored as it came, whether or not its run took up a
/// picture.
#[test]
fn an_overlay_call_with_a_box_target_is_stored_as_it_came_in_any_run() {
    let args = r#"{"records":[{"begin":{"v":1,"scene":"card","surface":"overlay","alt":"","others":"keep"}},{"step":{"id":"c1","put":[{"callout":{"id":"l","target":{"box":[10,20,30,40]},"text":"x"}}]}},{"end":{}}]}"#;
    let native =
        json!({"type":"function_call","id":"fc1","call_id":"model","name":"draw","arguments":args});
    let item =
        crate::engines::Opaque::new(serde_json::to_vec(&native).expect("item")).expect("small");
    for pictured in [true, false] {
        let mut core = offered(vec![T]);
        core.screen_marks = crate::drawing::ScreenMarks::On;
        if pictured {
            sent_picture(&mut core);
        }
        let key = super::engine_key(&core, R);
        core.handle(Input::Engine {
            key,
            event: TurnEvent::OutputItem {
                index: 0,
                item: item.clone(),
            },
        })
        .expect("engine event");
        let round = core
            .handle(Input::Engine {
                key,
                event: TurnEvent::Finished(Finish::Completed),
            })
            .expect("engine event")
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .expect("the round");
        assert_eq!(round.items[0].bytes(), item.bytes(), "{pictured}");
    }
}

const OFF: &str = "drawn: []; stopped: surface: drawing.ScreenOff; the user turned off sharing the screen; explain in words or draw a card";

#[test]
fn turning_sharing_off_tells_the_drawing_only_while_marks_are_on() {
    let begin = r#"{"begin":{"scene":"marks","surface":"screen","alt":"marks"}}"#;
    for (marks, pictured, told) in [
        (crate::drawing::ScreenMarks::On, true, vec![OFF.to_owned()]),
        (crate::drawing::ScreenMarks::On, false, vec![OFF.to_owned()]),
        (crate::drawing::ScreenMarks::Off, true, Vec::new()),
    ] {
        let mut core = offered(vec![T]);
        core.screen_marks = marks;
        if pictured {
            sent_picture(&mut core);
        }
        core.drawing
            .canvas(R, Ok(crate::context::Geometry::FIXTURE), None);
        core.handle(Input::ShowScreenOff).expect("the setting");
        let out = core
            .drawing
            .handle(crate::drawing::Input::Record {
                thread: T,
                run: R,
                call: CallId::new("c").expect("a short ASCII id"),
                index: 0,
                bytes: begin.as_bytes().to_vec(),
            })
            .expect("a record");
        let texts: Vec<String> = out
            .calls
            .iter()
            .map(crate::drawing::CallEnd::tool_text)
            .collect();
        assert_eq!(texts, told, "{marks:?} {pictured}");
    }
}

/// No mark's box or model-written display reaches a stored round, through the coordinator, after
/// sharing goes off, whatever the call holds.
#[test]
fn no_screen_number_reaches_a_stored_round_whatever_the_call_holds() {
    const BOX: &str = "917.375";
    const DISPLAY: &str = "2731";
    const LABEL: &str = "zq-label-7319";
    let begin = r#"{"begin":{"scene":"marks","surface":"screen","alt":"a"}}"#;
    let overlay = r#"{"begin":{"scene":"card","surface":"overlay","alt":"a"}}"#;
    let written = format!(
        r#"{{"begin":{{"scene":"marks","surface":"screen","alt":"a","display":{{"origin":[0,0],"size":[{DISPLAY},1537],"dpi":97}}}}}}"#
    );
    let marked = format!(
        r#"{{"step":{{"id":"s1","put":[{{"highlight":{{"id":"m","target":{{"box":[{BOX},300,50,40]}},"style":"box"}}}},{{"callout":{{"id":"l","target":{{"box":[{BOX},300,50,40]}},"text":"{LABEL}","side":"top"}}}}]}}}}"#
    );
    let shaped = format!(
        r##"{{"step":{{"id":"s1","put":[{{"shape":{{"id":"m","kind":"rect","box":[{BOX},300,50,40],"fill":"#ffffff"}}}}]}}}}"##
    );
    let end = r#"{"end":{}}"#;
    let mut calls: Vec<String> = [
        vec![begin, marked.as_str(), end],
        vec![overlay, begin, marked.as_str(), end],
        vec![begin, shaped.as_str(), end],
        vec![written.as_str(), marked.as_str()],
    ]
    .iter()
    .map(|records| format!(r#"{{"records":[{}]}}"#, records.join(",")))
    .collect();
    calls.extend([
        format!(r#"{{"records":[{overlay},{end}]}} {{"records":[{begin},{marked},{end}]}}"#),
        format!(r#"{{"records":[{overlay},{end}],"records":[{written},{marked},{end}]}}"#),
    ]);
    let mut core = offered(vec![T]);
    core.screen_marks = crate::drawing::ScreenMarks::On;
    sent_picture(&mut core);
    let key = super::engine_key(&core, R);
    for (index, args) in (0..).zip(&calls) {
        let native = json!({"type":"function_call","id":"fc1","call_id":"model","name":"draw","arguments":args});
        let item =
            crate::engines::Opaque::new(serde_json::to_vec(&native).expect("item")).expect("small");
        core.handle(Input::Engine {
            key,
            event: TurnEvent::OutputItem { index, item },
        })
        .expect("engine event");
    }
    core.handle(Input::ShowScreenOff).expect("the setting");
    let round = core
        .handle(Input::Engine {
            key,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("engine event")
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => Some(round),
            _ => None,
        })
        .expect("the round");
    assert_eq!(round.items.len(), calls.len());
    let absent = |text: &str, marker: &str| {
        let listed = format!("{:?}", marker.as_bytes());
        !text.contains(marker) && !text.contains(listed.trim_matches(['[', ']']))
    };
    let stored: Vec<String> = round
        .items
        .iter()
        .map(|item| String::from_utf8(item.bytes().to_vec()).expect("UTF-8"))
        .collect();
    for text in stored.iter().chain([&format!("{round:?}")]) {
        assert!(absent(text, BOX) && absent(text, DISPLAY), "{text}");
    }
    assert!(stored[0].contains(LABEL), "{}", stored[0]);
}

/// A complete call's result waits for the shell, so a refusal of its last step while the shell
/// prepares it reaches the model, which draws again in the next round of the same run, and the
/// wait's tick stops once nothing is held.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one run from the held refusal through the next round's redraw"
)]
fn a_held_refusal_of_its_last_step_is_redrawn_in_the_same_run() {
    use kumochi_protocol::message::SceneFailure;
    let scratch = Scratch::new("draw-tool-held");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let args = String::from_utf8(args(None)).expect("JSON");
    let key = super::engine_key(&core, R);
    core.handle(Input::Engine {
        key,
        event: TurnEvent::ToolCallStarted {
            call: id("model"),
            name: crate::engines::ToolName::Draw,
        },
    })
    .expect("started");
    let lookup = core
        .handle(Input::Engine {
            key,
            event: TurnEvent::ToolArgsDelta {
                call: id("model"),
                delta: args.clone(),
            },
        })
        .expect("intake");
    core.handle(Input::Engine {
        key,
        event: TurnEvent::ToolCallReady {
            call: id("model"),
            args,
        },
    })
    .expect("ready");
    core.handle(Input::Engine {
        key,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .expect("round done");
    let first = write(&mut core, &mut store, &lookup);
    let second = write(&mut core, &mut store, &first);
    let (serial, last) = pending(&second).expect("the last step's write");
    let reply = store.drawing_write(&last).expect("durable write");
    let held = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    let scene = held
        .iter()
        .find_map(|effect| match effect {
            Effect::Data(DataToShell::SceneStepCommit {
                scene, index: 1, ..
            }) => Some(*scene),
            _ => None,
        })
        .expect("the last step");
    assert!(
        !held
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
    );
    assert_eq!(
        held.iter()
            .filter(|effect| matches!(effect, Effect::ArmReadyWait))
            .count(),
        1
    );
    assert_eq!(
        core.handle(Input::ReadyWait),
        Ok(vec![Effect::ArmReadyWait])
    );

    let failed = core
        .handle(Input::Drawing(crate::drawing::Input::StepFailed {
            run: R,
            scene,
            index: 1,
            reason: SceneFailure::PaintWork,
            cut: kumochi_scene::StopCut {
                activated: 0,
                revealed: kumochi_scene::text::Bounded::new(Vec::new()).expect("none"),
            },
        }))
        .expect("a committed step");
    assert!(failed.contains(&Effect::CancelReadyWait));
    let failed = settle(&mut core, &mut store, failed);
    assert!(
        !failed
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
    );
    let result = failed
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call.clone()),
            _ => None,
        })
        .expect("the refused call's result");
    assert_eq!(
        result.output,
        ToolOutput::Text("drawn: [s1, s2]; stopped: drawing.TooMuchToPaint; nothing of this scene was shown, so it was removed; everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows; begin a new scene with a new name and fewer or simpler shapes".into())
    );
    let outcome = store.commit_call(&result).expect("durable result");
    let settled = core
        .handle(Input::CallCommitted {
            key: result.key,
            outcome,
            then: None,
        })
        .expect("acknowledgement");
    assert_eq!(core.handle(Input::ReadyWait), Ok(Vec::new()));
    let round = settled
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => Some(round),
            _ => None,
        })
        .expect("the refused round");
    store
        .append_round(
            (round.thread, round.run),
            round.connection,
            round.text.as_deref(),
            &round.items,
            &round.calls,
        )
        .expect("durable round");
    let building = core
        .handle(Input::RoundStored {
            run: R,
            round: round.round,
        })
        .expect("the refusal starts the next round");
    let (key, _) = admitted_request(&mut core, &building, vec![T]);
    assert_eq!(key.run, R);
    let fixed = serde_json::to_string(&json!({"records":[
        {"begin":{"v":1,"scene":"smaller","surface":"overlay","alt":"smaller"}},
        {"step":{"id":"t1","put":[{"shape":{"id":"c","kind":"rect","box":[0,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("fixture");
    core.handle(Input::Engine {
        key,
        event: TurnEvent::ToolCallStarted {
            call: id("fixed"),
            name: crate::engines::ToolName::Draw,
        },
    })
    .expect("started");
    let lookup = core
        .handle(Input::Engine {
            key,
            event: TurnEvent::ToolArgsDelta {
                call: id("fixed"),
                delta: fixed.clone(),
            },
        })
        .expect("intake");
    core.handle(Input::Engine {
        key,
        event: TurnEvent::ToolCallReady {
            call: id("fixed"),
            args: fixed,
        },
    })
    .expect("ready");
    core.handle(Input::Engine {
        key,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .expect("round done");
    let drawn = settle(&mut core, &mut store, lookup);
    assert_eq!(
        answer(&mut core, &mut store, &drawn),
        ToolOutput::Text("drawn: [t1]".into())
    );
    assert_eq!(core.current.as_ref().expect("the same run").id, R);
    assert_eq!(core.handle(Input::ReadyWait), Ok(Vec::new()));
}

/// Applies `next` and every effect it leads to: drawing writes made durable, scene clears
/// cleared and acknowledged. Every effect seen, in order.
fn settle(core: &mut Coordinator, store: &mut Store, mut next: Vec<Effect>) -> Vec<Effect> {
    let mut all = Vec::new();
    while !next.is_empty() {
        let mut more = Vec::new();
        if pending(&next).is_some() {
            more = write(core, store, &next);
        }
        for effect in &next {
            if let Effect::Store(StoreOp::ClearScenes { scenes }) = effect {
                store.clear_scenes(scenes).expect("clear");
                more.extend(
                    core.handle(Input::ScenesCleared {
                        scenes: scenes.clone(),
                        ok: true,
                    })
                    .expect("clear acknowledgement"),
                );
            }
        }
        all.append(&mut next);
        next = more;
    }
    all
}

/// Commits the call result among `effects` and acknowledges it; the result's output.
fn answer(core: &mut Coordinator, store: &mut Store, effects: &[Effect]) -> ToolOutput {
    let result = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call.clone()),
            _ => None,
        })
        .expect("a result");
    let outcome = store.commit_call(&result).expect("durable result");
    core.handle(Input::CallCommitted {
        key: result.key,
        outcome,
        then: None,
    })
    .expect("acknowledgement");
    result.output
}

/// Streams the screen call `name` with `args` into the coordinator's draw step, which announces
/// the canvas before every input, without provider stream framing.
fn screen(core: &mut Coordinator, name: &str, args: &str) -> Vec<Effect> {
    let key = crate::dispatch::LedgerKey {
        run: R,
        route: crate::dispatch::Route::Model,
        call: name.into(),
    };
    let records: Value = serde_json::from_str(args).expect("JSON");
    let mut actions = vec![crate::dispatch::DrawAction::Start];
    actions.extend(
        (0..)
            .zip(records["records"].as_array().expect("records"))
            .map(|(index, record)| crate::dispatch::DrawAction::Record {
                index,
                bytes: serde_json::to_vec(record).expect("a record"),
            }),
    );
    actions.push(crate::dispatch::DrawAction::Finish(None));
    let mut effects = Vec::new();
    for action in actions {
        assert_eq!(
            core.drawing_call(T, &key, action, &mut effects),
            Dispatch::Committing
        );
    }
    effects
}

/// A skipped screen scene's run refuses its next screen call through the real path, which
/// announces the still-held picture before every draw input, with or without a card between:
/// nothing is looked up, written or shown. The refusal's text is the reducer's, pinned there.
#[test]
fn a_skipped_screen_scene_s_run_refuses_its_next_screen_call_through_the_coordinator() {
    const AGAIN: &str = r#"{"records":[{"begin":{"scene":"again","surface":"screen","alt":"marks"}},{"step":{"id":"s2","put":[{"highlight":{"id":"e","target":{"box":[186,598,450,78]},"style":"box"}}]}},{"end":{}}]}"#;
    for card in [false, true] {
        let scratch = Scratch::new(&format!("draw-tool-skip-{card}"));
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        core.screen_marks = crate::drawing::ScreenMarks::On;
        sent_picture(&mut core);
        let lookup = screen(&mut core, "a", MARKED);
        let drawn = settle(&mut core, &mut store, lookup);
        let scene = drawn
            .iter()
            .find_map(|effect| match effect {
                Effect::Data(DataToShell::SceneBegin { scene, .. }) => Some(*scene),
                _ => None,
            })
            .expect("a screen scene");
        let skipped = core
            .handle(Input::Drawing(crate::drawing::Input::SceneSkipped {
                scene,
            }))
            .expect("the skip");
        settle(&mut core, &mut store, skipped);
        if card {
            let (_, lookup) = call(&mut core, "c", &args(None));
            let drawn = settle(&mut core, &mut store, lookup);
            assert_eq!(
                answer(&mut core, &mut store, &drawn),
                ToolOutput::Text("drawn: [s1, s2]".into())
            );
        }
        let refused = screen(&mut core, "b", AGAIN);
        let refused = settle(&mut core, &mut store, refused);
        assert!(
            !refused.iter().any(|effect| matches!(
                effect,
                Effect::Data(_) | Effect::Store(StoreOp::Drawing { .. })
            )),
            "{card}"
        );
        assert_eq!(
            core.drawing.snapshot().scenes.len(),
            usize::from(card),
            "{card}"
        );
    }
}

#[test]
fn stale_drawing_handoff_commits_and_answers_ended_without_drawing() {
    let scratch = Scratch::new("draw-tool-stale-handoff");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    // Close the run in the drawing reducer while the coordinator still holds it so `drawing_call`
    // hits the `drawing_input` `Err(Stale)` fallback directly.
    core.drawing
        .handle(crate::drawing::Input::RunClosed {
            run: R,
            loss: crate::drawing::Loss::Drop,
        })
        .expect("close drawing run");
    let (dispatch, effects) = call(&mut core, "stale", &args(None));
    assert_eq!(dispatch, Dispatch::Committing);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. })))
    );
    let expected = ToolOutput::Error(ToolError::Ended);
    assert_eq!(answer(&mut core, &mut store, &effects), expected);
    assert_eq!(
        core.current.as_ref().expect("current").answered,
        vec![(
            Caller::Model {
                run: R,
                call: id("stale"),
            },
            expected.clone()
        )]
    );
    assert_eq!(store.drawing_scenes().expect("no scene").scenes.len(), 0);
    assert_eq!(
        call(&mut core, "stale", &args(None)),
        (Dispatch::Answered(expected), Vec::new())
    );
}

#[test]
fn empty_truncated_and_mistyped_envelopes_refuse_through_the_draw_tool() {
    for (name, raw, committed, expected_reason) in [
        (
            "empty",
            br#"{"records":[]}"#.as_slice(),
            true,
            "$: scene.BeforeBegin",
        ),
        (
            "truncated",
            br#"{"records":[{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"a"}}"#,
            false,
            "truncated drawing",
        ),
        (
            "mistyped",
            br#"{"records":null}"#,
            false,
            "malformed drawing at byte 11",
        ),
    ] {
        let scratch = Scratch::new(&format!("draw-tool-envelope-{name}"));
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let (dispatch, effects) = call(&mut core, name, raw);
        assert_eq!(dispatch, Dispatch::Committing, "{name}");
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. }))),
            "{name}: no drawing write"
        );
        let output = if committed {
            answer(&mut core, &mut store, &effects)
        } else {
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_)))),
                "{name}: rejected envelope does not write a ledger row"
            );
            let run = core.current.as_ref().expect("current");
            assert_eq!(run.answered.len(), 1, "{name}");
            run.answered[0].1.clone()
        };
        let ToolOutput::Text(text) = &output else {
            panic!("{name}: expected typed refusal text, got {output:?}");
        };
        assert_eq!(
            text,
            &format!("drawn: []; stopped: {expected_reason}{RETRY}"),
            "{name}"
        );
        assert_eq!(
            store.drawing_scenes().expect("no scene").scenes.len(),
            0,
            "{name}"
        );
        assert_eq!(
            call(&mut core, name, &args(None)).0,
            Dispatch::Answered(if committed {
                ToolOutput::Error(ToolError::Reused)
            } else {
                output
            }),
            "{name}: retry stays refused"
        );
    }
}
