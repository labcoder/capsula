//! Tool calls through the coordinator: run resolution, MCP authority and acknowledgements.

use kumochi_protocol::message::{ControlToShell, Deleted, RunId, ThreadId};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, Stale, StoreOp, T, fresh};
use crate::bridge::{CallKey, LaunchId};
use crate::context::{Built, ModelRequest, Origin};
use crate::coordinator::{BuildContext, LeaseId, SendKey};
use crate::dispatch::{
    Caller, Committed, Dispatch, LocalWrite, Route, Then, ToolError, ToolOutput, schemas,
};
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
    rendering(key, Origin::Task(T), vec![T])
}

/// `key`'s build, offering `reach`'s test tools and rendered from `sources`.
fn rendering(key: BuildKey, reach: Origin, sources: Vec<ThreadId>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: None,
        result: Ok(Built {
            request: ModelRequest {
                choice: None,
                instructions: "built".to_owned(),
                input: Vec::new(),
                tools: schemas(reach, Route::Model),
            },
            sources,
        }),
    })
}

const KUMO: ThreadId = ThreadId(1);
const B: ThreadId = ThreadId(2);
/// B's text, which Kumo's response carries into a call that names no B id.
const FROM_B: &[u8] = br#"{"word":"zqxfromtaskb","title":"zqxfromtaskb"}"#;

fn build_key(attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt,
    }
}

/// Kumo's run 10, whose admitted build rendered task B and whose request was sent.
fn kumo_rendered_b() -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(KUMO),
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(rendering(build_key(1), Origin::Kumo, vec![KUMO, B]))
        .expect("admitted");
    let send = SendKey::Build(build_key(1));
    core.handle(Input::SendReady {
        key: send,
        hold: None,
    })
    .expect("granted");
    core.handle(Input::SendSettled {
        lease: LeaseId { key: send, send: 1 },
    })
    .expect("settled");
    core
}

fn delete_b(core: &mut Coordinator) {
    assert_eq!(
        core.handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false,
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: B })])
    );
}

fn b_hidden() -> Input {
    Input::Deleted {
        thread: B,
        outcome: Deleted::Hidden,
        last_run: None,
    }
}

/// Kumo's round built again, from the snapshot it never read.
fn rebuilt(attempt: u64) -> Effect {
    Effect::Store(StoreOp::BuildContext(BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: build_key(attempt),
        thread: KUMO,
        origin: Origin::Kumo,
        snapshot: None,
        first: true,
    }))
}

fn call_from_b(core: &mut Coordinator) -> (Dispatch, Vec<Effect>) {
    let mut effects = Vec::new();
    let dispatch = core.tool_call(model(R, "c1"), ToolName::Probe, FROM_B, &mut effects);
    (dispatch, effects)
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
        outcome: Committed::Done,
        then: None,
    }
}

fn refused(caller: &Caller) -> Input {
    Input::CallCommitted {
        key: caller.key(),
        outcome: Committed::Failed,
        then: None,
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
fn a_call_from_a_build_that_rendered_a_task_being_deleted_runs_nothing() {
    let mut core = kumo_rendered_b();
    delete_b(&mut core);
    assert_eq!(
        call_from_b(&mut core),
        (ended(), vec![Effect::CancelEngine { run: R }]),
        "the rebuild waits for B's delete"
    );
    assert_eq!(
        core.handle(b_hidden()),
        Ok(vec![
            rebuilt(2),
            Effect::Control(ControlToShell::ThreadDeleted {
                thread: B,
                outcome: Deleted::Hidden,
                last_run: None,
            })
        ])
    );
    assert_eq!(call_from_b(&mut core), (ended(), Vec::new()));
    assert_eq!(answered(&core), []);
}

#[test]
fn a_call_from_a_build_that_rendered_a_task_deleted_since_runs_nothing() {
    let mut core = kumo_rendered_b();
    delete_b(&mut core);
    core.handle(b_hidden()).expect("answered");
    assert_eq!(
        call_from_b(&mut core),
        (ended(), vec![Effect::CancelEngine { run: R }, rebuilt(2)])
    );
    assert_eq!(call_from_b(&mut core), (ended(), Vec::new()));
    assert_eq!(answered(&core), []);
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

const TO_KUMO: &[u8] = br#"{"task":1}"#;
const TO_B: &[u8] = br#"{"task":2}"#;

/// Sends a focus call, returning what it came to and the follow-up its write carries.
fn focusing(core: &mut Coordinator, caller: Caller, args: &[u8]) -> (Dispatch, Option<Then>) {
    let mut effects = Vec::new();
    let dispatch = core.tool_call(caller, ToolName::Focus, args, &mut effects);
    let then = match effects.as_slice() {
        [] => None,
        [Effect::Store(StoreOp::CommitCall(call))] => call.then,
        other => panic!("unexpected effects {other:?}"),
    };
    (dispatch, then)
}

fn settled(caller: &Caller, outcome: Committed, thread: ThreadId) -> Input {
    Input::CallCommitted {
        key: caller.key(),
        outcome,
        then: Some(Then::Focus(thread)),
    }
}

fn moved(thread: ThreadId) -> Vec<Effect> {
    vec![Effect::Control(ControlToShell::Focus { run: R, thread })]
}

fn focused() -> ToolOutput {
    ToolOutput::Text(
        "The user's next message should go to that task, unless they have already moved \
         elsewhere."
            .to_owned(),
    )
}

#[test]
fn a_committed_focus_moves_the_input_once_and_never_after_stop() {
    let mut core = fresh();
    running(&mut core, R, None);
    let caller = model(R, "f1");
    assert_eq!(
        focusing(&mut core, caller.clone(), TO_KUMO),
        (Dispatch::Committing, Some(Then::Focus(KUMO)))
    );
    assert_eq!(
        core.handle(settled(&caller, Committed::Done, KUMO)),
        Ok(moved(KUMO))
    );
    assert_eq!(answered(&core), [(caller.clone(), focused())]);
    assert_eq!(
        core.handle(settled(&caller, Committed::Done, KUMO)),
        Ok(Vec::new()),
        "a second acknowledgement moves nothing"
    );

    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        focusing(&mut core, caller.clone(), TO_KUMO).0,
        Dispatch::Committing
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("stop");
    assert_eq!(
        core.handle(settled(&caller, Committed::Done, KUMO)),
        Err(Stale)
    );
}

#[test]
fn a_task_focuses_only_kumo_and_kumo_focuses_a_task() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        focusing(&mut core, model(R, "f1"), TO_B),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::NotHere)),
            None
        )
    );
    let mut core = kumo_rendered_b();
    assert_eq!(
        focusing(&mut core, model(R, "f1"), TO_B),
        (Dispatch::Committing, Some(Then::Focus(B)))
    );
}

#[test]
fn a_refused_focus_answers_every_caller_stays_refused_and_the_run_goes_on() {
    let mut core = fresh();
    running(&mut core, R, None);
    let caller = model(R, "f1");
    assert_eq!(
        focusing(&mut core, caller.clone(), TO_KUMO).0,
        Dispatch::Committing
    );
    assert_eq!(
        focusing(&mut core, caller.clone(), TO_KUMO),
        (Dispatch::Joined, None)
    );
    let no_task = ToolOutput::Error(ToolError::NoTask);
    assert_eq!(
        core.handle(settled(
            &caller,
            Committed::Refused(ToolError::NoTask),
            KUMO
        )),
        Ok(Vec::new())
    );
    assert_eq!(
        answered(&core),
        [
            (caller.clone(), no_task.clone()),
            (caller.clone(), no_task.clone())
        ]
    );
    assert_eq!(core.current_run(), Some(R));
    assert_eq!(
        focusing(&mut core, caller.clone(), TO_KUMO),
        (Dispatch::Answered(no_task), None)
    );
    assert_eq!(
        core.handle(settled(&caller, Committed::Done, KUMO)),
        Ok(Vec::new()),
        "a late acknowledgement can't undo the refusal"
    );
}

const NEW_TASK: &[u8] = br#"{"title":"Trip plan","focus":true}"#;
const QUIET_TASK: &[u8] = br#"{"title":"Trip plan","focus":false}"#;
const CREATED: ThreadId = ThreadId(4);

/// Sends a `create_task` call, returning what it came to and, when it sent a write, the id it
/// reserved and the follow-up it carries.
fn creating(
    core: &mut Coordinator,
    caller: Caller,
    args: &[u8],
) -> (Dispatch, Option<(ThreadId, Option<Then>)>) {
    let mut effects = Vec::new();
    let dispatch = core.tool_call(caller, ToolName::CreateTask, args, &mut effects);
    let write = match effects.as_slice() {
        [] => None,
        [Effect::Store(StoreOp::CommitCall(call))] => match &call.write {
            LocalWrite::CreateTask { thread, .. } => Some((*thread, call.then)),
            other => panic!("unexpected write {other:?}"),
        },
        other => panic!("unexpected effects {other:?}"),
    };
    (dispatch, write)
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the tool's own focus argument"
)]
fn created(caller: &Caller, focus: bool) -> Input {
    Input::CallCommitted {
        key: caller.key(),
        outcome: Committed::Done,
        then: Some(Then::Created {
            thread: CREATED,
            focus,
        }),
    }
}

/// The thread the next ordinary new task's turn is stored under.
fn next_new_task(core: &mut Coordinator) -> ThreadId {
    let effects = core
        .handle(Input::Turn {
            turn: 2,
            thread: None,
            text: "another".into(),
        })
        .expect("a turn is never stale");
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BeginTurn { thread, .. }) => Some(*thread),
            _ => None,
        })
        .expect("the turn is stored")
}

#[test]
fn only_kumo_creates_a_task_at_the_next_id_and_the_next_new_task_follows_it() {
    let mut core = fresh();
    running(&mut core, R, None);
    assert_eq!(
        creating(&mut core, model(R, "c1"), NEW_TASK),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::NotHere)),
            None
        )
    );

    let mut core = kumo_rendered_b();
    let caller = model(R, "c1");
    assert_eq!(
        creating(&mut core, caller.clone(), NEW_TASK),
        (
            Dispatch::Committing,
            Some((
                CREATED,
                Some(Then::Created {
                    thread: CREATED,
                    focus: true
                })
            ))
        )
    );
    assert_eq!(
        core.handle(created(&caller, true)),
        Ok(vec![
            Effect::Control(ControlToShell::ThreadsChanged),
            Effect::Control(ControlToShell::Focus {
                run: R,
                thread: CREATED
            }),
        ])
    );
    assert_eq!(
        answered(&core),
        [(
            caller,
            ToolOutput::Text(
                "Created task 4. The user's next message should go to it, unless they have \
                 already moved elsewhere."
                    .to_owned()
            )
        )]
    );
    assert_eq!(next_new_task(&mut core), ThreadId(5));
}

#[test]
fn a_created_id_is_spent_before_its_write_is_acknowledged() {
    let mut core = kumo_rendered_b();
    assert_eq!(
        creating(&mut core, model(R, "c1"), QUIET_TASK)
            .1
            .map(|(thread, _)| thread),
        Some(CREATED)
    );
    assert_eq!(
        next_new_task(&mut core),
        ThreadId(5),
        "a new task typed while the create is in flight takes the id after it"
    );
}

#[test]
fn a_quiet_or_late_creation_lists_the_task_and_moves_nothing() {
    let mut core = kumo_rendered_b();
    let caller = model(R, "c1");
    assert_eq!(
        creating(&mut core, caller.clone(), QUIET_TASK).0,
        Dispatch::Committing
    );
    assert_eq!(
        core.handle(created(&caller, false)),
        Ok(vec![Effect::Control(ControlToShell::ThreadsChanged)])
    );

    let mut core = kumo_rendered_b();
    assert_eq!(
        creating(&mut core, caller.clone(), NEW_TASK).0,
        Dispatch::Committing
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("stop");
    assert_eq!(
        core.handle(created(&caller, true)),
        Ok(vec![Effect::Control(ControlToShell::ThreadsChanged)]),
        "the task exists, but its run no longer moves the input"
    );
}

#[test]
fn a_title_must_be_one_line_of_1_to_80_characters_and_a_refusal_reserves_nothing() {
    let mut core = kumo_rendered_b();
    let args = |title: &str| format!(r#"{{"title":"{title}","focus":false}}"#).into_bytes();
    let invalid = Dispatch::Answered(ToolOutput::Error(ToolError::Invalid));
    for (call, title) in [
        ("c1", String::new()),
        ("c2", "a".repeat(81)),
        ("c3", "a\\nb".to_owned()),
    ] {
        assert_eq!(
            creating(&mut core, model(R, call), &args(&title)),
            (invalid.clone(), None),
            "{title:?}"
        );
    }
    assert_eq!(
        creating(&mut core, model(R, "c4"), &args(&"a".repeat(80))).1,
        Some((
            CREATED,
            Some(Then::Created {
                thread: CREATED,
                focus: false
            })
        ))
    );
}

#[test]
fn a_created_tasks_title_never_shows_in_a_carrier() {
    let mut core = kumo_rendered_b();
    let mut effects = Vec::new();
    core.tool_call(
        model(R, "c1"),
        ToolName::CreateTask,
        br#"{"title":"Zqxsecrettitle","focus":true}"#,
        &mut effects,
    );
    let shown = format!("{effects:?}");
    assert!(shown.contains("CreateTask"), "{shown}");
    let bytes = format!("{:?}", b"Zqxsecrettitle");
    for marker in ["Zqxsecrettitle", &bytes[1..bytes.len() - 1]] {
        assert!(!shown.contains(marker), "{shown}");
    }
}
