//! Asking about a clicked part in the core loop (docs/development/runs.md, "Asking about a part").

use std::sync::Arc;

use kumochi_protocol::message::{
    ControlToShell, Deleted, MAX_PENDING_DELETES, Phase, RunId, SceneId, ThreadId,
};
use kumochi_scene::Handle;

use super::{R, T, WALL_CLOCK, admit, built, start_engine, state};
use crate::coordinator::availability::sign_in_needed;
use crate::coordinator::{
    Availability, BuildKey, Coordinator, Effect, Input, SendKey, Stale, StoreOp,
};
use crate::drawing::{self, Reply, Target, Write};
use crate::engines::CallId;
use crate::store::{RunEnd, Store};
use crate::test_support::Scratch;

/// A coordinator whose last ids were thread 4 and run 10, holding run 10's stored car in thread
/// 4: solid "a" (handle 1/1) is labelled and solid "b" (handle 1/2) isn't.
fn drawn(scratch: &Scratch) -> (Coordinator, SceneId) {
    drawn_by(scratch, Drew::Public)
}

/// Whether the run that drew the car carried protected material.
#[derive(Clone, Copy, PartialEq)]
enum Drew {
    Public,
    Protected,
}

/// `drawn`, by a run that drew it `drew`.
fn drawn_by(scratch: &Scratch, drew: Drew) -> (Coordinator, SceneId) {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store.main_thread().expect("main");
    store.begin_turn(T, Some("test"), R, "turn").expect("run");
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"car","surface":"overlay","alt":"a car"},"layout_version":2})).expect("header");
    let step = serde_json::from_value(serde_json::json!({"id":"s1","put":[{"view3d":{"id":"v"}},
        {"solid":{"in":"v","id":"a","shape":"box","size":[2,2,2],"label":"Wheel","info":"Front left"}},
        {"solid":{"in":"v","id":"b","shape":"box","size":[1,1,1]}}]}))
    .expect("typed step");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread: T,
            run: R,
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step: Arc::new(step),
            protected: drew == Drew::Protected,
        })
        .expect("first step")
    else {
        panic!("step reply")
    };
    store.end_run(T, R, &[], RunEnd::Done).expect("outcome");
    let mut core = Coordinator::ready_for_tests(ThreadId(1), T, R, Vec::new());
    assert_eq!(core.restore_drawings(store.drawing_scenes()), Vec::new());
    (core, scene)
}

fn handle(slot: usize) -> Handle {
    Handle::new(1, slot).expect("in range")
}

fn ask(scene: SceneId, element: Handle) -> Input {
    Input::Ask {
        turn: 2,
        scene,
        element,
    }
}

fn refused() -> Vec<Effect> {
    vec![Effect::Control(ControlToShell::TurnRefused {
        turn: 2,
        thread: None,
    })]
}

fn delete(thread: ThreadId, after_turns: u32) -> Input {
    Input::Delete {
        thread,
        after_turns,
        resend: false,
    }
}

fn answer(thread: ThreadId, outcome: Deleted) -> Input {
    Input::Deleted {
        thread,
        outcome,
        last_run: None,
    }
}

fn typed() -> Input {
    Input::Turn {
        turn: 3,
        thread: None,
        text: "something else".into(),
    }
}

/// The ask's run, the next id after run 10.
const ASKED: RunId = RunId(11);

#[test]
fn an_ask_starts_a_run_in_the_scenes_thread_with_the_parts_text_and_the_shell_follows_it() {
    let scratch = Scratch::new("ask-starts");
    let (mut core, scene) = drawn(&scratch);
    let text = "Tell me about this part of the drawing: \u{201c}Wheel\u{201d}.\n\
                Its description: \u{201c}Front left\u{201d}.\n\
                The drawing shows: \u{201c}a car\u{201d}.";
    let mut thinking = state(ASKED, T, Phase::Thinking);
    if let Effect::Control(ControlToShell::RunState { origin, .. }) = &mut thinking {
        *origin = Some(kumochi_protocol::message::RunOrigin::Typed { turn: 2 });
    }
    assert_eq!(
        core.handle(ask(scene, handle(1))),
        Ok(vec![
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: ASKED,
                text: text.into(),
                screen: None,
            }),
            thinking,
            Effect::ArmDeadline {
                run: ASKED,
                after: WALL_CLOCK,
            },
            Effect::Control(ControlToShell::Focus {
                run: ASKED,
                thread: T,
            }),
        ])
    );
}

/// A valid ask while signed out fails at once, but the shell is told to follow its run first,
/// so the failure shows in the scene's thread.
#[test]
fn a_signed_out_ask_is_focused_before_it_fails() {
    let scratch = Scratch::new("ask-signed-out");
    let (mut core, scene) = drawn(&scratch);
    core.handle(Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Unavailable(sign_in_needed()),
    ))
    .expect("never stale");
    let effects = core.handle(ask(scene, handle(1))).expect("not stale");
    assert!(
        effects.contains(&Effect::Control(ControlToShell::Focus {
            run: ASKED,
            thread: T,
        })),
        "{effects:?}"
    );
    let stored = core
        .handle(Input::TurnStored { run: ASKED })
        .expect("current");
    let attempt = stored
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key.attempt),
            _ => None,
        })
        .expect("a build starts");
    let failed = core.handle(built(ASKED, attempt, &[T])).expect("current");
    assert!(
        failed.contains(&Effect::Control(ControlToShell::RunState {
            run: ASKED,
            thread: T,
            phase: Phase::Failed,
            error: Some(sign_in_needed()),
            origin: None,
        })),
        "it fails at its first build: {failed:?}"
    );
}

#[test]
fn an_ask_replaces_the_current_run_in_another_thread_and_focuses_its_own() {
    let scratch = Scratch::new("ask-replaces");
    let (mut core, scene) = drawn(&scratch);
    core.handle(typed()).expect("a turn is never stale");
    assert_eq!(core.current_run(), Some(ASKED));
    let effects = core.handle(ask(scene, handle(1))).expect("not stale");
    let next = RunId(12);
    assert!(
        effects.contains(&state(ASKED, ThreadId(5), Phase::Interrupted)),
        "{effects:?}"
    );
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Control(ControlToShell::RunState { run, thread: T, phase: Phase::Thinking, .. })
                if *run == next
        )),
        "{effects:?}"
    );
    assert_eq!(
        effects.last(),
        Some(&Effect::Control(ControlToShell::Focus {
            run: next,
            thread: T
        }))
    );
    assert_eq!(core.current_run(), Some(next));
}

#[test]
fn a_part_that_does_not_resolve_is_refused_without_a_run_or_an_id() {
    let scratch = Scratch::new("ask-refused");
    let (mut core, scene) = drawn(&scratch);
    for (scene, element) in [
        (scene, handle(0)),
        (scene, handle(2)),
        (scene, handle(3)),
        (scene, Handle::new(2, 1).expect("in range")),
        (SceneId(scene.0 + 1), handle(1)),
    ] {
        assert_eq!(
            core.handle(ask(scene, element)),
            Ok(refused()),
            "{element:?}"
        );
        assert_eq!(core.current_run(), None);
    }
    core.handle(ask(scene, handle(1))).expect("not stale");
    assert_eq!(core.current_run(), Some(ASKED), "a refusal takes no id");
}

/// A part of a scene a protected run drew is never quoted into a turn, whose text every later
/// model reads.
#[test]
fn a_part_of_a_protected_scene_is_refused_without_a_run() {
    let scratch = Scratch::new("ask-protected");
    let (mut core, scene) = drawn_by(&scratch, Drew::Protected);
    assert_eq!(core.handle(ask(scene, handle(1))), Ok(refused()));
    assert_eq!(core.current_run(), None);
}

#[test]
fn an_ask_in_a_thread_being_deleted_or_deleted_is_refused() {
    let scratch = Scratch::new("ask-deleting");
    let (mut core, scene) = drawn(&scratch);
    core.handle(delete(T, 0)).expect("accepted");
    assert_eq!(core.handle(ask(scene, handle(1))), Ok(refused()));
    core.handle(answer(T, Deleted::NotHidden))
        .expect("answered");
    core.handle(delete(T, 1)).expect("accepted");
    core.handle(answer(T, Deleted::Removed)).expect("answered");
    assert_eq!(
        core.handle(ask(scene, handle(1))),
        Ok(refused()),
        "its scene left with it"
    );
    assert_eq!(core.current_run(), None);
}

/// A scene whose thread was deleted while a drawing write is still pending is held until that
/// write answers, but it is gone: an ask about it is refused.
#[test]
fn a_scene_held_after_its_thread_left_is_refused() {
    let scratch = Scratch::new("ask-held-gone");
    let (mut core, scene) = drawn(&scratch);
    core.handle(typed()).expect("a turn is never stale");
    admit(&mut core, ASKED);
    let args = serde_json::to_vec(&serde_json::json!({"records":[
        {"begin":{"v":1,"scene":"flat","surface":"overlay","alt":"a box"}},
        {"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("arguments");
    let pending = core
        .handle(Input::Drawing(drawing::Input::Call {
            thread: ThreadId(5),
            run: ASKED,
            call: CallId::new("draw").expect("call"),
            args,
        }))
        .expect("fresh call");
    assert!(
        pending
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. }))),
        "a write is pending: {pending:?}"
    );
    core.handle(delete(T, core.turns_read)).expect("accepted");
    core.handle(answer(T, Deleted::Removed)).expect("answered");
    assert_eq!(core.handle(ask(scene, handle(1))), Ok(refused()));
    assert_eq!(
        core.current_run(),
        Some(ASKED),
        "the refusal replaced nothing"
    );
}

/// M1: an ask the brain reads retires the fences it passed even when its part doesn't resolve,
/// so stale asks never pile fences up past what the shell holds.
#[test]
fn stale_asks_retire_fences_so_deletes_past_the_shells_limit_never_break() {
    let scratch = Scratch::new("ask-fences");
    let (mut core, scene) = drawn(&scratch);
    for (read, thread) in (0u32..).zip(20u64..).take(2 * MAX_PENDING_DELETES) {
        let thread = ThreadId(thread);
        core.handle(delete(thread, read + 1)).expect("accepted");
        assert_eq!(
            core.handle(answer(thread, Deleted::Removed)),
            Ok(vec![Effect::Control(ControlToShell::ThreadDeleted {
                thread,
                outcome: Deleted::Removed,
                last_run: None,
            })])
        );
        assert_eq!(core.handle(ask(scene, handle(2))), Ok(refused()));
        assert!(core.fenced.is_empty());
    }
}

/// How an ask's run is cut short.
#[derive(Clone, Copy, Debug)]
enum Cut {
    Delete,
    SignOut,
    Replace,
}

/// How far the ask's run got before the cut.
#[derive(Clone, Copy, Debug)]
enum Stage {
    Unstored,
    Building,
    Started,
}

fn key(attempt: u64) -> BuildKey {
    BuildKey {
        run: ASKED,
        round: 1,
        attempt,
    }
}

/// Whether `effects` move the ask's run toward the model.
fn advances(effects: &Result<Vec<Effect>, Stale>) -> bool {
    effects.as_ref().is_ok_and(|effects| {
        effects.iter().any(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => build.key.run == ASKED,
            Effect::StartEngine { request } => request.run == ASKED,
            Effect::Grant(lease) => matches!(lease.key, SendKey::Build(key) if key.run == ASKED),
            _ => false,
        })
    })
}

/// M3: a delete, a sign-out or a typed turn cut the ask's run before its turn is stored, while
/// its build is held, or after its engine started but before the send is granted; whatever
/// arrives for it later builds, starts and grants nothing.
#[test]
fn a_cut_at_each_stage_before_the_grant_leaves_the_ask_nothing_to_send() {
    for stage in [Stage::Unstored, Stage::Building, Stage::Started] {
        for cut in [Cut::Delete, Cut::SignOut, Cut::Replace] {
            let scratch = Scratch::new("ask-cut");
            let (mut core, scene) = drawn(&scratch);
            core.handle(ask(scene, handle(1))).expect("not stale");
            let mut attempt = 1;
            if !matches!(stage, Stage::Unstored) {
                let effects = core
                    .handle(Input::TurnStored { run: ASKED })
                    .expect("current");
                let Some(Effect::Store(StoreOp::BuildContext(build))) = effects.first() else {
                    panic!("a build starts: {effects:?}");
                };
                attempt = build.key.attempt;
            }
            if matches!(stage, Stage::Started) {
                assert_eq!(
                    core.handle(built(ASKED, attempt, &[T])),
                    Ok(vec![start_engine(ASKED, T, attempt)]),
                    "{stage:?}"
                );
            }
            let after = core.turns_read;
            let effects = core.handle(match cut {
                Cut::Delete => delete(T, after),
                Cut::SignOut => Input::Engines(
                    kumochi_protocol::message::ConnectionKind::ChatGpt,
                    Availability::Unavailable(sign_in_needed()),
                ),
                Cut::Replace => typed(),
            });
            assert!(!advances(&effects), "{stage:?} {cut:?}");
            assert_ne!(core.current_run(), Some(ASKED), "{stage:?} {cut:?}");
            for later in [
                Input::TurnStored { run: ASKED },
                built(ASKED, attempt, &[T]),
                Input::SendReady {
                    key: SendKey::Build(key(attempt)),
                    hold: None,
                },
            ] {
                let effects = core.handle(later);
                assert!(!advances(&effects), "{stage:?} {cut:?} {effects:?}");
                assert!(
                    !effects.iter().flatten().any(|effect| matches!(
                        effect,
                        Effect::Control(ControlToShell::Focus { .. })
                    )),
                    "{stage:?} {cut:?}"
                );
            }
            if matches!(cut, Cut::Replace) {
                assert_eq!(core.current_run(), Some(RunId(12)), "{stage:?}");
            }
        }
    }
}

/// An ask on a recovered `ChatGPT` route in a thread from before the cutoff would carry an old
/// drawing's own text as a new turn, so it fails unsent. A typed turn in that thread, an ask in a
/// thread after the cutoff and an ask answered on Claude Code all start.
#[test]
fn an_ask_from_before_the_cutoff_never_reaches_the_recovered_route() {
    use kumochi_protocol::message::{
        AssistantName, ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug,
    };
    let haiku = ModelSlug::new("haiku").expect("slug");
    let claude = ModelChoice {
        model: haiku.clone(),
        effort: Effort::Low,
        connection: ConnectionKind::ClaudeCode,
    };
    for (case, (last_thread, asked, choice, starts)) in [
        (4, true, None, false),
        (4, false, None, true),
        (3, true, None, true),
        (4, true, Some(claude), true),
    ]
    .into_iter()
    .enumerate()
    {
        let scratch = Scratch::new(&format!("ask-recovered-{case}"));
        let (mut core, scene) = drawn(&scratch);
        let cut = crate::store::Boundary::fixture(0, last_thread, 10);
        core.recovered_chatgpt = vec![cut];
        core.availability[ConnectionKind::ClaudeCode] =
            Availability::Ready(crate::coordinator::Ready {
                connection: crate::context::ConnectionTag(5),
                models: vec![ModelInfo {
                    slug: haiku.clone(),
                    display_name: ModelName::new("Haiku").expect("name"),
                    supported_efforts: None,
                }],
                trusted_draw: false,
                images: Vec::new(),
            });
        core.handle(if asked {
            ask(scene, handle(1))
        } else {
            Input::Turn {
                turn: 2,
                thread: Some(T),
                text: "plain".into(),
            }
        })
        .expect("never stale");
        core.handle(Input::TurnStored { run: ASKED })
            .expect("current");
        let mut made = built(ASKED, 1, &[T]);
        if let Input::ContextBuilt(made) = &mut made {
            made.snapshot = Some(crate::context::RunSnapshot {
                origin: crate::context::Origin::Task(T),
                assistant: AssistantName::new("Kumo").expect("name"),
                user: None,
                choice: choice.clone(),
                connection: crate::context::ConnectionTag(0),
            });
            if let Ok(body) = &mut made.result {
                body.recovered_context = choice.is_none().then_some(cut);
            }
        }
        let effects = core.handle(made).expect("current");
        assert_eq!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartEngine { .. })),
            starts,
            "case {case}"
        );
        assert_eq!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::Store(StoreOp::EndRun {
                    run: ASKED,
                    end: RunEnd::Failed(crate::store::Failure::Store),
                    ..
                })
            )),
            !starts,
            "case {case}"
        );
    }
}
