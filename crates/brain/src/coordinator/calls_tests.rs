//! Tool calls through the coordinator: run resolution, MCP authority and acknowledgements.

use kumochi_protocol::message::{RunId, ThreadId};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, Stale, StoreOp, T, fresh};
use crate::bridge::{CallKey, LaunchId};
use crate::context::{Built, ModelRequest};
use crate::dispatch::{Caller, Dispatch, Route, ToolError, ToolOutput, schemas};
use crate::engines::{CallId, ToolName};
use crate::store::{Failure, RunEnd};

const L1: LaunchId = LaunchId(1);
const L2: LaunchId = LaunchId(2);
const ARGS: &[u8] = br#"{"word":"hi"}"#;

/// Starts run `run`'s turn in `thread` (a new one when `None`) and admits a build that offers
/// the test tools.
fn running(core: &mut Coordinator, run: RunId, thread: Option<ThreadId>) {
    core.handle(Input::Turn {
        turn: 1,
        thread,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    let effects = core.handle(Input::TurnStored { run }).expect("current");
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("a build starts");
    core.handle(offering(key)).expect("admitted");
}

fn offering(key: BuildKey) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: None,
        result: Ok(Built {
            request: ModelRequest {
                choice: None,
                instructions: "built".to_owned(),
                input: Vec::new(),
                tools: schemas(crate::context::Origin::Task(T), Route::Model),
            },
            sources: vec![T],
        }),
    })
}

fn model(run: RunId, id: &str) -> Caller {
    Caller::Model {
        run,
        call: CallId::new(id).expect("short ASCII"),
    }
}

fn mcp(launch: LaunchId) -> Caller {
    Caller::Mcp {
        run: R,
        launch,
        key: CallKey("m:n:1".to_owned()),
    }
}

fn ended() -> Dispatch {
    Dispatch::Answered(ToolOutput::Error(ToolError::Ended))
}

fn probed() -> ToolOutput {
    ToolOutput::Text("probed hi".to_owned())
}

/// Sends a call, returning what it came to and whether it sent a write.
fn call(core: &mut Coordinator, caller: Caller) -> (Dispatch, bool) {
    let mut effects = Vec::new();
    let dispatch = core.tool_call(caller, ToolName::Probe, ARGS, &mut effects);
    let wrote = match effects.as_slice() {
        [] => false,
        [Effect::Store(StoreOp::CommitCall(_))] => true,
        other => panic!("unexpected effects {other:?}"),
    };
    (dispatch, wrote)
}

fn answered(core: &Coordinator) -> &[(Caller, ToolOutput)] {
    &core.current.as_ref().expect("a current run").answered
}

fn ack(caller: &Caller) -> Input {
    Input::CallCommitted {
        key: caller.key(),
        committed: true,
    }
}

fn refused(caller: &Caller) -> Input {
    Input::CallCommitted {
        key: caller.key(),
        committed: false,
    }
}

#[test]
fn a_call_is_answered_only_on_its_writes_acknowledgement() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        call(&mut core, model(R, "c1")),
        (Dispatch::Committing, true)
    );
    assert_eq!(call(&mut core, model(R, "c1")), (Dispatch::Joined, false));
    assert_eq!(answered(&core), []);
    assert_eq!(core.handle(ack(&model(R, "c1"))), Ok(Vec::new()));
    assert_eq!(
        answered(&core),
        [(model(R, "c1"), probed()), (model(R, "c1"), probed())]
    );
    assert_eq!(
        call(&mut core, model(R, "c1")),
        (Dispatch::Answered(probed()), false)
    );
}

#[test]
fn a_call_before_the_build_is_admitted_runs_nothing() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("turn");
    assert_eq!(call(&mut core, model(R, "c1")), (ended(), false));
}

#[test]
fn a_queued_call_from_a_replaced_run_runs_nothing_in_the_new_one() {
    let mut core = fresh();
    running(&mut core, R, None);
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("stop");
    let next = RunId(R.0 + 1);
    running(&mut core, next, Some(T));
    assert_eq!(call(&mut core, model(R, "c1")), (ended(), false));
    assert_eq!(
        call(&mut core, model(next, "c1")),
        (Dispatch::Committing, true),
        "the old run's call left no entry in the new run's ledger"
    );
}

#[test]
fn a_call_after_its_threads_delete_was_accepted_runs_nothing() {
    let mut core = fresh();
    running(&mut core, R, None);
    core.handle(Input::Delete {
        thread: T,
        after_turns: 1,
        resend: false,
    })
    .expect("delete");
    assert_eq!(call(&mut core, model(R, "c1")), (ended(), false));
}

#[test]
fn an_mcp_call_needs_the_launch_authorized_now() {
    let mut core = fresh();
    running(&mut core, R, None);
    let refused = Dispatch::Answered(ToolOutput::Error(ToolError::NotHere));
    assert_eq!(
        call(&mut core, mcp(L1)),
        (refused.clone(), false),
        "none bound"
    );
    core.current.as_mut().expect("current").mcp = Some(L2);
    assert_eq!(call(&mut core, mcp(L1)), (refused, false), "another launch");
    core.current.as_mut().expect("current").mcp = Some(L1);
    assert_eq!(call(&mut core, mcp(L1)), (Dispatch::Committing, true));
}

#[test]
fn a_write_accepted_before_revocation_lands_but_answers_nobody() {
    let mut core = fresh();
    running(&mut core, R, None);
    core.current.as_mut().expect("current").mcp = Some(L1);
    assert_eq!(call(&mut core, mcp(L1)), (Dispatch::Committing, true));
    core.current.as_mut().expect("current").mcp = None;
    assert_eq!(core.handle(ack(&mcp(L1))), Ok(Vec::new()));
    assert_eq!(answered(&core), []);
}

#[test]
fn an_acknowledgement_after_stop_changes_nothing() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        call(&mut core, model(R, "c1")),
        (Dispatch::Committing, true)
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("stop");
    assert_eq!(core.handle(ack(&model(R, "c1"))), Err(Stale));
    assert_eq!(core.handle(refused(&model(R, "c1"))), Err(Stale));
}

#[test]
fn an_old_runs_acknowledgement_never_answers_the_same_id_in_the_next_run() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        call(&mut core, model(R, "c1")),
        (Dispatch::Committing, true)
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("stop");
    let next = RunId(R.0 + 1);
    running(&mut core, next, Some(T));
    assert_eq!(
        call(&mut core, model(next, "c1")),
        (Dispatch::Committing, true)
    );
    assert_eq!(core.handle(ack(&model(R, "c1"))), Err(Stale));
    assert_eq!(answered(&core), []);
}

#[test]
fn a_failed_write_fails_the_run_with_a_store_failure() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        call(&mut core, model(R, "c1")),
        (Dispatch::Committing, true)
    );
    let effects = core.handle(refused(&model(R, "c1"))).expect("current");
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::EndRun {
                end: RunEnd::Failed(Failure::Store),
                ..
            })
        )),
        "{effects:?}"
    );
    assert_eq!(core.current_run(), None);
}
