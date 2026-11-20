//! The tool loop: each round's calls get results, its native items are stored in output order,
//! the next round is built from the store, and the run's call and round budgets are charged
//! before anything over them is kept or sent (D13-M2).

use kumochi_protocol::message::{Deleted, Phase, ThreadId};

use super::{
    R, T, ack, committed, draw_call, end_run, ends_failed, engine, fresh, request, run_end,
    started, state, text,
};
use crate::context::{Built, ConnectionTag, ModelRequest, Origin};
use crate::coordinator::{
    BuildKey, ContextBuilt, Coordinator, Effect, Input, MAX_CALLS, MAX_ROUNDS, RoundItems, Stale,
    StoreOp,
};
use crate::dispatch::{Caller, Dispatch, Route, ToolError, ToolOutput, schemas};
use crate::engines::{CallId, Finish, Opaque, ToolName, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd};

fn id(n: u32) -> CallId {
    CallId::new(&format!("tool-{n}")).expect("short ASCII")
}

fn item(bytes: &str) -> Opaque {
    Opaque::new(bytes.as_bytes().to_vec()).expect("small")
}

fn called(n: u32) -> [TurnEvent; 3] {
    [
        TurnEvent::ToolCallStarted {
            call: id(n),
            name: ToolName::Unknown,
        },
        TurnEvent::ToolArgsDelta {
            call: id(n),
            delta: "{}".to_owned(),
        },
        TurnEvent::ToolCallReady {
            call: id(n),
            args: "{}".to_owned(),
        },
    ]
}

fn completed(core: &Coordinator) -> Input {
    engine(
        super::engine_key(core, R),
        TurnEvent::Finished(Finish::Completed),
    )
}

fn round_items(round: u32, text: Option<&str>, items: Vec<Opaque>, calls: Vec<CallId>) -> Effect {
    Effect::Store(StoreOp::RoundItems(RoundItems {
        thread: T,
        run: R,
        round,
        connection: crate::context::Route {
            kind: kumochi_protocol::message::ConnectionKind::ChatGpt,
            tag: ConnectionTag(0),
        },
        text: text.map(Into::into),
        items,
        key_calls: Vec::new(),
        calls: calls
            .into_iter()
            .map(|call| (call, ToolOutput::Error(ToolError::Unknown)))
            .collect(),
    }))
}

fn answered(round: u32, text: Option<&str>, calls: Vec<(CallId, ToolOutput)>) -> Effect {
    Effect::Store(StoreOp::RoundItems(RoundItems {
        thread: T,
        run: R,
        round,
        connection: crate::context::Route {
            kind: kumochi_protocol::message::ConnectionKind::ChatGpt,
            tag: ConnectionTag(0),
        },
        text: text.map(Into::into),
        items: Vec::new(),
        key_calls: Vec::new(),
        calls,
    }))
}

const B: ThreadId = ThreadId(2);

/// `key`'s build in task 4, rendered from `sources` and offering the test tools.
fn offering(key: BuildKey, sources: &[ThreadId]) -> Input {
    Input::ContextBuilt(ContextBuilt {
        key,
        snapshot: None,
        result: Ok(Built {
            request: ModelRequest {
                tools: schemas(Origin::Task(T), Route::Model),
                ..request()
            },
            sources: sources.to_vec(),
        }),
    })
}

fn attempt(round: u32, attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round,
        attempt,
    }
}

/// Run 10 in task 4, its first build rendered from `sources` and admitted.
fn probing(sources: &[ThreadId]) -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(offering(attempt(1, 1), sources))
        .expect("admitted");
    core
}

fn probe(n: u32, args: &str) -> [TurnEvent; 2] {
    [
        TurnEvent::ToolCallStarted {
            call: id(n),
            name: ToolName::Probe,
        },
        TurnEvent::ToolCallReady {
            call: id(n),
            args: args.to_owned(),
        },
    ]
}

fn probed(word: &str) -> ToolOutput {
    ToolOutput::Text(format!("probed {word}"))
}

/// The round's completion, which must send exactly one call's write; its acknowledgement.
fn commits(core: &mut Coordinator) -> Input {
    let effects = core.handle(completed(core)).expect("current");
    let [Effect::Store(StoreOp::CommitCall(call))] = effects.as_slice() else {
        panic!("one write, not {effects:?}");
    };
    Input::CallCommitted {
        key: call.key.clone(),
        outcome: crate::dispatch::Committed::Done,
        then: None,
    }
}

/// Plays each event of the current round, none of which may say anything.
fn play(core: &mut Coordinator, events: impl IntoIterator<Item = TurnEvent>) {
    for event in events {
        assert_eq!(
            core.handle(engine(super::engine_key(core, R), event)),
            Ok(Vec::new())
        );
    }
}

/// Acks round `round`'s write, which must build round `round + 1`, and admits that build.
fn next(core: &mut Coordinator, round: u32) -> Vec<Effect> {
    let effects = core
        .handle(Input::RoundStored { run: R, round })
        .expect("current");
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("one build, not {effects:?}");
    };
    assert_eq!(build.key.round, round + 1);
    core.handle(Input::ContextBuilt(ContextBuilt {
        key: build.key,
        snapshot: None,
        result: Ok(Built {
            request: request(),
            sources: vec![T],
        }),
    }))
    .expect("current")
}

fn starts_round(effects: &[Effect], round: u32) -> bool {
    matches!(
        effects,
        [Effect::StartEngine { request }]
            if matches!(request.key, crate::coordinator::SendKey::Build(BuildKey { round: r, .. }) if r == round)
    )
}

#[test]
fn a_typed_chatgpt_tool_round_reuses_the_first_granted_effort() {
    use crate::context::RunSnapshot;
    use crate::coordinator::{Availability, Ready, SendKey};
    use kumochi_protocol::message::{
        AssistantName, ConnectionKind, Effort, EffortSet, ModelChoice, ModelInfo, ModelName,
        ModelSlug,
    };

    let model = ModelSlug::new("sparse").unwrap();
    let catalog = |effort| {
        Input::Engines(
            ConnectionKind::ChatGpt,
            Availability::Ready(Ready {
                connection: ConnectionTag(0),
                models: vec![ModelInfo {
                    slug: model.clone(),
                    display_name: ModelName::new("Sparse").unwrap(),
                    supported_efforts: EffortSet::new([effort]),
                }],
                trusted_draw: false,
                images: Vec::new(),
            }),
        )
    };
    let wanted = ModelChoice {
        connection: ConnectionKind::ChatGpt,
        model: model.clone(),
        effort: Effort::High,
    };
    let mut core = fresh();
    core.handle(catalog(Effort::Low)).unwrap();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "words".into(),
    })
    .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    let first = crate::coordinator::tests::engine_key(&core, R);
    let built = |key, snapshot, choice| {
        Input::ContextBuilt(ContextBuilt {
            key,
            snapshot,
            result: Ok(Built {
                request: ModelRequest {
                    choice,
                    ..request()
                },
                sources: vec![T],
            }),
        })
    };
    let snapshot = RunSnapshot {
        origin: Origin::Task(T),
        assistant: AssistantName::new("Kumo").unwrap(),
        user: None,
        choice: Some(wanted.clone()),
        connection: ConnectionTag(0),
    };
    let assert_body = |effects: &[Effect]| {
        let [Effect::StartEngine { request }] = effects else {
            panic!("the round starts");
        };
        let body = crate::engines::responses::body(&request.request).unwrap();
        assert_eq!(body["model"], "sparse");
        assert_eq!(body["reasoning"]["effort"], "low");
        request.key
    };
    let grant = |core: &mut Coordinator, key| {
        let effects = core.handle(Input::SendReady { key, hold: None }).unwrap();
        let [Effect::Grant(lease), Effect::ArmSendDeadline { .. }] = &effects[..] else {
            panic!("the round is granted");
        };
        assert_eq!(lease.key, key);
        core.handle(Input::SendSettled { lease: *lease }).unwrap();
    };
    let sent = assert_body(
        &core
            .handle(built(first, Some(snapshot), Some(wanted)))
            .unwrap(),
    );
    grant(&mut core, sent);
    core.handle(catalog(Effort::Medium)).unwrap();
    play(&mut core, called(1));
    assert!(matches!(
        core.handle(completed(&core)).unwrap().as_slice(),
        [Effect::Store(StoreOp::RoundItems(_))]
    ));
    let effects = core
        .handle(Input::RoundStored { run: R, round: 1 })
        .unwrap();
    let [Effect::Store(StoreOp::BuildContext(read))] = &effects[..] else {
        panic!("a tool result starts round two");
    };
    assert_eq!(read.key.round, 2);
    let kept = read.snapshot.as_ref().unwrap().choice.clone().unwrap();
    assert_eq!(kept.effort, Effort::Low);
    let sent = assert_body(&core.handle(built(read.key, None, Some(kept))).unwrap());
    assert_eq!(sent, SendKey::Build(read.key));
    grant(&mut core, sent);
}

#[test]
fn a_round_with_calls_is_stored_with_their_results_then_the_next_round_is_built() {
    let mut core = started();
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::TextDelta("a".to_owned())
        )),
        Ok(vec![state(R, T, Phase::Answering), text("a")])
    );
    play(&mut core, called(1));
    play(&mut core, called(2));
    play(
        &mut core,
        [
            TurnEvent::OutputItem {
                index: 2,
                item: item("call two"),
            },
            TurnEvent::OutputItem {
                index: 0,
                item: item("message"),
            },
            TurnEvent::OutputItem {
                index: 1,
                item: item("call one"),
            },
        ],
    );
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![round_items(
            1,
            Some("a"),
            vec![item("message"), item("call one"), item("call two")],
            vec![id(1), id(2)],
        )]),
        "items in output order, results in call order"
    );
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 2 }),
        Err(Stale),
        "only round 1 is being written"
    );
    assert!(starts_round(&next(&mut core, 1), 2));
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![end_run(None, None, RunEnd::Done)]),
        "a round with no calls and no items ends the run"
    );
}

#[test]
fn anthropic_unoffered_name_gets_the_shared_unknown_tool_result() {
    use crate::connections::anthropic::sse_map::Reader;
    use crate::engines::anthropic::Mapper;
    let wire = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"unoffered\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let mut mapper = Mapper::default();
    let read = Reader::default().feed(wire.as_bytes(), &mut |event| mapper.event(event));
    let finish = mapper.finish(read);
    assert_eq!(finish, Some(Finish::Completed));
    let mut core = started();
    play(&mut core, mapper.take());
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![round_items(1, None, Vec::new(), vec![id(1)])])
    );
}

#[test]
fn a_last_round_with_items_is_stored_before_done() {
    let mut core = started();
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::TextDelta("bye".to_owned()),
    ))
    .expect("current");
    play(
        &mut core,
        [TurnEvent::OutputItem {
            index: 0,
            item: item("message"),
        }],
    );
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![round_items(
            1,
            Some("bye"),
            vec![item("message")],
            Vec::new()
        )])
    );
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Err(Stale),
        "the engine finished within its budget"
    );
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 1 }),
        Ok(vec![end_run(None, None, RunEnd::Done)])
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![state(R, T, Phase::Done), run_end(R)])
    );
}

#[test]
fn the_ninth_round_is_refused_before_its_build() {
    let mut core = started();
    let mut sends = 1;
    for round in 1..MAX_ROUNDS {
        play(&mut core, called(round));
        assert_eq!(
            core.handle(completed(&core)),
            Ok(vec![round_items(round, None, Vec::new(), vec![id(round)])])
        );
        assert!(starts_round(&next(&mut core, round), round + 1));
        sends += 1;
    }
    assert_eq!(sends, 8);
    play(&mut core, called(MAX_ROUNDS));
    assert_eq!(
        core.handle(completed(&core)),
        Ok(ends_failed(Failure::Budget)),
        "no ninth build, so no ninth ask or send"
    );
}

#[test]
fn the_seventeenth_call_fails_the_run_in_one_round_or_across_rounds() {
    let mut core = started();
    for n in 1..=MAX_CALLS {
        assert_eq!(
            core.handle(engine(
                super::engine_key(&core, R),
                TurnEvent::ToolCallStarted {
                    call: id(n),
                    name: ToolName::Unknown,
                }
            )),
            Ok(Vec::new()),
            "call {n}"
        );
    }
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::ToolCallStarted {
                call: id(17),
                name: ToolName::Unknown,
            }
        )),
        Ok(ends_failed(Failure::Budget))
    );

    let mut core = started();
    for n in 1..=MAX_CALLS {
        play(&mut core, called(n));
    }
    core.handle(completed(&core)).expect("current");
    next(&mut core, 1);
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::ToolCallStarted {
                call: id(1),
                name: ToolName::Unknown,
            }
        )),
        Ok(ends_failed(Failure::Budget)),
        "the count is the run's, not the round's"
    );
}

#[test]
fn a_round_waits_for_its_calls_write_and_stores_one_answer_per_call() {
    let mut core = probing(&[T]);
    play(&mut core, probe(1, r#"{"word":"hi"}"#));
    play(&mut core, called(2));
    let ack = commits(&mut core);
    assert_eq!(
        core.handle(ack.clone()),
        Ok(vec![answered(
            1,
            None,
            vec![
                (id(1), probed("hi")),
                (id(2), ToolOutput::Error(ToolError::Unknown))
            ]
        )])
    );
    assert_eq!(
        core.handle(ack),
        Ok(Vec::new()),
        "a repeated ack stores nothing"
    );
}

#[test]
fn a_later_round_replays_a_finished_id_from_the_ledger_and_refuses_other_arguments() {
    let mut core = probing(&[T]);
    play(&mut core, probe(1, r#"{"word":"hi"}"#));
    let ack = commits(&mut core);
    core.handle(ack).expect("stored");
    assert!(starts_round(&next(&mut core, 1), 2));
    play(&mut core, probe(1, r#"{ "word" : "hi" }"#));
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![answered(2, None, vec![(id(1), probed("hi"))])]),
        "the stored answer, with no second write"
    );
    assert!(starts_round(&next(&mut core, 2), 3));
    play(&mut core, probe(1, r#"{"word":"other"}"#));
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![answered(
            3,
            None,
            vec![(id(1), ToolOutput::Error(ToolError::Reused))]
        )])
    );
}

#[test]
fn a_stop_while_a_calls_write_is_pending_stores_no_round() {
    let mut core = probing(&[T]);
    play(&mut core, probe(1, r#"{"word":"hi"}"#));
    let ack = commits(&mut core);
    let stopped = core
        .handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("current");
    assert!(
        !stopped
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_)))),
        "{stopped:?}"
    );
    assert_eq!(core.handle(ack), Err(Stale));
}

#[test]
fn a_response_from_a_build_a_delete_voided_is_dropped_and_its_round_built_again() {
    let mut core = probing(&[T, B]);
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::TextDelta("from b".to_owned()),
    ))
    .expect("current");
    play(&mut core, probe(1, r#"{"word":"zqxfromtaskb"}"#));
    play(&mut core, probe(2, r#"{"word":"hi"}"#));
    assert_eq!(
        core.handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false,
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: B })])
    );
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![Effect::CancelEngine { run: R }]),
        "no write, no round, and the rebuild waits for B"
    );
    let effects = core
        .handle(Input::Deleted {
            thread: B,
            outcome: Deleted::Hidden,
            last_run: None,
        })
        .expect("answered");
    assert!(
        matches!(
            effects.first(),
            Some(Effect::Store(StoreOp::BuildContext(build))) if build.key == attempt(1, 2)
        ),
        "{effects:?}"
    );
    assert!(starts_round(
        &core
            .handle(offering(attempt(1, 2), &[T]))
            .expect("admitted"),
        1
    ));
    play(&mut core, probe(1, r#"{"word":"hi"}"#));
    let ack = commits(&mut core);
    assert_eq!(
        core.handle(ack),
        Ok(vec![answered(1, None, vec![(id(1), probed("hi"))])]),
        "only the rebuilt round, its text and its call, and the same id runs fresh"
    );
}

#[test]
fn a_delete_while_a_calls_write_is_pending_drops_the_round_and_the_call_never_runs_again() {
    let mut core = probing(&[T, B]);
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::TextDelta("from b".to_owned()),
    ))
    .expect("current");
    play(
        &mut core,
        [TurnEvent::OutputItem {
            index: 0,
            item: item("zqxfromtaskb"),
        }],
    );
    play(&mut core, probe(1, r#"{"word":"zqxfromtaskb"}"#));
    let ack = commits(&mut core);
    assert_eq!(
        core.handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false,
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: B })])
    );
    assert_eq!(
        core.handle(ack),
        Ok(vec![Effect::CancelEngine { run: R }]),
        "the call settles, but its round is not stored and the rebuild waits for B"
    );
    let effects = core
        .handle(Input::Deleted {
            thread: B,
            outcome: Deleted::Hidden,
            last_run: None,
        })
        .expect("answered");
    assert!(
        matches!(
            effects.first(),
            Some(Effect::Store(StoreOp::BuildContext(build))) if build.key == attempt(1, 2)
        ),
        "{effects:?}"
    );
    assert!(starts_round(
        &core
            .handle(offering(attempt(1, 2), &[T]))
            .expect("admitted"),
        1
    ));
    play(&mut core, probe(1, r#"{"word":"zqxfromtaskb"}"#));
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![answered(
            1,
            None,
            vec![(id(1), probed("zqxfromtaskb"))]
        )]),
        "the ledger's answer with no second write, and no text or item of the voided round"
    );
}

#[test]
fn a_round_with_two_committing_calls_is_stored_once_after_both_writes_in_call_order() {
    let mut core = probing(&[T]);
    play(&mut core, probe(1, r#"{"word":"one"}"#));
    play(&mut core, probe(2, r#"{"word":"two"}"#));
    let effects = core.handle(completed(&core)).expect("current");
    let [
        Effect::Store(StoreOp::CommitCall(first)),
        Effect::Store(StoreOp::CommitCall(second)),
    ] = effects.as_slice()
    else {
        panic!("two writes, not {effects:?}");
    };
    let ack = |key: &crate::dispatch::LedgerKey| Input::CallCommitted {
        key: key.clone(),
        outcome: crate::dispatch::Committed::Done,
        then: None,
    };
    let (first, second) = (ack(&first.key), ack(&second.key));
    assert_eq!(
        core.handle(second),
        Ok(Vec::new()),
        "the first acknowledgement stores nothing"
    );
    let mut joined = Vec::new();
    assert_eq!(
        core.tool_call(
            Caller::Model {
                run: R,
                call: id(1)
            },
            ToolName::Probe,
            br#"{"word":"one"}"#,
            &mut joined,
        ),
        Dispatch::Joined
    );
    assert_eq!(joined, Vec::new());
    assert_eq!(
        core.handle(first),
        Ok(vec![answered(
            1,
            None,
            vec![(id(1), probed("one")), (id(2), probed("two"))]
        )]),
        "one record in call order, and the joined answer adds no result"
    );
}

#[test]
fn a_refused_focus_completes_its_round_and_the_run_goes_on() {
    let mut core = probing(&[T]);
    play(
        &mut core,
        [
            TurnEvent::ToolCallStarted {
                call: id(1),
                name: ToolName::Focus,
            },
            TurnEvent::ToolCallReady {
                call: id(1),
                args: r#"{"task":1}"#.to_owned(),
            },
        ],
    );
    let effects = core.handle(completed(&core)).expect("current");
    let [Effect::Store(StoreOp::CommitCall(call))] = effects.as_slice() else {
        panic!("one write, not {effects:?}");
    };
    let refused = Input::CallCommitted {
        key: call.key.clone(),
        outcome: crate::dispatch::Committed::Refused(ToolError::NoTask),
        then: call.then,
    };
    assert_eq!(
        core.handle(refused),
        Ok(vec![answered(
            1,
            None,
            vec![(id(1), ToolOutput::Error(ToolError::NoTask))]
        )]),
        "the refusal is the call's one result, and no Focus is sent"
    );
    next(&mut core, 1);
}

#[test]
fn a_call_must_be_new_and_its_arguments_follow_it() {
    let mut core = started();
    play(&mut core, called(1));
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::ToolCallStarted {
                call: id(1),
                name: ToolName::Unknown,
            }
        )),
        Ok(ends_failed(Failure::Engine)),
        "the same call twice"
    );

    let mut core = started();
    play(&mut core, called(1));
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::ToolArgsDelta {
                call: id(1),
                delta: "more".to_owned(),
            }
        )),
        Ok(ends_failed(Failure::Engine)),
        "a ready call takes no more arguments"
    );

    let mut core = started();
    play(&mut core, [called(1)[0].clone()]);
    assert_eq!(
        core.handle(completed(&core)),
        Ok(ends_failed(Failure::Engine)),
        "a round can't complete with a call that never got ready"
    );
}

#[test]
fn a_late_round_ack_only_lets_the_ended_run_be_written() {
    let mut core = started();
    play(&mut core, called(1));
    core.handle(completed(&core)).expect("current");
    assert_eq!(
        core.handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        }),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, T, Phase::Interrupted),
            run_end(R),
        ]),
        "the outcome waits for the round's write"
    );
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 1 }),
        Ok(vec![end_run(
            None,
            Some(Vec::new()),
            RunEnd::Interrupted(Interruption::Stop)
        )]),
        "no next round"
    );

    let mut core = started();
    play(&mut core, called(1));
    core.handle(completed(&core)).expect("current");
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Ok(vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
        ]),
        "a round's write counts against the run's deadline"
    );
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 1 }),
        Ok(vec![end_run(None, None, RunEnd::Failed(Failure::Budget))])
    );

    let mut core = started();
    play(
        &mut core,
        [TurnEvent::OutputItem {
            index: 0,
            item: item("message"),
        }],
    );
    core.handle(completed(&core)).expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 1 }),
        Ok(vec![end_run(
            None,
            Some(Vec::new()),
            RunEnd::Interrupted(Interruption::Stop)
        )]),
        "a stopped run's last round is answered, not refused"
    );
}

#[test]
fn a_failed_round_write_fails_the_run_with_no_next_round() {
    let mut core = started();
    play(&mut core, called(1));
    core.handle(completed(&core)).expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Ok(ends_failed(Failure::Store))
    );
    assert_eq!(
        core.handle(Input::RoundStored { run: R, round: 1 }),
        Err(Stale),
        "the failed write stays failed"
    );
}

#[test]
fn steps_are_stored_before_their_rounds_items() {
    let mut core = started();
    core.handle(engine(super::engine_key(&core, R), draw_call()))
        .expect("current");
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::ToolArgsDelta {
            call: super::call(),
            delta: "a\n".to_owned(),
        },
    ))
    .expect("current");
    play(
        &mut core,
        called(1).into_iter().chain([TurnEvent::OutputItem {
            index: 0,
            item: item("call"),
        }]),
    );
    assert_eq!(
        core.handle(completed(&core)),
        Ok(Vec::new()),
        "a's write is pending"
    );
    assert_eq!(
        core.handle(Input::Deadline { run: R }),
        Ok(vec![
            Effect::CancelEngine { run: R },
            state(R, T, Phase::Failed),
            run_end(R),
        ]),
        "a round with calls left still counts against the deadline"
    );

    let mut core = started();
    core.handle(engine(super::engine_key(&core, R), draw_call()))
        .expect("current");
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::ToolArgsDelta {
            call: super::call(),
            delta: "a\n".to_owned(),
        },
    ))
    .expect("current");
    play(&mut core, called(1));
    core.handle(completed(&core)).expect("current");
    assert_eq!(
        core.handle(ack(0, "a")),
        Ok(vec![
            committed(0, "a"),
            round_items(1, None, Vec::new(), vec![id(1)])
        ])
    );
    assert!(starts_round(&next(&mut core, 1), 2));
}

#[test]
fn a_rounds_items_fill_the_request_bound_and_no_more() {
    let mut core = started();
    let full = || TurnEvent::OutputItem {
        index: 0,
        item: Opaque::new(vec![b'x'; Opaque::MAX]).expect("the cap fits"),
    };
    assert_eq!(crate::context::BOUND, 2 * Opaque::MAX);
    play(&mut core, [full(), full()]);
    assert_eq!(
        core.handle(engine(
            super::engine_key(&core, R),
            TurnEvent::OutputItem {
                index: 2,
                item: item("x"),
            }
        )),
        Ok(ends_failed(Failure::Budget)),
        "one byte over"
    );
}

#[test]
fn round_writes_show_lengths_never_content() {
    const MARKER: &str = "zqxplantedmarker";
    let call = CallId::new(&format!("call-{MARKER}")).expect("short ASCII");
    let op = Effect::Store(StoreOp::RoundItems(RoundItems {
        thread: T,
        run: R,
        round: 1,
        connection: crate::context::Route {
            kind: kumochi_protocol::message::ConnectionKind::ChatGpt,
            tag: ConnectionTag(0),
        },
        text: Some(format!("said {MARKER}").into()),
        items: vec![item(&format!("{{\"text\":\"{MARKER}\"}}"))],
        key_calls: Vec::new(),
        calls: vec![(call.clone(), ToolOutput::Text(format!("says {MARKER}")))],
    }));
    let result = crate::context::Body::Result {
        call: call.clone(),
        output: MARKER.to_owned(),
    };
    let request = [
        crate::context::InputItem::FunctionCall {
            call: call.clone(),
            name: MARKER.to_owned(),
            arguments: MARKER.to_owned(),
        },
        crate::context::InputItem::FunctionOutput {
            call: call.clone(),
            output: MARKER.to_owned(),
        },
    ];
    let events = [
        TurnEvent::ToolCallStarted {
            call: call.clone(),
            name: ToolName::Unknown,
        },
        TurnEvent::ToolArgsDelta {
            call: call.clone(),
            delta: MARKER.to_owned(),
        },
        TurnEvent::ToolCallReady {
            call,
            args: MARKER.to_owned(),
        },
    ];
    let printed = format!("{op:?} {result:?} {request:?} {events:?}");
    assert!(!printed.contains(MARKER), "{printed}");
    let bytes = format!("{:?}", MARKER.as_bytes());
    let listed = bytes.trim_start_matches('[').trim_end_matches(']');
    assert!(!printed.contains(listed), "{printed}");
}

#[test]
fn a_key_round_ends_the_run_when_a_source_it_carries_on_has_gone() {
    use crate::context::RunSnapshot;
    use crate::coordinator::{Availability, Ready};
    use kumochi_protocol::message::{
        AssistantName, ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug,
    };

    let model = ModelSlug::new("claude-sonnet-5-5").expect("slug");
    let choice = ModelChoice {
        connection: ConnectionKind::AnthropicKey,
        model: model.clone(),
        effort: Effort::Low,
    };
    let other = ThreadId(5);
    for kept in [true, false] {
        let mut core = fresh();
        core.handle(Input::Engines(
            ConnectionKind::AnthropicKey,
            Availability::Ready(Ready {
                connection: ConnectionTag(41),
                models: vec![ModelInfo {
                    slug: model.clone(),
                    display_name: ModelName::new("Sonnet").expect("name"),
                    supported_efforts: None,
                }],
                trusted_draw: false,
                images: Vec::new(),
            }),
        ))
        .expect("never stale");
        core.handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "words".into(),
        })
        .expect("a turn is never stale");
        core.handle(Input::TurnStored { run: R }).expect("current");
        let built = |key, snapshot, sources: Vec<ThreadId>| {
            Input::ContextBuilt(ContextBuilt {
                key,
                snapshot,
                result: Ok(Built {
                    request: request(),
                    sources,
                }),
            })
        };
        let snapshot = RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(choice.clone()),
            connection: ConnectionTag(41),
        };
        let first = super::engine_key(&core, R);
        let effects = core
            .handle(built(first, Some(snapshot), vec![T, other]))
            .expect("current");
        assert!(starts_round(&effects, 1));
        play(&mut core, called(1));
        core.handle(completed(&core)).expect("current");
        let effects = core
            .handle(Input::RoundStored { run: R, round: 1 })
            .expect("current");
        let [Effect::Store(StoreOp::BuildContext(second))] = effects.as_slice() else {
            panic!("one build, not {effects:?}");
        };
        let sources = if kept { vec![T, other] } else { vec![T] };
        let effects = core
            .handle(built(second.key, None, sources))
            .expect("current");
        if kept {
            assert!(starts_round(&effects, 2));
        } else {
            assert_eq!(effects, ends_failed(Failure::Engine));
        }
    }
}

fn admitted_key_round(sources: Vec<ThreadId>) -> Coordinator {
    use crate::context::RunSnapshot;
    use crate::coordinator::{Availability, Ready};
    use kumochi_protocol::message::{
        AssistantName, ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug,
    };

    let mut core = fresh();
    let model = ModelSlug::new("claude-sonnet-5-5").expect("slug");
    let kind = ConnectionKind::AnthropicKey;
    let tag = ConnectionTag(41);
    core.handle(Input::Engines(
        kind,
        Availability::Ready(Ready {
            connection: tag,
            models: vec![ModelInfo {
                slug: model.clone(),
                display_name: ModelName::new("Sonnet").expect("name"),
                supported_efforts: None,
            }],
            trusted_draw: false,
            images: Vec::new(),
        }),
    ))
    .expect("never stale");
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "words".into(),
    })
    .expect("never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    let first = Input::ContextBuilt(ContextBuilt {
        key: attempt(1, 1),
        snapshot: Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(ModelChoice {
                connection: kind,
                model,
                effort: Effort::Low,
            }),
            connection: tag,
        }),
        result: Ok(Built {
            request: request(),
            sources,
        }),
    });
    assert!(starts_round(&core.handle(first).expect("admitted"), 1));
    core
}

#[test]
fn a_granted_key_round_retires_on_withdrawal_before_the_delete_can_write() {
    let mut core = admitted_key_round(vec![T, B]);
    let key = super::engine_key(&core, R);
    let effects = core
        .handle(Input::SendReady {
            key: crate::coordinator::SendKey::Build(key),
            hold: None,
        })
        .expect("send granted");
    let lease = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("one lease");
    let effects = core
        .handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false,
        })
        .expect("delete accepted");
    assert!(effects.contains(&Effect::CancelEngine { run: R }));
    assert!(!effects.contains(&Effect::Store(StoreOp::Delete { thread: B })));
    assert_eq!(
        core.handle(Input::Engine {
            key,
            event: TurnEvent::TextDelta("late".into())
        }),
        Err(Stale)
    );
    let settled = core
        .handle(Input::SendSettled { lease })
        .expect("retired send settles");
    assert!(settled.contains(&Effect::Store(StoreOp::Delete { thread: B })));
}

#[test]
fn a_key_rebuild_stores_only_calls_from_the_replacement_round() {
    use crate::engines::anthropic::encode::ToolUse;

    let mut core = admitted_key_round(vec![T, B]);
    let typed = |n: u32, word: &str| {
        ToolUse::projected(&format!(
            r#"{{"type":"tool_use","id":"tool-{n}","name":"list_tasks","input":{{"word":"{word}"}}}}"#
        ))
        .expect("a call")
    };
    play(&mut core, called(1));
    play(
        &mut core,
        [TurnEvent::KeyCall {
            index: 0,
            call: typed(1, "withdrawn-source"),
        }],
    );
    assert_eq!(
        core.handle(Input::Delete {
            thread: B,
            after_turns: 1,
            resend: false
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: B })])
    );
    assert_eq!(
        core.handle(completed(&core)),
        Ok(vec![Effect::CancelEngine { run: R }])
    );
    let effects = core
        .handle(Input::Deleted {
            thread: B,
            outcome: Deleted::Hidden,
            last_run: None,
        })
        .expect("answered");
    let Some(Effect::Store(StoreOp::BuildContext(replacement))) = effects.first() else {
        panic!("expected the replacement build: {effects:?}");
    };
    assert_eq!(replacement.key, attempt(1, 2));
    assert!(starts_round(
        &core
            .handle(offering(replacement.key, &[T]))
            .expect("admitted"),
        1
    ));
    play(&mut core, called(2));
    let fresh_call = typed(2, "replacement");
    play(
        &mut core,
        [TurnEvent::KeyCall {
            index: 0,
            call: fresh_call.clone(),
        }],
    );
    let effects = core.handle(completed(&core)).expect("current");
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("expected the replacement write: {effects:?}");
    };
    assert_eq!(
        round.key_calls,
        [fresh_call],
        "the withdrawn call must not be written"
    );
    assert_eq!(
        round.calls,
        [(id(2), ToolOutput::Error(ToolError::Unknown))]
    );
}
