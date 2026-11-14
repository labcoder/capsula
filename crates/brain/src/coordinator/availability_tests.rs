//! Whether a run may be answered, and on which connection (D13-H1), its model check (S4), and
//! drawing only from an offered tool (D13-H2).

use kumochi_protocol::message::{
    Action, AssistantName, ControlToShell, Effort, ErrorKey, ModelChoice, ModelInfo, ModelName,
    ModelSlug, Phase, RunId, ThreadId, UserError,
};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, StoreOp, T, delta, draw_call};
use crate::context::{Built, ConnectionTag, ModelRequest, Origin, RunSnapshot, ToolSchema};
use crate::coordinator::{Availability, JobKey, Ready, RoundItems, SendKey};
use crate::engines::{EngineFailure, Finish, TurnEvent};
use crate::store::{Failure, RunEnd};

fn info(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).expect("valid"),
        display_name: ModelName::new("A model").expect("valid"),
    }
}

fn ready(connection: u64, models: &[&str]) -> Input {
    Input::Engines(Availability::Ready(Ready {
        connection: ConnectionTag(connection),
        models: models.iter().map(|slug| info(slug)).collect(),
        trusted_draw: false,
    }))
}

fn reason(key: ErrorKey, action: Action) -> UserError {
    UserError::new(key, Vec::new(), vec![action]).expect("one action")
}

/// A coordinator that heard `availability`, with run 10 typed in a new thread 4, and what the
/// turn did after it was written and announced as Thinking.
fn typed_with(availability: Option<Input>) -> (Coordinator, Vec<Effect>) {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    if let Some(availability) = availability {
        assert_eq!(core.handle(availability), Ok(Vec::new()));
    }
    let mut effects = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale");
    let after = effects.split_off(3);
    assert!(matches!(
        effects[0],
        Effect::Store(StoreOp::BeginTurn { run: R, .. })
    ));
    assert_eq!(
        effects[1..],
        [
            Effect::Control(ControlToShell::RunState {
                run: R,
                thread: T,
                phase: Phase::Thinking,
                error: None,
                origin: Some(kumochi_protocol::message::RunOrigin::Typed { turn: 1 }),
            }),
            Effect::ArmDeadline {
                run: R,
                after: crate::coordinator::WALL_CLOCK,
            },
        ]
    );
    (core, after)
}

fn typed(availability: Option<Input>) -> Coordinator {
    typed_with(availability).0
}

fn sign_in() -> UserError {
    reason(ErrorKey::SignInNeeded, Action::SignIn)
}

fn key(attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt,
    }
}

/// Attempt 1's result: a snapshot naming `choice`, and a request offering `tools`.
fn built(choice: Option<&str>, tools: Vec<ToolSchema>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key: key(1),
        snapshot: Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: choice.map(|slug| ModelChoice {
                model: ModelSlug::new(slug).expect("valid"),
                effort: Effort::Low,
                connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
            }),
            connection: ConnectionTag(7),
            invalid_default: false,
        }),
        result: Ok(Built {
            request: ModelRequest {
                choice: None,
                instructions: String::new(),
                input: Vec::new(),
                tools,
            },
            sources: vec![T],
        }),
    })
}

fn draw_tool() -> ToolSchema {
    ToolSchema {
        name: "draw",
        description: "",
        parameters: serde_json::json!({}),
    }
}

/// The shell hears run 10 failed, with `reason`.
fn told(reason: Option<UserError>) -> Vec<Effect> {
    vec![
        Effect::Control(ControlToShell::RunState {
            run: R,
            thread: T,
            phase: Phase::Failed,
            error: reason,
            origin: None,
        }),
        Effect::Data(kumochi_protocol::message::DataToShell::RunEnd { run: R }),
    ]
}

/// Run 10's failure is written.
fn written() -> Vec<Effect> {
    vec![Effect::Store(StoreOp::EndRun {
        thread: T,
        run: R,
        text: None,
        revealed: None,
        clear: None,
        end: RunEnd::Failed(Failure::Engine),
    })]
}

/// Run 10 ended after its turn was stored, before it answered, and the shell hears `reason`.
fn failed(reason: Option<UserError>) -> Vec<Effect> {
    let mut effects = vec![Effect::CancelEngine { run: R }];
    effects.extend(told(reason));
    effects.extend(written());
    effects
}

fn started(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::StartEngine { .. }))
}

#[test]
fn a_signed_out_turn_is_kept_and_fails_before_any_build() {
    let (mut core, effects) = typed_with(None);
    assert_eq!(effects, told(Some(sign_in())));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Ok(written()));
}

#[test]
fn the_accounts_own_reason_reaches_the_shell() {
    let plan = UserError::from(ErrorKey::PlanUnavailable);
    let (mut core, effects) = typed_with(Some(Input::Engines(Availability::Unavailable(
        plan.clone(),
    ))));
    assert_eq!(effects, told(Some(plan)));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Ok(written()));
}

/// M143-1: a sign-out accepted while the turn is still being written ends its run at once, and
/// the next account never answers it.
#[test]
fn a_sign_out_before_the_turn_is_stored_ends_the_run_and_a_new_account_never_answers_it() {
    let mut core = typed(Some(ready(7, &["m"])));
    assert_eq!(
        core.handle(Input::Engines(Availability::Unavailable(sign_in()))),
        Ok(told(Some(sign_in())))
    );
    assert_eq!(core.handle(ready(8, &["m"])), Ok(Vec::new()));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Ok(written()));
}

/// M143-1: a turn sent while signed out stays refused when a sign-in lands before its write.
#[test]
fn a_turn_sent_signed_out_stays_refused_after_a_sign_in_beats_its_write() {
    let (mut core, effects) = typed_with(None);
    assert_eq!(effects, told(Some(sign_in())));
    assert_eq!(core.handle(ready(7, &["m"])), Ok(Vec::new()));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Ok(written()));
}

#[test]
fn a_sign_out_during_the_first_build_ends_the_run_and_its_late_result_is_stale() {
    let mut core = typed(Some(ready(7, &["m"])));
    let stored = core.handle(Input::TurnStored { run: R }).expect("current");
    assert!(
        stored
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::BuildContext(_))))
    );
    assert_eq!(
        core.handle(Input::Engines(Availability::Unavailable(sign_in()))),
        Ok(failed(Some(sign_in())))
    );
    assert_eq!(core.handle(built(Some("m"), Vec::new())), Err(super::Stale));
    assert_eq!(
        core.handle(Input::SendReady {
            key: SendKey::Build(key(1)),
            hold: None,
        }),
        Ok(vec![Effect::Rebuild(SendKey::Build(key(1)))])
    );
}

/// The check before Grant on its own: the availability changes under an admitted run without
/// the eager cancellation an `Engines` input brings, so only `send_ready` can refuse it.
#[test]
fn the_grant_checks_the_runs_connection_itself() {
    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert!(started(
        &core.handle(built(Some("m"), Vec::new())).expect("current")
    ));
    core.availability = Availability::Ready(Ready {
        connection: ConnectionTag(8),
        models: vec![info("m")],
        trusted_draw: false,
    });
    let mut refused = vec![Effect::Rebuild(SendKey::Build(key(1)))];
    refused.extend(failed(Some(sign_in())));
    assert_eq!(
        core.handle(Input::SendReady {
            key: SendKey::Build(key(1)),
            hold: None,
        }),
        Ok(refused)
    );
}

#[test]
fn a_catalog_change_keeps_the_run_and_another_connection_ends_it_before_any_grant() {
    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    let admitted = core.handle(built(Some("m"), Vec::new())).expect("current");
    assert!(started(&admitted));
    assert_eq!(
        core.handle(ready(7, &[])),
        Ok(Vec::new()),
        "a run keeps the catalog its first build checked"
    );
    assert_eq!(
        core.handle(ready(8, &["m"])),
        Ok(failed(Some(reason(ErrorKey::SignInNeeded, Action::SignIn))))
    );
    assert_eq!(
        core.handle(Input::SendReady {
            key: SendKey::Build(key(1)),
            hold: None,
        }),
        Ok(vec![Effect::Rebuild(SendKey::Build(key(1)))]),
        "the old round's held ask is answered with no send"
    );
}

#[test]
fn an_accepted_sign_out_ends_the_run_with_its_reason() {
    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(Some("m"), Vec::new())).expect("current");
    let signing_out = reason(ErrorKey::SignInNeeded, Action::SignIn);
    assert_eq!(
        core.handle(Input::Engines(Availability::Unavailable(
            signing_out.clone()
        ))),
        Ok(failed(Some(signing_out)))
    );
}

#[test]
fn a_model_the_catalog_doesnt_list_fails_the_run_and_a_listed_one_starts() {
    let unavailable = reason(ErrorKey::ModelUnavailable, Action::OpenSettings);
    for (models, choice) in [
        (vec!["m"], Some("other")),
        (vec!["m"], None),
        (Vec::new(), Some("m")),
    ] {
        let mut core = typed(Some(ready(7, &models)));
        core.handle(Input::TurnStored { run: R }).expect("current");
        assert_eq!(
            core.handle(built(choice, Vec::new())),
            Ok(failed(Some(unavailable.clone()))),
            "{models:?} {choice:?}"
        );
    }
    let mut core = typed(Some(ready(7, &["other", "m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert!(started(
        &core.handle(built(Some("m"), Vec::new())).expect("current")
    ));
}

#[test]
fn a_draw_the_request_didnt_offer_commits_nothing() {
    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(Some("m"), Vec::new())).expect("current");
    for event in [
        draw_call(),
        delta("head"),
        TurnEvent::ToolCallReady {
            call: super::call(),
            args: "head\n".to_owned(),
        },
    ] {
        assert_eq!(
            core.handle(super::engine(super::engine_key(&core, R), event)),
            Ok(Vec::new()),
            "no draw state is kept, so no step follows"
        );
    }
    assert_eq!(
        core.handle(super::engine(
            super::engine_key(&core, R),
            TurnEvent::Finished(Finish::Completed)
        )),
        Ok(vec![Effect::Store(StoreOp::RoundItems(RoundItems {
            thread: T,
            run: R,
            round: 1,
            connection: ConnectionTag(7),
            text: None,
            items: Vec::new(),
            calls: vec![(
                super::call(),
                crate::dispatch::ToolOutput::Error(crate::dispatch::ToolError::NotHere)
            )],
        }))]),
        "the registered but unoffered tool gets one refusal"
    );

    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(Some("m"), vec![draw_tool()]))
        .expect("current");
    assert_eq!(
        core.handle(super::engine(super::engine_key(&core, R), draw_call())),
        Ok(Vec::new())
    );
    let args = r#"{"records":[{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"}},{"step":{"id":"s1"}},{"end":{}}]}"#;
    let mut effects = Vec::new();
    for event in [
        TurnEvent::ToolArgsDelta {
            call: super::call(),
            delta: args.to_owned(),
        },
        TurnEvent::ToolCallReady {
            call: super::call(),
            args: args.to_owned(),
        },
        TurnEvent::Finished(Finish::Completed),
    ] {
        effects.extend(
            core.handle(super::engine(super::engine_key(&core, R), event))
                .expect("current"),
        );
    }
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. })))
            .count(),
        1,
        "the offered call's closed records reach the scene reducer"
    );
}

/// A round takes the bearer of its run's connection only while that run is current: once Stop
/// ends it, its late ask gets none, though the run stays bound while it ends. A job takes the
/// ready connection's, and none signed out.
#[test]
fn a_send_takes_its_runs_connection_only_while_the_run_is_current() {
    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    let round = SendKey::Build(key(1));
    let job = SendKey::Job(JobKey {
        thread: T,
        serial: 2,
    });
    assert_eq!(core.sending(round), Some(ConnectionTag(7)));
    assert_eq!(core.handle(ready(8, &["m"])).map(|_| ()), Ok(()));
    assert_eq!(core.sending(job), Some(ConnectionTag(8)));
    assert_eq!(
        core.sending(SendKey::Build(BuildKey {
            run: RunId(11),
            ..key(1)
        })),
        None,
        "another run's round"
    );

    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(core.bound(R), Some(ConnectionTag(7)), "the run is ending");
    assert_eq!(core.sending(round), None);

    let core = typed(None);
    assert_eq!(core.sending(job), None);
}

/// M159-1: a round the account gave no bearer fails with the account's own reason while the
/// account stays ready, so a refresh that couldn't connect keeps its Retry rather than asking
/// for a sign-in.
#[test]
fn an_access_failure_keeps_the_accounts_own_reason() {
    for key in [ErrorKey::AccountOffline, ErrorKey::CredentialsUnavailable] {
        let mut core = typed(Some(ready(7, &["m"])));
        core.handle(Input::TurnStored { run: R }).expect("current");
        assert!(started(
            &core.handle(built(Some("m"), Vec::new())).expect("current")
        ));
        let finish = Finish::Failed(EngineFailure::Access(Some(reason(key, Action::Retry))));
        assert_eq!(
            core.handle(Input::Engine {
                key: super::engine_key(&core, R),
                event: TurnEvent::Finished(finish),
            }),
            Ok(failed(Some(reason(key, Action::Retry)))),
            "{key:?}"
        );
    }
}
