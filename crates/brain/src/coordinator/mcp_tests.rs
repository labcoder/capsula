//! MCP calls through the coordinator: one authorized launch per build, current authority before
//! any replay, the run's call budget, answers only on acknowledgement, and the same durable draw
//! prefix and error as a direct call.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{AssistantName, DataToShell, ThreadId};
use serde_json::{Value, json};

use super::{R, T, fresh};
use crate::bridge::listener::McpOrigin;
use crate::bridge::{CallKey, LaunchId};
use crate::context::{Built, ConnectionTag, Origin, RunSnapshot, ToolSchema};
use crate::coordinator::{BuildKey, ContextBuilt, Coordinator, Effect, Input, StoreOp};
use crate::dispatch::{Caller, Committed, Dispatch, Route, ToolError, ToolOutput};
use crate::engines::claude_code::audit::AuditGate;
use crate::engines::claude_code::launch::{Bind, Offer};
use crate::engines::{CallId, Finish, ToolName, TurnEvent};
use crate::store::{Failure, Store};
use crate::test_support::Scratch;

const L1: LaunchId = LaunchId(1);
const L2: LaunchId = LaunchId(2);
const PROBE: &[u8] = br#"{"word":"hi"}"#;
const RETRY: &str = "; drawn lists what this call added, and earlier steps stay as they were; correct this and call draw again";
const DRAWN_RETRY: &str = "; the steps in drawn are on screen, and earlier steps stay as they were; correct this by drawing only what is missing, with new step and object ids, and call draw again";

/// Run 10's admitted build offering `tools`, with untrusted drawing, and the build's key.
fn offering(tools: Vec<ToolSchema>) -> (Coordinator, BuildKey) {
    offering_from(tools, vec![T])
}

/// As [`offering`], its request rendered from `sources`.
fn offering_from(tools: Vec<ToolSchema>, sources: Vec<ThreadId>) -> (Coordinator, BuildKey) {
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
    core.handle(built_from(key, tools, sources))
        .expect("offered");
    let crate::coordinator::Availability::Ready(ready) = &mut core.availability else {
        panic!("ready");
    };
    ready.trusted_draw = false;
    (core, key)
}

fn built(key: BuildKey, tools: Vec<ToolSchema>) -> Input {
    built_from(key, tools, vec![T])
}

fn built_from(key: BuildKey, tools: Vec<ToolSchema>, sources: Vec<ThreadId>) -> Input {
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
                tools,
                ..super::request()
            },
            sources,
        }),
    })
}

/// The production offer: `draw` among the model's tools, and no test tool.
fn drawing() -> Vec<ToolSchema> {
    crate::context::tools(Origin::Task(T))
}

/// Every tool, the test probe included.
fn probing() -> Vec<ToolSchema> {
    crate::dispatch::schemas(Origin::Task(T), Route::Model)
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

fn key(n: u32) -> CallKey {
    CallKey(format!("m:n:{n}"))
}

fn caller(launch: LaunchId, n: u32) -> Caller {
    Caller::Mcp {
        run: R,
        launch,
        key: key(n),
    }
}

/// The main thread; run 10 asks in task `T`.
const MAIN: ThreadId = ThreadId(1);

/// Another thread a build may render from.
const OTHER: ThreadId = ThreadId(3);

/// A passed gate and an hour to go: what a live launch binds with.
fn passed() -> (AuditGate, Instant) {
    let gate = AuditGate::new();
    gate.pass();
    (gate, Instant::now() + Duration::from_secs(3600))
}

/// Binds as the launcher would: `launch` for `build`, asked in `thread` with `offer`, holding
/// `gate` until the effective deadline `until`.
fn binds(
    core: &mut Coordinator,
    (build, launch, thread): (BuildKey, LaunchId, ThreadId),
    offer: Offer,
    (gate, until): (AuditGate, Instant),
) -> bool {
    let bind = Bind {
        build,
        launch,
        offer,
        until,
        reply: tokio::sync::oneshot::channel().0,
    };
    core.mcp_bound(&bind, thread, gate).is_some()
}

/// Binds `launch` for `build` as the launcher would: the run's thread and origin, offering the
/// draw and, when the build offers it, the probe, with a passed gate.
fn bound(core: &mut Coordinator, build: BuildKey, launch: LaunchId) -> bool {
    let probes = core
        .current
        .as_ref()
        .and_then(|run| run.build.as_ref())
        .is_some_and(|current| current.offered().contains(&ToolName::Probe));
    let tools = if probes {
        vec![ToolName::Draw, ToolName::Probe]
    } else {
        vec![ToolName::Draw]
    };
    binds(
        core,
        (build, launch, T),
        Offer::Mcp(Origin::Task(T), tools),
        passed(),
    )
}

fn ended() -> Dispatch {
    Dispatch::Answered(ToolOutput::Error(ToolError::Ended))
}

fn mcp(
    core: &mut Coordinator,
    (build, launch, n): (BuildKey, LaunchId, u32),
    tool: ToolName,
    args: &[u8],
) -> (Dispatch, Vec<Effect>) {
    let mut effects = Vec::new();
    let dispatch = core.mcp_call(build, launch, key(n), (tool, args), &mut effects);
    (dispatch, effects)
}

fn probe(core: &mut Coordinator, at: (BuildKey, LaunchId, u32)) -> Dispatch {
    mcp(core, at, ToolName::Probe, PROBE).0
}

fn acked(core: &mut Coordinator, caller: &Caller) {
    core.handle(Input::CallCommitted {
        key: caller.key(),
        outcome: Committed::Done,
        then: None,
    })
    .expect("current");
}

/// Writes every drawing step the effects hold until none is pending: the call's own result
/// write, and how many steps the shell heard committed.
fn drawn(
    core: &mut Coordinator,
    store: &mut Store,
    mut effects: Vec<Effect>,
) -> (Option<ToolOutput>, usize) {
    let mut commits = 0;
    while let Some((serial, write)) = effects.iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
        _ => None,
    }) {
        let reply = store.drawing_write(&write).expect("durable write");
        effects = core
            .handle(Input::DrawingWritten {
                serial,
                reply: Ok(reply),
            })
            .expect("acknowledgement");
        effects = super::prepared(core, effects);
        commits += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Data(DataToShell::SceneStepCommit { .. })))
            .count();
    }
    let result = effects.into_iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::CommitCall(call)) => Some(call.output),
        _ => None,
    });
    (result, commits)
}

fn steps(store: &Store) -> Vec<usize> {
    store
        .drawing_scenes()
        .expect("scenes")
        .scenes
        .iter()
        .map(|scene| scene.log.len())
        .collect()
}

#[test]
fn a_build_authorizes_one_launch_and_its_next_build_withdraws_it() {
    let (mut core, build) = offering(probing());
    let stale = BuildKey {
        attempt: build.attempt + 1,
        ..build
    };
    assert!(!bound(&mut core, stale, L1), "not the current build");
    let draw = |origin| Offer::Mcp(origin, vec![ToolName::Draw]);
    assert!(
        !binds(&mut core, (build, L1, T), draw(Origin::Kumo), passed()),
        "another origin"
    );
    assert!(
        !binds(
            &mut core,
            (build, L1, MAIN),
            draw(Origin::Task(T)),
            passed()
        ),
        "another thread"
    );
    assert!(
        !binds(&mut core, (build, L1, T), Offer::Nothing, passed()),
        "no offer"
    );
    assert_eq!(probe(&mut core, (build, L1, 1)), ended(), "nothing bound");
    assert!(bound(&mut core, build, L1));
    assert!(!bound(&mut core, build, L2), "one launch per build");
    assert_eq!(
        probe(&mut core, (build, L2, 1)),
        ended(),
        "not the bound launch"
    );
    assert_eq!(
        probe(&mut core, (stale, L1, 1)),
        ended(),
        "not the bound build"
    );
    assert_eq!(probe(&mut core, (build, L1, 1)), Dispatch::Committing);

    core.build(R, 1, &mut Vec::new());
    assert_eq!(core.current.as_ref().expect("run").mcp, None);
    assert_eq!(
        probe(&mut core, (build, L1, 2)),
        ended(),
        "its build was replaced"
    );
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Err(super::Stale));
    let next = core
        .current
        .as_ref()
        .expect("run")
        .build
        .as_ref()
        .expect("build")
        .key();
    core.handle(built(next, probing())).expect("admitted");
    assert!(
        !bound(&mut core, build, L1),
        "a late bind for the old build"
    );
    assert!(bound(&mut core, next, L2));
    assert_eq!(probe(&mut core, (next, L1, 1)), ended());
    assert_eq!(
        probe(&mut core, (next, L2, 1)),
        Dispatch::Committing,
        "the same request id from the next launch is its own call"
    );
}

#[test]
fn an_answer_waits_for_its_acknowledgement_and_goes_to_its_own_call_once() {
    let (mut core, build) = offering(probing());
    assert!(bound(&mut core, build, L1));
    assert_eq!(probe(&mut core, (build, L1, 1)), Dispatch::Committing);
    assert_eq!(probe(&mut core, (build, L1, 2)), Dispatch::Committing);
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Ok(None));
    acked(&mut core, &caller(L1, 2));
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Ok(None));
    let probed = ToolOutput::Text("probed hi".to_owned());
    assert_eq!(
        core.take_mcp_answer(build, L1, &key(2)),
        Ok(Some(probed.clone()))
    );
    assert_eq!(
        core.take_mcp_answer(build, L1, &key(2)),
        Ok(None),
        "taken once"
    );
    acked(&mut core, &caller(L1, 1));
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Ok(Some(probed)));
}

#[test]
fn current_authority_decides_before_the_ledger_replays_a_call() {
    let (mut core, build) = offering(probing());
    assert!(bound(&mut core, build, L1));
    assert_eq!(probe(&mut core, (build, L1, 1)), Dispatch::Committing);
    acked(&mut core, &caller(L1, 1));
    let probed = Dispatch::Answered(ToolOutput::Text("probed hi".to_owned()));
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Probe, PROBE),
        (probed, Vec::new()),
        "a retry replays without a second write"
    );
    let run = core.current.as_ref().expect("run");
    run.build.as_ref().expect("build").control.0.cancel();
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Probe, PROBE),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::Ended)),
            Vec::new()
        ),
        "a cancelled build's launch gets no replay"
    );
    assert_eq!(
        mcp(&mut core, (build, L1, 2), ToolName::Probe, PROBE),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::Ended)),
            Vec::new()
        ),
        "nor a fresh key"
    );
}

#[test]
fn only_tools_the_build_offers_run_over_mcp_and_a_refusal_stays() {
    let (mut core, build) = offering(drawing());
    assert!(
        !binds(
            &mut core,
            (build, L1, T),
            Offer::Mcp(Origin::Task(T), vec![ToolName::Probe]),
            passed()
        ),
        "a launch offering a tool its build doesn't"
    );
    assert!(bound(&mut core, build, L1));
    let refused = Dispatch::Answered(ToolOutput::Error(ToolError::NotHere));
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Probe, PROBE),
        (refused.clone(), Vec::new())
    );
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Probe, PROBE),
        (refused, Vec::new())
    );
    assert_eq!(
        mcp(&mut core, (build, L1, 2), ToolName::SearchHistory, b"{}"),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::NotHere)),
            Vec::new()
        ),
        "a model-only tool"
    );
}

#[test]
fn the_seventeenth_fresh_call_ends_the_run_on_its_budget() {
    let (mut core, build) = offering(probing());
    assert!(bound(&mut core, build, L1));
    for n in 1..=16 {
        assert_eq!(
            probe(&mut core, (build, L1, n)),
            Dispatch::Committing,
            "call {n}"
        );
    }
    assert_eq!(
        mcp(&mut core, (build, L1, 16), ToolName::Probe, PROBE),
        (Dispatch::Joined, Vec::new()),
        "a retry costs nothing"
    );
    let (dispatch, effects) = mcp(&mut core, (build, L1, 17), ToolName::Probe, PROBE);
    assert_eq!(dispatch, ended());
    assert_eq!(effects, super::ends_failed(Failure::Budget));
    assert!(core.current.is_none());
}

#[test]
fn a_semantic_failure_in_the_third_step_keeps_the_same_prefix_and_error_on_both_paths() {
    let third = || args(Some(json!({"id":"s3","remove":["missing"]})));
    let expected = ToolOutput::Text(format!(
        "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}"
    ));

    let direct = Scratch::new("mcp-parity-direct");
    let mut direct_store = store(&direct);
    let (mut core, _) = offering(drawing());
    let mut effects = Vec::new();
    let model = Caller::Model {
        run: R,
        call: CallId::new("partial").expect("ASCII"),
    };
    let dispatch = core.tool_call(model, ToolName::Draw, &third(), &mut effects);
    assert_eq!(dispatch, Dispatch::Committing);
    let direct_result = drawn(&mut core, &mut direct_store, effects);

    let over_mcp = Scratch::new("mcp-parity-mcp");
    let mut mcp_store = store(&over_mcp);
    let (mut core, build) = offering(drawing());
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &third());
    assert_eq!(dispatch, Dispatch::Committing);
    let mcp_result = drawn(&mut core, &mut mcp_store, effects);
    assert_eq!(
        core.take_mcp_answer(build, L1, &key(1)),
        Ok(None),
        "not before its write"
    );
    acked(&mut core, &caller(L1, 1));

    assert_eq!(direct_result, (Some(expected.clone()), 2));
    assert_eq!(mcp_result, direct_result);
    assert_eq!(steps(&direct_store), [2]);
    assert_eq!(steps(&mcp_store), [2]);
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Ok(Some(expected)));
}

#[test]
fn a_structurally_invalid_third_record_writes_nothing_over_mcp_and_stays_refused() {
    let typo = args(Some(
        json!({"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[0,0,10,10],"typo":true}}]}),
    ));
    let scratch = Scratch::new("mcp-structural");
    let mut store = store(&scratch);
    let (mut core, build) = offering(drawing());
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &typo);
    assert_eq!(dispatch, Dispatch::Committing);
    let (result, commits) = drawn(&mut core, &mut store, effects);
    assert_eq!(
        (result, commits),
        (None, 0),
        "a refusal commits no result write"
    );
    assert_eq!(steps(&store), Vec::<usize>::new());
    let Ok(Some(ToolOutput::Text(text))) = core.take_mcp_answer(build, L1, &key(1)) else {
        panic!("a typed refusal");
    };
    assert!(text.starts_with("drawn: []; stopped: step s3: "), "{text}");
    assert!(
        text.ends_with(&format!("scene.UnknownField{RETRY}")),
        "{text}"
    );
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Draw, &args(None)).0,
        Dispatch::Answered(ToolOutput::Text(text))
    );
}

#[test]
fn a_stopped_draw_answers_as_an_error_through_its_replay_and_a_whole_one_does_not() {
    let third = args(Some(json!({"id":"s3","remove":["missing"]})));
    let expected =
        format!("drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}");
    let partial = Scratch::new("mcp-stopped");
    let mut partial_store = store(&partial);
    let (mut core, build) = offering(drawing());
    let origin = McpOrigin {
        build,
        launch: L1,
        key: key(1),
    };
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &third);
    assert_eq!(dispatch, Dispatch::Committing);
    assert_eq!(drawn(&mut core, &mut partial_store, effects).1, 2);
    acked(&mut core, &caller(L1, 1));
    let Ok(Some(settled)) = core.take_mcp_answer(build, L1, &key(1)) else {
        panic!("settled on its acknowledgement");
    };
    let reply = core.mcp_reply(&origin, &settled);
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (expected.as_str(), true)
    );
    let Dispatch::Answered(replayed) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &third).0
    else {
        panic!("a replay is answered from the ledger");
    };
    let reply = core.mcp_reply(&origin, &replayed);
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (expected.as_str(), true),
        "the replay reads the same end"
    );

    let whole = Scratch::new("mcp-whole");
    let mut whole_store = store(&whole);
    let (mut core, build) = offering(drawing());
    let origin = McpOrigin { build, ..origin };
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &args(None));
    assert_eq!(dispatch, Dispatch::Committing);
    assert_eq!(drawn(&mut core, &mut whole_store, effects).1, 2);
    acked(&mut core, &caller(L1, 1));
    let Ok(Some(settled)) = core.take_mcp_answer(build, L1, &key(1)) else {
        panic!("settled on its acknowledgement");
    };
    let reply = core.mcp_reply(&origin, &settled);
    assert_eq!(
        (reply.text.as_str(), reply.error),
        ("drawn: [s1, s2]", false)
    );
}

/// Feeds a streamed model call's arguments to the run's engine.
fn streamed(core: &mut Coordinator, event: TurnEvent) -> Vec<Effect> {
    let key = super::engine_key(core, R);
    core.handle(Input::Engine { key, event })
        .expect("current provider event")
}

#[test]
fn a_structurally_invalid_third_record_keeps_the_streamed_prefix_and_writes_nothing_over_mcp() {
    let typo = String::from_utf8(args(Some(
        json!({"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[0,0,10,10],"typo":true}}]}),
    )))
    .expect("JSON");
    let at = typo
        .find(r#"{"step":{"id":"s3""#)
        .expect("the third record");
    let call = CallId::new("streamed").expect("ASCII");

    let direct = Scratch::new("mcp-structural-direct");
    let mut direct_store = store(&direct);
    let (mut core, _) = offering(drawing());
    let started = TurnEvent::ToolCallStarted {
        call: call.clone(),
        name: ToolName::Draw,
    };
    assert_eq!(streamed(&mut core, started), Vec::new());
    let prefix = TurnEvent::ToolArgsDelta {
        call: call.clone(),
        delta: typo[..at].into(),
    };
    let effects = streamed(&mut core, prefix);
    assert_eq!(drawn(&mut core, &mut direct_store, effects), (None, 2));
    assert_eq!(steps(&direct_store), [2], "two verified commits");
    let rest = TurnEvent::ToolArgsDelta {
        call,
        delta: typo[at..].into(),
    };
    let effects = streamed(&mut core, rest);
    let (Some(ToolOutput::Text(direct_text)), 0) = drawn(&mut core, &mut direct_store, effects)
    else {
        panic!("a streamed call that stopped");
    };

    let over_mcp = Scratch::new("mcp-structural-whole");
    let mut mcp_store = store(&over_mcp);
    let (mut core, build) = offering(drawing());
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, typo.as_bytes());
    assert_eq!(dispatch, Dispatch::Committing);
    assert_eq!(drawn(&mut core, &mut mcp_store, effects), (None, 0));
    let Ok(Some(ToolOutput::Text(mcp_text))) = core.take_mcp_answer(build, L1, &key(1)) else {
        panic!("a typed refusal");
    };

    assert_eq!(steps(&direct_store), [2], "the streamed prefix stays");
    assert_eq!(
        steps(&mcp_store),
        Vec::<usize>::new(),
        "the whole call writes nothing"
    );
    let direct_error = direct_text
        .strip_prefix("drawn: [s1, s2]; stopped: ")
        .and_then(|text| text.strip_suffix(DRAWN_RETRY))
        .expect("the streamed prefix and its retry note");
    let mcp_error = mcp_text
        .strip_prefix("drawn: []; stopped: ")
        .and_then(|text| text.strip_suffix(RETRY))
        .expect("no prefix and its retry note");
    assert_eq!(direct_error, mcp_error, "the same typed error");
    assert!(
        direct_error.starts_with("step s3: ") && direct_error.ends_with("scene.UnknownField"),
        "{direct_error}"
    );
}

/// A step's write the effects issue to the store.
fn issued(effects: &[Effect]) -> Option<(u64, crate::drawing::Write)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
        _ => None,
    })
}

#[test]
fn a_rebuild_retires_its_old_launchs_draw_once_the_step_it_issued_settles() {
    let scratch = Scratch::new("mcp-retired");
    let mut store = store(&scratch);
    let (mut core, build) = offering(drawing());
    assert!(bound(&mut core, build, L1));
    let (dispatch, effects) = mcp(&mut core, (build, L1, 1), ToolName::Draw, &args(None));
    assert_eq!(dispatch, Dispatch::Committing);
    let (serial, lookup) = issued(&effects).expect("the scene's lookup");
    let reply = store.drawing_write(&lookup).expect("durable lookup");
    let looked = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    let looked = super::prepared(&mut core, looked);
    let (serial, write) = issued(&looked).expect("the first step's write");
    assert!(
        matches!(write, crate::drawing::Write::Step { .. }),
        "a step, held unacknowledged"
    );

    let mut rebuilt = Vec::new();
    core.build(R, 1, &mut rebuilt);
    assert_eq!(issued(&rebuilt), None);
    let reply = store.drawing_write(&write).expect("durable write");
    let settled = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    let settled = super::prepared(&mut core, settled);
    assert_eq!(issued(&settled), None, "no later old step is submitted");
    assert_eq!(steps(&store), [1], "the issued step settled once");
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Err(super::Stale));

    let next = core
        .current
        .as_ref()
        .expect("run")
        .build
        .as_ref()
        .expect("build")
        .key();
    core.handle(built(next, drawing())).expect("admitted");
    assert!(bound(&mut core, next, L2));
    assert_eq!(
        core.take_mcp_answer(next, L2, &key(1)),
        Ok(None),
        "no old answer reaches the replacement"
    );
}

#[test]
fn stale_work_at_the_limit_never_ends_the_run_on_its_budget() {
    let (mut core, build) = offering(probing());
    assert!(bound(&mut core, build, L1));
    for n in 1..=16 {
        assert_eq!(probe(&mut core, (build, L1, n)), Dispatch::Committing);
    }
    core.current.as_mut().expect("run").finishing = true;
    assert_eq!(
        mcp(&mut core, (build, L1, 17), ToolName::Probe, PROBE),
        (ended(), Vec::new())
    );
    assert!(core.current.is_some(), "the run goes on to finish");
}

/// Starts `launch`'s two-step draw for `build` and settles the scene's lookup, so the first
/// step's write is issued and held unacknowledged: its serial and write.
fn first_step_held(
    core: &mut Coordinator,
    store: &mut Store,
    (build, launch): (BuildKey, LaunchId),
) -> (u64, crate::drawing::Write) {
    let (dispatch, effects) = mcp(core, (build, launch, 1), ToolName::Draw, &args(None));
    assert_eq!(dispatch, Dispatch::Committing);
    let (serial, lookup) = issued(&effects).expect("the scene's lookup");
    let reply = store.drawing_write(&lookup).expect("durable lookup");
    let looked = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    let looked = super::prepared(core, looked);
    let (serial, write) = issued(&looked).expect("the first step's write");
    assert!(
        matches!(write, crate::drawing::Write::Step { .. }),
        "a step, held unacknowledged"
    );
    (serial, write)
}

/// Makes the held step durable and acknowledges it: what the core does next.
fn settle_held(
    core: &mut Coordinator,
    store: &mut Store,
    (serial, write): (u64, crate::drawing::Write),
) -> Vec<Effect> {
    let reply = store.drawing_write(&write).expect("durable write");
    let settled = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("acknowledgement");
    super::prepared(core, settled)
}

/// The run's current build: its key and whether it's admitted.
fn current_build(core: &Coordinator) -> Option<(BuildKey, bool)> {
    core.current
        .as_ref()
        .and_then(|run| run.build.as_ref())
        .map(|build| (build.key(), build.admitted()))
}

#[test]
fn a_launch_that_loses_its_authority_with_its_build_current_issues_no_further_step() {
    for case in ["failed its audit", "expired"] {
        let scratch = Scratch::new(&format!("mcp-lost-{}", case.len()));
        let mut store = store(&scratch);
        let (mut core, build) = offering(drawing());
        let (gate, _) = passed();
        let left = Duration::from_millis(if case == "expired" { 1_000 } else { 3_600_000 });
        let until = Instant::now() + left;
        let draw = Offer::Mcp(Origin::Task(T), vec![ToolName::Draw]);
        assert!(binds(
            &mut core,
            (build, L1, T),
            draw,
            (gate.clone(), until)
        ));
        let held = first_step_held(&mut core, &mut store, (build, L1));
        match case {
            "expired" => std::thread::sleep(until.saturating_duration_since(Instant::now())),
            _ => gate.fail(),
        }
        let settled = settle_held(&mut core, &mut store, held);
        assert_eq!(issued(&settled), None, "{case}: no later step is submitted");
        assert_eq!(steps(&store), [1], "{case}: the issued step settled once");
        assert_eq!(
            core.take_mcp_answer(build, L1, &key(1)),
            Err(super::Stale),
            "{case}: no reply"
        );
        assert_eq!(
            current_build(&core),
            Some((build, true)),
            "{case}: the build stays current"
        );
    }
}

#[test]
fn a_source_deleted_before_the_held_step_settles_issues_no_further_step_and_no_reply() {
    let scratch = Scratch::new("mcp-voided");
    let mut store = store(&scratch);
    let (mut core, build) = offering_from(drawing(), vec![T, OTHER]);
    assert!(bound(&mut core, build, L1));
    let held = first_step_held(&mut core, &mut store, (build, L1));
    core.handle(Input::Delete {
        thread: OTHER,
        after_turns: 1,
        resend: false,
    })
    .expect("a delete");
    assert_eq!(current_build(&core), Some((build, true)), "nothing rebuilt");
    let settled = settle_held(&mut core, &mut store, held);
    assert_eq!(issued(&settled), None, "no later step is submitted");
    assert_eq!(steps(&store), [1], "the issued step settled once");
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Err(super::Stale));
}

#[test]
fn a_rebuild_blocked_on_a_pending_delete_issues_no_further_step_of_its_launch() {
    let scratch = Scratch::new("mcp-pending-delete");
    let mut store = store(&scratch);
    let (mut core, build) = offering_from(drawing(), vec![T, OTHER]);
    assert!(bound(&mut core, build, L1));
    let held = first_step_held(&mut core, &mut store, (build, L1));
    core.handle(Input::Delete {
        thread: OTHER,
        after_turns: 1,
        resend: false,
    })
    .expect("a delete");
    let model = Caller::Model {
        run: R,
        call: CallId::new("after").expect("ASCII"),
    };
    let dispatch = core.tool_call(model, ToolName::Draw, &args(None), &mut Vec::new());
    assert_eq!(dispatch, ended());
    assert_eq!(
        current_build(&core),
        Some((build, false)),
        "the rebuild waits for the delete"
    );
    let settled = settle_held(&mut core, &mut store, held);
    assert_eq!(issued(&settled), None, "no later step is submitted");
    assert_eq!(steps(&store), [1], "the issued step settled once");
    assert_eq!(core.take_mcp_answer(build, L1, &key(1)), Err(super::Stale));
}

#[test]
fn a_tool_its_launch_doesnt_offer_is_refused_before_any_charge_or_replay() {
    let (mut core, build) = offering(probing());
    let draw_only = Offer::Mcp(Origin::Task(T), vec![ToolName::Draw]);
    assert!(binds(&mut core, (build, L1, T), draw_only, passed()));
    let over = vec![b' '; crate::dispatch::MAX_ARGS + 1];
    let too_large = Dispatch::Answered(ToolOutput::Error(ToolError::TooLarge));
    let not_here = Dispatch::Answered(ToolOutput::Error(ToolError::NotHere));
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Draw, &over),
        (too_large.clone(), Vec::new())
    );
    assert_eq!(
        mcp(&mut core, (build, L1, 1), ToolName::Probe, PROBE),
        (not_here.clone(), Vec::new()),
        "refused before its key's refusal replays"
    );
    for n in 2..=17 {
        assert_eq!(
            mcp(&mut core, (build, L1, n), ToolName::Probe, PROBE),
            (not_here.clone(), Vec::new()),
            "call {n}"
        );
    }
    assert_eq!(
        mcp(&mut core, (build, L1, 18), ToolName::Draw, &over),
        (too_large, Vec::new()),
        "no refusal was charged"
    );
}

/// Each transition in the core that ends a binding fails its launch's gate in that transition,
/// before any later input, so the listener stops releasing at once.
#[test]
fn a_transition_that_ends_a_binding_fails_its_gate_before_the_next_input() {
    let cases = [
        "next round",
        "source deleted",
        "finished with a step pending",
        "finished",
        "stopped",
    ];
    for case in cases {
        let scratch = Scratch::new(&format!("mcp-raise-{}", case.len()));
        let mut store = store(&scratch);
        let (mut core, build) = offering_from(drawing(), vec![T, OTHER]);
        let (gate, until) = passed();
        let draw = Offer::Mcp(Origin::Task(T), vec![ToolName::Draw]);
        assert!(
            binds(&mut core, (build, L1, T), draw, (gate.clone(), until)),
            "{case}"
        );
        match case {
            "next round" => core.build(R, 1, &mut Vec::new()),
            "source deleted" => {
                core.handle(Input::Delete {
                    thread: OTHER,
                    after_turns: 1,
                    resend: false,
                })
                .expect("a delete");
            }
            "finished with a step pending" => {
                first_step_held(&mut core, &mut store, (build, L1));
                streamed(&mut core, TurnEvent::Finished(Finish::Completed));
            }
            "finished" => {
                streamed(&mut core, TurnEvent::Finished(Finish::Completed));
            }
            _ => {
                core.handle(Input::Stop {
                    run: R,
                    revealed: Vec::new(),
                })
                .expect("current");
            }
        }
        assert!(!gate.may_call(), "{case}");
    }
}

/// After sixteen charged calls, a fresh key from a launch that lost its authority, its gate
/// failed by a withdrawal, its deadline passed or its build's control cancelled, gets exactly
/// `Ended`: no budget end, nothing recorded and nothing written.
#[test]
fn at_the_limit_a_fresh_key_after_a_lost_authority_is_refused_uncharged() {
    for case in ["withdrawn", "expired", "build cancelled"] {
        let (mut core, build) = offering(probing());
        let (gate, _) = passed();
        let left = Duration::from_millis(if case == "expired" { 1_000 } else { 3_600_000 });
        let probe_only = Offer::Mcp(Origin::Task(T), vec![ToolName::Probe]);
        let bound = (gate.clone(), Instant::now() + left);
        assert!(binds(&mut core, (build, L1, T), probe_only, bound));
        for n in 1..=16 {
            assert_eq!(
                probe(&mut core, (build, L1, n)),
                Dispatch::Committing,
                "{case}: call {n}"
            );
        }
        let run = core.current.as_ref().expect("run");
        match case {
            "withdrawn" => gate.fail(),
            "expired" => std::thread::sleep(left),
            _ => run.build.as_ref().expect("build").control.0.cancel(),
        }
        assert_eq!(
            mcp(&mut core, (build, L1, 17), ToolName::Probe, PROBE),
            (ended(), Vec::new()),
            "{case}"
        );
        let run = core.current.as_ref().expect("no budget end");
        assert!(!run.ledger.has(&caller(L1, 17).key()), "{case}");
    }
}

/// A binding releases by the earliest of its launch's, its question's and its build's deadlines.
#[test]
fn a_binding_releases_by_the_earliest_of_its_deadlines() {
    let draw = Offer::Mcp(Origin::Task(T), vec![ToolName::Draw]);
    let (mut core, build) = offering(probing());
    let end = core
        .current
        .as_ref()
        .and_then(|run| run.build.as_ref())
        .expect("build")
        .control
        .0
        .end;
    let sooner = Instant::now() + Duration::from_secs(1);
    assert!(sooner < end, "the build's deadline is further off");
    for (until, earliest) in [(end + Duration::from_secs(1), end), (sooner, sooner)] {
        let bind = Bind {
            build,
            launch: L1,
            offer: draw.clone(),
            until,
            reply: tokio::sync::oneshot::channel().0,
        };
        assert_eq!(core.mcp_bound(&bind, T, AuditGate::new()), Some(earliest));
        core.current.as_mut().expect("run").mcp = None;
    }
}

/// A run that finishes while its speech still drains revokes its launch's binding as it starts
/// draining, not once the speech ends.
#[test]
fn a_run_draining_its_speech_after_it_finished_fails_its_gate_at_once() {
    let mut core = super::spoken();
    let (gate, until) = passed();
    let build = super::engine_key(&core, R);
    let offer = Offer::Mcp(Origin::Task(T), Vec::new());
    assert!(binds(
        &mut core,
        (build, L1, T),
        offer,
        (gate.clone(), until)
    ));
    streamed(&mut core, TurnEvent::TextDelta("Hello there. ".to_owned()));
    streamed(&mut core, TurnEvent::Finished(Finish::Completed));
    assert_eq!(core.current_run(), Some(R), "still draining its speech");
    assert!(!gate.may_call());
}
