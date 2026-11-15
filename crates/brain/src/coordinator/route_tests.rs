//! A run bound to a route: the kind of its model choice, under the sign-in that kind had when the
//! turn arrived.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::message::{
    Action, AssistantName, ConnectionKind, ControlToShell, Effort, ErrorKey, GestureId,
    ModelChoice, ModelInfo, ModelName, ModelSlug, Phase, RunId, ThreadId, UserError,
};

use super::{BuildKey, ContextBuilt, Coordinator, Effect, Input, R, SendKey, Stale, T};
use crate::context::{Built, ConnectionTag, ModelRequest, Origin, RunSnapshot};
use crate::coordinator::availability::model_unavailable;
use crate::coordinator::{Availability, HoldKey, HoldSnapshot, Ready, StoreOp};
use crate::store::Generation;

const CHATGPT: ConnectionKind = ConnectionKind::ChatGpt;
const CLAUDE: ConnectionKind = ConnectionKind::ClaudeCode;
const KEY: ConnectionKind = ConnectionKind::AnthropicKey;

fn info(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).expect("slug"),
        display_name: ModelName::new("A model").expect("name"),
    }
}

fn ready(kind: ConnectionKind, tag: u64, models: &[&str]) -> Input {
    Input::Engines(
        kind,
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: models.iter().map(|slug| info(slug)).collect(),
            trusted_draw: false,
        }),
    )
}

fn reason(key: ErrorKey, action: Action) -> UserError {
    UserError::new(key, Vec::new(), vec![action]).expect("one action")
}

fn sign_in() -> UserError {
    reason(ErrorKey::SignInNeeded, Action::SignIn)
}

fn declined() -> UserError {
    reason(ErrorKey::PlanNotEnabled, Action::Enable)
}

fn unavailable(kind: ConnectionKind, why: UserError) -> Input {
    Input::Engines(kind, Availability::Unavailable(why))
}

fn choice(kind: ConnectionKind, slug: &str) -> ModelChoice {
    ModelChoice {
        connection: kind,
        model: ModelSlug::new(slug).expect("slug"),
        effort: Effort::Low,
    }
}

fn key() -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt: 1,
    }
}

/// Run 10 typed in a new thread 4 after `inputs`, with its turn stored.
fn typed(inputs: Vec<Input>) -> Coordinator {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    for input in inputs {
        core.handle(input).expect("an availability is never stale");
    }
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("kept");
    core
}

fn defaults(choice: Option<ModelChoice>) -> RunSnapshot {
    RunSnapshot {
        origin: Origin::Task(T),
        assistant: AssistantName::new("Kumo").expect("name"),
        user: None,
        choice,
        connection: ConnectionTag(7),
    }
}

/// The first build's result, on a snapshot naming `choice`.
fn built(choice: Option<ModelChoice>) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key: key(),
        snapshot: Some(defaults(choice)),
        result: Ok(Built {
            request: ModelRequest {
                choice: None,
                instructions: String::new(),
                input: Vec::new(),
                tools: Vec::new(),
            },
            sources: vec![T],
        }),
    })
}

/// The reason run 10 failed with, when `effects` end it with one.
fn failure(effects: &[Effect]) -> Option<UserError> {
    effects.iter().find_map(|effect| match effect {
        Effect::Control(ControlToShell::RunState {
            run: R,
            phase: Phase::Failed,
            error,
            ..
        }) => error.clone(),
        _ => None,
    })
}

fn started(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::StartEngine { .. }))
}

#[test]
fn equal_tags_on_two_kinds_are_different_routes() {
    for (bound, other) in [(CHATGPT, CLAUDE), (CLAUDE, CHATGPT)] {
        let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 7, &["m"])]);
        assert!(started(
            &core
                .handle(built(Some(choice(bound, "m"))))
                .expect("current")
        ));
        assert_eq!(
            core.handle(unavailable(other, declined())),
            Ok(Vec::new()),
            "another kind's sign-out leaves a run bound to {bound:?}"
        );
        assert_eq!(core.handle(ready(other, 8, &["m"])), Ok(Vec::new()));
        let ended = core.handle(ready(bound, 8, &["m"])).expect("current");
        let new_sign_in = if bound == CHATGPT {
            sign_in()
        } else {
            model_unavailable()
        };
        assert_eq!(failure(&ended), Some(new_sign_in), "{bound:?}");

        let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 7, &["m"])]);
        core.handle(built(Some(choice(bound, "m"))))
            .expect("current");
        let ended = core
            .handle(unavailable(bound, declined()))
            .expect("current");
        assert_eq!(
            failure(&ended),
            Some(declined()),
            "its own kind's reason: {bound:?}"
        );
    }
}

#[test]
fn another_kinds_change_keeps_an_unbound_run_and_takes_out_only_its_own_entry() {
    for (kept, changed) in [(CHATGPT, CLAUDE), (CLAUDE, CHATGPT)] {
        let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 5, &["m"])]);
        assert_eq!(
            core.handle(unavailable(changed, declined())),
            Ok(Vec::new())
        );
        assert!(
            started(
                &core
                    .handle(built(Some(choice(kept, "m"))))
                    .expect("current")
            ),
            "{kept:?} still answers"
        );

        let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 5, &["m"])]);
        assert_eq!(
            core.handle(unavailable(changed, declined())),
            Ok(Vec::new())
        );
        let tag = if changed == CHATGPT { 7 } else { 5 };
        assert_eq!(core.handle(ready(changed, tag, &["m"])), Ok(Vec::new()));
        let refused = core
            .handle(built(Some(choice(changed, "m"))))
            .expect("current");
        let expected = if changed == CHATGPT {
            sign_in()
        } else {
            model_unavailable()
        };
        assert_eq!(
            failure(&refused),
            Some(expected),
            "{changed:?} came back, but the run arrived before"
        );
    }
    let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 5, &["m"])]);
    assert_eq!(core.handle(unavailable(CHATGPT, sign_in())), Ok(Vec::new()));
    let ended = core
        .handle(unavailable(CLAUDE, declined()))
        .expect("current");
    assert_eq!(
        failure(&ended),
        Some(declined()),
        "the last kind to go ends the run with its reason"
    );
}

/// The change reaches only the availability, not as an input, so only the first build sees it.
#[test]
fn a_kind_whose_sign_in_changed_since_arrival_is_refused_at_the_first_build() {
    let mut core = typed(vec![ready(CHATGPT, 7, &["m"])]);
    core.availability = Availability::Ready(Ready {
        connection: ConnectionTag(8),
        models: vec![info("m")],
        trusted_draw: false,
    });
    let refused = core
        .handle(built(Some(choice(CHATGPT, "m"))))
        .expect("current");
    assert_eq!(failure(&refused), Some(sign_in()));
    assert!(!started(&refused));
    assert_eq!(core.sending(SendKey::Build(key())), None);
}

#[test]
fn a_kind_not_ready_at_arrival_is_refused_even_once_it_is() {
    for signs_in in [false, true] {
        let mut core = typed(vec![
            unavailable(CHATGPT, declined()),
            ready(CLAUDE, 5, &["m"]),
        ]);
        assert_eq!(core.current_run(), Some(R), "a kind was ready at arrival");
        if signs_in {
            assert_eq!(core.handle(ready(CHATGPT, 7, &["m"])), Ok(Vec::new()));
        }
        let refused = core
            .handle(built(Some(choice(CHATGPT, "m"))))
            .expect("current");
        let expected = if signs_in { sign_in() } else { declined() };
        assert_eq!(failure(&refused), Some(expected), "{signs_in}");
        assert!(!started(&refused));
        assert_eq!(
            core.handle(built(Some(choice(CHATGPT, "m")))),
            Err(Stale),
            "nothing revives it"
        );
        assert_eq!(core.sending(SendKey::Build(key())), None);
    }
}

#[test]
fn the_model_check_reads_the_routes_own_catalog() {
    for (kind, slug, starts) in [
        (CLAUDE, "b", true),
        (CLAUDE, "a", false),
        (CHATGPT, "b", false),
        (CHATGPT, "a", true),
    ] {
        let mut core = typed(vec![ready(CHATGPT, 7, &["a"]), ready(CLAUDE, 5, &["b"])]);
        let effects = core
            .handle(built(Some(choice(kind, slug))))
            .expect("current");
        assert_eq!(started(&effects), starts, "{kind:?} {slug}");
        let expected = (!starts).then(|| reason(ErrorKey::ModelUnavailable, Action::OpenSettings));
        assert_eq!(failure(&effects), expected, "{kind:?} {slug}");
    }
}

#[test]
fn only_a_chatgpt_route_gets_a_bearer() {
    for (kind, bearer) in [(CHATGPT, Some(ConnectionTag(7))), (CLAUDE, None)] {
        let mut core = typed(vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 7, &["m"])]);
        assert!(started(
            &core
                .handle(built(Some(choice(kind, "m"))))
                .expect("current")
        ));
        assert_eq!(core.sending(SendKey::Build(key())), bearer, "{kind:?}");
    }
}

/// A hold on `choice` in `thread`, prepared after `inputs` and read.
fn held(
    inputs: Vec<Input>,
    choice: ModelChoice,
    thread: Option<ThreadId>,
) -> (Coordinator, HoldSnapshot) {
    let mut core = Coordinator::new(ThreadId(1), ThreadId(3), RunId(9), Vec::new());
    for input in inputs {
        core.handle(input).expect("an availability is never stale");
    }
    let key = HoldKey {
        epoch: 0,
        gesture: GestureId(1),
    };
    let Ok(StoreOp::ReadHold(read)) = core.prepare_hold(
        key,
        thread,
        0,
        defaults(Some(choice)),
        Arc::new(AtomicBool::new(false)),
    ) else {
        panic!("a kind is ready, so the hold is read");
    };
    let mut snapshot = read.snapshot;
    snapshot.generation = thread.map(|_| Generation(0));
    (core, snapshot)
}

fn granted(core: &mut Coordinator) -> bool {
    core.handle(Input::SendReady {
        key: SendKey::Build(key()),
        hold: None,
    })
    .expect("current")
    .iter()
    .any(|effect| matches!(effect, Effect::Grant(_)))
}

#[test]
fn a_hold_on_another_kinds_choice_is_refused_and_stays_refused() {
    for kind in [CLAUDE, KEY] {
        for thread in [None, Some(T)] {
            let (mut core, hold) = held(vec![ready(CHATGPT, 7, &["m"])], choice(kind, "m"), thread);
            assert!(!core.admits_hold(&hold), "{kind:?} {thread:?}");
            assert_eq!(
                core.submit_hold(hold.clone(), "words".into(), None),
                Err(())
            );
            assert_eq!(
                core.submit_hold(hold, "words".into(), None),
                Err(()),
                "the same snapshot again: {kind:?} {thread:?}"
            );
            assert_eq!(core.current_run(), None);
        }
    }
}

#[test]
fn a_pending_hold_fails_only_on_its_own_kinds_change_once_its_kind_is_known() {
    let (core, hold) = held(
        vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 5, &["m"])],
        choice(CLAUDE, "m"),
        None,
    );
    let chatgpt = Availability::Ready(Ready {
        connection: ConnectionTag(8),
        models: vec![info("m")],
        trusted_draw: false,
    });
    let claude = |tag| {
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: vec![info("m")],
            trusted_draw: false,
        })
    };
    assert!(!core.hold_availability_changes(CHATGPT, &chatgpt, Some(&hold)));
    assert!(
        !core.hold_availability_changes(CLAUDE, &claude(5), Some(&hold)),
        "an equal state"
    );
    assert!(core.hold_availability_changes(CLAUDE, &claude(6), Some(&hold)));
    assert!(
        core.hold_availability_changes(CHATGPT, &chatgpt, None),
        "its read is out, so any kind's change fails it"
    );
}

#[test]
fn a_hold_on_another_kind_lives_through_chatgpts_changes_and_ends_on_its_own() {
    for at_the_send in [false, true] {
        let (mut core, hold) = held(
            vec![ready(CHATGPT, 7, &["m"]), ready(CLAUDE, 5, &["m"])],
            choice(CLAUDE, "m"),
            Some(T),
        );
        let cancel = Arc::clone(&hold.cancel);
        core.submit_hold(hold, "words".into(), None)
            .expect("admitted");
        core.handle(Input::TurnStored { run: R }).expect("current");
        assert!(started(
            &core
                .handle(built(Some(choice(CLAUDE, "m"))))
                .expect("current")
        ));
        for change in [ready(CHATGPT, 8, &["m"]), unavailable(CHATGPT, declined())] {
            assert_eq!(core.handle(change), Ok(Vec::new()));
        }
        assert_eq!(
            core.handle(ready(CLAUDE, 5, &["m"])),
            Ok(Vec::new()),
            "an equal state"
        );
        assert!(!cancel.load(Ordering::SeqCst));
        assert!(granted(&mut core), "ChatGPT's changes leave it");

        if at_the_send {
            core.claude_code = Availability::Unavailable(declined());
            assert!(!granted(&mut core));
        } else {
            let ended = core.handle(ready(CLAUDE, 6, &["m"])).expect("current");
            assert_eq!(failure(&ended), Some(model_unavailable()));
            assert!(cancel.load(Ordering::SeqCst), "the hold is cancelled too");
        }
        assert_eq!(core.current_run(), None, "{at_the_send}");
    }
}

#[test]
fn another_connections_choice_fails_while_chatgpt_lists_its_slug() {
    for kind in [CLAUDE, KEY] {
        let mut core = typed(vec![ready(CHATGPT, 7, &["m"])]);
        let effects = core
            .handle(built(Some(choice(kind, "m"))))
            .expect("current");
        assert_eq!(
            failure(&effects),
            Some(reason(ErrorKey::ModelUnavailable, Action::OpenSettings)),
            "{kind:?}"
        );
        assert!(!started(&effects));
        assert_eq!(core.sending(SendKey::Build(key())), None);
    }
}
