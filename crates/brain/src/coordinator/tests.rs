use kumochi_protocol::message::{
    Content, ControlToShell, DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, SceneId, Step,
    ThreadId,
};

use super::{
    BuildContext, BuildKey, ContextBuilt, Coordinator, Effect, Input, MAX_DELTA, MAX_TEXT, SendKey,
    Stale, StoreOp, WALL_CLOCK,
};
use crate::context::{Built, ModelRequest, Origin};
use crate::engines::{CallId, Finish, RunRequest, ToolName, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd, SceneTarget};

#[path = "admission_tests.rs"]
mod admission;
#[path = "ask_tests.rs"]
mod asks;
#[path = "availability_tests.rs"]
mod availability;
#[path = "calls_tests.rs"]
mod calls;
#[cfg(windows)]
#[path = "claude_hold_tests.rs"]
mod claude_hold;
#[path = "delete_tests.rs"]
mod deletes;
#[path = "draw_tool_tests.rs"]
mod draw_tool;
#[path = "drawing_tests.rs"]
mod drawing;
#[path = "engine_tests.rs"]
mod engines;
#[path = "history_failure_tests.rs"]
mod history_failure;
#[path = "hold_tests.rs"]
mod hold;
#[path = "inline_tests.rs"]
mod inline;
#[path = "job_tests.rs"]
mod jobs;
#[path = "lease_tests.rs"]
mod leases;
#[path = "mcp_tests.rs"]
pub(super) mod mcp;
#[path = "route_tests.rs"]
pub(super) mod routes;
#[path = "scene_tests.rs"]
mod scenes;
#[path = "send_fence_tests.rs"]
mod send_fence;
#[path = "speech_tests.rs"]
mod speech;
#[path = "thread_op_tests.rs"]
mod thread_ops;
#[path = "tools_tests.rs"]
mod tools;

const T: ThreadId = ThreadId(4);
const R: RunId = RunId(10);
/// The scene run 10's first step begins.
const S: SceneId = SceneId(50);

impl Coordinator {
    /// Drives one send when the fixture has no store behind the coordinator.
    pub(crate) fn handle_with_screen_grant(&mut self, input: Input) -> Result<Vec<Effect>, Stale> {
        let effects = self.handle(input)?;
        if let [
            Effect::Store(StoreOp::GrantScreen { lease }),
            Effect::ArmSendDeadline { .. },
        ] = effects.as_slice()
        {
            return self.handle(Input::ScreenGrantStored {
                lease: *lease,
                result: Ok(()),
            });
        }
        Ok(effects)
    }
}

fn step(label: &str) -> Step {
    Step {
        label: label.to_owned(),
    }
}

fn state(run: RunId, thread: ThreadId, phase: Phase) -> Effect {
    Effect::Control(ControlToShell::RunState {
        run,
        thread,
        phase,
        error: None,
        origin: (phase == Phase::Thinking)
            .then_some(kumochi_protocol::message::RunOrigin::Typed { turn: 1 }),
    })
}

fn engine_key(core: &Coordinator, run: RunId) -> BuildKey {
    core.current
        .iter()
        .chain(core.ending.iter())
        .find(|held| held.id == run)
        .and_then(|held| held.build.as_ref())
        .expect("the fixture retained the actual build")
        .key()
}

fn engine(key: BuildKey, event: TurnEvent) -> Input {
    Input::Engine { key, event }
}

/// The draw call every test run's steps stream in.
fn call() -> CallId {
    CallId::new("call-1").expect("short ASCII")
}

fn draw_call() -> TurnEvent {
    TurnEvent::ToolCallStarted {
        call: call(),
        name: ToolName::Draw,
    }
}

/// One step as a closed line of the draw call's arguments.
fn delta(label: &str) -> TurnEvent {
    TurnEvent::ToolArgsDelta {
        call: call(),
        delta: format!("{label}\n"),
    }
}

/// Streams one step to `run`, starting its draw call first if it isn't open yet.
fn draw_step(core: &mut Coordinator, run: RunId, label: &str) -> Result<Vec<Effect>, Stale> {
    let open = core
        .current
        .as_ref()
        .is_some_and(|current| current.id == run && current.draw.is_some());
    let mut effects = Vec::new();
    if !open {
        effects = core.handle(engine(engine_key(core, run), draw_call()))?;
    }
    effects.extend(core.handle(engine(engine_key(core, run), delta(label)))?);
    Ok(effects)
}

/// What every test build sends.
fn request() -> ModelRequest {
    ModelRequest {
        choice: None,
        instructions: "built".to_owned(),
        input: Vec::new(),
        tools: Vec::new(),
    }
}

/// Attempt `attempt` of `run`'s first round in task `thread`, before the run read its snapshot.
fn build_context(run: RunId, thread: ThreadId, attempt: u64) -> Effect {
    Effect::Store(StoreOp::BuildContext(BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: BuildKey {
            run,
            round: 1,
            attempt,
        },
        thread,
        origin: Origin::Task(thread),
        snapshot: None,
        first: true,
        arrived: crate::coordinator::Arrived::of(
            kumochi_protocol::message::ConnectionKind::ChatGpt,
            crate::context::ConnectionTag(0),
        ),
        drawings: crate::drawing::Digest::default(),
        picture_models: Vec::new(),
        key_results: Vec::new(),
    }))
}

/// That build's request, rendered from `sources`.
fn built(run: RunId, attempt: u64, sources: &[ThreadId]) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key: BuildKey {
            run,
            round: 1,
            attempt,
        },
        snapshot: None,
        result: Ok(Built {
            request: request(),
            sources: sources.to_vec(),
        }),
    })
}

fn start_engine(run: RunId, thread: ThreadId, attempt: u64) -> Effect {
    Effect::StartEngine {
        request: RunRequest {
            key: SendKey::Build(BuildKey {
                run,
                round: 1,
                attempt,
            }),
            run,
            thread,
            origin: Origin::Task(thread),
            request: request(),
        },
    }
}

/// Stores `run`'s turn and admits its build, which rendered only the run's own thread.
fn admit(core: &mut Coordinator, run: RunId) -> Vec<Effect> {
    let mut effects = core.handle(Input::TurnStored { run }).expect("current");
    let at = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Store(StoreOp::BuildContext(_))))
        .expect("a build starts");
    let Effect::Store(StoreOp::BuildContext(build)) = effects.remove(at) else {
        unreachable!("matched above");
    };
    let done = built(run, build.key.attempt, &[build.thread]);
    effects.extend(core.handle(done).expect("admitted"));
    effects
}

fn ack(index: u32, label: &str) -> Input {
    Input::StepStored {
        run: R,
        index,
        step: step(label),
        scene: S,
    }
}

fn committed(index: u32, label: &str) -> Effect {
    Effect::Data(DataToShell::StepCommit {
        run: R,
        scene: S,
        index,
        step: step(label),
    })
}

/// Run 10's write of a step that waited for its scene's id.
fn commit_in_scene(text: Option<&str>, index: u32, label: &str) -> Effect {
    Effect::Store(StoreOp::CommitStep {
        thread: T,
        run: R,
        text: text.map(Content::from),
        scene: SceneTarget::Existing(S),
        index,
        step: step(label),
    })
}

/// A coordinator whose last ids were thread 3 and run 9, with nothing visible.
/// The shell double prepares each step a batch committed at once, as a quiet shell does.
fn prepared(core: &mut Coordinator, mut effects: Vec<Effect>) -> Vec<Effect> {
    let ready: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::SceneStepCommit {
                run, scene, index, ..
            }) => Some(crate::drawing::Input::StepReady {
                run: *run,
                scene: *scene,
                index: *index,
            }),
            _ => None,
        })
        .collect();
    for input in ready {
        effects.extend(
            core.handle(Input::Drawing(input))
                .expect("a committed step"),
        );
    }
    effects
}

fn fresh() -> Coordinator {
    Coordinator::ready_for_tests(ThreadId(1), ThreadId(3), RunId(9), Vec::new())
}

fn run_end(run: RunId) -> Effect {
    Effect::Data(DataToShell::RunEnd { run })
}

fn text(delta: &str) -> Effect {
    Effect::Data(DataToShell::AssistantText {
        run: R,
        delta: delta.into(),
    })
}

/// Run 10's outcome in thread 4.
fn end_run(text: Option<&str>, revealed: Option<Vec<u32>>, end: RunEnd) -> Effect {
    Effect::Store(StoreOp::EndRun {
        screen: None,
        thread: T,
        run: R,
        text: text.map(super::ReplyText::from),
        revealed,
        clear: None,
        end,
    })
}

/// Run 10's outcome after a Stop that kept none of its scene's steps, clearing the scene.
fn end_run_clearing(revealed: Vec<u32>, end: RunEnd) -> Effect {
    Effect::Store(StoreOp::EndRun {
        screen: None,
        thread: T,
        run: R,
        text: None,
        revealed: Some(revealed),
        clear: Some(S),
        end,
    })
}

/// The write that replaces an outcome the store refused.
fn failed_store() -> Effect {
    end_run(None, None, RunEnd::Failed(Failure::Store))
}

/// A coordinator whose last ids were thread 3 and run 9, with run 10 started in a new thread 4
/// and its turn stored.
fn started() -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    admit(&mut core, R);
    core
}

/// Run 10 started in a new thread 4, admitted while spoken replies are loaded and the shell has
/// a 24 kHz output.
fn spoken() -> Coordinator {
    let mut core = fresh();
    core.handle(Input::AudioOutput {
        rate: 24_000,
        generation: kumochi_protocol::message::OutputGeneration(1),
    })
    .expect("never stale");
    core.handle(Input::SpokenReplies { loaded: true })
        .expect("never stale");
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    admit(&mut core, R);
    core
}

#[test]
fn a_turn_is_stored_before_the_engine_starts() {
    let mut core = fresh();
    let turn = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "  \n  Draw a cat\nplease".into(),
        })
        .expect("turn");
    assert_eq!(
        turn,
        vec![
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: Some("Draw a cat".into()),
                run: R,
                text: "  \n  Draw a cat\nplease".into(),
                screen: None,
            }),
            state(R, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: R,
                after: WALL_CLOCK
            },
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("current"),
        vec![build_context(R, T, 1)]
    );
    assert_eq!(
        core.handle(built(R, 1, &[T])).expect("current"),
        vec![start_engine(R, T, 1)]
    );
}

#[test]
fn a_step_is_sent_only_once_it_is_committed() {
    let mut core = started();
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("a cat".to_owned())
        ))
        .expect("current"),
        vec![state(R, T, Phase::Answering), text("a cat")]
    );
    assert_eq!(
        draw_step(&mut core, R, "head").expect("current"),
        vec![Effect::Store(StoreOp::CommitStep {
            thread: T,
            run: R,
            text: Some("a cat".into()),
            scene: SceneTarget::New { evict: None },
            index: 0,
            step: step("head"),
        })],
        "the text before the step is stored with it, and nothing reaches the shell yet"
    );
    assert_eq!(
        core.handle(ack(0, "head")).expect("current"),
        vec![committed(0, "head")]
    );
}

#[test]
fn text_waits_behind_a_pending_step() {
    let mut core = started();
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("before".to_owned()),
    ))
    .expect("current");
    assert_eq!(
        draw_step(&mut core, R, "a").expect("current"),
        vec![Effect::Store(StoreOp::CommitStep {
            thread: T,
            run: R,
            text: Some("before".into()),
            scene: SceneTarget::New { evict: None },
            index: 0,
            step: step("a"),
        })]
    );
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("after ".to_owned())
        )),
        Ok(Vec::new()),
        "held behind a"
    );
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("a".to_owned()),
    ))
    .expect("current");
    assert_eq!(
        draw_step(&mut core, R, "b"),
        Ok(Vec::new()),
        "b waits for a's scene id"
    );
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("after b".to_owned())
        )),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(ack(0, "a")).expect("current"),
        vec![
            committed(0, "a"),
            text("after a"),
            commit_in_scene(Some("after a"), 1, "b")
        ],
        "released up to the next pending step, as one frame, and b goes to a's scene"
    );
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("Stop is handled while a step is pending"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ]
    );
    assert_eq!(
        core.handle(ack(1, "b")).expect("still ending"),
        vec![end_run(
            Some("after b"),
            Some(vec![0]),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "the held text is stored but never sent"
    );
}

#[test]
fn stop_tells_the_shell_at_once_and_refuses_everything_late() {
    let mut core = started();
    for event in [
        draw_call(),
        delta("a"),
        delta("b"),
        delta("c"),
        TurnEvent::TextDelta("tail".to_owned()),
    ] {
        core.handle(engine(engine_key(&core, R), event))
            .expect("current");
    }
    core.handle(ack(0, "a")).expect("current");
    core.handle(ack(1, "b")).expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![1, 0, 2, 1, 7],
        })
        .expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ],
        "the outcome waits for the step still in the store"
    );
    let late = [
        engine(engine_key(&core, R), delta("d")),
        engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("more".to_owned()),
        ),
        engine(engine_key(&core, R), TurnEvent::Finished(Finish::Completed)),
        Input::Stop {
            run: R,
            revealed: Vec::new(),
        },
        Input::Deadline { run: R },
        Input::Stalled { run: R },
        Input::RunStored { run: R },
    ];
    for input in late {
        assert_eq!(core.handle(input.clone()), Err(Stale), "{input:?}");
    }
    assert_eq!(
        core.handle(ack(2, "c")).expect("ending"),
        vec![end_run(
            Some("tail"),
            Some(vec![0, 1]),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "nothing more is sent, and revealed keeps only steps the shell got, once each, in order"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
}

#[test]
fn a_stop_before_the_turn_is_stored_ends_it_after_the_turn_and_the_engine_never_starts() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(ThreadId(2)),
        text: "hi".into(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("current"),
        vec![
            Effect::Revoke { run: R },
            state(R, ThreadId(2), Phase::Interrupted),
            run_end(R)
        ],
        "nothing to cancel"
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("ending"),
        vec![Effect::Store(StoreOp::EndRun {
            screen: None,
            thread: ThreadId(2),
            run: R,
            text: None,
            revealed: Some(Vec::new()),
            clear: None,
            end: RunEnd::Interrupted(Interruption::Stop),
        })]
    );
}

#[test]
fn a_new_turn_replaces_the_current_run_with_a_higher_id() {
    let mut core = started();
    let next = core
        .handle(Input::Turn {
            turn: 1,
            thread: Some(T),
            text: "again".into(),
        })
        .expect("turn");
    assert_eq!(
        next[..6],
        [
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(None, None, RunEnd::Interrupted(Interruption::Replaced)),
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: RunId(11),
                text: "again".into(),
                screen: None,
            }),
        ]
    );
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("old".to_owned())
        )),
        Err(Stale)
    );
    admit(&mut core, RunId(11));
    assert_eq!(
        core.handle(engine(
            engine_key(&core, RunId(11)),
            TurnEvent::Finished(Finish::Completed)
        ))
        .expect("current"),
        vec![Effect::Store(StoreOp::EndRun {
            screen: None,
            thread: T,
            run: RunId(11),
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Done,
        })]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: RunId(11) })
            .expect("ending"),
        vec![state(RunId(11), T, Phase::Done), run_end(RunId(11))]
    );
}

#[test]
fn done_is_told_only_once_it_is_stored_and_needs_no_cancel() {
    let mut core = started();
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("x".to_owned()),
    ))
    .expect("current");
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        ))
        .expect("current"),
        vec![end_run(Some("x"), None, RunEnd::Done)]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        }),
        Err(Stale),
        "a Stop for a run that already ended changes nothing"
    );
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()), "no run left");
}

#[test]
fn done_waits_for_the_last_step_to_be_committed_and_sent() {
    let mut core = started();
    draw_step(&mut core, R, "last").expect("current");
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        ))
        .expect("current"),
        Vec::new(),
        "nothing ends while a step is in the store"
    );
    assert_eq!(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("after done".to_owned())
        )),
        Err(Stale)
    );
    assert_eq!(
        core.handle(ack(0, "last")).expect("current"),
        vec![committed(0, "last"), end_run(None, None, RunEnd::Done)]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
}

#[test]
fn a_repeated_step_ack_changes_nothing() {
    let mut core = started();
    draw_step(&mut core, R, "a").expect("current");
    draw_step(&mut core, R, "b").expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(
        core.handle(ack(0, "a")).expect("first ack"),
        vec![committed(0, "a"), commit_in_scene(None, 1, "b")]
    );
    assert_eq!(core.handle(ack(0, "a")), Err(Stale), "a repeat");
    assert_eq!(core.handle(ack(5, "z")), Err(Stale), "out of range");
    assert_eq!(
        core.handle(ack(1, "b")).expect("second ack"),
        vec![committed(1, "b"), end_run(None, None, RunEnd::Done)],
        "the run ends only with the second distinct ack"
    );

    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(ack(0, "early")),
        Err(Stale),
        "before the turn is stored"
    );
}

#[test]
fn a_failed_ending_is_reported_after_done_after_stop_and_after_a_replacement() {
    let mut core = started();
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("x".to_owned()),
    ))
    .expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![state(R, T, Phase::Failed), run_end(R), failed_store()],
        "after Done, the shell is never told Done"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));

    let mut core = started();
    draw_step(&mut core, R, "a").expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: vec![0],
    })
    .expect("current");
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run_clearing(
            Vec::new(),
            RunEnd::Interrupted(Interruption::Stop)
        )]
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![
            state(R, T, Phase::Failed),
            Effect::Store(StoreOp::EndRun {
                screen: None,
                thread: T,
                run: R,
                text: None,
                revealed: Some(Vec::new()),
                clear: Some(S),
                end: RunEnd::Failed(Failure::Store),
            })
        ],
        "after Stop, with its selection, and the scene it kept nothing of is still cleared"
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Ok(vec![Effect::ArmRetry {
            after: std::time::Duration::from_millis(500)
        }]),
        "the failed(store) write failing too is the run's last word, and its clear is retried"
    );
    assert!(!core.settled(), "but its scene's clear is still owed");
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));

    let mut core = started();
    draw_step(&mut core, R, "a").expect("current");
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "again".into(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![
            state(R, T, Phase::Failed),
            failed_store(),
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: RunId(11),
                text: "again".into(),
                screen: None,
            }),
        ],
        "the old run's step failed while its replacement waits"
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: RunId(11) })
            .expect("the replacement is untouched"),
        vec![build_context(RunId(11), T, 2)]
    );
}

#[test]
fn a_store_failure_mid_run_records_failed_store() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Err(Stale),
        "nothing was pending"
    );
    draw_step(&mut core, R, "a").expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
            failed_store(),
        ]
    );
}

#[test]
fn budgets_are_absolute_for_the_run() {
    let mut core = started();
    let half = "x".repeat(MAX_TEXT / 2);
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta(half.clone()),
    ))
    .expect("first half");
    core.handle(engine(engine_key(&core, R), TurnEvent::TextDelta(half)))
        .expect("exactly the cap");
    let over = core
        .handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta("y".to_owned()),
        ))
        .expect("current");
    assert_eq!(
        over[..3],
        [
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R)
        ],
        "the byte past the cap is never sent"
    );
    assert!(matches!(
        &over[3],
        Effect::Store(StoreOp::EndRun {
            text: Some(stored),
            end: RunEnd::Failed(Failure::Budget),
            ..
        }) if stored.len() == MAX_TEXT
    ));

    let mut core = started();
    assert_eq!(
        core.handle(Input::Deadline { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
            end_run(None, None, RunEnd::Failed(Failure::Budget)),
        ]
    );
}

#[test]
fn a_lost_shell_ends_the_run_without_telling_it() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::ShellLost).expect("never stale"),
        vec![
            Effect::CancelEngine { run: R },
            end_run(None, None, RunEnd::Interrupted(Interruption::ShellLost)),
        ]
    );
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("ending"),
        vec![failed_store()],
        "and a failure isn't told either"
    );
}

#[test]
fn a_stalled_view_or_a_refused_turn_ends_the_run() {
    let mut core = started();
    assert_eq!(
        core.handle(Input::Stalled { run: R }).expect("current"),
        vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            end_run(None, None, RunEnd::Interrupted(Interruption::ViewStalled)),
        ]
    );

    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(ThreadId(99)),
        text: "hi".into(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }).expect("current"),
        vec![state(R, ThreadId(99), Phase::Failed), run_end(R)],
        "nothing more is written for a turn the store refused"
    );
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

fn ends_failed(failure: Failure) -> Vec<Effect> {
    vec![
        Effect::CancelEngine { run: R },
        state(R, T, Phase::Failed),
        run_end(R),
        end_run(None, None, RunEnd::Failed(failure)),
    ]
}

#[test]
fn over_budget_steps_and_oversized_labels_commit_and_send_nothing() {
    let mut core = started();
    for index in 0..MAX_STEPS {
        draw_step(&mut core, R, &index.to_string()).expect("within the step budget");
        let index = u32::try_from(index).expect("small");
        core.handle(ack(index, &index.to_string())).expect("stored");
    }
    let over = draw_step(&mut core, R, "one too many").expect("current");
    assert_eq!(over, ends_failed(Failure::Budget));

    let mut core = started();
    let label = "x".repeat(MAX_STEP_LABEL + 1);
    let over = draw_step(&mut core, R, &label).expect("current");
    assert_eq!(over, ends_failed(Failure::Engine));
}

#[test]
fn a_repeated_turn_ack_never_starts_a_second_engine() {
    let mut core = started();
    assert_eq!(core.handle(Input::TurnStored { run: R }), Err(Stale));
}

#[test]
fn a_long_delta_goes_out_in_frames_that_fit_the_data_pipe() {
    let mut core = started();
    // Three-byte characters, so a plain byte split would land inside one.
    let delta = "€".repeat(MAX_DELTA / 3 * 2 + 5);
    let sent = core
        .handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta(delta.clone()),
        ))
        .expect("current");
    let pieces: Vec<&str> = sent
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::AssistantText { delta, .. }) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(pieces.len(), 3);
    assert!(pieces.iter().all(|piece| piece.len() <= MAX_DELTA));
    assert_eq!(pieces.concat(), delta);
}

#[test]
fn a_stop_while_the_last_step_is_stored_interrupts_the_run() {
    let mut core = started();
    draw_step(&mut core, R, "a").expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .expect("still current"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R)
        ]
    );
    assert_eq!(
        core.handle(ack(0, "a")).expect("ending"),
        vec![end_run_clearing(
            Vec::new(),
            RunEnd::Interrupted(Interruption::Stop)
        )],
        "the shell never got the step, so its scene keeps nothing and is cleared"
    );
}

#[test]
fn a_lost_shell_isnt_told_about_runs_still_ending() {
    let mut core = started();
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("x".to_owned()),
    ))
    .expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(core.handle(Input::ShellLost), Ok(Vec::new()));
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
}

#[test]
fn the_engine_is_heard_only_once_its_turn_is_stored() {
    let key = engine_key(&started(), R);
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("turn");
    assert_eq!(
        core.handle(engine(key, TurnEvent::Finished(Finish::Completed))),
        Err(Stale)
    );
}

#[test]
fn a_deadline_after_done_leaves_the_finished_run_alone() {
    let mut core = started();
    draw_step(&mut core, R, "last").expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("tail".to_owned()),
    ))
    .expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Err(Stale),
        "the engine finished within its budget"
    );
    assert_eq!(
        core.handle(ack(0, "last")).expect("current"),
        vec![
            committed(0, "last"),
            text("tail"),
            end_run(Some("tail"), None, RunEnd::Done)
        ]
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }).expect("ending"),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
}

#[test]
fn a_replacement_is_stored_and_started_only_after_the_runs_before_it() {
    let (b, c) = (RunId(11), RunId(12));
    let turn = |run: RunId, text: &str| {
        Effect::Store(StoreOp::BeginTurn {
            thread: T,
            title: None,
            run,
            text: text.into(),
            screen: None,
        })
    };
    let mut core = started();
    draw_step(&mut core, R, "one").expect("current");
    core.handle(engine(
        engine_key(&core, R),
        TurnEvent::TextDelta("tail".to_owned()),
    ))
    .expect("current");
    assert_eq!(
        core.handle(Input::Turn {
            turn: 1,
            thread: Some(T),
            text: "b".into(),
        })
        .expect("turn"),
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
            state(b, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: b,
                after: WALL_CLOCK
            },
        ],
        "run 10's step is still in the store, so run 11's turn waits"
    );
    assert_eq!(
        core.handle(Input::Stop {
            run: b,
            revealed: Vec::new(),
        })
        .expect("current"),
        vec![
            Effect::Revoke { run: b },
            state(b, T, Phase::Interrupted),
            run_end(b)
        ]
    );
    assert_eq!(core.handle(Input::TurnStored { run: b }), Err(Stale));
    assert_eq!(
        core.handle(Input::Turn {
            turn: 1,
            thread: Some(T),
            text: "c".into(),
        })
        .expect("turn"),
        vec![
            state(c, T, Phase::Thinking),
            Effect::ArmDeadline {
                run: c,
                after: WALL_CLOCK
            },
        ]
    );
    assert_eq!(
        core.handle(ack(0, "one")).expect("ending"),
        vec![
            end_run(
                Some("tail"),
                None,
                RunEnd::Interrupted(Interruption::Replaced)
            ),
            turn(b, "b"),
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: b }).expect("ending"),
        vec![
            Effect::Store(StoreOp::EndRun {
                screen: None,
                thread: T,
                run: b,
                text: None,
                revealed: Some(Vec::new()),
                clear: None,
                end: RunEnd::Interrupted(Interruption::Stop),
            }),
            turn(c, "c"),
        ]
    );
    assert_eq!(
        core.handle(Input::TurnStored { run: c }).expect("current"),
        vec![build_context(c, T, 2)]
    );
}

/// OSC-228: a step's label is model text, so the store write that carries it prints its length,
/// never the label.
#[test]
fn a_step_write_prints_its_labels_length_never_the_label() {
    const SENTINEL: &str = "zqxprivatelabel";
    let write = StoreOp::CommitStep {
        thread: T,
        run: R,
        text: None,
        scene: SceneTarget::Existing(S),
        index: 0,
        step: step(SENTINEL),
    };
    let shown = format!("{write:?}");
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    assert!(!shown.contains(SENTINEL), "the label leaked: {shown}");
    assert!(
        !shown.contains(byte_list.trim_matches(['[', ']'])),
        "the bytes leaked"
    );
    assert!(shown.contains("label_len: 15"), "{shown}");
}

/// H159-1: neither the typed prompt nor the answer's text prints through the input or the
/// outermost effects that carry them: the turn's write, the answer's frames and its last write.
#[test]
fn formatting_a_runs_carriers_shows_lengths_never_text() {
    const SENTINEL: &str = "zqxprivateanswer";
    let mut core = fresh();
    let turn = Input::Turn {
        turn: 1,
        thread: None,
        text: SENTINEL.into(),
    };
    let mut effects = core.handle(turn.clone()).expect("a turn is never stale");
    effects.extend(admit(&mut core, R));
    effects.extend(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::TextDelta(SENTINEL.to_owned()),
        ))
        .expect("current"),
    );
    effects.extend(
        core.handle(engine(
            engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed),
        ))
        .expect("current"),
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::BeginTurn { title: Some(_), .. })
    )));
    assert!(effects.contains(&text(SENTINEL)));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { text: Some(_), .. })))
    );
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    let byte_list = byte_list.trim_matches(['[', ']']);
    let length = format!("Content(len={})", SENTINEL.len());
    for shown in [format!("{turn:?}"), format!("{effects:?}")] {
        assert!(!shown.contains(SENTINEL), "the text leaked: {shown}");
        assert!(!shown.contains(byte_list), "the bytes leaked");
        assert!(shown.contains(&length), "the length is missing");
    }
}
