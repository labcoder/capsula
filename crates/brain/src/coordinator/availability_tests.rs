//! Whether a run may be answered, and on which connection (D13-H1), its model check (S4), and
//! drawing only from an offered tool (D13-H2).

use kumochi_protocol::message::{
    Action, AssistantName, ControlToShell, Effort, ErrorKey, ModelChoice, ModelInfo, ModelName,
    ModelSlug, Phase, RunId, ThreadId, UserError,
};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, StoreOp, T, delta, draw_call};
use crate::context::{Built, ConnectionTag, ModelRequest, Origin, RunSnapshot, ToolSchema};
use crate::coordinator::{Availability, Ready};
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

/// A coordinator that heard `availability`, with run 10 typed in a new thread 4.
fn typed(availability: Option<Input>) -> Coordinator {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    if let Some(availability) = availability {
        assert_eq!(core.handle(availability), Ok(Vec::new()));
    }
    core.handle(Input::Turn {
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("a turn is never stale");
    core
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
            }),
            connection: ConnectionTag(7),
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

/// Run 10 ended before it answered, and the shell hears `reason`.
fn failed(reason: Option<UserError>) -> Vec<Effect> {
    vec![
        Effect::CancelEngine { run: R },
        Effect::Control(ControlToShell::RunState {
            run: R,
            thread: T,
            phase: Phase::Failed,
            error: reason,
        }),
        Effect::Data(kumochi_protocol::message::DataToShell::RunEnd { run: R }),
        Effect::Store(StoreOp::EndRun {
            thread: T,
            run: R,
            text: None,
            revealed: None,
            clear: None,
            end: RunEnd::Failed(Failure::Engine),
        }),
    ]
}

fn started(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::StartEngine { .. }))
}

#[test]
fn a_signed_out_turn_is_kept_and_fails_before_any_build() {
    let mut core = typed(None);
    assert_eq!(
        core.handle(Input::TurnStored { run: R }),
        Ok(failed(Some(reason(ErrorKey::SignInNeeded, Action::SignIn))))
    );
}

#[test]
fn the_accounts_own_reason_reaches_the_shell() {
    let plan = UserError::from(ErrorKey::PlanUnavailable);
    let mut core = typed(Some(Input::Engines(Availability::Unavailable(
        plan.clone(),
    ))));
    assert_eq!(
        core.handle(Input::TurnStored { run: R }),
        Ok(failed(Some(plan)))
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
        core.handle(Input::SendReady { key: key(1) }),
        Ok(vec![Effect::Rebuild(key(1))]),
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
    assert_eq!(
        core.handle(super::engine(R, draw_call())),
        Ok(failed(None)),
        "no draw state is kept, so no step can follow"
    );

    let mut core = typed(Some(ready(7, &["m"])));
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(Some("m"), vec![draw_tool()]))
        .expect("current");
    assert_eq!(core.handle(super::engine(R, draw_call())), Ok(Vec::new()));
    let step = core
        .handle(super::engine(R, delta("head")))
        .expect("current");
    assert!(
        step.iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitStep { .. }))),
        "an offered draw streams its steps"
    );
}
