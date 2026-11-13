use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::message::{
    AssistantName, ConnectionKind, Effort, GestureId, ModelChoice, ModelInfo, ModelName, ModelSlug,
    RunOrigin, ThreadOp,
};

use super::*;
use crate::context::{ConnectionTag, RunSnapshot};
use crate::coordinator::{Availability, HoldKey, HoldRead, HoldSnapshot, Ready};
use crate::store::Generation;

const KEY: HoldKey = HoldKey {
    epoch: 7,
    gesture: GestureId(11),
};

fn defaults() -> RunSnapshot {
    RunSnapshot {
        origin: Origin::Kumo,
        assistant: AssistantName::new("private-name-sentinel").unwrap(),
        user: None,
        choice: Some(choice("captured", Effort::High)),
        connection: ConnectionTag(99),
    }
}

fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(model).unwrap(),
        effort,
        connection: ConnectionKind::ChatGpt,
    }
}

fn read(core: &Coordinator, thread: Option<ThreadId>) -> HoldRead {
    let StoreOp::ReadHold(read) = core
        .prepare_hold(
            KEY,
            thread,
            12,
            defaults(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
    else {
        panic!("hold read")
    };
    read
}

fn prepared(core: &Coordinator, thread: Option<ThreadId>) -> HoldSnapshot {
    let mut snapshot = read(core, thread).snapshot;
    snapshot.generation = thread.map(|_| Generation(0));
    snapshot
}

fn sending(core: &mut Coordinator) -> Vec<Effect> {
    core.handle(Input::SendReady {
        key: SendKey::Build(engine_key(core, R)),
        hold: None,
    })
    .unwrap()
}

#[test]
fn new_hold_allocates_only_at_submit_with_real_origin_and_no_typed_watermark() {
    let mut core = fresh();
    let read = read(&core, None);
    assert_eq!(
        (
            core.last_thread,
            core.last_run,
            core.current_run(),
            core.turns_read
        ),
        (ThreadId(3), RunId(9), None, 0)
    );
    assert_eq!(read.snapshot.thread, None);
    assert_eq!(read.snapshot.defaults.connection, ConnectionTag(0));
    let effects = core
        .submit_hold(read.snapshot, "exact recognized words".into(), None)
        .unwrap();
    assert_eq!(
        effects[0],
        Effect::Store(StoreOp::BeginTurn {
            thread: T,
            title: Some("exact recognized words".into()),
            run: R,
            text: "exact recognized words".into(),
        })
    );
    assert_eq!(
        effects[1],
        Effect::Control(ControlToShell::RunState {
            run: R,
            thread: T,
            phase: Phase::Thinking,
            error: None,
            origin: Some(RunOrigin::Hold {
                gesture: KEY.gesture
            }),
        })
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::BeginTurn { .. })))
            .count(),
        1
    );
    assert_eq!(core.turns_read, 0);
    let current = core.current.as_ref().unwrap();
    assert_eq!(current.snapshot.as_ref().unwrap().origin, Origin::Task(T));
    assert_eq!(current.hold.as_ref().unwrap().selection, 12);
    assert_eq!(
        core.handle(Input::HoldPrepared {
            key: KEY,
            result: Err(())
        }),
        Err(Stale)
    );
}

#[test]
fn preparation_refuses_pending_delete_unavailable_or_cancelled_without_revival() {
    for refusal in 0..3 {
        let mut core = fresh();
        let cancel = Arc::new(AtomicBool::new(false));
        match refusal {
            0 => cancel.store(true, Ordering::SeqCst),
            1 => {
                core.availability =
                    Availability::Unavailable(super::super::availability::sign_in_needed());
            }
            2 => {
                core.handle(Input::Delete {
                    thread: T,
                    after_turns: 0,
                    resend: false,
                })
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            core.prepare_hold(KEY, Some(T), 12, defaults(), Arc::clone(&cancel)),
            Err(())
        );
        core.availability = Availability::Ready(Ready::fixture());
        assert_eq!(
            core.prepare_hold(KEY, None, 12, defaults(), cancel),
            Err(())
        );
        assert_eq!(
            (core.last_run, core.last_thread, core.turns_read),
            (RunId(9), ThreadId(3), 0)
        );
    }
}

#[test]
fn stale_cancel_connection_pending_delete_and_unread_target_create_no_turn() {
    for refusal in 0..5 {
        let mut core = fresh();
        let mut snapshot = prepared(&core, Some(T));
        match refusal {
            0 => snapshot.cancel.store(true, Ordering::SeqCst),
            1 => snapshot.defaults.connection = ConnectionTag(2),
            2 => {
                core.handle(Input::Delete {
                    thread: T,
                    after_turns: 3,
                    resend: false,
                })
                .unwrap();
            }
            3 => snapshot.generation = None,
            4 => {
                core.availability = Availability::Ready(Ready {
                    connection: ConnectionTag(0),
                    models: Vec::new(),
                    trusted_draw: false,
                });
            }
            _ => unreachable!(),
        }
        let retry = snapshot.clone();
        assert_eq!(core.submit_hold(snapshot, "words".into(), None), Err(()));
        core.availability = Availability::Ready(Ready::fixture());
        assert_eq!(
            core.submit_hold(retry, "later valid words".into(), None),
            Err(())
        );
        assert_eq!(
            (
                core.last_run,
                core.last_thread,
                core.turns_read,
                core.current_run()
            ),
            (RunId(9), ThreadId(3), 0, None)
        );
    }
}

#[test]
fn pinned_choice_survives_delayed_build_but_still_gets_the_first_catalog_check() {
    for listed in [false, true] {
        let mut core = fresh();
        core.submit_hold(prepared(&core, Some(T)), "words".into(), None)
            .unwrap();
        let effects = core.handle(Input::TurnStored { run: R }).unwrap();
        let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
            panic!("build: {effects:?}")
        };
        assert!(!build.first, "a pinned build must not reread thread choice");
        let mut build = build.clone();
        let mut changed = defaults();
        changed.choice = Some(choice("later", Effort::Low));
        build.snapshot.get_or_insert(changed.clone());
        assert_eq!(
            build.snapshot.as_ref().unwrap().choice,
            Some(choice("captured", Effort::High))
        );
        core.handle(Input::Engines(Availability::Ready(Ready {
            connection: ConnectionTag(0),
            models: if listed {
                vec![ModelInfo {
                    slug: ModelSlug::new("captured").unwrap(),
                    display_name: ModelName::new("captured model").unwrap(),
                }]
            } else {
                Vec::new()
            },
            trusted_draw: false,
        })))
        .unwrap();
        let Input::ContextBuilt(mut done) = built(R, 1, &[T]) else {
            unreachable!()
        };
        done.snapshot = Some(changed);
        let effects = core.handle(Input::ContextBuilt(done)).unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::StartEngine { .. }))
                .count(),
            usize::from(listed)
        );
        if listed {
            assert_eq!(
                core.current
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .choice,
                Some(choice("captured", Effort::High))
            );
            assert_eq!(
                sending(&mut core)
                    .iter()
                    .filter(|effect| matches!(effect, Effect::Grant(_)))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn cancellation_after_begin_turn_and_model_away_back_never_grant() {
    for invalidation in 0..3 {
        let mut core = fresh();
        let snapshot = prepared(&core, Some(T));
        let cancel = Arc::clone(&snapshot.cancel);
        core.submit_hold(snapshot, "words".into(), None).unwrap();
        core.handle(Input::TurnStored { run: R }).unwrap();
        core.handle(built(R, 1, &[T])).unwrap();
        let key = engine_key(&core, R);
        match invalidation {
            0 => cancel.store(true, Ordering::SeqCst),
            1 => {
                for model in ["away", "captured"] {
                    core.handle(Input::ThreadOp(ThreadOp::SetModel {
                        thread: T,
                        choice: choice(model, Effort::High),
                    }))
                    .unwrap();
                }
            }
            2 => {
                for connection in [ConnectionTag(2), ConnectionTag(0)] {
                    core.handle(Input::Engines(Availability::Ready(Ready {
                        connection,
                        ..Ready::fixture()
                    })))
                    .unwrap();
                }
            }
            _ => unreachable!(),
        }
        assert!(cancel.load(Ordering::SeqCst));
        assert!(
            !core
                .handle(Input::SendReady {
                    key: SendKey::Build(key),
                    hold: None
                })
                .unwrap()
                .iter()
                .any(|effect| matches!(effect, Effect::Grant(_)))
        );
        assert!(
            !core
                .handle(Input::SendReady {
                    key: SendKey::Build(key),
                    hold: None
                })
                .unwrap()
                .iter()
                .any(|effect| matches!(effect, Effect::Grant(_)))
        );
    }
}

#[test]
fn explicit_cancel_matches_epoch_and_preserves_the_typed_counter() {
    let mut core = fresh();
    core.submit_hold(prepared(&core, None), "words".into(), None)
        .unwrap();
    assert_eq!(core.cancel_hold(HoldKey { epoch: 8, ..KEY }), Vec::new());
    assert_eq!(core.current_run(), Some(R));
    assert!(!core.cancel_hold(KEY).is_empty());
    assert_eq!(core.current_run(), None);
    assert_eq!(core.turns_read, 0);
    assert_eq!(core.cancel_hold(KEY), Vec::new());
}

#[test]
fn hold_carriers_redact_content_and_ignore_execution_capabilities_in_equality() {
    let mut core = fresh();
    let read = read(&core, None);
    let mut other = read.clone();
    other.control = crate::recall::control::Control::new(std::time::Instant::now());
    other.snapshot.cancel = Arc::new(AtomicBool::new(true));
    assert_eq!(read, other);
    let prepared = Input::HoldPrepared {
        key: KEY,
        result: Ok(read.snapshot.clone()),
    };
    let failed = StoreOp::ReadHold(read.clone()).failed();
    assert_eq!(
        failed,
        Some(Input::HoldPrepared {
            key: KEY,
            result: Err(())
        })
    );
    let effects = core
        .submit_hold(read.snapshot.clone(), "transcript-sentinel".into(), None)
        .unwrap();
    for debug in [
        format!("{:?}", StoreOp::ReadHold(read)),
        format!("{prepared:?}"),
        format!("{effects:?}"),
    ] {
        for marker in ["transcript-sentinel", "private-name-sentinel"] {
            assert!(!debug.contains(marker));
            assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
        }
    }
}

#[test]
fn a_held_runs_screen_goes_with_its_request_or_fails_the_run() {
    use crate::context::{InputItem, Jpeg, ModelRequest, Role, Screen, ScreenClass};
    let image = Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    let user = InputItem::Message {
        role: Role::User,
        text: "words".to_owned(),
    };
    let line = |text: &str| InputItem::Message {
        role: Role::Developer,
        text: text.to_owned(),
    };
    let snapshot = "The image is a snapshot from when the user spoke.";
    let not_ready = "The screen was not included: it wasn't ready in time.";
    let cases = [
        (
            Some(Screen::Image(image.clone())),
            ConnectionKind::ChatGpt,
            Some(vec![
                user.clone(),
                InputItem::Image(image.clone()),
                line(snapshot),
            ]),
        ),
        (
            Some(Screen::NotIncluded(ScreenClass::NotReady)),
            ConnectionKind::ChatGpt,
            Some(vec![user.clone(), line(not_ready)]),
        ),
        (None, ConnectionKind::ChatGpt, Some(vec![user.clone()])),
        (
            Some(Screen::Image(image)),
            ConnectionKind::AnthropicKey,
            None,
        ),
    ];
    for (screen, connection, sent) in cases {
        let mut core = fresh();
        core.handle(Input::Engines(Availability::Ready(Ready {
            connection: ConnectionTag(0),
            models: vec![ModelInfo {
                slug: ModelSlug::new("captured").unwrap(),
                display_name: ModelName::new("captured model").unwrap(),
            }],
            trusted_draw: false,
        })))
        .unwrap();
        core.submit_hold(prepared(&core, Some(T)), "words".into(), screen)
            .unwrap();
        core.handle(Input::TurnStored { run: R }).unwrap();
        let Input::ContextBuilt(mut done) = built(R, 1, &[T]) else {
            unreachable!()
        };
        done.result = Ok(Built {
            request: ModelRequest {
                choice: Some(ModelChoice {
                    connection,
                    ..choice("captured", Effort::High)
                }),
                instructions: String::new(),
                input: vec![user.clone()],
                tools: Vec::new(),
            },
            sources: vec![T],
        });
        let effects = core.handle(Input::ContextBuilt(done)).unwrap();
        let started = effects.iter().find_map(|effect| match effect {
            Effect::StartEngine { request } => Some(request.request.input.clone()),
            _ => None,
        });
        assert_eq!(started, sent);
        if sent.is_none() {
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::Control(ControlToShell::RunState { error: Some(error), .. })
                    if error.key() == kumochi_protocol::message::ErrorKey::ScreenUnavailable
            )));
        }
    }
}

#[test]
fn a_held_runs_image_stays_out_of_inline_compaction_and_follows_its_turn_after() {
    use kumochi_protocol::message::EventId;

    use crate::context::{InputItem, Jpeg, ModelRequest, Role, Screen};
    use crate::coordinator::{CompactBuilt, ContextBuilt, JobAsk, Unbuilt};
    let image = Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    let message = |role, text: &str| InputItem::Message {
        role,
        text: text.to_owned(),
    };
    let request = |input| ModelRequest {
        choice: Some(choice("captured", Effort::High)),
        instructions: String::new(),
        input,
        tools: Vec::new(),
    };
    let mut core = ready_for("captured");
    core.submit_hold(
        prepared(&core, Some(T)),
        "words".into(),
        Some(Screen::Image(image.clone())),
    )
    .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    let first = engine_key(&core, R);
    let effects = core
        .handle(Input::ContextBuilt(ContextBuilt {
            key: first,
            snapshot: None,
            result: Err(Unbuilt::TooLarge { room: Some(500) }),
        }))
        .unwrap();
    assert!(matches!(
        effects[..],
        [Effect::Store(StoreOp::CompactContext(_))]
    ));
    let compacting = engine_key(&core, R);
    let summary = vec![message(Role::User, "older runs")];
    let effects = core
        .handle(Input::CompactBuilt(CompactBuilt {
            key: compacting,
            result: Ok(Some((
                JobAsk {
                    generation: Generation(5),
                    built: Built {
                        request: request(summary.clone()),
                        sources: vec![T],
                    },
                    covers: Some((EventId(20), EventId(31))),
                },
                100,
            ))),
        }))
        .unwrap();
    assert_eq!(
        started(&effects),
        vec![summary],
        "the summary goes without the image"
    );
    core.handle(Input::SendReady {
        key: SendKey::Build(compacting),
        hold: None,
    })
    .unwrap();
    core.handle(engine(compacting, TurnEvent::TextDelta("Short".to_owned())))
        .unwrap();
    core.handle(engine(compacting, TurnEvent::Finished(Finish::Completed)))
        .unwrap();
    core.handle(Input::SummaryStored {
        owner: SendKey::Build(compacting),
        applied: true,
    })
    .unwrap();
    let rebuilt = engine_key(&core, R);
    let note = message(Role::Developer, "summary of older runs");
    let user = message(Role::User, "words");
    let effects = core
        .handle(Input::ContextBuilt(ContextBuilt {
            key: rebuilt,
            snapshot: None,
            result: Ok(Built {
                request: request(vec![note.clone(), user.clone()]),
                sources: vec![T],
            }),
        }))
        .unwrap();
    assert_eq!(
        started(&effects),
        vec![vec![
            note,
            user,
            InputItem::Image(image),
            message(
                Role::Developer,
                "The image is a snapshot from when the user spoke."
            ),
        ]]
    );
}

/// A coordinator whose engines are ready with `model` listed.
fn ready_for(model: &str) -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Engines(Availability::Ready(Ready {
        connection: ConnectionTag(0),
        models: vec![ModelInfo {
            slug: ModelSlug::new(model).unwrap(),
            display_name: ModelName::new("captured model").unwrap(),
        }],
        trusted_draw: false,
    })))
    .unwrap();
    core
}

/// The inputs of every engine start in `effects`.
fn started(effects: &[Effect]) -> Vec<Vec<crate::context::InputItem>> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::StartEngine { request } => Some(request.request.input.clone()),
            _ => None,
        })
        .collect()
}
