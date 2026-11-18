//! Whether a run may be answered, and on which connection (D13-H1), its model check (S4), and
//! drawing only from an offered tool (D13-H2).

use kumochi_protocol::message::{
    Action, AssistantName, ControlToShell, Effort, ErrorKey, ModelChoice, ModelInfo, ModelName,
    ModelSlug, Phase, RunId, ThreadId, UserError,
};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, StoreOp, T, delta, draw_call};
use crate::context::{Built, ConnectionTag, ModelRequest, Origin, Route, RunSnapshot, ToolSchema};
use crate::coordinator::availability::{connection_needed, model_unavailable};
use crate::coordinator::{Availability, JobKey, Ready, RoundItems, SendKey};
use crate::engines::{EngineFailure, Finish, TurnEvent};
use crate::store::{Failure, RunEnd};

fn info(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).expect("valid"),
        display_name: ModelName::new("A model").expect("valid"),
        supported_efforts: kumochi_protocol::message::EffortSet::new([
            kumochi_protocol::message::Effort::Low,
        ]),
    }
}

fn ready(connection: u64, models: &[&str]) -> Input {
    Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Ready(Ready {
            connection: ConnectionTag(connection),
            models: models.iter().map(|slug| info(slug)).collect(),
            trusted_draw: false,
        }),
    )
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

/// What the first build of a turn typed after `inputs`, on no saved choice, does with nothing
/// ready.
fn arrived_after(inputs: Vec<Input>) -> Vec<Effect> {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    for input in inputs {
        assert_eq!(core.handle(input), Ok(Vec::new()));
    }
    let arrived = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale")
        .split_off(3);
    assert_eq!(arrived, Vec::new(), "nothing fails at arrival");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(None, Vec::new())).expect("current")
}

/// The owner's rule for a turn nothing can answer (OSC-742): `ConnectionNeeded`, with Open
/// Settings, only when `ChatGPT` needs a sign-in and Claude Code is installed but signed out.
/// Every other Claude Code state, the one before it reports included, keeps `ChatGPT`'s reason
/// and its action; any other `ChatGPT` reason is kept whatever Claude Code's is.
#[test]
fn connection_needed_goes_out_only_when_claude_code_is_installed_but_signed_out() {
    let open = |key| reason(key, Action::OpenSettings);
    let declined = reason(ErrorKey::PlanNotEnabled, Action::Enable);
    // Claude Code's state (None: not reported yet), and what a turn gets with ChatGPT signed out
    // and with ChatGPT's plan declined.
    let rows: Vec<(Option<UserError>, UserError, UserError)> = vec![
        (None, sign_in(), declined.clone()),
        (
            Some(open(ErrorKey::ClaudeCodeSignedOut)),
            connection_needed(),
            declined.clone(),
        ),
        (
            Some(open(ErrorKey::ClaudeCodeNotFound)),
            sign_in(),
            declined.clone(),
        ),
        (Some(model_unavailable()), sign_in(), declined.clone()),
        (
            Some(open(ErrorKey::ClaudeCodeUnsupported)),
            sign_in(),
            declined.clone(),
        ),
        (
            Some(open(ErrorKey::ClaudeCodePolicy)),
            sign_in(),
            declined.clone(),
        ),
        (
            Some(open(ErrorKey::ClaudeCodeAuditFailed)),
            sign_in(),
            declined.clone(),
        ),
        (
            Some(open(ErrorKey::ClaudeCodeCleanupPending)),
            sign_in(),
            declined.clone(),
        ),
    ];
    for (claude, when_signed_out, when_declined) in rows {
        for (chatgpt, expected) in [
            (sign_in(), when_signed_out),
            (declined.clone(), when_declined),
        ] {
            let mut inputs = vec![Input::Engines(
                kumochi_protocol::message::ConnectionKind::ChatGpt,
                Availability::Unavailable(chatgpt.clone()),
            )];
            inputs.extend(claude.clone().map(|why| {
                Input::Engines(
                    kumochi_protocol::message::ConnectionKind::ClaudeCode,
                    Availability::Unavailable(why),
                )
            }));
            assert_eq!(
                arrived_after(inputs),
                failed(Some(expected)),
                "{:?} with {:?}",
                chatgpt.key(),
                claude.as_ref().map(UserError::key)
            );
        }
    }
}

/// L1: the key goes out with Open Settings only, since a notice's Sign in would start
/// `ChatGPT`'s sign-in for it. Both Claude kinds start as `ModelUnavailable` until reported.
#[test]
fn connection_needed_offers_only_open_settings_and_the_claude_kinds_start_unavailable() {
    assert_eq!(connection_needed().key(), ErrorKey::ConnectionNeeded);
    assert_eq!(connection_needed().actions(), [Action::OpenSettings]);
    let core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    for kind in [
        kumochi_protocol::message::ConnectionKind::ClaudeCode,
        kumochi_protocol::message::ConnectionKind::AnthropicKey,
    ] {
        assert_eq!(
            core.availability_of(kind),
            &Availability::Unavailable(model_unavailable()),
            "{kind:?}"
        );
    }
}

#[test]
fn a_signed_out_turn_is_kept_and_fails_at_its_first_build() {
    let (mut core, effects) = typed_with(None);
    assert_eq!(effects, Vec::new());
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert_eq!(
        core.handle(built(None, Vec::new())),
        Ok(failed(Some(sign_in())))
    );
}

#[test]
fn the_accounts_own_reason_reaches_the_shell() {
    let plan = UserError::from(ErrorKey::PlanUnavailable);
    let (mut core, effects) = typed_with(Some(Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Unavailable(plan.clone()),
    )));
    assert_eq!(effects, Vec::new());
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert_eq!(core.handle(built(None, Vec::new())), Ok(failed(Some(plan))));
}

/// M143-1: a sign-out accepted while the turn is still being written ends its run at once, and
/// the next account never answers it.
#[test]
fn a_sign_out_before_the_turn_is_stored_ends_the_run_and_a_new_account_never_answers_it() {
    let mut core = typed(Some(ready(7, &["m"])));
    assert_eq!(
        core.handle(Input::Engines(
            kumochi_protocol::message::ConnectionKind::ChatGpt,
            Availability::Unavailable(sign_in())
        )),
        Ok(told(Some(sign_in())))
    );
    assert_eq!(core.handle(ready(8, &["m"])), Ok(Vec::new()));
    assert_eq!(core.handle(Input::TurnStored { run: R }), Ok(written()));
}

/// M143-1: a turn sent while signed out stays refused when a sign-in lands before its write.
#[test]
fn a_turn_sent_signed_out_stays_refused_after_a_sign_in_beats_its_write() {
    let (mut core, effects) = typed_with(None);
    assert_eq!(effects, Vec::new());
    assert_eq!(core.handle(ready(7, &["m"])), Ok(Vec::new()));
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert_eq!(
        core.handle(built(Some("m"), Vec::new())),
        Ok(failed(Some(sign_in())))
    );
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
        core.handle(Input::Engines(
            kumochi_protocol::message::ConnectionKind::ChatGpt,
            Availability::Unavailable(sign_in())
        )),
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
        core.handle(Input::Engines(
            kumochi_protocol::message::ConnectionKind::ChatGpt,
            Availability::Unavailable(signing_out.clone())
        )),
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
            connection: crate::context::Route {
                kind: kumochi_protocol::message::ConnectionKind::ChatGpt,
                tag: ConnectionTag(7)
            },
            text: None,
            items: Vec::new(),
            key_calls: Vec::new(),
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
    assert_eq!(core.sending(round), None, "unbound before its first build");
    core.handle(built(Some("m"), Vec::new())).expect("current");
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

/// A turn's first build on a snapshot naming `slug` over `kind`.
fn built_on(kind: kumochi_protocol::message::ConnectionKind, slug: &str) -> Input {
    let Input::ContextBuilt(mut built) = built(Some(slug), Vec::new()) else {
        unreachable!("built builds a context");
    };
    if let Some(choice) = built
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.choice.as_mut())
    {
        choice.connection = kind;
    }
    Input::ContextBuilt(built)
}

const CLAUDE: kumochi_protocol::message::ConnectionKind =
    kumochi_protocol::message::ConnectionKind::ClaudeCode;
const R2: RunId = RunId(11);
/// How long a typed turn waits at its first build for Claude Code's next report.
const FINDING_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

fn claude(now: Availability) -> Input {
    Input::Engines(CLAUDE, now)
}

/// Claude Code ready under sign-in `tag`, offering haiku.
fn claude_ready(tag: u64) -> Input {
    claude(Availability::Ready(Ready {
        connection: ConnectionTag(tag),
        models: vec![info("haiku")],
        trusted_draw: false,
    }))
}

fn chatgpt_signed_out() -> Input {
    Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Unavailable(sign_in()),
    )
}

fn open(key: ErrorKey) -> UserError {
    reason(key, Action::OpenSettings)
}

/// Run 10 typed in a new thread after `before`, its turn written and its first build out.
fn turn_after(before: Vec<Input>) -> Coordinator {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    for input in before {
        assert_eq!(core.handle(input), Ok(Vec::new()));
    }
    let arrived = core
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale")
        .split_off(3);
    assert_eq!(arrived, Vec::new(), "nothing fails at arrival");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core
}

/// A turn typed on haiku over Claude Code after `before`, with `between` heard before its first
/// build, and what that build did.
fn claude_turn(before: Vec<Input>, between: Vec<Input>) -> (Coordinator, Vec<Effect>) {
    let mut core = turn_after(before);
    for input in between {
        assert_eq!(core.handle(input), Ok(Vec::new()));
    }
    let effects = core.handle(built_on(CLAUDE, "haiku")).expect("current");
    (core, effects)
}

/// A Claude Code turn typed after `before` while Claude Code was being found, waiting at its
/// first build.
fn waiting_after(before: Vec<Input>) -> Coordinator {
    let before = before
        .into_iter()
        .chain([claude(Availability::Finding)])
        .collect();
    let (core, effects) = claude_turn(before, Vec::new());
    assert_eq!(
        effects,
        vec![Effect::ArmDeadline {
            run: R,
            after: FINDING_WAIT,
        }],
        "the first build waits"
    );
    core
}

/// The usual waiting turn: `ChatGPT` signed out, so only Claude Code could answer it.
fn waiting() -> Coordinator {
    waiting_after(vec![chatgpt_signed_out()])
}

/// `input`, a build result, as `run`'s first-round attempt `attempt`.
fn keyed(input: Input, run: RunId, attempt: u64) -> Input {
    let Input::ContextBuilt(mut built) = input else {
        unreachable!("a build result");
    };
    built.key = BuildKey {
        run,
        round: 1,
        attempt,
    };
    Input::ContextBuilt(built)
}

/// `effects` give `run` the wall clock again and start its first build afresh, and do nothing
/// else.
fn released(effects: &[Effect], run: RunId) {
    let [
        Effect::ArmDeadline {
            run: armed,
            after: crate::coordinator::WALL_CLOCK,
        },
        Effect::Store(StoreOp::BuildContext(build)),
    ] = effects
    else {
        panic!("not a release: {effects:?}");
    };
    assert_eq!((*armed, build.key.run), (run, run));
    assert!(build.first && build.snapshot.is_none(), "a first build");
}

/// How `run` ended in `effects`, if it did.
fn end_of(effects: &[Effect], run: RunId) -> Option<RunEnd> {
    effects.iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::EndRun {
            run: ended, end, ..
        }) if *ended == run => Some(*end),
        _ => None,
    })
}

/// The current run's route; it must be bound.
fn route(core: &Coordinator) -> Route {
    core.current
        .as_ref()
        .and_then(|run| run.route)
        .expect("a bound current run")
}

fn on(kind: kumochi_protocol::message::ConnectionKind, tag: u64) -> Route {
    Route {
        kind,
        tag: ConnectionTag(tag),
    }
}

/// Another kind's Ready on `tag`, offering haiku.
fn kind_ready(kind: kumochi_protocol::message::ConnectionKind, tag: u64) -> Input {
    Input::Engines(
        kind,
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: vec![info("haiku")],
            trusted_draw: false,
        }),
    )
}

/// OSC-760: a new Claude Code sign-in while an unbound run's first build is out fails the run
/// with Open Settings, never `ChatGPT`'s Sign in, and the late build is stale.
#[test]
fn a_new_claude_code_tag_before_the_bind_fails_the_run_with_open_settings() {
    let mut core = turn_after(vec![chatgpt_signed_out(), claude_ready(5)]);
    assert_eq!(
        core.handle(claude_ready(6)),
        Ok(failed(Some(model_unavailable())))
    );
    assert_eq!(core.handle(built_on(CLAUDE, "haiku")), Err(super::Stale));
}

/// OSC-760: a run bound to Claude Code or the Anthropic key ends with Open Settings when its kind
/// reports a new sign-in; `ChatGPT`'s own case keeps its Sign in
/// (`a_catalog_change_keeps_the_run_and_another_connection_ends_it_before_any_grant`).
#[test]
fn a_new_tag_on_a_bound_non_chatgpt_run_ends_it_with_open_settings() {
    for kind in [
        CLAUDE,
        kumochi_protocol::message::ConnectionKind::AnthropicKey,
    ] {
        let (mut core, effects) = turn_on(kind, vec![chatgpt_signed_out(), kind_ready(kind, 5)]);
        assert!(started(&effects), "{kind:?}");
        assert_eq!(route(&core), on(kind, 5));
        assert_eq!(
            core.handle(kind_ready(kind, 6)),
            Ok(failed(Some(model_unavailable()))),
            "{kind:?}"
        );
    }
}

/// A turn typed on haiku over `kind` after `before`, and what its first build did.
fn turn_on(
    kind: kumochi_protocol::message::ConnectionKind,
    before: Vec<Input>,
) -> (Coordinator, Vec<Effect>) {
    let mut core = turn_after(before);
    let effects = core.handle(built_on(kind, "haiku")).expect("current");
    (core, effects)
}

/// OSC-753: a Claude-only user who types while Claude Code is still being found isn't failed at
/// the first build; it waits for the next report, and Ready answers it on that sign-in.
#[test]
fn a_turn_on_claude_code_while_it_is_being_found_waits_for_its_report() {
    let mut core = waiting();
    released(&core.handle(claude_ready(5)).expect("current"), R);
    assert!(started(
        &core
            .handle(keyed(built_on(CLAUDE, "haiku"), R, 2))
            .expect("current")
    ));
    assert_eq!(route(&core), on(CLAUDE, 5));
}

/// Claude Code turns Ready between the arrival, during Finding, and the first build: the build
/// binds that sign-in and answers.
#[test]
fn a_claude_code_found_ready_after_arrival_answers_the_run() {
    let (core, effects) = claude_turn(
        vec![chatgpt_signed_out(), claude(Availability::Finding)],
        vec![claude_ready(5)],
    );
    assert!(started(&effects));
    assert_eq!(route(&core), on(CLAUDE, 5));
}

/// A report that isn't Ready ends the wait with its own reason, and the failure stays.
#[test]
fn a_report_that_claude_code_is_not_found_refuses_the_waiting_turn_with_its_reason() {
    let mut core = waiting();
    let not_found = open(ErrorKey::ClaudeCodeNotFound);
    let report = claude(Availability::Unavailable(not_found.clone()));
    released(&core.handle(report).expect("current"), R);
    assert_eq!(
        core.handle(keyed(built_on(CLAUDE, "haiku"), R, 2)),
        Ok(failed(Some(not_found)))
    );
    assert_eq!(core.handle(claude_ready(5)), Ok(Vec::new()), "failed stays");
}

/// The runtime reports after every op and wake, so `Finding` repeats; only a report that isn't
/// `Finding` ends the wait.
#[test]
fn a_repeated_finding_report_leaves_the_wait_as_it_is() {
    let mut core = waiting();
    assert_eq!(core.handle(claude(Availability::Finding)), Ok(Vec::new()));
    released(&core.handle(claude_ready(5)).expect("current"), R);
}

#[test]
fn thirty_seconds_without_a_report_fail_the_waiting_turn_with_open_settings() {
    let mut core = waiting();
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Ok(failed(Some(model_unavailable())))
    );
    assert_eq!(core.handle(claude_ready(5)), Ok(Vec::new()), "failed stays");
}

/// Once released, the run's deadline is its wall clock again.
#[test]
fn a_deadline_after_the_release_is_the_wall_clock() {
    let mut core = waiting();
    core.handle(claude_ready(5)).expect("current");
    let effects = core.handle(Input::Deadline { run: R }).expect("current");
    assert_eq!(end_of(&effects, R), Some(RunEnd::Failed(Failure::Budget)));
}

#[test]
fn esc_while_waiting_ends_the_turn_and_a_later_report_finds_nothing() {
    let mut core = waiting();
    let effects = core
        .handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("current");
    assert_eq!(
        end_of(&effects, R),
        Some(RunEnd::Interrupted(crate::store::Interruption::Stop))
    );
    assert_eq!(core.handle(claude_ready(5)), Ok(Vec::new()));
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Err(crate::coordinator::Stale)
    );
}

#[test]
fn deleting_the_thread_of_a_waiting_turn_ends_it_and_a_later_report_finds_nothing() {
    let mut core = waiting();
    let effects = core
        .handle(Input::Delete {
            thread: T,
            after_turns: 1,
            resend: false,
        })
        .expect("accepted");
    assert_eq!(
        end_of(&effects, R),
        Some(RunEnd::Interrupted(crate::store::Interruption::Deleted))
    );
    assert_eq!(core.handle(claude_ready(5)), Ok(Vec::new()));
}

#[test]
fn losing_the_shell_while_waiting_ends_the_turn_and_a_later_report_finds_nothing() {
    let mut core = waiting();
    let effects = core.handle(Input::ShellLost).expect("never stale");
    assert_eq!(
        end_of(&effects, R),
        Some(RunEnd::Interrupted(crate::store::Interruption::ShellLost))
    );
    assert_eq!(core.handle(claude_ready(5)), Ok(Vec::new()));
}

/// A second turn replaces the waiting one, as any turn replaces the current run; only the newer
/// waits, and Ready releases only it.
#[test]
fn of_two_turns_typed_while_claude_code_is_found_only_the_newer_waits_and_is_released() {
    let mut core = waiting();
    let effects = core
        .handle(Input::Turn {
            turn: 2,
            thread: Some(T),
            text: "again".into(),
        })
        .expect("a turn is never stale");
    assert_eq!(
        end_of(&effects, R),
        Some(RunEnd::Interrupted(crate::store::Interruption::Replaced))
    );
    core.handle(Input::TurnStored { run: R2 }).expect("current");
    assert_eq!(
        core.handle(keyed(built_on(CLAUDE, "haiku"), R2, 2)),
        Ok(vec![Effect::ArmDeadline {
            run: R2,
            after: FINDING_WAIT,
        }])
    );
    released(&core.handle(claude_ready(5)).expect("current"), R2);
}

/// Guard: a turn on another kind's choice is answered at its first build as before.
#[test]
fn a_turn_on_chatgpt_while_claude_code_is_found_is_answered_at_its_first_build() {
    let mut core = turn_after(vec![ready(7, &["m"]), claude(Availability::Finding)]);
    assert!(started(
        &core.handle(built(Some("m"), Vec::new())).expect("current")
    ));
}

/// Only a Claude Code choice waits: a turn on `ChatGPT`, signed out, fails at its first build
/// while Claude Code is being found.
#[test]
fn a_refused_turn_on_another_kind_fails_at_once_while_claude_code_is_found() {
    let mut core = turn_after(vec![chatgpt_signed_out(), claude(Availability::Finding)]);
    assert_eq!(
        core.handle(built(None, Vec::new())),
        Ok(failed(Some(sign_in())))
    );
}

/// Guard (#376): only `Finding` waits. Claude Code signed out at arrival and Ready before the first
/// build still refuses the run with Open Settings, and turned off refuses it without waiting.
#[test]
fn only_a_claude_code_being_found_makes_a_turn_wait() {
    let signed_out = claude(Availability::Unavailable(open(
        ErrorKey::ClaudeCodeSignedOut,
    )));
    let (_, effects) = claude_turn(
        vec![chatgpt_signed_out(), signed_out],
        vec![claude_ready(5)],
    );
    assert_eq!(effects, failed(Some(model_unavailable())), "signed out");

    let turned_off = claude(Availability::Unavailable(model_unavailable()));
    let (_, effects) = claude_turn(
        vec![chatgpt_signed_out(), turned_off],
        vec![claude_ready(5)],
    );
    assert_eq!(effects, failed(Some(model_unavailable())), "turned off");
}

/// Before the first build the newest report wins: a second sign-in replaces the first, `Finding`
/// again sends it back to waiting, and any other report refuses the run with its reason.
#[test]
fn before_the_first_build_the_newest_claude_code_report_wins() {
    let finding = || vec![chatgpt_signed_out(), claude(Availability::Finding)];
    let (core, effects) = claude_turn(finding(), vec![claude_ready(5), claude_ready(6)]);
    assert!(started(&effects));
    assert_eq!(route(&core), on(CLAUDE, 6));

    let (mut core, effects) = claude_turn(
        finding(),
        vec![claude_ready(5), claude(Availability::Finding)],
    );
    assert_eq!(
        effects,
        vec![Effect::ArmDeadline {
            run: R,
            after: FINDING_WAIT,
        }]
    );
    released(&core.handle(claude_ready(6)).expect("current"), R);
    assert!(started(
        &core
            .handle(keyed(built_on(CLAUDE, "haiku"), R, 2))
            .expect("current")
    ));
    assert_eq!(route(&core), on(CLAUDE, 6));

    let signed_out = open(ErrorKey::ClaudeCodeSignedOut);
    let (_, effects) = claude_turn(
        finding(),
        vec![
            claude_ready(5),
            claude(Availability::Unavailable(signed_out.clone())),
        ],
    );
    assert_eq!(effects, failed(Some(signed_out.clone())), "on Claude Code");

    // With no saved choice the first build ends it with the owner's rule (OSC-742), as on main.
    let mut core = turn_after(finding());
    for report in [
        claude_ready(5),
        claude(Availability::Unavailable(signed_out)),
    ] {
        assert_eq!(core.handle(report), Ok(Vec::new()));
    }
    assert_eq!(
        core.handle(built(None, Vec::new())),
        Ok(failed(Some(connection_needed())))
    );
}

/// The release build reads the choice again: a choice changed to a kind that was ready at
/// arrival is answered there, and one changed to a kind that turned ready later is refused.
#[test]
fn a_choice_changed_during_the_wait_routes_as_the_new_choice() {
    let mut core = waiting_after(vec![ready(7, &["m"])]);
    released(&core.handle(claude_ready(5)).expect("current"), R);
    assert!(started(
        &core
            .handle(keyed(built(Some("m"), Vec::new()), R, 2))
            .expect("current")
    ));
    assert_eq!(
        route(&core),
        on(kumochi_protocol::message::ConnectionKind::ChatGpt, 7)
    );

    let key = kumochi_protocol::message::ConnectionKind::AnthropicKey;
    let mut core = waiting();
    let later = Input::Engines(
        key,
        Availability::Ready(Ready {
            connection: ConnectionTag(8),
            models: vec![info("haiku")],
            trusted_draw: false,
        }),
    );
    assert_eq!(core.handle(later), Ok(Vec::new()));
    released(&core.handle(claude_ready(5)).expect("current"), R);
    assert_eq!(
        core.handle(keyed(built_on(key, "haiku"), R, 2)),
        Ok(failed(Some(model_unavailable())))
    );
}

/// Once bound, found ready before its first build or released, the run follows today's rule for a
/// bound run: its kind being found again ends it.
#[test]
fn a_run_bound_to_claude_code_ends_when_it_is_found_again() {
    let (mut core, effects) = claude_turn(
        vec![chatgpt_signed_out(), claude(Availability::Finding)],
        vec![claude_ready(5)],
    );
    assert!(started(&effects));
    assert_eq!(
        core.handle(claude(Availability::Finding)),
        Ok(failed(Some(model_unavailable()))),
        "found before the first build"
    );

    let mut core = waiting();
    released(&core.handle(claude_ready(5)).expect("current"), R);
    assert!(started(
        &core
            .handle(keyed(built_on(CLAUDE, "haiku"), R, 2))
            .expect("current")
    ));
    assert_eq!(
        core.handle(claude(Availability::Finding)),
        Ok(failed(Some(model_unavailable())))
    );
}

/// The release build is a first build, so the model check (S4) runs on the released route.
#[test]
fn a_model_claude_code_doesnt_offer_is_refused_at_the_release_build() {
    let mut core = waiting();
    released(&core.handle(claude_ready(5)).expect("current"), R);
    assert_eq!(
        core.handle(keyed(built_on(CLAUDE, "opus"), R, 2)),
        Ok(failed(Some(model_unavailable())))
    );
}

/// A waiting run has no build, so a second result for the build it waited at is stale.
#[test]
fn a_second_result_for_the_waiting_build_is_stale() {
    let mut core = waiting();
    assert_eq!(
        core.handle(built_on(CLAUDE, "haiku")),
        Err(crate::coordinator::Stale)
    );
    released(&core.handle(claude_ready(5)).expect("current"), R);
}

/// A waiting run has no build, so a delete accepted and answered meanwhile builds nothing.
#[test]
fn a_delete_answered_during_the_wait_builds_nothing() {
    let mut core = waiting();
    let other = ThreadId(8);
    let delete = Input::Delete {
        thread: other,
        after_turns: 0,
        resend: false,
    };
    let answered = Input::Deleted {
        thread: other,
        outcome: kumochi_protocol::message::Deleted::Removed,
        last_run: None,
    };
    for input in [delete, answered] {
        let effects = core.handle(input).expect("accepted");
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::Store(StoreOp::BuildContext(_)) | Effect::ArmDeadline { .. }
            )),
            "{effects:?}"
        );
    }
    released(&core.handle(claude_ready(5)).expect("current"), R);
}
