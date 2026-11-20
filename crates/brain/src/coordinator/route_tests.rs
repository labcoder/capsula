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
        supported_efforts: kumochi_protocol::message::EffortSet::new([
            kumochi_protocol::message::Effort::Low,
        ]),
    }
}

fn ready(kind: ConnectionKind, tag: u64, models: &[&str]) -> Input {
    Input::Engines(
        kind,
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: models.iter().map(|slug| info(slug)).collect(),
            trusted_draw: false,
            images: Vec::new(),
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
        images: Vec::new(),
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

/// A same-key catalog refresh between a key build's admission and its send grant: the grant
/// rechecks the model, so a model the refresh dropped gets no grant and the run fails unavailable.
#[test]
fn a_key_send_whose_model_a_refresh_dropped_is_refused_at_the_grant() {
    for (refreshed, grants) in [(&["k", "n"][..], true), (&["n"][..], false)] {
        let mut core = typed(vec![ready(KEY, 7, &["k"])]);
        assert!(started(
            &core.handle(built(Some(choice(KEY, "k")))).expect("current")
        ));
        core.handle(ready(KEY, 7, refreshed))
            .expect("the same key's refreshed catalog");
        let effects = core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current");
        assert_eq!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Grant(_))),
            grants,
            "{refreshed:?}"
        );
        let expected = (!grants).then(|| reason(ErrorKey::ModelUnavailable, Action::OpenSettings));
        assert_eq!(failure(&effects), expected, "{refreshed:?}");
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
        images: Vec::new(),
    });
    let claude = |tag| {
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: vec![info("m")],
            trusted_draw: false,
            images: Vec::new(),
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

/// The key ready under `tag` with model `m`, its fresh fact saying `images` take pictures.
fn pictured(tag: u64, images: &[&str]) -> Availability {
    Availability::Ready(Ready {
        connection: ConnectionTag(tag),
        models: vec![info("m")],
        trusted_draw: false,
        images: images
            .iter()
            .map(|slug| ModelSlug::new(slug).expect("slug"))
            .collect(),
    })
}

fn jpeg() -> crate::context::Jpeg {
    crate::context::Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xD9]).expect("a jpeg")
}

fn user() -> crate::context::InputItem {
    crate::context::InputItem::Message {
        role: crate::context::Role::User,
        text: "words".to_owned(),
    }
}

fn line(text: &str) -> crate::context::InputItem {
    crate::context::InputItem::Message {
        role: crate::context::Role::Developer,
        text: text.to_owned(),
    }
}

const SNAPSHOT: &str = "The image is a snapshot from when the user spoke.";
const CANT_SEE: &str = "The screen was not included: this model can't be sent images.";

/// A build of `key` whose request names `choice`, with the first build's snapshot.
pub(crate) fn built_for(key: BuildKey, choice: &ModelChoice) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: (key.attempt == 1).then(|| defaults(Some(choice.clone()))),
        result: Ok(Built {
            request: ModelRequest {
                choice: Some(choice.clone()),
                instructions: String::new(),
                input: vec![user()],
                tools: Vec::new(),
            },
            sources: vec![T],
        }),
    })
}

fn started_input(effects: &[Effect]) -> Option<Vec<crate::context::InputItem>> {
    effects.iter().find_map(|effect| match effect {
        Effect::StartEngine { request } => Some(request.request.input.clone()),
        _ => None,
    })
}

fn told_cant_see(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| {
            **effect
                == Effect::Control(ControlToShell::HoldScreen {
                    gesture: GestureId(1),
                    outcome: kumochi_protocol::message::HeldScreen::NotIncluded(
                        crate::context::ScreenClass::ModelCantSee,
                    ),
                })
        })
        .count()
}

/// A screen hold's image on `kind`'s model `m`, whose fact says it takes pictures, admitted
/// with `routes` and built once.
pub(crate) fn pictured_run(
    kind: ConnectionKind,
    routes: crate::coordinator::PictureRoutes,
) -> (Coordinator, Vec<Effect>) {
    let (mut core, hold) = held(
        vec![
            ready(CHATGPT, 7, &["m"]),
            Input::Engines(kind, pictured(5, &["m"])),
        ],
        choice(kind, "m"),
        Some(T),
    );
    core.set_pictures(routes);
    core.submit_hold(
        hold,
        "words".into(),
        Some(crate::context::Screen::Image(jpeg())),
    )
    .expect("admitted");
    core.handle(Input::TurnStored { run: R }).expect("current");
    let effects = core
        .handle(built_for(key(), &choice(kind, "m")))
        .expect("current");
    (core, effects)
}

pub(crate) fn pictured_claude_run(
    available: Availability,
    jpeg: crate::context::Jpeg,
) -> (Coordinator, Vec<Effect>) {
    let choice = choice(CLAUDE, "haiku");
    let (mut core, hold) = held(
        vec![Input::Engines(CLAUDE, available)],
        choice.clone(),
        Some(ThreadId(1)),
    );
    core.set_pictures(crate::coordinator::PictureRoutes::new(false, true));
    core.submit_hold(
        hold,
        "words".into(),
        Some(crate::context::Screen::Image(jpeg)),
    )
    .expect("admitted");
    core.handle(Input::TurnStored { run: R }).expect("current");
    let Input::ContextBuilt(mut built) = built_for(key(), &choice) else {
        unreachable!("built fixture");
    };
    built.result.as_mut().expect("built").request.tools = crate::context::tools(Origin::Kumo);
    let effects = core.handle(Input::ContextBuilt(built)).expect("current");
    (core, effects)
}

impl Coordinator {
    pub(crate) fn picture_lost_for_test(&self, run: RunId) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.id == run && current.screen_lost.is_some())
    }
}

#[test]
fn a_claude_routes_picture_goes_only_with_its_own_switch_on() {
    use crate::coordinator::PictureRoutes;
    for (routes, sent) in [
        (PictureRoutes::SHIPPED, false),
        (PictureRoutes::new(false, true), false),
        (PictureRoutes::new(true, false), true),
    ] {
        let (_, effects) = pictured_run(KEY, routes);
        let expected = if sent {
            vec![
                user(),
                crate::context::InputItem::Image(jpeg()),
                line(SNAPSHOT),
            ]
        } else {
            vec![user(), line(CANT_SEE)]
        };
        assert_eq!(started_input(&effects), Some(expected), "{routes:?}");
        assert_eq!(told_cant_see(&effects), usize::from(!sent), "{routes:?}");
        assert_eq!(
            effects.contains(&Effect::Store(StoreOp::CantSeeScreen { run: R })),
            !sent,
            "{routes:?}"
        );
    }
}

#[test]
fn a_picture_fact_changing_alone_never_fails_a_hold() {
    let (core, hold) = held(
        vec![
            ready(CHATGPT, 7, &["m"]),
            Input::Engines(KEY, pictured(5, &["m"])),
        ],
        choice(KEY, "m"),
        None,
    );
    for images in [&[][..], &["m", "other"], &["other"]] {
        assert!(
            !core.hold_availability_changes(KEY, &pictured(5, images), Some(&hold)),
            "{images:?}"
        );
    }
    assert!(core.hold_availability_changes(KEY, &pictured(6, &["m"]), Some(&hold)));
}

#[test]
fn a_grant_refuses_a_picture_whose_fact_alone_went_false_and_rebuilds_without_it() {
    use crate::coordinator::PictureRoutes;
    let (mut core, effects) = pictured_run(KEY, PictureRoutes::new(true, false));
    assert_eq!(
        started_input(&effects),
        Some(vec![
            user(),
            crate::context::InputItem::Image(jpeg()),
            line(SNAPSHOT)
        ])
    );
    assert_eq!(
        core.handle(Input::Engines(KEY, pictured(5, &[]))),
        Ok(Vec::new()),
        "text authority, kind, tag and slug are unchanged"
    );
    let refused = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .expect("current");
    assert!(refused.contains(&Effect::Rebuild(SendKey::Build(key()))));
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_)))
    );
    assert!(refused.contains(&Effect::Store(StoreOp::CantSeeScreen { run: R })));
    assert_eq!(told_cant_see(&refused), 1);
    assert_eq!(failure(&refused), None);
    let again = super::engine_key(&core, R);
    assert_ne!(again, key(), "a new attempt");
    let rebuilt = core
        .handle(built_for(again, &choice(KEY, "m")))
        .expect("current");
    assert_eq!(started_input(&rebuilt), Some(vec![user(), line(CANT_SEE)]));
    let granted = core
        .handle(Input::SendReady {
            key: SendKey::Build(again),
            hold: None,
        })
        .expect("current");
    assert!(
        granted
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_)))
    );
}

#[test]
fn a_revoked_picture_grant_that_wrote_nothing_rebuilds_without_the_picture() {
    use crate::coordinator::PictureRoutes;
    for unsent in [true, false] {
        let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
        let effects = core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current");
        let lease = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(*lease),
                _ => None,
            })
            .expect("granted");
        let open = core.picture_grant(lease).expect("a revocable grant");
        assert!(open.is_open());
        core.handle(Input::Engines(KEY, pictured(5, &[])))
            .expect("current");
        assert!(!open.is_open(), "revoked when the fact went false");
        let settled = core
            .handle(if unsent {
                Input::SendUnsent { lease }
            } else {
                Input::SendSettled { lease }
            })
            .expect("current");
        assert_eq!(
            settled.contains(&Effect::Store(StoreOp::CantSeeScreen { run: R })),
            unsent,
            "{unsent}"
        );
        assert_eq!(told_cant_see(&settled), usize::from(unsent), "{unsent}");
        assert_eq!(
            super::engine_key(&core, R) != key(),
            unsent,
            "only an unsent send is built again: {unsent}"
        );
    }
}

#[test]
fn a_chatgpt_picture_grant_is_never_revocable() {
    let (mut core, hold) = held(
        vec![ready(CHATGPT, 7, &["m"])],
        choice(CHATGPT, "m"),
        Some(T),
    );
    core.submit_hold(
        hold,
        "words".into(),
        Some(crate::context::Screen::Image(jpeg())),
    )
    .expect("admitted");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built_for(key(), &choice(CHATGPT, "m")))
        .expect("current");
    let effects = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .expect("current");
    let lease = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted");
    assert!(core.picture_grant(lease).is_none());
}

#[test]
fn only_a_lease_granted_with_its_picture_grant_carries_a_picture() {
    use crate::coordinator::PictureRoutes;
    use crate::engines::SendLease;
    let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
    let effects = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .expect("current");
    let lease = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted");
    let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
    let pictured = SendLease::new(lease, inputs.clone()).with_picture(core.picture_grant(lease));
    assert!(pictured.carries_picture());
    assert!(!SendLease::new(lease, inputs.clone()).carries_picture());
    assert!(
        !SendLease::new(lease, inputs)
            .with_picture(None)
            .carries_picture()
    );
}

/// A screen hold's image on `kind`'s model `m`, whose fact says it takes pictures, submitted
/// with `routes`; its turn isn't stored yet, so it has no build.
fn pictured_hold(kind: ConnectionKind, routes: crate::coordinator::PictureRoutes) -> Coordinator {
    let (mut core, hold) = held(
        vec![
            ready(CHATGPT, 7, &["m"]),
            Input::Engines(kind, pictured(5, &["m"])),
        ],
        choice(kind, "m"),
        Some(T),
    );
    core.set_pictures(routes);
    core.submit_hold(
        hold,
        "words".into(),
        Some(crate::context::Screen::Image(jpeg())),
    )
    .expect("admitted");
    core
}

fn the_fact_goes_and_comes_back(core: &mut Coordinator) {
    for images in [&[][..], &["m"]] {
        core.handle(Input::Engines(KEY, pictured(5, images)))
            .expect("an availability is never stale");
    }
}

#[test]
fn a_picture_lost_and_back_before_its_turn_is_stored_never_goes() {
    use crate::coordinator::PictureRoutes;
    let mut core = pictured_hold(KEY, PictureRoutes::new(true, false));
    the_fact_goes_and_comes_back(&mut core);
    core.handle(Input::TurnStored { run: R }).expect("current");
    let effects = core
        .handle(built_for(key(), &choice(KEY, "m")))
        .expect("current");
    assert_eq!(started_input(&effects), Some(vec![user(), line(CANT_SEE)]));
    assert_eq!(told_cant_see(&effects), 1);
}

#[test]
fn a_picture_lost_while_building_stays_lost_in_a_replacement_build() {
    use crate::coordinator::PictureRoutes;
    let mut core = pictured_hold(KEY, PictureRoutes::new(true, false));
    core.handle(Input::TurnStored { run: R }).expect("current");
    the_fact_goes_and_comes_back(&mut core);
    let mut effects = Vec::new();
    core.build(R, 1, &mut effects);
    let again = super::engine_key(&core, R);
    assert_ne!(
        again,
        key(),
        "a replacement, as a delete or a compaction starts"
    );
    let rebuilt = core
        .handle(built_for(again, &choice(KEY, "m")))
        .expect("current");
    assert_eq!(started_input(&rebuilt), Some(vec![user(), line(CANT_SEE)]));
}

#[test]
fn a_pictured_request_whose_picture_went_and_came_back_is_never_granted() {
    use crate::coordinator::PictureRoutes;
    let (mut core, effects) = pictured_run(KEY, PictureRoutes::new(true, false));
    assert_eq!(
        started_input(&effects),
        Some(vec![
            user(),
            crate::context::InputItem::Image(jpeg()),
            line(SNAPSHOT)
        ])
    );
    the_fact_goes_and_comes_back(&mut core);
    let refused = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .expect("current");
    assert!(refused.contains(&Effect::Rebuild(SendKey::Build(key()))));
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_)))
    );
    assert_eq!(told_cant_see(&refused), 1);
    let again = super::engine_key(&core, R);
    let rebuilt = core
        .handle(built_for(again, &choice(KEY, "m")))
        .expect("current");
    assert_eq!(started_input(&rebuilt), Some(vec![user(), line(CANT_SEE)]));
}

#[test]
fn a_grant_judges_the_request_the_engine_holds_not_the_runs_screen() {
    use crate::coordinator::PictureRoutes;
    let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
    core.current.as_mut().expect("current").screen = Some(crate::context::Screen::NotIncluded(
        crate::context::ScreenClass::ModelCantSee,
    ));
    let effects = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .expect("current");
    let lease = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted");
    assert!(
        core.picture_grant(lease).is_some(),
        "the engine's request still carries the image"
    );
}

#[test]
fn a_claude_code_generation_change_loses_only_a_claude_code_picture() {
    use crate::coordinator::PictureRoutes;
    for (kind, lost) in [(CLAUDE, true), (KEY, false)] {
        let mut core = pictured_hold(kind, PictureRoutes::new(true, true));
        core.handle(Input::TurnStored { run: R }).expect("current");
        core.lose_pictures(Some(CLAUDE));
        let effects = core
            .handle(built_for(key(), &choice(kind, "m")))
            .expect("current");
        let expected = if lost {
            vec![user(), line(CANT_SEE)]
        } else {
            vec![
                user(),
                crate::context::InputItem::Image(jpeg()),
                line(SNAPSHOT),
            ]
        };
        assert_eq!(started_input(&effects), Some(expected), "{kind:?}");
    }
}

/// F4: a granted picture's permit stays shut once its fact went false and came back. Proved
/// unsent, the request goes again without the picture; possibly written, the build is never
/// granted again and its replacement fails closed instead of sending the turn without it.
#[test]
fn a_granted_picture_lost_and_back_stays_shut_and_a_written_one_never_goes_again() {
    use crate::coordinator::PictureRoutes;
    for unsent in [true, false] {
        let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
        let ask = Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        };
        let effects = core.handle(ask.clone()).expect("current");
        let lease = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(*lease),
                _ => None,
            })
            .expect("granted");
        let permit = core.picture_grant(lease).expect("a revocable grant");
        the_fact_goes_and_comes_back(&mut core);
        assert!(!permit.is_open(), "{unsent}");
        let settled = core
            .handle(if unsent {
                Input::SendUnsent { lease }
            } else {
                Input::SendSettled { lease }
            })
            .expect("current");
        if unsent {
            assert_eq!(written_settlement(&settled), (0, 0, 0));
            assert_eq!(told_cant_see(&settled), 1);
            let again = super::engine_key(&core, R);
            assert_ne!(again, key(), "built again");
            let rebuilt = core
                .handle(built_for(again, &choice(KEY, "m")))
                .expect("current");
            assert_eq!(started_input(&rebuilt), Some(vec![user(), line(CANT_SEE)]));
            continue;
        }
        assert_eq!(written_settlement(&settled), (1, 1, 1));
        let refused = core.handle(ask).expect("current");
        assert_ended_without_a_send(&refused);
        assert_eq!(told_cant_see(&refused), 0);
        assert_eq!(
            failure(&refused),
            Some(kumochi_protocol::message::ErrorKey::ScreenPictureStopped.into())
        );
    }
}

fn lease_of(effects: &[Effect]) -> crate::coordinator::LeaseId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted")
}

/// How many `HoldScreen`, `SentScreen` and `EventsChanged` a written settlement of run R's
/// hold pushed, after checking the marker comes after the store op.
fn written_settlement(effects: &[Effect]) -> (usize, usize, usize) {
    let hold = HoldKey {
        epoch: 0,
        gesture: GestureId(1),
    };
    let sent = Effect::Store(StoreOp::SentScreen { run: R });
    let marker = Effect::EventsChanged { hold, thread: T };
    let at = |wanted: &Effect| effects.iter().position(|effect| effect == wanted);
    if let (Some(sent), Some(marker)) = (at(&sent), at(&marker)) {
        assert!(sent < marker, "the marker follows the store op");
    }
    let count =
        |wanted: &dyn Fn(&Effect) -> bool| effects.iter().filter(|effect| wanted(effect)).count();
    (
        count(&|effect| matches!(effect, Effect::HoldScreen { hold: key, .. } if *key == hold)),
        count(&|effect| *effect == sent),
        count(&|effect| *effect == marker),
    )
}

fn assert_ended_without_a_send(refused: &[Effect]) {
    assert!(refused.contains(&Effect::Rebuild(SendKey::Build(key()))));
    assert!(!refused.iter().any(|effect| matches!(
        effect,
        Effect::Grant(_) | Effect::Store(StoreOp::BuildContext(_)) | Effect::StartEngine { .. }
    )));
}

/// A picture lost after its grant took it ends the run by the first cause recorded: a Claude Code
/// generation move as the model being unavailable, a lost capability as the picture stopping. The
/// lease still unsettled then settles for real once its run has gone.
#[test]
fn a_written_picture_lost_ends_its_run_by_its_first_cause_and_its_lease_settles_after() {
    use crate::coordinator::PictureRoutes;
    for generation_first in [true, false] {
        let (mut core, _) = pictured_run(CLAUDE, PictureRoutes::new(true, true));
        let ask = Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        };
        let lease = lease_of(&core.handle(ask.clone()).expect("current"));
        let fact = |core: &mut Coordinator| {
            for images in [&[][..], &["m"]] {
                core.handle(Input::Engines(CLAUDE, pictured(5, images)))
                    .expect("an availability is never stale");
            }
        };
        if generation_first {
            core.lose_pictures(Some(CLAUDE));
            fact(&mut core);
        } else {
            fact(&mut core);
            core.lose_pictures(Some(CLAUDE));
        }
        let refused = core.handle(ask).expect("current");
        assert_ended_without_a_send(&refused);
        let expected = if generation_first {
            model_unavailable()
        } else {
            kumochi_protocol::message::ErrorKey::ScreenPictureStopped.into()
        };
        assert_eq!(failure(&refused), Some(expected), "{generation_first}");
        core.handle(Input::RunStored { run: R })
            .expect("its end is stored");
        assert_eq!(core.ending_runs(), 0, "the run has gone");
        let settled = core
            .handle(Input::SendSettled { lease })
            .expect("a lease outlives its run");
        assert_eq!(
            written_settlement(&settled),
            (1, 1, 1),
            "{generation_first}"
        );
    }
}

/// A written settlement tells the shell nothing for a thread whose delete it holds, and lets
/// that delete go.
#[test]
fn a_written_settlement_sends_no_marker_for_a_thread_being_deleted_and_releases_its_delete() {
    use crate::coordinator::PictureRoutes;
    let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
    let lease = lease_of(
        &core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current"),
    );
    core.handle(Input::Delete {
        thread: T,
        after_turns: 1,
        resend: false,
    })
    .expect("a delete is never stale");
    assert_eq!(core.held_deletes(), vec![T]);
    let settled = core.handle(Input::SendSettled { lease }).expect("settled");
    assert_eq!(written_settlement(&settled), (1, 1, 0));
    assert_eq!(core.held_deletes(), Vec::<ThreadId>::new());
}

/// A `ChatGPT` picture's written settlement stays as on main: no marker.
#[test]
fn a_chatgpt_written_settlement_sends_no_marker() {
    let (mut core, hold) = held(
        vec![ready(CHATGPT, 7, &["m"])],
        choice(CHATGPT, "m"),
        Some(T),
    );
    core.submit_hold(
        hold,
        "words".into(),
        Some(crate::context::Screen::Image(jpeg())),
    )
    .expect("admitted");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built_for(key(), &choice(CHATGPT, "m")))
        .expect("current");
    let lease = lease_of(
        &core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current"),
    );
    let settled = core.handle(Input::SendSettled { lease }).expect("settled");
    assert_eq!(written_settlement(&settled), (1, 1, 0));
}

/// Turning screen sharing off after a send carried the picture stops the key run with the
/// neutral screen line, and history keeps what was carried.
#[test]
fn screen_sharing_turned_off_after_a_write_stops_the_turn_and_keeps_its_history() {
    use crate::coordinator::PictureRoutes;
    let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
    let lease = lease_of(
        &core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current"),
    );
    core.handle(Input::SendSettled { lease }).expect("settled");
    let off = core.handle(Input::ScreenHoldsOff).expect("the setting");
    assert_eq!(
        failure(&off),
        Some(kumochi_protocol::message::ErrorKey::ScreenUnavailable.into())
    );
    assert!(
        !off.iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::WithdrawScreen { .. })))
    );
}

/// A later build of a run whose picture a send carried, lost since, ends as the picture stopping
/// instead of claiming the turn wasn't sent.
#[test]
fn a_later_build_of_a_written_picture_lost_since_ends_as_the_picture_stopping() {
    use crate::coordinator::PictureRoutes;
    let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
    let lease = lease_of(
        &core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .expect("current"),
    );
    core.handle(Input::SendSettled { lease }).expect("settled");
    the_fact_goes_and_comes_back(&mut core);
    let mut effects = Vec::new();
    core.build(R, 2, &mut effects);
    let rebuilt = core
        .handle(built_for(super::engine_key(&core, R), &choice(KEY, "m")))
        .expect("current");
    assert_eq!(started_input(&rebuilt), None);
    assert_eq!(
        failure(&rebuilt),
        Some(kumochi_protocol::message::ErrorKey::ScreenPictureStopped.into())
    );
}

/// A key run whose first pictured round was written, and whose picture is then lost, sends
/// nothing more: neither the inline compaction a hungry continuation would take, before its read
/// or after it, nor a later pictured grant that proves unsent, which reads no replacement.
#[test]
fn a_written_picture_lost_stops_every_later_send_of_its_run() {
    use crate::coordinator::{CompactBuilt, JobAsk, PictureRoutes, Unbuilt};
    let stopped: Option<UserError> =
        Some(kumochi_protocol::message::ErrorKey::ScreenPictureStopped.into());
    let reads = |effects: &[Effect]| {
        effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::Store(StoreOp::BuildContext(_) | StoreOp::CompactContext(_))
            )
        })
    };
    let mut outcomes = Vec::new();
    for schedule in [
        "before the inline read",
        "during the inline read",
        "later grant unsent",
    ] {
        let (mut core, _) = pictured_run(KEY, PictureRoutes::new(true, false));
        let ask = |key| Input::SendReady {
            key: SendKey::Build(key),
            hold: None,
        };
        let first = lease_of(&core.handle(ask(key())).expect("current"));
        core.handle(Input::SendSettled { lease: first })
            .expect("written");
        if schedule == "before the inline read" {
            the_fact_goes_and_comes_back(&mut core);
        }
        let mut effects = Vec::new();
        core.build(R, 2, &mut effects);
        let second = super::engine_key(&core, R);
        if schedule == "later grant unsent" {
            core.handle(built_for(second, &choice(KEY, "m")))
                .expect("current");
            let lease = lease_of(&core.handle(ask(second)).expect("current"));
            the_fact_goes_and_comes_back(&mut core);
            let unsent = core.handle(Input::SendUnsent { lease }).expect("current");
            outcomes.push((schedule, reads(&unsent), failure(&unsent)));
            continue;
        }
        let hungry = core
            .handle(Input::ContextBuilt(ContextBuilt {
                key: second,
                snapshot: None,
                result: Err(Unbuilt::TooLarge { room: Some(500) }),
            }))
            .expect("current");
        if schedule == "before the inline read" {
            outcomes.push((schedule, reads(&hungry), failure(&hungry)));
            continue;
        }
        assert!(reads(&hungry), "the compaction reads");
        the_fact_goes_and_comes_back(&mut core);
        let inline = super::engine_key(&core, R);
        core.handle(Input::CompactBuilt(CompactBuilt {
            key: inline,
            result: Ok(Some((
                JobAsk {
                    generation: Generation(5),
                    built: Built {
                        request: ModelRequest {
                            choice: Some(choice(KEY, "m")),
                            instructions: String::new(),
                            input: vec![user()],
                            tools: Vec::new(),
                        },
                        sources: vec![T],
                    },
                    covers: Some((
                        kumochi_protocol::message::EventId(20),
                        kumochi_protocol::message::EventId(31),
                    )),
                },
                100,
            ))),
        }))
        .expect("current");
        let refused = core.handle(ask(inline)).expect("current");
        let granted = refused
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_)));
        outcomes.push((schedule, granted, failure(&refused)));
    }
    assert_eq!(
        outcomes,
        [
            ("before the inline read", false, stopped.clone()),
            ("during the inline read", false, stopped.clone()),
            ("later grant unsent", false, stopped),
        ]
    );
}
