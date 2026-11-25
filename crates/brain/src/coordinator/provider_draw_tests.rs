use super::*;

const PREFIX: &str = r#"{"records":[{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"}},{"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}}"#;
const END: &str = r#",{"end":{}}]}"#;

fn send(core: &mut Coordinator, event: TurnEvent) -> Vec<Effect> {
    let key = super::super::engine_key(core, R);
    core.handle(Input::Engine { key, event })
        .expect("current provider event")
}

fn start(core: &mut Coordinator, name: &str) {
    assert_eq!(
        send(
            core,
            TurnEvent::ToolCallStarted {
                call: id(name),
                name: crate::engines::ToolName::Draw,
            },
        ),
        Vec::new()
    );
}

fn delta(core: &mut Coordinator, name: &str, bytes: &str) -> Vec<Effect> {
    send(
        core,
        TurnEvent::ToolArgsDelta {
            call: id(name),
            delta: bytes.into(),
        },
    )
}

fn ready(core: &mut Coordinator, name: &str, bytes: &str) -> Vec<Effect> {
    send(
        core,
        TurnEvent::ToolCallReady {
            call: id(name),
            args: bytes.into(),
        },
    )
}

fn drain(core: &mut Coordinator, store: &mut Store, mut effects: Vec<Effect>) -> Vec<Effect> {
    let mut all = Vec::new();
    loop {
        let next = pending(&effects).map(|_| write(core, store, &effects));
        all.extend(effects);
        let Some(next) = next else {
            return all;
        };
        effects = next;
    }
}

fn result(effects: &[Effect]) -> crate::dispatch::CommitCall {
    let calls: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::CommitCall(call)) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "one result transaction, not one per record");
    calls[0].clone()
}

fn commit_result(
    core: &mut Coordinator,
    store: &mut Store,
    scratch: &Scratch,
    result: &crate::dispatch::CommitCall,
) -> Vec<Effect> {
    assert_eq!(result.write, crate::dispatch::LocalWrite::None);
    assert_eq!(
        store.commit_call(result).expect("real result INSERT"),
        Committed::Done
    );
    let db = rusqlite::Connection::open(scratch.db()).expect("read result row");
    let count: u32 = db
        .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
        .expect("result row count");
    assert_eq!(count, 1);
    let row: (String, String, Vec<u8>, String) = db
        .query_row(
            "SELECT call, tool, hash, result FROM tool_calls
             WHERE run = ?1 AND thread = ?2 AND route = 'model'",
            rusqlite::params![
                i64::try_from(R.0).expect("small run id"),
                i64::try_from(T.0).expect("small thread id"),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("model result row");
    assert_eq!(
        row,
        (
            result.key.call.to_string(),
            "draw".into(),
            result.hash.to_vec(),
            result.output.stored(),
        )
    );
    assert!(matches!(
        store.commit_call(result),
        Err(crate::store::StoreError::Call)
    ));
    core.handle(Input::CallCommitted {
        key: result.key.clone(),
        outcome: Committed::Done,
        then: None,
    })
    .expect("result acknowledgement")
}

fn ends(effects: &[Effect]) -> Vec<bool> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::SceneEnd { complete, .. }) => Some(*complete),
            _ => None,
        })
        .collect()
}

fn assert_prefix(store: &Store, expected: &[&str]) {
    let recovery = store.drawing_scenes().expect("real durable prefix");
    assert_eq!(recovery.skipped, Vec::new());
    assert_eq!(recovery.scenes.len(), 1);
    assert_eq!(recovery.scenes[0].thread, T);
    assert_eq!(
        recovery.scenes[0]
            .log
            .iter()
            .map(|event| match event {
                kumochi_scene::SceneEvent::Step { revision, step } => (*revision, step.id.as_str()),
                kumochi_scene::SceneEvent::Stop(_) => panic!("no cut was requested"),
            })
            .collect::<Vec<_>>(),
        expected
            .iter()
            .enumerate()
            .map(|(index, id)| (u32::try_from(index + 1).expect("bounded revision"), *id))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_closed_provider_step_is_durable_before_ready_but_an_open_object_has_no_effect() {
    let scratch = Scratch::new("provider-draw-open-step");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    start(&mut core, "early");
    let cut = PREFIX.find(r#",{"step"#).expect("step boundary");
    let lookup = delta(&mut core, "early", &PREFIX[..cut]);
    assert_eq!(write(&mut core, &mut store, &lookup), Vec::new());
    assert_eq!(
        store
            .drawing_scenes()
            .expect("begin is not a scene")
            .scenes
            .len(),
        0
    );
    assert_eq!(
        delta(&mut core, "early", &PREFIX[cut..PREFIX.len() - 1]),
        Vec::new()
    );
    assert_eq!(store.drawing_scenes().expect("open object").scenes.len(), 0);
    assert_eq!(
        call(&mut core, "early", &args(None)),
        (
            Dispatch::Answered(ToolOutput::Error(ToolError::Reused)),
            Vec::new()
        )
    );
    let step = delta(&mut core, "early", "}");
    let (
        _,
        crate::drawing::Write::Step {
            revision,
            step: typed,
            ..
        },
    ) = pending(&step).expect("closed first step")
    else {
        panic!("only the closed step may request a scene write");
    };
    assert_eq!(revision, 1);
    assert_eq!(typed.id.as_str(), "s1");
    let committed = write(&mut core, &mut store, &step);
    assert_prefix(&store, &["s1"]);
    let scene = store.drawing_scenes().expect("scene").scenes[0].scene;
    let frames: Vec<_> = committed
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::SceneStepCommit {
                run,
                scene,
                index,
                step,
                ..
            }) => Some((*run, *scene, *index, step.id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(frames, [(R, scene, 0, "s1")]);
    assert_eq!(ends(&committed), Vec::<bool>::new());
    assert!(!committed.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::CommitCall(_) | StoreOp::RoundItems(_))
    )));
    assert_eq!(delta(&mut core, "early", END), Vec::new());
    assert_eq!(
        ready(&mut core, "early", &format!("{PREFIX}{END}")),
        Vec::new()
    );
    assert!(core.current.as_ref().expect("run").answered.is_empty());
}

#[test]
fn semantic_failure_identity_is_independent_of_ready_timing_and_early_ack_keeps_the_round_open() {
    let mut hashes = Vec::new();
    for ready_first in [true, false] {
        let scratch = Scratch::new("provider-draw-semantic-timing");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let bytes = String::from_utf8(args(Some(json!({"id":"s3","remove":["missing"]}))))
            .expect("JSON fixture");
        start(&mut core, "semantic");
        let lookup = delta(&mut core, "semantic", &bytes);
        let mut effects = Vec::new();
        if ready_first {
            effects.extend(ready(&mut core, "semantic", &bytes));
        }
        effects.extend(drain(&mut core, &mut store, lookup));
        if !ready_first {
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
            );
            effects.extend(ready(&mut core, "semantic", &bytes));
        }
        assert_prefix(&store, &["s1", "s2"]);
        assert_eq!(ends(&effects), [false]);
        let result = result(&effects);
        let expected = ToolOutput::Text(format!(
            "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}"
        ));
        assert_eq!(result.output, expected);
        hashes.push(result.hash);
        assert_eq!(
            call(&mut core, "semantic", bytes.as_bytes()),
            (Dispatch::Joined, Vec::new())
        );
        assert_eq!(
            call(&mut core, "semantic", &args(None)),
            (
                Dispatch::Answered(ToolOutput::Error(ToolError::Reused)),
                Vec::new()
            )
        );
        let acked = commit_result(&mut core, &mut store, &scratch, &result);
        assert!(!acked.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::RoundItems(_) | StoreOp::BuildContext(_))
        )));
        assert!(
            !core
                .current
                .as_ref()
                .expect("provider still streaming")
                .finishing
        );
        assert_eq!(
            call(&mut core, "semantic", bytes.as_bytes()),
            (Dispatch::Answered(expected.clone()), Vec::new())
        );
        let native =
            crate::engines::Opaque::new(br#"{"type":"reasoning","id":"after-result"}"#.to_vec())
                .expect("small native item");
        assert_eq!(
            send(
                &mut core,
                TurnEvent::OutputItem {
                    index: 0,
                    item: native.clone(),
                }
            ),
            Vec::new()
        );
        let finished = send(&mut core, TurnEvent::Finished(Finish::Completed));
        let rounds: Vec<_> = finished
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].items, [native]);
        assert_eq!(rounds[0].calls, [(id("semantic"), expected)]);
        assert_eq!(ends(&finished), Vec::<bool>::new());
        assert_prefix(&store, &["s1", "s2"]);
    }
    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes[0], hashes[1]);
}

#[test]
fn text_after_the_records_array_completes_the_call_and_draws_nothing_more() {
    let second = r#",{"end":{}}],"records":[{"step":{"id":"s2","put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10]}}]}},{"end":{}}]}"#;
    for tail in [r#"] , "end":{}}"#, second, "] not json"] {
        let bytes = format!("{PREFIX}{tail}");
        for streamed in [true, false] {
            let scratch = Scratch::new("draw-after-records");
            let mut store = store(&scratch);
            let mut core = offered(vec![T]);
            let effects = if streamed {
                start(&mut core, "after");
                let mut effects = delta(&mut core, "after", &bytes);
                effects.extend(ready(&mut core, "after", &bytes));
                let mut effects = drain(&mut core, &mut store, effects);
                effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
                effects
            } else {
                let (dispatch, effects) = call(&mut core, "after", bytes.as_bytes());
                assert_eq!(dispatch, Dispatch::Committing);
                drain(&mut core, &mut store, effects)
            };
            assert_eq!(
                result(&effects).output,
                ToolOutput::Text("drawn: [s1]; text after the records array was ignored".into())
            );
            assert_eq!(ends(&effects), [true]);
            assert_prefix(&store, &["s1"]);
            assert_eq!(
                call(&mut core, "after", &args(None)),
                (
                    Dispatch::Answered(ToolOutput::Error(ToolError::Reused)),
                    Vec::new()
                ),
                "the call keeps its canonical identity, so changed arguments are refused"
            );
        }
    }
}

#[test]
fn a_closed_records_array_without_end_completes_the_call() {
    let bytes = format!("{PREFIX}]}}");
    for streamed in [true, false] {
        let scratch = Scratch::new("draw-closed-without-end");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let effects = if streamed {
            start(&mut core, "closed");
            let mut effects = delta(&mut core, "closed", &bytes);
            effects.extend(ready(&mut core, "closed", &bytes));
            let mut effects = drain(&mut core, &mut store, effects);
            effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
            effects
        } else {
            let (dispatch, effects) = call(&mut core, "closed", bytes.as_bytes());
            assert_eq!(dispatch, Dispatch::Committing);
            drain(&mut core, &mut store, effects)
        };
        assert_eq!(
            result(&effects).output,
            ToolOutput::Text("drawn: [s1]".into())
        );
        assert_eq!(ends(&effects), [true]);
        assert_prefix(&store, &["s1"]);
    }
}

#[test]
fn an_empty_records_array_with_text_after_it_draws_nothing() {
    let bytes = r#"{"records":[]}x"#;
    for streamed in [true, false] {
        let scratch = Scratch::new("draw-empty-after-records");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let effects = if streamed {
            start(&mut core, "empty");
            let mut effects = delta(&mut core, "empty", bytes);
            effects.extend(ready(&mut core, "empty", bytes));
            let mut effects = drain(&mut core, &mut store, effects);
            effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
            effects
        } else {
            let (dispatch, effects) = call(&mut core, "empty", bytes.as_bytes());
            assert_eq!(dispatch, Dispatch::Committing);
            drain(&mut core, &mut store, effects)
        };
        assert_eq!(
            result(&effects).output,
            ToolOutput::Text(format!("drawn: []; stopped: $: scene.BeforeBegin{RETRY}"))
        );
        assert_eq!(ends(&effects), Vec::<bool>::new());
    }
}

#[test]
fn terminal_syntax_typed_and_truncated_tails_keep_one_prefix_and_a_fixed_refusal() {
    for (suffix, verify, stopped) in [
        (
            ",!",
            false,
            format!("malformed drawing at byte {}", PREFIX.len() + 1),
        ),
        (
            r#",{"step":{"id":"s2","set":[{"target":"a","info":null}]}},{"end":{}}]}"#,
            false,
            "step s2: set[0].info: scene.Unsupported".into(),
        ),
        ("", true, "truncated drawing".into()),
    ] {
        let scratch = Scratch::new("provider-draw-terminal-tail");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        start(&mut core, "refused");
        let lookup = delta(&mut core, "refused", PREFIX);
        let prefix = drain(&mut core, &mut store, lookup);
        assert_eq!(ends(&prefix), Vec::<bool>::new());
        let failed = if verify {
            ready(&mut core, "refused", PREFIX)
        } else {
            delta(&mut core, "refused", suffix)
        };
        assert_eq!(ends(&failed), [false]);
        let result = result(&failed);
        let expected = ToolOutput::Text(format!("drawn: [s1]; stopped: {stopped}{DRAWN_RETRY}"));
        assert_eq!(result.output, expected);
        assert_eq!(
            call(&mut core, "refused", &args(None)),
            (Dispatch::Joined, Vec::new())
        );
        let acked = commit_result(&mut core, &mut store, &scratch, &result);
        assert_eq!(ends(&acked), Vec::<bool>::new());
        if !verify {
            assert_eq!(delta(&mut core, "refused", END), Vec::new());
        }
        assert_eq!(
            call(&mut core, "refused", &args(None)),
            (Dispatch::Answered(expected), Vec::new())
        );
        assert_prefix(&store, &["s1"]);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the pending-step and pending-result schedules share one replacement race"
)]
fn replacement_admission_does_not_turn_an_unverified_same_id_into_an_old_result_waiter() {
    for (hold_result, verify_before_ack, changed) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let scratch = Scratch::new("provider-draw-build-join");
        let mut store = store(&scratch);
        let source = ThreadId(2);
        let mut core = offered(vec![T, source]);
        let bytes = String::from_utf8(args(Some(json!({"id":"s3","remove":["missing"]}))))
            .expect("JSON fixture");
        start(&mut core, "same");
        let lookup = delta(&mut core, "same", &bytes);
        assert_eq!(ready(&mut core, "same", &bytes), Vec::new());
        let step = write(&mut core, &mut store, &lookup);
        let held = if hold_result {
            drain(&mut core, &mut store, step)
        } else {
            step
        };
        let old = super::super::engine_key(&core, R);
        let deleting = core
            .handle(Input::Delete {
                thread: source,
                after_turns: 1,
                resend: false,
            })
            .expect("source invalidates B1");
        assert_eq!(ends(&deleting), Vec::<bool>::new());
        let rebuilding = core
            .handle(Input::Deleted {
                thread: source,
                outcome: kumochi_protocol::message::Deleted::Hidden,
                last_run: None,
            })
            .expect("source deletion acknowledged");
        let key = rebuilding
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                _ => None,
            })
            .expect("B2 build");
        assert_ne!(key, old);
        core.handle(built(key, vec![T]))
            .expect("B2 admitted before B1 settles");
        let retry = if changed {
            String::from_utf8(args(None)).expect("changed JSON fixture")
        } else {
            bytes.clone()
        };
        start(&mut core, "same");
        assert_eq!(delta(&mut core, "same", &retry), Vec::new());
        if verify_before_ack {
            assert_eq!(ready(&mut core, "same", &retry), Vec::new());
        }
        let settled = if hold_result {
            held
        } else {
            let settled = write(&mut core, &mut store, &held);
            assert_eq!(
                pending(&settled),
                None,
                "B1's queued second step was discarded"
            );
            assert_eq!(ends(&settled), [false]);
            settled
        };
        let result = result(&settled);
        if hold_result {
            assert_eq!(
                result.output,
                ToolOutput::Text(format!(
                    "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}"
                ))
            );
        } else {
            let ToolOutput::Text(text) = &result.output else {
                panic!("incomplete interrupted result");
            };
            assert!(text.starts_with("drawn: [s1]; stopped: "));
        }
        let acked = commit_result(&mut core, &mut store, &scratch, &result);
        assert!(
            !acked
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
        );
        assert!(
            core.current.as_ref().expect("B2").answered.is_empty(),
            "old model delivery cannot fall into the direct/MCP sink"
        );
        if !verify_before_ack {
            assert_eq!(ready(&mut core, "same", &retry), Vec::new());
        }
        for event in [
            TurnEvent::ToolArgsDelta {
                call: id("same"),
                delta: END.into(),
            },
            TurnEvent::ToolCallReady {
                call: id("same"),
                args: bytes.clone(),
            },
            TurnEvent::Finished(Finish::Completed),
            TurnEvent::Finished(Finish::Failed(crate::engines::EngineFailure::Provider)),
        ] {
            assert_eq!(
                core.handle(Input::Engine { key: old, event }),
                Err(super::super::Stale)
            );
        }
        assert_eq!(
            call(&mut core, "same", &args(None)),
            (
                Dispatch::Answered(ToolOutput::Error(ToolError::Reused)),
                Vec::new()
            )
        );
        let completed = send(&mut core, TurnEvent::Finished(Finish::Completed));
        let rounds: Vec<_> = completed
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds.len(), 1);
        assert_eq!(
            rounds[0].calls,
            [(
                id("same"),
                if changed {
                    ToolOutput::Error(ToolError::Reused)
                } else {
                    result.output
                }
            )]
        );
        assert!(
            !completed
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
        );
        assert_prefix(&store, if hold_result { &["s1", "s2"] } else { &["s1"] });
    }
}

#[test]
fn mapper_changed_final_item_and_failure_after_valid_ready_never_complete_a_prefix() {
    for changed in [true, false] {
        let scratch = Scratch::new("provider-draw-mapped-failure");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let mut mapper = crate::engines::responses::Round::default();
        let mut reader = crate::connections::model::sse::Reader::default();
        for event in [
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","id":"fc1","call_id":"mapped","name":"draw"
            }}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":PREFIX}),
        ] {
            assert_eq!(
                reader.feed(format!("data: {event}\n\n").as_bytes(), &mut |event| {
                    mapper.event(event);
                }),
                Ok(crate::connections::model::sse::Progress::More)
            );
        }
        let mut prefix = Vec::new();
        for event in mapper.take() {
            prefix.extend(send(&mut core, event));
        }
        let prefix = drain(&mut core, &mut store, prefix);
        assert_eq!(ends(&prefix), Vec::<bool>::new());
        let bytes = format!("{PREFIX}{END}");
        for event in [
            json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":END}),
            json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","id":"fc1","call_id":"mapped","name":"draw",
                "arguments":if changed { "{\"records\":[]}" } else { &bytes }
            }}),
        ] {
            assert_eq!(
                reader.feed(format!("data: {event}\n\n").as_bytes(), &mut |event| {
                    mapper.event(event);
                }),
                Ok(crate::connections::model::sse::Progress::More)
            );
        }
        let tail = mapper.take();
        assert_eq!(
            tail.iter()
                .filter(|event| matches!(event, TurnEvent::ToolCallReady { .. }))
                .count(),
            usize::from(!changed)
        );
        for event in tail {
            assert_eq!(send(&mut core, event), Vec::new());
        }
        let failure = mapper
            .end(Err(crate::connections::model::error::Failure::Truncated))
            .expect("mapper failure event");
        let expected = if changed {
            crate::connections::model::error::Failure::Protocol
        } else {
            crate::connections::model::error::Failure::Truncated
        };
        assert_eq!(
            failure,
            TurnEvent::Finished(Finish::Failed(crate::engines::EngineFailure::Stream(
                expected
            )))
        );
        let failed = send(&mut core, failure);
        assert_eq!(ends(&failed), [false]);
        assert!(
            !failed
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
        );
        assert_prefix(&store, &["s1"]);
        assert!(core.current.is_none());
    }
}

#[test]
fn a_messages_start_object_without_fragments_streams_before_ready_and_draws() {
    let scratch = Scratch::new("provider-draw-messages-start");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let input: Value = serde_json::from_str(&format!("{PREFIX}{END}")).expect("a valid drawing");
    let args = serde_json::to_string(&input).expect("the start object's bytes");
    let mut mapper = crate::engines::anthropic::Mapper::default();
    let mut reader = crate::connections::model::sse::Reader::default();
    for event in [
        json!({"type":"message_start","message":{"usage":{}}}),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"messages","name":"draw","input":input
        }}),
        json!({"type":"content_block_stop","index":0}),
    ] {
        assert_eq!(
            reader.feed(format!("data: {event}\n\n").as_bytes(), &mut |event| {
                mapper.event(event);
            }),
            Ok(crate::connections::model::sse::Progress::More)
        );
    }
    let events = mapper.take();
    assert_eq!(
        events,
        [
            TurnEvent::ToolCallStarted {
                call: id("messages"),
                name: crate::engines::ToolName::Draw,
            },
            TurnEvent::ToolArgsDelta {
                call: id("messages"),
                delta: args.clone(),
            },
            TurnEvent::ToolCallReady {
                call: id("messages"),
                args,
            },
        ]
    );
    let mut streamed = Vec::new();
    for event in events {
        streamed.extend(send(&mut core, event));
    }
    let mut effects = drain(&mut core, &mut store, streamed);
    effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
    assert_eq!(ends(&effects), [true]);
    assert_eq!(
        result(&effects).output,
        ToolOutput::Text("drawn: [s1]".into())
    );
    assert_prefix(&store, &["s1"]);
}

#[test]
fn recorded_desk_draws_without_a_version_draw_their_steps() {
    for (name, args, step) in [
        (
            "desk-1",
            include_str!("../../fixtures/responses/draw_y_squared_1.json"),
            "plot",
        ),
        (
            "desk-2",
            include_str!("../../fixtures/responses/draw_y_squared_2.json"),
            "graph",
        ),
    ] {
        let scratch = Scratch::new(name);
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        start(&mut core, name);
        let chars: Vec<char> = args.chars().collect();
        let mut streamed = Vec::new();
        for chunk in chars.chunks(48) {
            streamed.extend(delta(&mut core, name, &chunk.iter().collect::<String>()));
        }
        streamed.extend(ready(&mut core, name, args));
        let mut effects = drain(&mut core, &mut store, streamed);
        effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
        assert_eq!(
            result(&effects).output.json()["text"],
            format!("drawn: [{step}]"),
            "{name}"
        );
        assert_eq!(ends(&effects), [true], "{name}");
        assert_prefix(&store, &[step]);
    }
}

#[test]
fn the_recorded_water_cycle_draws_without_a_diagram_container() {
    struct Measure;
    impl kumochi_scene::display::TextMeasure for Measure {
        fn measure(
            &mut self,
            _: &str,
            _: kumochi_scene::display::TextStyle,
            _: Option<f32>,
        ) -> Result<[f32; 2], kumochi_scene::display::MeasureError> {
            Ok([80.0, 20.0])
        }
    }
    let args = include_str!("../../fixtures/responses/draw_water_cycle.json");
    let scratch = Scratch::new("water-cycle");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    start(&mut core, "water-cycle");
    let mut streamed = Vec::new();
    let chars: Vec<char> = args.chars().collect();
    for chunk in chars.chunks(48) {
        streamed.extend(delta(
            &mut core,
            "water-cycle",
            &chunk.iter().collect::<String>(),
        ));
    }
    streamed.extend(ready(&mut core, "water-cycle", args));
    let mut effects = drain(&mut core, &mut store, streamed);
    effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
    assert_eq!(
        result(&effects).output,
        ToolOutput::Text("drawn: [cycle]".into())
    );
    assert_eq!(ends(&effects), [true]);
    assert_prefix(&store, &["cycle"]);
    let recovery = store.drawing_scenes().expect("stored scene");
    let stored = &recovery.scenes[0];
    let (state, layout) = kumochi_scene::layout::recover(
        stored.header.begin.clone(),
        &stored.log,
        kumochi_scene::layout::SurfaceGeometry::default(),
        &std::sync::Arc::new(kumochi_scene::theme::Theme::default()),
        &mut Measure,
        &std::sync::atomic::AtomicBool::new(false),
        None,
    )
    .expect("stored authored records replay and prepare");
    assert_eq!(state.object_count(), 8);
    assert_eq!(layout.items().len(), 8);
    assert!(
        layout
            .items()
            .iter()
            .all(|item| !item.primitives.is_empty())
    );
    let kumochi_scene::SceneEvent::Step { step, .. } = &stored.log[0] else {
        panic!("step")
    };
    for op in step.put.iter() {
        match op {
            kumochi_scene::Op::Node(node) => assert_eq!(node.parent, None),
            kumochi_scene::Op::Edge(edge) => assert_eq!(edge.parent, None),
            _ => panic!("recorded node or edge"),
        }
    }
}

#[test]
fn the_prompt_example_draws_two_steps_without_promising_an_overlay_canvas() {
    let prompt = include_str!("../../prompts/draw.md");
    assert!(
        !prompt.contains("the size is its drawing canvas"),
        "overlay size comes from the host"
    );
    let examples: Vec<_> = prompt
        .split("```json")
        .skip(1)
        .map(|example| example.split_once("```").expect("closed example").0.trim())
        .collect();
    let cases = [
        (
            "prompt-screen",
            None,
            &["point"][..],
            "drawn: [point]; the marks' positions are not kept after this turn",
        ),
        (
            "prompt-diagram",
            Some([600, 360]),
            &["cycle", "heat"][..],
            "drawn: [cycle, heat]",
        ),
        (
            "prompt-sine",
            Some([400, 300]),
            &["graph"][..],
            "drawn: [graph]",
        ),
    ];
    assert_eq!(examples.len(), cases.len());
    for (args, (call, size, steps, expected)) in examples.iter().zip(cases) {
        assert!(
            args.len() <= 16 * 1024,
            "example fits the prompt's call target"
        );
        let envelope: Value = serde_json::from_str(args).expect("example JSON");
        assert_eq!(envelope["records"][0]["begin"]["size"], json!(size));
        let scratch = Scratch::new(call);
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        if size.is_none() {
            sent_picture(&mut core);
        }
        start(&mut core, call);
        let mut streamed = delta(&mut core, call, args);
        streamed.extend(ready(&mut core, call, args));
        let mut effects = drain(&mut core, &mut store, streamed);
        effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
        assert_eq!(result(&effects).output.json()["text"], expected);
        assert_eq!(ends(&effects), [true]);
        assert_prefix(&store, steps);
    }
    let sine: Value = serde_json::from_str(examples[2]).expect("sine JSON");
    let data = sine["records"][1]["step"]["put"][1]["series"]["data"]
        .as_array()
        .expect("sine data");
    assert_eq!(data.len(), 41);
    assert!(
        data.iter()
            .all(|point| point.as_array().is_some_and(|p| p.len() == 2))
    );
    assert_eq!(
        [0, 10, 20, 30, 40].map(|index| data[index].clone()),
        [
            json!([0, 0]),
            json!([std::f64::consts::FRAC_PI_2, 1]),
            json!([std::f64::consts::PI, 0]),
            json!([3.0 * std::f64::consts::FRAC_PI_2, -1]),
            json!([std::f64::consts::TAU, 0]),
        ]
    );
}

#[test]
fn overlay_lines_and_arrows_draw_but_the_screen_refuses_them() {
    for kind in ["line", "arrow"] {
        for surface in ["overlay", "screen"] {
            let scratch = Scratch::new(&format!("{surface}-{kind}"));
            let mut store = store(&scratch);
            let mut core = offered(vec![T]);
            sent_picture(&mut core);
            let args = serde_json::to_string(&json!({"records":[
                {"begin":{"scene":"wire","surface":surface,"alt":"A connection"}},
                {"step":{"id":"wire","put":[{(kind):{
                    "id":"connection","from":[10,10],"to":[90,40]
                }}]}},
                {"end":{}}
            ]}))
            .expect("fixture");
            start(&mut core, "connection");
            let mut streamed = delta(&mut core, "connection", &args);
            streamed.extend(ready(&mut core, "connection", &args));
            let mut effects = drain(&mut core, &mut store, streamed);
            effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
            if surface == "overlay" {
                assert_eq!(result(&effects).output.json()["text"], "drawn: [wire]");
                assert_eq!(ends(&effects), [true]);
                assert_prefix(&store, &["wire"]);
            } else {
                assert_eq!(
                    result(&effects).output.json()["text"],
                    format!(
                        "drawn: []; stopped: step wire: put[0]: scene.Unsupported{RETRY}; the marks' positions are not kept after this turn"
                    )
                );
                assert_eq!(ends(&effects), Vec::<bool>::new());
                assert_eq!(store.drawing_scenes().expect("recovery").scenes.len(), 0);
            }
        }
    }
}

#[test]
fn a_refused_draw_logs_one_warning_with_only_its_code_and_field() {
    let logs = crate::test_support::Captured::default();
    let _logging = logs.install();
    let mut core = offered(vec![T]);
    let inline = PREFIX.replace("\"surface\":\"overlay\"", "\"surface\":\"inline\"");
    start(&mut core, "inline");
    let refused = result(&delta(&mut core, "inline", &inline));
    assert_eq!(
        refused.output,
        ToolOutput::Text(format!(
            "drawn: []; stopped: surface: scene.Unsupported{RETRY}"
        ))
    );
    let text = logs.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("draw call"))
        .filter_map(|line| line.split_once(' '))
        .map(|(_, line)| line.trim_start())
        .collect();
    assert_eq!(
        lines,
        [
            "WARN kumochi_brain::coordinator::drawing: draw call refused code=scene.Unsupported field=surface"
        ]
    );
}

#[test]
fn stop_settles_the_submitted_step_without_resurrecting_queued_records_or_content() {
    let scratch = Scratch::new("provider-draw-stop");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let marker = "providerprivacysentinel";
    let bytes = String::from_utf8(args(None))
        .expect("fixture")
        .replace("diagram", marker);
    start(&mut core, marker);
    let old = super::super::engine_key(&core, R);
    let lookup = delta(&mut core, marker, &bytes);
    let pending = write(&mut core, &mut store, &lookup);
    let stopped = core
        .handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("stop with submitted step");
    assert_eq!(ends(&stopped), Vec::<bool>::new());
    let settled = write(&mut core, &mut store, &pending);
    assert_eq!(
        super::pending(&settled),
        None,
        "queued second step cannot escape Stop"
    );
    assert_eq!(ends(&settled), [false]);
    assert_prefix(&store, &["s1"]);
    let frames: Vec<_> = settled
        .iter()
        .filter_map(|effect| match effect {
            Effect::Data(DataToShell::SceneStepCommit { index, step, .. }) => {
                Some((*index, step.id.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(frames, [(0, "s1")]);
    for event in [
        TurnEvent::ToolArgsDelta {
            call: id(marker),
            delta: END.into(),
        },
        TurnEvent::ToolCallReady {
            call: id(marker),
            args: bytes,
        },
        TurnEvent::Finished(Finish::Completed),
    ] {
        assert_eq!(
            core.handle(Input::Engine { key: old, event }),
            Err(super::super::Stale)
        );
    }
    let debug = format!("{core:?} {:?} {stopped:?} {settled:?}", core.ending);
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    assert!(!core.drawing.pending_for(R));
    assert_eq!(
        settled
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
            .count(),
        1
    );
    store
        .end_run(
            T,
            R,
            &[],
            crate::store::RunEnd::Interrupted(crate::store::Interruption::Stop),
        )
        .expect("durable outcome");
    core.handle(Input::RunStored { run: R })
        .expect("retire settled run");
    assert!(core.current.is_none());
    assert!(core.ending.is_empty());
}

#[test]
fn failed_first_lookup_or_step_cannot_invent_a_scene_or_an_end() {
    for fail_lookup in [true, false] {
        let scratch = Scratch::new("provider-draw-first-write-failure");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        start(&mut core, "failed");
        let old = super::super::engine_key(&core, R);
        let lookup = delta(&mut core, "failed", &format!("{PREFIX}{END}"));
        let attempted = if fail_lookup {
            lookup
        } else {
            write(&mut core, &mut store, &lookup)
        };
        let (serial, write) = pending(&attempted).expect("first lookup or first step");
        assert_eq!(
            matches!(write, crate::drawing::Write::Lookup { .. }),
            fail_lookup
        );
        let failed = core
            .handle(Input::DrawingWritten {
                serial,
                reply: Err(()),
            })
            .expect("failed accepted write settles");
        assert!(!attempted.iter().chain(&failed).any(|effect| matches!(
            effect,
            Effect::Data(
                DataToShell::SceneBegin { .. }
                    | DataToShell::SceneStepCommit { .. }
                    | DataToShell::SceneEnd { .. }
            ) | Effect::Store(StoreOp::CommitCall(_))
        )));
        assert_eq!(pending(&failed), None);
        assert_eq!(
            store
                .drawing_scenes()
                .expect("nothing committed")
                .scenes
                .len(),
            0
        );
        assert_eq!(
            call(&mut core, "failed", &args(None)),
            (
                Dispatch::Answered(ToolOutput::Error(ToolError::Ended)),
                Vec::new()
            )
        );
        assert_eq!(
            core.handle(Input::Engine {
                key: old,
                event: TurnEvent::ToolArgsDelta {
                    call: id("failed"),
                    delta: END.into(),
                },
            }),
            Err(super::super::Stale)
        );
    }
}

#[test]
fn provider_byte_and_record_bounds_keep_the_prefix_and_require_both_success_fences() {
    for (byte_bound, over) in [(true, false), (true, true), (false, false), (false, true)] {
        let scratch = Scratch::new("provider-draw-bounds");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        let mut records =
            vec![json!({"begin":{"v":1,"scene":"bounded","surface":"overlay","alt":"bounded"}})];
        for index in 0..64 {
            records.push(json!({"step":{"id":format!("s{index}")}}));
        }
        records.push(json!({"end":{}}));
        if !byte_bound && over {
            records.push(json!({"end":{}}));
        }
        assert_eq!(records.len(), if !byte_bound && over { 67 } else { 66 });
        let mut bytes = serde_json::to_vec(&json!({"records":records})).expect("bound fixture");
        if byte_bound {
            bytes.resize(16 * 1024 + usize::from(over), b' ');
            assert_eq!(bytes.len(), if over { 16_385 } else { 16_384 });
        }
        let bytes = String::from_utf8(bytes).expect("ASCII fixture");
        start(&mut core, "bounded");
        let lookup = delta(&mut core, "bounded", &bytes);
        let mut effects = drain(&mut core, &mut store, lookup);
        let ids: Vec<_> = (0..64).map(|index| format!("s{index}")).collect();
        let borrowed: Vec<_> = ids.iter().map(String::as_str).collect();
        assert_prefix(&store, &borrowed);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::Data(DataToShell::SceneStepCommit { .. })
                ))
                .count(),
            64
        );
        assert_eq!(ends(&effects), if over { vec![false] } else { Vec::new() });
        if !over {
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
            );
        }
        let verified = ready(&mut core, "bounded", &bytes);
        assert_eq!(
            verified,
            Vec::new(),
            "Ready alone never completes the drawing"
        );
        let completed = send(&mut core, TurnEvent::Finished(Finish::Completed));
        effects.extend(completed);
        assert_eq!(ends(&effects), [!over]);
        let result = result(&effects);
        let stopped = if !over {
            String::new()
        } else if byte_bound {
            format!("; stopped: drawing exceeds byte limit{FULL}")
        } else {
            "; stopped: drawing.BudgetSpent".to_owned()
        };
        let expected = ToolOutput::Text(format!("drawn: [{}]{stopped}", ids.join(", ")));
        assert_eq!(result.output, expected);
        let acked = commit_result(&mut core, &mut store, &scratch, &result);
        assert_eq!(ends(&acked), Vec::<bool>::new());
        let changed = args(None);
        assert_eq!(
            call(
                &mut core,
                "bounded",
                if over { &changed } else { bytes.as_bytes() },
            ),
            (Dispatch::Answered(expected), Vec::new())
        );
        assert_prefix(&store, &borrowed);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "replacement must precede the real result and round acknowledgements"
)]
fn replacing_a_reserved_draw_before_its_first_record_settles_an_empty_interrupted_result() {
    for partial in ["", r#"{"records":[{"begin":{"v":1"#] {
        let scratch = Scratch::new("provider-draw-empty-interruption");
        let mut store = store(&scratch);
        let source = ThreadId(2);
        let mut core = offered(vec![T, source]);
        start(&mut core, "empty");
        if !partial.is_empty() {
            assert_eq!(delta(&mut core, "empty", partial), Vec::new());
        }
        let old = super::super::engine_key(&core, R);
        let mut effects = core
            .handle(Input::Delete {
                thread: source,
                after_turns: 1,
                resend: false,
            })
            .expect("invalidate the reservation's source");
        let rebuilding = core
            .handle(Input::Deleted {
                thread: source,
                outcome: kumochi_protocol::message::Deleted::Hidden,
                last_run: None,
            })
            .expect("source deletion acknowledged");
        let key = rebuilding
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                _ => None,
            })
            .expect("actual replacement build");
        effects.extend(rebuilding);
        assert_ne!(key, old);
        let result = result(&effects);
        let expected = ToolOutput::Text("drawn: []; stopped: drawing.Interrupted".into());
        assert_eq!(result.output, expected);
        core.handle(built(key, vec![T]))
            .expect("B2 admitted before result settlement");
        start(&mut core, "empty");
        let bytes = String::from_utf8(args(None)).expect("valid replacement stream");
        assert_eq!(delta(&mut core, "empty", &bytes), Vec::new());
        assert_eq!(ready(&mut core, "empty", &bytes), Vec::new());
        assert!(
            core.current
                .as_ref()
                .expect("B2")
                .tools
                .provider_registered(&id("empty"), Some(key))
        );
        assert_eq!(
            send(&mut core, TurnEvent::Finished(Finish::Completed)),
            Vec::new()
        );
        assert!(core.current.as_ref().expect("B2").tools.waiting("empty"));
        assert!(core.current.as_ref().expect("B2").answered.is_empty());
        let acked = commit_result(&mut core, &mut store, &scratch, &result);
        let rounds: Vec<_> = acked
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds.len(), 1);
        let round = rounds[0].clone();
        assert_eq!(round.calls, [(id("empty"), expected.clone())]);
        assert_eq!(round.items, Vec::new());
        assert_eq!(round.round, key.round);
        assert!(
            !core
                .current
                .as_ref()
                .expect("B2")
                .tools
                .provider_registered(&id("empty"), Some(key))
        );
        assert_eq!(
            core.handle(Input::CallCommitted {
                key: result.key.clone(),
                outcome: Committed::Done,
                then: None,
            }),
            Ok(Vec::new())
        );
        assert_eq!(
            call(&mut core, "empty", &args(None)),
            (Dispatch::Answered(expected), Vec::new())
        );
        assert!(!effects.iter().chain(&acked).any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::Drawing { .. })
                | Effect::Data(
                    DataToShell::SceneBegin { .. }
                        | DataToShell::SceneStepCommit { .. }
                        | DataToShell::SceneEnd { .. }
                )
        )));
        assert_eq!(
            store
                .drawing_scenes()
                .expect("no scene was invented")
                .scenes
                .len(),
            0
        );
        store
            .append_round(
                (round.thread, round.run),
                round.connection,
                round.text.as_deref(),
                &round.items,
                &round.calls,
            )
            .expect("real replacement round write");
        let next = core
            .handle(Input::RoundStored {
                run: R,
                round: round.round,
            })
            .expect("round retired");
        assert_eq!(
            next.iter()
                .filter_map(|effect| match effect {
                    Effect::Store(StoreOp::BuildContext(build)) => Some(build.key.round),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [key.round + 1]
        );
        let stopped = core
            .handle(Input::Stop {
                run: R,
                revealed: Vec::new(),
            })
            .expect("retire run after stored round");
        assert_eq!(
            stopped
                .iter()
                .filter(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
                .count(),
            1
        );
        store
            .end_run(
                T,
                R,
                &[],
                crate::store::RunEnd::Interrupted(crate::store::Interruption::Stop),
            )
            .expect("durable outcome");
        core.handle(Input::RunStored { run: R })
            .expect("retired run");
        assert!(core.current.is_none());
        assert!(core.ending.is_empty());
        let db = rusqlite::Connection::open(scratch.db()).expect("retired row counts");
        let rows: (u32, u32) = db
            .query_row(
                "SELECT (SELECT count(*) FROM tool_calls),
                        (SELECT count(*) FROM events WHERE kind IN
                            ('scene_begin', 'scene_step', 'scene_cut', 'scene_cleared'))",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("one result and no drawing rows");
        assert_eq!(rows, (1, 0));
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "fixed-refusal timing and the canonical control use the same replacement schedule"
)]
fn invalid_replacement_streams_join_only_noncanonical_fixed_results_before_or_after_ack() {
    for (first, second, ack_first) in [
        ("malformed", "malformed", false),
        ("malformed", "malformed", true),
        ("malformed", "truncated", false),
        ("malformed", "truncated", true),
        ("truncated", "malformed", false),
        ("truncated", "malformed", true),
        ("truncated", "truncated", false),
        ("truncated", "truncated", true),
        ("canonical", "malformed", false),
        ("canonical", "malformed", true),
        ("canonical", "truncated", false),
        ("canonical", "truncated", true),
    ] {
        let scratch = Scratch::new("provider-draw-invalid-replacement");
        let mut store = store(&scratch);
        let source = ThreadId(2);
        let mut core = offered(vec![T, source]);
        start(&mut core, "fixed");
        let effects = if first == "canonical" {
            let bytes = String::from_utf8(args(Some(json!({"id":"s3","remove":["missing"]}))))
                .expect("semantic fixture");
            let lookup = delta(&mut core, "fixed", &bytes);
            assert_eq!(ready(&mut core, "fixed", &bytes), Vec::new());
            drain(&mut core, &mut store, lookup)
        } else {
            let lookup = delta(&mut core, "fixed", PREFIX);
            let prefix = drain(&mut core, &mut store, lookup);
            assert_eq!(ends(&prefix), Vec::<bool>::new());
            if first == "malformed" {
                delta(&mut core, "fixed", ",!")
            } else {
                ready(&mut core, "fixed", PREFIX)
            }
        };
        let result = result(&effects);
        let expected = ToolOutput::Text(match first {
            "canonical" => {
                format!(
                    "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef{DRAWN_RETRY}"
                )
            }
            "malformed" => format!(
                "drawn: [s1]; stopped: malformed drawing at byte {}{DRAWN_RETRY}",
                PREFIX.len() + 1
            ),
            _ => format!("drawn: [s1]; stopped: truncated drawing{DRAWN_RETRY}"),
        });
        assert_eq!(result.output, expected);
        let old = super::super::engine_key(&core, R);
        let deleting = core
            .handle(Input::Delete {
                thread: source,
                after_turns: 1,
                resend: false,
            })
            .expect("invalidate B1");
        let rebuilding = core
            .handle(Input::Deleted {
                thread: source,
                outcome: kumochi_protocol::message::Deleted::Hidden,
                last_run: None,
            })
            .expect("rebuild after source deletion");
        let key = rebuilding
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                _ => None,
            })
            .expect("actual B2 key");
        assert_ne!(key, old);
        assert!(
            !deleting
                .iter()
                .chain(&rebuilding)
                .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
        );
        core.handle(built(key, vec![T]))
            .expect("B2 admitted with B1 result pending");
        start(&mut core, "fixed");
        let mut settled = Vec::new();
        if ack_first {
            settled.extend(commit_result(&mut core, &mut store, &scratch, &result));
        }
        let bytes = if second == "malformed" {
            r#"{"records":[!"#
        } else {
            r#"{"records":["#
        };
        assert_eq!(delta(&mut core, "fixed", bytes), Vec::new());
        assert_eq!(ready(&mut core, "fixed", bytes), Vec::new());
        assert!(
            core.current
                .as_ref()
                .expect("B2")
                .tools
                .provider_registered(&id("fixed"), Some(key))
        );
        let completed = send(&mut core, TurnEvent::Finished(Finish::Completed));
        if !ack_first && first != "canonical" {
            assert_eq!(
                completed,
                Vec::new(),
                "Joined is a durable wait, not Reused"
            );
            assert!(core.current.as_ref().expect("B2").tools.waiting("fixed"));
        }
        settled.extend(completed);
        if !ack_first {
            settled.extend(commit_result(&mut core, &mut store, &scratch, &result));
        }
        assert!(core.current.as_ref().expect("B2").answered.is_empty());
        assert!(!settled.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::CommitCall(_) | StoreOp::Drawing { .. })
        )));
        let rounds: Vec<_> = settled
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds.len(), 1);
        let answer = if first == "canonical" {
            ToolOutput::Error(ToolError::Reused)
        } else {
            expected
        };
        assert_eq!(rounds[0].calls, [(id("fixed"), answer.clone())]);
        assert_eq!(
            call(&mut core, "fixed", &args(None)),
            (Dispatch::Answered(answer), Vec::new())
        );
        assert_prefix(
            &store,
            if first == "canonical" {
                &["s1", "s2"]
            } else {
                &["s1"]
            },
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the existing-entry control shares the unavailable provider acknowledgement path"
)]
fn unavailable_provider_draw_verifies_before_a_durable_refusal_and_preserves_existing_identity() {
    for existing in [false, true] {
        let scratch = Scratch::new("provider-draw-unavailable-verification");
        let mut store = store(&scratch);
        let mut core = offered(vec![T]);
        assert_eq!(
            core.restore_drawings(Err(crate::store::StoreError::Closed)),
            Vec::new()
        );
        let bytes = format!("{PREFIX}{END}");
        if existing {
            let (dispatch, effects) = call(&mut core, "unavailable", bytes.as_bytes());
            assert_eq!(dispatch, Dispatch::Committing);
            let result = result(&effects);
            assert_eq!(result.output, ToolOutput::Error(ToolError::NotHere));
            assert_eq!(
                commit_result(&mut core, &mut store, &scratch, &result),
                Vec::new()
            );
        }
        let answered = core.current.as_ref().expect("run").answered.len();
        start(&mut core, "unavailable");
        assert_eq!(delta(&mut core, "unavailable", &bytes), Vec::new());
        let key = super::super::engine_key(&core, R);
        assert!(
            !core
                .current
                .as_ref()
                .expect("run")
                .tools
                .provider_registered(&id("unavailable"), Some(key))
        );
        assert_eq!(core.current.as_ref().expect("run").answered.len(), answered);
        let mut effects = Vec::new();
        let expected = if existing {
            let mismatched = String::from_utf8(args(None)).expect("different valid JSON");
            assert_eq!(ready(&mut core, "unavailable", &mismatched), Vec::new());
            effects.extend(send(&mut core, TurnEvent::Finished(Finish::Completed)));
            ToolOutput::Error(ToolError::Reused)
        } else {
            let verified = ready(&mut core, "unavailable", &bytes);
            let result = result(&verified);
            assert_eq!(result.output, ToolOutput::Error(ToolError::NotHere));
            assert_eq!(
                verified.len(),
                1,
                "only the result transaction is submitted"
            );
            assert_eq!(
                send(&mut core, TurnEvent::Finished(Finish::Completed)),
                Vec::new()
            );
            assert!(
                core.current
                    .as_ref()
                    .expect("run")
                    .tools
                    .waiting("unavailable")
            );
            assert_eq!(core.current.as_ref().expect("run").answered.len(), answered);
            effects.extend(commit_result(&mut core, &mut store, &scratch, &result));
            assert_eq!(
                core.handle(Input::CallCommitted {
                    key: result.key,
                    outcome: Committed::Done,
                    then: None,
                }),
                Ok(Vec::new())
            );
            ToolOutput::Error(ToolError::NotHere)
        };
        let rounds: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::RoundItems(round)) => Some(round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].calls, [(id("unavailable"), expected)]);
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::Drawing { .. } | StoreOp::CommitCall(_))
                | Effect::Data(
                    DataToShell::SceneBegin { .. }
                        | DataToShell::SceneStepCommit { .. }
                        | DataToShell::SceneEnd { .. }
                )
        )));
        assert_eq!(
            store
                .drawing_scenes()
                .expect("unavailable has no scenes")
                .scenes
                .len(),
            0
        );
        assert!(core.current.as_ref().expect("run").draw_calls.is_empty());
        core.drawing = crate::drawing::Drawing::default();
        assert_eq!(
            call(&mut core, "unavailable", bytes.as_bytes()),
            (
                Dispatch::Answered(ToolOutput::Error(ToolError::NotHere)),
                Vec::new()
            )
        );
        let db = rusqlite::Connection::open(scratch.db()).expect("unchanged logical result");
        let rows: (u32, String) = db
            .query_row("SELECT count(*), result FROM tool_calls", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("one result row");
        assert_eq!(rows, (1, ToolOutput::Error(ToolError::NotHere).stored()));
    }
}

#[test]
fn a_streamed_inline_begin_is_refused_before_lookup_or_scene_work_and_overlay_repair_succeeds() {
    let scratch = Scratch::new("provider-draw-inline-begin");
    let mut store = store(&scratch);
    let mut core = offered(vec![T]);
    let inline = PREFIX.replace("\"surface\":\"overlay\"", "\"surface\":\"inline\"");
    start(&mut core, "inline");
    let failed = delta(&mut core, "inline", &inline);
    assert!(pending(&failed).is_none());
    assert_eq!(ends(&failed), Vec::<bool>::new());
    assert!(store.drawing_scenes().expect("no scene").scenes.is_empty());
    assert!(core.drawing.snapshot().scenes.is_empty());
    let refused = result(&failed);
    let expected = ToolOutput::Text(format!(
        "drawn: []; stopped: surface: scene.Unsupported{RETRY}"
    ));
    assert_eq!(refused.output, expected);
    assert_eq!(
        call(&mut core, "inline", &args(None)),
        (Dispatch::Joined, Vec::new())
    );
    assert_eq!(delta(&mut core, "inline", END), Vec::new());
    assert_eq!(
        ready(&mut core, "inline", &format!("{inline}{END}")),
        Vec::new()
    );
    let acked = commit_result(&mut core, &mut store, &scratch, &refused);
    assert_eq!(ends(&acked), Vec::<bool>::new());
    assert_eq!(
        call(&mut core, "inline", &args(None)),
        (Dispatch::Answered(expected.clone()), Vec::new())
    );
    start(&mut core, "overlay");
    let lookup = delta(&mut core, "overlay", PREFIX);
    let prefix = drain(&mut core, &mut store, lookup);
    assert_eq!(ends(&prefix), Vec::<bool>::new());
    assert_prefix(&store, &["s1"]);
    assert_eq!(delta(&mut core, "overlay", END), Vec::new());
    let overlay = format!("{PREFIX}{END}");
    assert_eq!(ready(&mut core, "overlay", &overlay), Vec::new());
    let completed = send(&mut core, TurnEvent::Finished(Finish::Completed));
    assert_eq!(ends(&completed), [true]);
    let repaired = result(&completed);
    let expected_repair = ToolOutput::Text("drawn: [s1]".into());
    assert_eq!(repaired.output, expected_repair);
    assert_eq!(
        store
            .commit_call(&repaired)
            .expect("real repair result INSERT"),
        Committed::Done
    );
    let settled = core
        .handle(Input::CallCommitted {
            key: repaired.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .expect("repair result acknowledgement");
    let rounds: Vec<_> = settled
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => Some(round),
            _ => None,
        })
        .collect();
    assert_eq!(rounds.len(), 1);
    assert_eq!(
        rounds[0].calls,
        [
            (id("inline"), expected),
            (id("overlay"), expected_repair.clone())
        ]
    );
    assert_eq!(
        call(&mut core, "overlay", overlay.as_bytes()),
        (Dispatch::Answered(expected_repair), Vec::new())
    );
    assert_prefix(&store, &["s1"]);
}

fn key_build(core: &mut Coordinator, key: BuildKey, snapshot: Option<RunSnapshot>) -> Vec<Effect> {
    let Input::ContextBuilt(mut built) = super::built(key, vec![T]) else {
        panic!("a build");
    };
    built.snapshot = snapshot;
    core.handle(Input::ContextBuilt(built))
        .expect("current build")
}

fn key_offered() -> Coordinator {
    use crate::coordinator::{Availability, Ready};
    use kumochi_protocol::message::{
        ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug,
    };

    let mut core = super::super::fresh();
    let model = ModelSlug::new("claude-sonnet-5-5").expect("slug");
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
        text: "hi".into(),
    })
    .expect("turn");
    let effects = core
        .handle(Input::TurnStored { run: R })
        .expect("turn stored");
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("build");
    let started = key_build(
        &mut core,
        key,
        Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(ModelChoice {
                connection: ConnectionKind::AnthropicKey,
                model,
                effort: Effort::Low,
            }),
            connection: ConnectionTag(41),
        }),
    );
    assert!(
        started
            .iter()
            .any(|effect| matches!(effect, Effect::StartEngine { .. })),
        "the key round starts: {started:?}"
    );
    if let crate::coordinator::Availability::Ready(ready) = &mut core.availability {
        ready.trusted_draw = false;
    }
    assert!(
        !core.trusted_draw(),
        "the offered draw takes the provider stream"
    );
    core
}

/// Streams one draw call under the reused id and ends the round; acknowledging its stored result
/// stores the round.
fn key_draw_round(
    core: &mut Coordinator,
    store: &mut Store,
    bytes: &str,
    expected: &ToolOutput,
) -> Vec<Effect> {
    start(core, "reused");
    let mut streamed = delta(core, "reused", bytes);
    streamed.extend(ready(core, "reused", bytes));
    let mut effects = drain(core, store, streamed);
    effects.extend(send(core, TurnEvent::Finished(Finish::Completed)));
    let result = result(&effects);
    assert_eq!(result.output, *expected);
    assert_eq!(
        store.commit_call(&result).expect("real result INSERT"),
        Committed::Done
    );
    core.handle(Input::CallCommitted {
        key: result.key.clone(),
        outcome: Committed::Done,
        then: None,
    })
    .expect("result acknowledgement")
}

fn round_calls(effects: &[Effect]) -> Vec<(CallId, ToolOutput)> {
    let rounds: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => Some(round.calls.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rounds.len(), 1, "one stored round: {effects:?}");
    rounds[0].clone()
}

const KEY_MARKS: &str = r#"{"records":[{"begin":{"scene":"key_scene_private","surface":"screen","title":"key_title_private","alt":"key_alt_private"}},{"step":{"id":"key_step_private","put":[{"highlight":{"id":"key_object_private","target":{"box":[186,598,450,78]},"style":"box"}}]}},{"end":{}}]}"#;

fn pictured_key() -> (Coordinator, RunSnapshot) {
    use super::super::routes::{built_for, choice, held, jpeg, key, pictured};
    use crate::context::{Geometry, Screen};
    use crate::coordinator::{PictureRoutes, SendKey};
    use kumochi_protocol::message::ConnectionKind::AnthropicKey;

    let model = choice(AnthropicKey, "m");
    let (mut core, hold) = held(
        vec![Input::Engines(AnthropicKey, pictured(5, &["m"]))],
        model.clone(),
        Some(T),
    );
    let snapshot = hold.defaults.clone();
    core.set_pictures(PictureRoutes::new(true, false));
    core.submit_hold(
        hold,
        "ordinary conversation".into(),
        Some(Screen::Picture(jpeg(), None, Geometry::FIXTURE)),
    )
    .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    let Input::ContextBuilt(mut built) = built_for(key(), &model) else {
        unreachable!()
    };
    built.result.as_mut().unwrap().request.tools = crate::context::tools(snapshot.origin);
    core.handle(Input::ContextBuilt(built)).unwrap();
    let leases = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .unwrap();
    let lease = leases
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(lease),
            _ => None,
        })
        .expect("the pictured Key round entered write admission");
    assert!(core.picture_grant(lease).is_some());
    core.handle(Input::SendSettled { lease }).unwrap();
    (core, snapshot)
}

fn pictured_key_call(core: &mut Coordinator, store: &mut Store) -> crate::dispatch::CommitCall {
    key_call(core, store, (R, "pictured-draw"), KEY_MARKS)
}

/// `run`'s one streamed Key draw `call` of `marks`, through to its result.
fn key_call(
    core: &mut Coordinator,
    store: &mut Store,
    (run, call): (kumochi_protocol::message::RunId, &str),
    marks: &str,
) -> crate::dispatch::CommitCall {
    use crate::engines::ToolName;
    use crate::engines::anthropic::encode::ToolUse;

    let send = |core: &mut Coordinator, event| {
        let key = super::super::engine_key(core, run);
        core.handle(Input::Engine { key, event })
            .expect("current provider event")
    };
    let name = id(call);
    let started = TurnEvent::ToolCallStarted {
        call: name.clone(),
        name: ToolName::Draw,
    };
    assert_eq!(send(core, started), Vec::new());
    let mut streamed = send(
        core,
        TurnEvent::ToolArgsDelta {
            call: name.clone(),
            delta: marks.into(),
        },
    );
    streamed.extend(send(
        core,
        TurnEvent::ToolCallReady {
            call: name,
            args: marks.into(),
        },
    ));
    let mut effects = drain(core, store, streamed);
    let call = ToolUse::projected(&format!(
        r#"{{"type":"tool_use","id":"{call}","name":"draw","input":{marks}}}"#
    ))
    .expect("a valid Key tool_use");
    assert!(send(core, TurnEvent::KeyCall { index: 0, call }).is_empty());
    effects.extend(send(core, TurnEvent::Finished(Finish::Completed)));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(DataToShell::SceneStepCommit { .. })))
    );
    result(&effects)
}

fn built_key(
    store: &Store,
    snapshot: &RunSnapshot,
    run: kumochi_protocol::message::RunId,
    picture: Option<&crate::context::PictureRecipient>,
    key_results: &[(CallId, ToolOutput)],
    drawings: crate::drawing::Digest,
) -> crate::context::ModelRequest {
    use crate::context::Supplied;
    use crate::context::build::{Turn, build};
    build(
        crate::store::context::Snapshot::begin(store.conn_for_tests()).unwrap(),
        snapshot,
        Turn {
            thread: T,
            run,
            local_time: "10:00",
            picture,
            key_results,
        },
        Supplied {
            drawings,
            ..Supplied::default()
        },
        crate::context::BOUND,
    )
    .unwrap()
    .request
}

fn key_recipient(snapshot: &RunSnapshot) -> crate::context::PictureRecipient {
    let choice = snapshot.choice.as_ref().unwrap();
    crate::context::PictureRecipient {
        route: crate::context::Route {
            kind: choice.connection,
            tag: snapshot.connection,
        },
        model: choice.model.clone(),
    }
}

fn key_wire(request: &crate::context::ModelRequest, tag: ConnectionTag) -> Value {
    let (body, _) = crate::engines::anthropic::encode::body(request, tag).unwrap();
    serde_json::from_slice(body.sent()).unwrap()
}

fn assert_pictured_key_rows(store: &Store) {
    let row: String = store
        .conn_for_tests()
        .query_row(
            "SELECT body FROM events WHERE run = 10 AND kind = 'output_item'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&row).unwrap(),
        json!({
            "connection": 5, "kind": "anthropic_key", "screen_model": "m",
            "item": {"type":"tool_use", "id":"pictured-draw", "name":"draw", "input":{"records":[]}}
        })
    );
    let stored_result: String = store
        .conn_for_tests()
        .query_row(
            "SELECT body FROM events WHERE run = 10 AND kind = 'tool_result'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored_result).unwrap(),
        json!({
            "call": "pictured-draw",
            "output": {"text": "Screen-derived tool result omitted."}
        })
    );
}

#[test]
fn pictured_key_storage_and_wire_are_scrubbed_but_the_live_round_gets_its_result() {
    use crate::store::RunEnd;

    let scratch = Scratch::new("provider-draw-key-picture-storage");
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    store
        .append(
            T,
            R,
            &[crate::store::NewEvent {
                kind: crate::store::EventKind::AssistantText,
                text: Some("ordinary reply"),
                body: None,
            }],
        )
        .unwrap();
    let commit = pictured_key_call(&mut core, &mut store);
    assert_eq!(store.commit_call(&commit).unwrap(), Committed::Done);
    let ledger_result: String = store
        .conn_for_tests()
        .query_row(
            "SELECT result FROM tool_calls WHERE run = 10 AND tool = 'draw'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ledger_result,
        r#"{"text":"Screen-derived tool result omitted."}"#
    );
    let effects = core
        .handle(Input::CallCommitted {
            key: commit.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a completed round: {effects:?}");
    };
    assert_eq!(round.pictured_key_model.as_ref().unwrap().as_str(), "m");
    store
        .append_key_round_events(
            (T, R),
            snapshot.connection,
            &[],
            &round.key_calls,
            &round.calls,
            round.pictured_key_model.as_ref(),
        )
        .unwrap();
    assert_eq!(
        commit.output,
        ToolOutput::Text("Screen-derived tool result omitted.".into())
    );
    assert_pictured_key_rows(&store);
    let effects = core
        .handle(Input::RoundStored { run: R, round: 1 })
        .unwrap();
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("a continuation build: {effects:?}");
    };
    let live = key_wire(
        &built_key(
            &store,
            &snapshot,
            R,
            Some(&key_recipient(&snapshot)),
            &build.key_results,
            crate::drawing::Digest::default(),
        ),
        snapshot.connection,
    );
    assert!(live.to_string().contains("drawn: [key_step_private]"));
    assert!(!format!("{build:?}").contains("key_step_private"));
    store.end_run(T, R, &[], RunEnd::Done).unwrap();
    assert_pictured_key_switches(&mut store, &snapshot);
}

fn assert_pictured_key_switches(store: &mut Store, snapshot: &RunSnapshot) {
    use crate::store::RunEnd;
    use kumochi_protocol::message::{ModelSlug, RunId};

    for (offset, slug, capable, same) in [
        (1, "m", true, true),
        (2, "text-only", false, false),
        (3, "other-vision", true, false),
        (4, "m", false, false),
        (5, "m", true, true),
    ] {
        let run = RunId(R.0 + offset);
        store
            .begin_turn(T, None, run, "ordinary next turn")
            .unwrap();
        let mut next = snapshot.clone();
        next.choice.as_mut().unwrap().model = ModelSlug::new(slug).unwrap();
        let recipient = capable.then(|| key_recipient(&next));
        let body = key_wire(
            &built_key(
                store,
                &next,
                run,
                recipient.as_ref(),
                &[],
                crate::drawing::Digest::default(),
            ),
            next.connection,
        );
        assert_eq!(body["model"], slug);
        let blocks: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .collect();
        let call = blocks
            .iter()
            .find(|block| block["id"] == "pictured-draw")
            .unwrap();
        let output = blocks
            .iter()
            .find(|block| block["tool_use_id"] == "pictured-draw")
            .unwrap();
        let (input, result) = if same {
            (json!({"records":[]}), "Screen-derived tool result omitted.")
        } else {
            (json!({}), "Tool result not available on this model.")
        };
        assert_eq!(call["input"], input);
        assert_eq!(output["content"], result);
        let wire = body.to_string();
        assert_eq!(wire.matches("pictured-draw").count(), 2);
        for private in [
            "key_scene_private",
            "key_alt_private",
            "key_object_private",
            "key_step_private",
            "186,598,450,78",
        ] {
            assert!(!wire.contains(private));
        }
        assert!(wire.contains("ordinary reply"));
        assert!(wire.contains("ordinary next turn"));
        store.end_run(T, run, &[], RunEnd::Done).unwrap();
    }
}

#[test]
fn interrupting_a_pictured_key_draw_leaves_only_a_scrubbed_early_result() {
    let scratch = Scratch::new("provider-draw-key-picture-stopped");
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    let call = pictured_key_call(&mut core, &mut store);
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    let effects = core
        .handle(Input::Stop {
            run: R,
            revealed: vec![0],
        })
        .unwrap();
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::RoundItems(_))))
    );
    let row: (String, i64) = store.conn_for_tests().query_row(
        "SELECT result, (SELECT count(*) FROM events WHERE run = 10 AND kind = 'output_item') FROM tool_calls WHERE run = 10",
        [], |r| Ok((r.get(0)?, r.get(1)?))
    ).unwrap();
    assert_eq!(
        row,
        (
            r#"{"text":"Screen-derived tool result omitted."}"#.into(),
            0
        )
    );
    assert_interrupted_key_wires(&mut core, &mut store, &snapshot);
}

/// After the pictured draw was stopped, every later request's whole wire, for the recipient or
/// not, carries no box, no raw result and no half of its call pair; only the recipient gets the
/// scene's names.
fn assert_interrupted_key_wires(core: &mut Coordinator, store: &mut Store, snapshot: &RunSnapshot) {
    use crate::store::{Interruption, RunEnd};
    use kumochi_protocol::message::{ConnectionKind, RunId};

    store
        .end_run(T, R, &[], RunEnd::Interrupted(Interruption::Stop))
        .unwrap();
    let run = RunId(R.0 + 1);
    store
        .begin_turn(T, None, run, "ordinary next turn")
        .unwrap();
    let drawings = core.drawing.digest(T);
    for (connection, slug, capable) in [
        (ConnectionKind::AnthropicKey, "m", true),
        (ConnectionKind::AnthropicKey, "other-vision", true),
        (ConnectionKind::AnthropicKey, "text-only", false),
        (ConnectionKind::ChatGpt, "m", true),
    ] {
        let mut receiving = snapshot.clone();
        receiving.choice = Some(super::super::routes::choice(connection, slug));
        let pictured = capable.then(|| key_recipient(&receiving));
        let request = built_key(
            store,
            &receiving,
            run,
            pictured.as_ref(),
            &[],
            drawings.clone(),
        );
        let wire = if connection == ConnectionKind::ChatGpt {
            crate::engines::responses::body(&request)
                .unwrap()
                .to_string()
        } else {
            key_wire(&request, receiving.connection).to_string()
        };
        let recipient = connection == ConnectionKind::AnthropicKey && slug == "m";
        for private in ["key_scene_private", "key_alt_private", "key_object_private"] {
            assert_eq!(
                wire.contains(private),
                recipient,
                "{connection:?} {slug} {private}"
            );
        }
        for never in [
            "pictured-draw",
            "key_step_private",
            "186,598,450,78",
            "input_image",
        ] {
            assert!(!wire.contains(never), "{connection:?} {slug} {never}");
        }
        assert!(wire.contains("ordinary next turn"));
    }
}

/// A typed Key turn with no picture of its own that is sent the earlier pictured turn's digest is
/// marked as that turn was: its call history is stored scrubbed, a different model on the same
/// key gets a neutral pair, and the overlay it drew reaches no other model.
#[test]
fn a_typed_key_turn_sent_a_protected_digest_stamps_what_it_authors() {
    use crate::store::RunEnd;
    use kumochi_protocol::message::{ModelSlug, RunId};

    let scratch = Scratch::new("provider-draw-key-digest-only");
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    end_pictured_key_run(&mut core, &mut store, &snapshot);
    let next = RunId(R.0 + 1);
    let key = begin_typed_key_turn(&mut core, &mut store, next);
    let first = built_key(
        &store,
        &snapshot,
        next,
        Some(&key_recipient(&snapshot)),
        &[],
        core.drawing.digest(T),
    );
    let wire = key_wire(&first, snapshot.connection).to_string();
    assert!(wire.contains("typed follow-up") && wire.contains("key_scene_private"));
    admit_key_build(&mut core, key, &snapshot, first);

    let commit = key_call(
        &mut core,
        &mut store,
        (next, "digest-draw"),
        COPIED_KEY_OVERLAY,
    );
    assert_eq!(
        commit.output,
        ToolOutput::Text(crate::dispatch::PICTURE_RESULT.into())
    );
    assert_eq!(store.commit_call(&commit).unwrap(), Committed::Done);
    let effects = core
        .handle(Input::CallCommitted {
            key: commit.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a completed round: {effects:?}");
    };
    assert_eq!(round.pictured_key_model.as_ref().unwrap().as_str(), "m");
    store
        .append_key_round_events(
            (T, next),
            snapshot.connection,
            &[],
            &round.key_calls,
            &round.calls,
            round.pictured_key_model.as_ref(),
        )
        .unwrap();
    store.end_run(T, next, &[], RunEnd::Done).unwrap();
    let drawings = core.drawing.digest(T);
    assert!(!drawings.off_screen().contains("digest_copy_private"));

    let later = RunId(R.0 + 2);
    store
        .begin_turn(T, None, later, "ordinary later turn")
        .unwrap();
    let mut other = snapshot;
    other.choice.as_mut().unwrap().model = ModelSlug::new("other-vision").unwrap();
    let body = key_wire(
        &built_key(
            &store,
            &other,
            later,
            Some(&key_recipient(&other)),
            &[],
            drawings,
        ),
        other.connection,
    );
    let wire = body.to_string();
    for private in [
        "digest_copy_private",
        "key_title_private",
        "key_object_private",
        "drawn: [d1]",
    ] {
        assert!(!wire.contains(private), "{private}");
    }
    let blocks: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .collect();
    for call in ["pictured-draw", "digest-draw"] {
        let input = blocks.iter().find(|block| block["id"] == call).unwrap();
        let output = blocks
            .iter()
            .find(|block| block["tool_use_id"] == call)
            .unwrap();
        assert_eq!(input["input"], json!({}), "{call}");
        assert_eq!(
            output["content"], "Tool result not available on this model.",
            "{call}"
        );
        assert_eq!(wire.matches(call).count(), 2, "{call}");
    }
    assert!(wire.contains("ordinary later turn"));
}

const COPIED_KEY_OVERLAY: &str = r#"{"records":[{"begin":{"v":1,"scene":"digest_copy_private","surface":"overlay","alt":"key_title_private"}},{"step":{"id":"d1","put":[{"shape":{"id":"key_object_private","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;

/// Streams the pictured draw of `snapshot` through its round to the end of run `R`.
fn end_pictured_key_run(core: &mut Coordinator, store: &mut Store, snapshot: &RunSnapshot) {
    use crate::store::{Interruption, RunEnd};

    let commit = pictured_key_call(core, store);
    assert_eq!(store.commit_call(&commit).unwrap(), Committed::Done);
    let effects = core
        .handle(Input::CallCommitted {
            key: commit.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a completed round: {effects:?}");
    };
    store
        .append_key_round_events(
            (T, R),
            snapshot.connection,
            &[],
            &round.key_calls,
            &round.calls,
            round.pictured_key_model.as_ref(),
        )
        .unwrap();
    core.handle(Input::RoundStored { run: R, round: 1 })
        .unwrap();
    let stopped = core
        .handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .unwrap();
    assert!(
        stopped
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
    );
    store
        .end_run(T, R, &[], RunEnd::Interrupted(Interruption::Stop))
        .unwrap();
    core.handle(Input::RunStored { run: R }).unwrap();
    assert!(core.current.is_none());
}

/// Starts typed turn `next` and returns its first build's key.
fn begin_typed_key_turn(
    core: &mut Coordinator,
    store: &mut Store,
    next: kumochi_protocol::message::RunId,
) -> BuildKey {
    core.handle(Input::Turn {
        turn: 2,
        thread: Some(T),
        text: "typed follow-up".into(),
    })
    .unwrap();
    store.begin_turn(T, None, next, "typed follow-up").unwrap();
    core.handle(Input::TurnStored { run: next })
        .unwrap()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("the typed turn's build")
}

/// A typed Key turn sent the pictured turn's digest writes under a revocable grant. Losing the
/// model's image fact, the key's sign-in, Key pictures, key validity, showing the screen or the
/// listed drawing between grant and first write stops that write; a rebuildable round 1 then
/// rebuilds cleanly without the revoked digest, and unchanged it writes.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "seven revocation triggers with round-1 rebuild check"
)]
fn a_digest_only_key_send_writes_only_while_its_digest_may_go() {
    use crate::connections::model::Lease as _;
    use crate::coordinator::{Availability, PictureRoutes, RefusedKey};
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, ErrorKey, RunId};

    type Change = fn(&mut Coordinator);
    let changes: [(&str, bool, Option<Change>); 7] = [
        ("unchanged", false, None),
        (
            "image-fact",
            true,
            Some(|core| {
                let none = super::super::routes::pictured(5, &[]);
                core.handle(Input::Engines(AnthropicKey, none)).unwrap();
            }),
        ),
        (
            "sign-out",
            false,
            Some(|core| {
                let out = Availability::Unavailable(ErrorKey::AccountRefused.into());
                core.handle(Input::Engines(AnthropicKey, out)).unwrap();
            }),
        ),
        (
            "pictures-off",
            true,
            Some(|core| core.set_pictures(PictureRoutes::new(false, false))),
        ),
        (
            "screen-off",
            true,
            Some(|core| {
                core.handle(Input::ShowScreenOff).unwrap();
            }),
        ),
        (
            "clear-visible",
            true,
            Some(|core| {
                core.handle(Input::ClearVisible).unwrap();
            }),
        ),
        (
            "key-refused",
            false,
            Some(|core| {
                core.handle(Input::KeyRefused(RefusedKey(
                    crate::context::ConnectionTag(5),
                    None,
                )))
                .unwrap();
            }),
        ),
    ];
    for (name, rebuilds, change) in changes {
        let scratch = Scratch::new(&format!("provider-draw-key-digest-grant-{name}"));
        let mut store = store(&scratch);
        let (mut core, snapshot) = pictured_key();
        end_pictured_key_run(&mut core, &mut store, &snapshot);
        let next = RunId(R.0 + 1);
        let key = begin_typed_key_turn(&mut core, &mut store, next);
        let request = built_key(
            &store,
            &snapshot,
            next,
            Some(&key_recipient(&snapshot)),
            &[],
            core.drawing.digest(T),
        );
        let wire = key_wire(&request, snapshot.connection).to_string();
        assert!(wire.contains("key_scene_private"), "{name}");
        let lease = grant_key_build(&mut core, key, &snapshot, request);
        let grant = core
            .picture_grant(lease)
            .expect("a digest-only send's grant");
        let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
        let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant));
        let settled = match change {
            None => {
                assert!(send.admit().is_some(), "{name}");
                Input::SendSettled { lease }
            }
            Some(change) => {
                change(&mut core);
                assert!(send.admit().is_none(), "{name}");
                Input::SendUnsent { lease }
            }
        };
        let effects = core.handle(settled).unwrap();
        if rebuilds {
            let again = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                    _ => None,
                })
                .expect("round 1 rebuilds cleanly after an unsent digest revocation");
            assert_ne!(again, key, "{name}");
            let rebuilt = built_key(&store, &snapshot, next, None, &[], core.drawing.digest(T));
            let wire = key_wire(&rebuilt, snapshot.connection).to_string();
            assert!(!wire.contains("key_scene_private"), "{name}");
            let plain = grant_key_build(&mut core, again, &snapshot, rebuilt);
            assert!(core.picture_grant(plain).is_none(), "{name}");
            core.handle(Input::SendSettled { lease: plain }).unwrap();
        }
    }
}

/// A Key continuation resends its first request, so clearing a screen scene that first request
/// digested stops round 2 even when round 1 drew a still-visible private overlay: at admission
/// when cleared before round 2's grant, and at write admission when cleared after it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "drives round 1 into round 2 before and after grant"
)]
fn a_key_continuation_stops_when_its_first_round_screen_digest_is_cleared() {
    use crate::connections::model::Lease as _;
    use crate::coordinator::SendKey;
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, RunId};

    for after_grant in [false, true] {
        let scratch = Scratch::new(&format!("provider-draw-key-cont-revoke-{after_grant}"));
        let mut store = store(&scratch);
        let (mut core, snapshot) = pictured_key();
        end_pictured_key_run(&mut core, &mut store, &snapshot);
        let next = RunId(R.0 + 1);
        let key = begin_typed_key_turn(&mut core, &mut store, next);
        let first = built_key(
            &store,
            &snapshot,
            next,
            Some(&key_recipient(&snapshot)),
            &[],
            core.drawing.digest(T),
        );
        admit_key_build(&mut core, key, &snapshot, first);
        let call = key_call(
            &mut core,
            &mut store,
            (next, "digest-draw"),
            LATER_KEY_OVERLAY,
        );
        assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
        let effects = core
            .handle(Input::CallCommitted {
                key: call.key.clone(),
                outcome: Committed::Done,
                then: None,
            })
            .unwrap();
        let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
            panic!("a completed round: {effects:?}");
        };
        store
            .append_key_round_events(
                (T, next),
                snapshot.connection,
                &[],
                &round.key_calls,
                &round.calls,
                round.pictured_key_model.as_ref(),
            )
            .unwrap();
        if !after_grant {
            core.handle(Input::ShowScreenOff).unwrap();
            assert!(
                !core.drawing.visible_to(T, &key_recipient(&snapshot)),
                "screen-off drops private overlays from outbound digests"
            );
            assert!(
                core.drawing.digest(T).as_str().contains("later_scene"),
                "round 1's overlay stays visible locally"
            );
        }
        let effects = core
            .handle(Input::RoundStored {
                run: next,
                round: 1,
            })
            .unwrap();
        let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
            panic!("round 2 build: {effects:?}");
        };
        let second = built_key(
            &store,
            &snapshot,
            next,
            Some(&key_recipient(&snapshot)),
            &build.key_results,
            core.drawing.digest(T),
        );
        let model = super::super::routes::choice(AnthropicKey, "m");
        let Input::ContextBuilt(mut context) = super::super::routes::built_for(build.key, &model)
        else {
            unreachable!()
        };
        context.snapshot = None;
        context.result.as_mut().unwrap().request = second;
        core.handle(Input::ContextBuilt(context)).unwrap();
        let ready = core
            .handle(Input::SendReady {
                key: SendKey::Build(build.key),
                hold: None,
            })
            .unwrap();
        if !after_grant {
            assert!(ready.contains(&Effect::Rebuild(SendKey::Build(build.key))));
            assert!(
                core.current.is_none(),
                "round 2 fails instead of rebuilding"
            );
            continue;
        }
        let lease = ready
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(lease),
                _ => None,
            })
            .expect("round 2 granted");
        let grant = core.picture_grant(lease).expect("round 2 carries a grant");
        let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
        let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant));
        core.handle(Input::ShowScreenOff).unwrap();
        assert!(send.admit().is_none(), "screen-off closes round 2's grant");
        let effects = core.handle(Input::SendUnsent { lease }).unwrap();
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::BuildContext(_)))),
            "round 2 never attempts a doomed rebuild"
        );
        assert!(core.current.is_none(), "round 2 fails cleanly");
    }
}

/// Seeds a pictured run on `kind` that authors only a `Surface::Overlay` (no screen scene) and
/// finishes cleanly, returning the coordinator, store, snapshot, recipient and overlay scene id.
fn seed_private_overlay_only(
    scratch: &Scratch,
    kind: kumochi_protocol::message::ConnectionKind,
) -> (
    Coordinator,
    Store,
    RunSnapshot,
    crate::context::PictureRecipient,
    kumochi_protocol::message::SceneId,
) {
    use super::super::routes::{built_for, choice, held, jpeg, key, pictured};
    use crate::context::{PictureRecipient, Route, Screen};
    use crate::coordinator::{PictureRoutes, SendKey};
    use crate::drawing::{Input as DrawInput, Reply, Write};
    use kumochi_protocol::message::ConnectionKind;

    let store = store(scratch);
    let tag = if kind == ConnectionKind::ChatGpt {
        7
    } else {
        5
    };
    let model = choice(kind, "m");
    let (mut core, hold) = held(
        vec![Input::Engines(kind, pictured(tag, &["m"]))],
        model.clone(),
        Some(T),
    );
    let snapshot = hold.defaults.clone();
    core.set_pictures(PictureRoutes::new(true, false));
    core.submit_hold(hold, "seed overlay".into(), Some(Screen::Image(jpeg())))
        .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    let Input::ContextBuilt(mut built) = built_for(key(), &model) else {
        unreachable!()
    };
    built.result.as_mut().unwrap().request.tools = crate::context::tools(snapshot.origin);
    core.handle(Input::ContextBuilt(built)).unwrap();
    let lease = core
        .handle(Input::SendReady {
            key: SendKey::Build(key()),
            hold: None,
        })
        .unwrap()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(lease),
            _ => None,
        })
        .expect("seed granted");
    core.handle(Input::SendSettled { lease }).unwrap();
    let recipient = PictureRecipient {
        route: Route {
            kind,
            tag: crate::context::ConnectionTag(tag),
        },
        model: model.model,
    };
    core.drawing.author(R, Some(&recipient));
    let mut effects = core
        .handle(Input::Drawing(DrawInput::Call {
            thread: T,
            run: R,
            call: id("seed-overlay"),
            args: COPIED_KEY_OVERLAY.into(),
        }))
        .unwrap();
    let mut scene = None;
    while let Some((serial, write)) = effects.iter().find_map(|effect| match effect {
        Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
        _ => None,
    }) {
        let reply = match write {
            Write::Lookup { .. } => Reply::Lookup(None),
            Write::Step { .. } => {
                let id = kumochi_protocol::message::SceneId(50);
                scene = Some(id);
                Reply::Step(id)
            }
            Write::Cut { .. } => Reply::Cut,
        };
        effects = core
            .handle(Input::DrawingWritten {
                serial,
                reply: Ok(reply),
            })
            .unwrap();
    }
    let scene = scene.expect("overlay stored");
    let end_key = super::super::engine_key(&core, R);
    core.handle(Input::Engine {
        key: end_key,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .unwrap();
    core.handle(Input::RunStored { run: R }).unwrap();
    assert!(core.drawing.visible_to(T, &recipient));
    (core, store, snapshot, recipient, scene)
}

/// Turning off Show Screen revokes outbound digest permission for private overlays too (not only
/// `Surface::Screen` scenes) across `ChatGPT` and Key — on suspended builds, open grants, and fresh
/// next-turn builds — while keeping those overlays visible locally with no retire or clear.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "checks suspended, granted and next-turn paths across ChatGPT and Key"
)]
fn show_screen_off_revokes_private_overlays_without_retiring_them() {
    use crate::connections::model::Lease as _;
    use crate::coordinator::SendKey;
    use kumochi_protocol::message::{ConnectionKind, RunId};

    for kind in [ConnectionKind::AnthropicKey, ConnectionKind::ChatGpt] {
        for phase in ["before_grant", "after_grant"] {
            let scratch = Scratch::new(&format!("overlay-screen-off-{kind:?}-{phase}"));
            let (mut core, mut store, snapshot, recipient, scene) =
                seed_private_overlay_only(&scratch, kind);
            let next = RunId(R.0 + 1);
            let key = begin_typed_key_turn(&mut core, &mut store, next);
            let model = super::super::routes::choice(kind, "m");
            let Input::ContextBuilt(mut context) = super::super::routes::built_for(key, &model)
            else {
                unreachable!()
            };
            context.snapshot = Some(snapshot.clone());
            core.handle(Input::ContextBuilt(context)).unwrap();

            let off_effects = if phase == "before_grant" {
                let off = core.handle(Input::ShowScreenOff).unwrap();
                let ready = core
                    .handle(Input::SendReady {
                        key: SendKey::Build(key),
                        hold: None,
                    })
                    .unwrap();
                assert!(
                    ready.contains(&Effect::Rebuild(SendKey::Build(key))),
                    "{kind:?} {phase}: suspended build is refused"
                );
                let rebuilt_build = ready
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::Store(StoreOp::BuildContext(build)) => Some(build),
                        _ => None,
                    })
                    .expect("suspended build rebuilds");
                assert_eq!(
                    rebuilt_build.drawings.line_for(&recipient),
                    None,
                    "{kind:?} {phase}: rebuilt digest omits private overlay"
                );
                off
            } else {
                let lease = core
                    .handle(Input::SendReady {
                        key: SendKey::Build(key),
                        hold: None,
                    })
                    .unwrap()
                    .into_iter()
                    .find_map(|effect| match effect {
                        Effect::Grant(lease) => Some(lease),
                        _ => None,
                    })
                    .expect("overlay digest granted");
                let grant = core.picture_grant(lease).expect("digest grant");
                let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
                let send =
                    crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant.clone()));
                let off = core.handle(Input::ShowScreenOff).unwrap();
                assert!(!grant.is_open(), "{kind:?} {phase}: open grant closes");
                assert!(send.admit().is_none(), "{kind:?} {phase}: write refused");
                let settled = core.handle(Input::SendUnsent { lease }).unwrap();
                let rebuilt_build = settled
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::Store(StoreOp::BuildContext(build)) => Some(build),
                        _ => None,
                    })
                    .expect("unsent revoked build rebuilds");
                assert_eq!(
                    rebuilt_build.drawings.line_for(&recipient),
                    None,
                    "{kind:?} {phase}: rebuilt digest omits private overlay"
                );
                let Input::ContextBuilt(mut again) =
                    super::super::routes::built_for(rebuilt_build.key, &model)
                else {
                    unreachable!()
                };
                again.snapshot = Some(snapshot.clone());
                core.handle(Input::ContextBuilt(again)).unwrap();
                let plain = core
                    .handle(Input::SendReady {
                        key: SendKey::Build(rebuilt_build.key),
                        hold: None,
                    })
                    .unwrap()
                    .into_iter()
                    .find_map(|effect| match effect {
                        Effect::Grant(lease) => Some(lease),
                        _ => None,
                    })
                    .expect("clean rebuild granted");
                assert!(core.picture_grant(plain).is_none());
                core.handle(Input::SendSettled { lease: plain }).unwrap();

                // Clean screen-free retry draws an ordinary overlay: it must store as public
                // (`protected: false`) and reach later public context, while the earlier private
                // overlay remains `Author::Unknown`.
                let retry_overlay = r#"{"records":[{"begin":{"v":1,"scene":"retry_overlay","surface":"overlay","alt":"clean retry","others":"keep"}},{"step":{"id":"r1","put":[{"shape":{"id":"retry_box","kind":"rect","box":[0,0,10,10],"label":"clean retry"}}]}},{"end":{}}]}"#;
                let mut draw_fx = core
                    .handle(Input::Drawing(crate::drawing::Input::Call {
                        thread: T,
                        run: next,
                        call: id("clean-retry-draw"),
                        args: retry_overlay.into(),
                    }))
                    .unwrap();
                let mut saw_public_step = false;
                while let Some((serial, write)) = draw_fx.iter().find_map(|effect| match effect {
                    Effect::Store(StoreOp::Drawing { serial, write }) => {
                        Some((*serial, write.clone()))
                    }
                    _ => None,
                }) {
                    let reply = match write {
                        crate::drawing::Write::Lookup { .. } => crate::drawing::Reply::Lookup(None),
                        crate::drawing::Write::Step { protected, .. } => {
                            assert!(
                                !protected,
                                "{kind:?}: clean retry overlay must store with protected = false"
                            );
                            saw_public_step = true;
                            crate::drawing::Reply::Step(kumochi_protocol::message::SceneId(55))
                        }
                        crate::drawing::Write::Cut { .. } => crate::drawing::Reply::Cut,
                    };
                    draw_fx = core
                        .handle(Input::DrawingWritten {
                            serial,
                            reply: Ok(reply),
                        })
                        .unwrap();
                }
                assert!(saw_public_step);
                assert!(
                    !core.drawing.visible_to(T, &recipient),
                    "{kind:?}: original private overlay stays Author::Unknown"
                );
                assert!(
                    core.drawing
                        .digest(T)
                        .off_screen()
                        .contains("retry_overlay"),
                    "{kind:?}: clean retry overlay enters public context"
                );
                assert!(
                    !core
                        .drawing
                        .digest(T)
                        .off_screen()
                        .contains("digest_copy_private"),
                    "{kind:?}: original private overlay never enters public context"
                );
                off
            };

            assert!(
                !off_effects.iter().any(|effect| matches!(
                    effect,
                    Effect::RetireScenes { scenes, .. }
                        | Effect::Store(StoreOp::ClearScenes { scenes })
                        if scenes.contains(&scene)
                )),
                "{kind:?} {phase}: overlay stays visible locally"
            );
            assert!(
                !core.drawing.visible_to(T, &recipient),
                "{kind:?} {phase}: overlay leaves outbound visibility"
            );
            assert_eq!(
                core.drawing.digest(T).line_for(&recipient),
                None,
                "{kind:?} {phase}: overlay leaves outbound digest"
            );
            assert!(
                core.drawing
                    .digest(T)
                    .as_str()
                    .contains("digest_copy_private"),
                "{kind:?} {phase}: overlay remains in local scenes"
            );

            // Fresh next turn omits the downgraded overlay too.
            let third = RunId(next.0 + 1);
            core.handle(Input::Stop {
                run: next,
                revealed: Vec::new(),
            })
            .unwrap();
            core.handle(Input::RunStored { run: next }).unwrap();
            core.handle(Input::Turn {
                turn: 9,
                thread: Some(T),
                text: "fresh next turn".into(),
            })
            .unwrap();
            let fresh_effects = core.handle(Input::TurnStored { run: third }).unwrap();
            let fresh_build = fresh_effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Store(StoreOp::BuildContext(build)) => Some(build),
                    _ => None,
                })
                .expect("fresh next turn build");
            assert_eq!(
                fresh_build.drawings.line_for(&recipient),
                None,
                "{kind:?} {phase}: fresh turn omits downgraded overlay"
            );
            if phase == "after_grant" {
                assert!(
                    fresh_build.drawings.off_screen().contains("retry_overlay"),
                    "{kind:?}: fresh turn receives clean retry's public overlay"
                );
                assert!(
                    !fresh_build
                        .drawings
                        .off_screen()
                        .contains("digest_copy_private"),
                    "{kind:?}: fresh turn never receives original private overlay"
                );
            }
        }
    }
}

/// Drives a typed Key run through round 1 carrying `FIRST_KEY_OVERLAY` (scene A), draws private
/// `LATER_KEY_OVERLAY` (scene B) in round 1, and checks every round-2 continuation invariant:
/// unrelated new scene B is permitted while scene A is intact; failing, replacing or evicting
/// scene A before round 2 (even while scene B survives) fails round 2; and dropping and restoring
/// the image fact between rounds on a typed run stays latched and fails round 2.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "covers five Key continuation scenarios against round-1 carried prefix"
)]
fn a_key_continuation_validates_round_one_scenes_and_latched_authority() {
    use crate::coordinator::SendKey;
    use crate::drawing::{Input as DrawInput, Reply, Write};
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, RunId, SceneId};

    let cases = [
        ("intact_with_new_scene", true),
        ("scene_a_failed", false),
        ("scene_a_edited", false),
        ("scene_a_replaced", false),
        ("scene_a_evicted", false),
        ("fact_drop_and_return", false),
    ];

    for (case, should_grant) in cases {
        let scratch = Scratch::new(&format!("key-cont-prefix-{case}"));
        let (mut core, mut store, snapshot, recipient, scene_a) =
            seed_private_overlay_only(&scratch, AnthropicKey);
        let next = RunId(R.0 + 1);
        let key = begin_typed_key_turn(&mut core, &mut store, next);
        let first = built_key(
            &store,
            &snapshot,
            next,
            Some(&recipient),
            &[],
            core.drawing.digest(T),
        );
        admit_key_build(&mut core, key, &snapshot, first);
        let call = key_call(
            &mut core,
            &mut store,
            (next, "round1-overlay-b"),
            LATER_KEY_OVERLAY,
        );
        assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
        let effects = core
            .handle(Input::CallCommitted {
                key: call.key.clone(),
                outcome: Committed::Done,
                then: None,
            })
            .unwrap();
        let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
            panic!("{case}: expected round items: {effects:?}");
        };
        store
            .append_key_round_events(
                (T, next),
                snapshot.connection,
                &[],
                &round.key_calls,
                &round.calls,
                round.pictured_key_model.as_ref(),
            )
            .unwrap();

        match case {
            "intact_with_new_scene" => {}
            "scene_a_failed" => {
                core.handle(Input::Drawing(DrawInput::SceneFailed { scene: scene_a }))
                    .unwrap();
                assert!(
                    core.drawing.visible_to(T, &recipient),
                    "private scene B still survives"
                );
            }
            "scene_a_edited" => {
                core.drawing.author(next, Some(&recipient));
                let edit = r#"{"records":[{"begin":{"v":1,"scene":"digest_copy_private","surface":"overlay","alt":"key_title_private"}},{"step":{"id":"d2","remove":["key_object_private"],"put":[{"shape":{"id":"key_object_edited","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;
                let mut draw_fx = core
                    .handle(Input::Drawing(DrawInput::Call {
                        thread: T,
                        run: next,
                        call: id("edit-a"),
                        args: edit.into(),
                    }))
                    .unwrap();
                while let Some((serial, write)) = draw_fx.iter().find_map(|effect| match effect {
                    Effect::Store(StoreOp::Drawing { serial, write }) => {
                        Some((*serial, write.clone()))
                    }
                    _ => None,
                }) {
                    let reply = match write {
                        Write::Lookup { .. } => Reply::Lookup(Some(scene_a)),
                        Write::Step { .. } => Reply::Step(scene_a),
                        Write::Cut { .. } => Reply::Cut,
                    };
                    draw_fx = core
                        .handle(Input::DrawingWritten {
                            serial,
                            reply: Ok(reply),
                        })
                        .unwrap();
                }
                assert!(
                    core.drawing.visible_to(T, &recipient),
                    "edited scene A and scene B stay visible"
                );
            }
            "scene_a_replaced" => {
                core.drawing.author(next, Some(&recipient));
                let replace = r#"{"records":[{"begin":{"v":1,"scene":"replace_scene","surface":"overlay","alt":"r","others":"clear"}},{"step":{"id":"r1","put":[{"shape":{"id":"rb","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;
                let mut draw_fx = core
                    .handle(Input::Drawing(DrawInput::Call {
                        thread: T,
                        run: next,
                        call: id("replace-a"),
                        args: replace.into(),
                    }))
                    .unwrap();
                let mut next_id = 80u64;
                while let Some((serial, write)) = draw_fx.iter().find_map(|effect| match effect {
                    Effect::Store(StoreOp::Drawing { serial, write }) => {
                        Some((*serial, write.clone()))
                    }
                    _ => None,
                }) {
                    let reply = match write {
                        Write::Lookup { .. } => Reply::Lookup(None),
                        Write::Step { .. } => {
                            next_id += 1;
                            Reply::Step(SceneId(next_id))
                        }
                        Write::Cut { .. } => Reply::Cut,
                    };
                    draw_fx = core
                        .handle(Input::DrawingWritten {
                            serial,
                            reply: Ok(reply),
                        })
                        .unwrap();
                }
                assert!(
                    core.drawing.visible_to(T, &recipient),
                    "replacement private scene stays visible"
                );
            }
            "scene_a_evicted" => {
                // MAX_VISIBLE_SCENES is 4: we already hold scene A and scene B; three more keep
                // scenes evict scene A (oldest) while private scene B survives.
                core.drawing.author(next, Some(&recipient));
                let mut next_id = 90u64;
                for extra in 0..3 {
                    let args = format!(
                        r#"{{"records":[{{"begin":{{"v":1,"scene":"evict_{extra}","surface":"overlay","alt":"e","others":"keep"}}}},{{"step":{{"id":"e{extra}","put":[{{"shape":{{"id":"eb{extra}","kind":"rect","box":[0,0,10,10]}}}}]}}}},{{"end":{{}}}}]}}"#
                    );
                    let mut draw_fx = core
                        .handle(Input::Drawing(DrawInput::Call {
                            thread: T,
                            run: next,
                            call: id(&format!("evict-{extra}")),
                            args: args.into(),
                        }))
                        .unwrap();
                    while let Some((serial, write)) = draw_fx.iter().find_map(|effect| match effect
                    {
                        Effect::Store(StoreOp::Drawing { serial, write }) => {
                            Some((*serial, write.clone()))
                        }
                        _ => None,
                    }) {
                        let reply = match write {
                            Write::Lookup { .. } => Reply::Lookup(None),
                            Write::Step { .. } => {
                                next_id += 1;
                                Reply::Step(SceneId(next_id))
                            }
                            Write::Cut { .. } => Reply::Cut,
                        };
                        draw_fx = core
                            .handle(Input::DrawingWritten {
                                serial,
                                reply: Ok(reply),
                            })
                            .unwrap();
                    }
                }
                assert!(
                    core.drawing.visible_to(T, &recipient),
                    "private scene B still survives after evicting A"
                );
            }
            "fact_drop_and_return" => {
                core.handle(Input::Engines(
                    AnthropicKey,
                    super::super::routes::pictured(5, &[]),
                ))
                .unwrap();
                core.handle(Input::Engines(
                    AnthropicKey,
                    super::super::routes::pictured(5, &["m"]),
                ))
                .unwrap();
            }
            _ => unreachable!(),
        }

        let effects = core
            .handle(Input::RoundStored {
                run: next,
                round: 1,
            })
            .unwrap();
        let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
            panic!("{case}: expected round 2 build: {effects:?}");
        };
        let second = built_key(
            &store,
            &snapshot,
            next,
            Some(&recipient),
            &build.key_results,
            core.drawing.digest(T),
        );
        let model = super::super::routes::choice(AnthropicKey, "m");
        let Input::ContextBuilt(mut context) = super::super::routes::built_for(build.key, &model)
        else {
            unreachable!()
        };
        context.snapshot = None;
        context.result.as_mut().unwrap().request = second;
        core.handle(Input::ContextBuilt(context)).unwrap();
        let ready = core
            .handle(Input::SendReady {
                key: SendKey::Build(build.key),
                hold: None,
            })
            .unwrap();
        if should_grant {
            let lease = ready
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::Grant(lease) => Some(lease),
                    _ => None,
                })
                .expect("unrelated new scene B leaves round 2 granted");
            assert!(core.picture_grant(lease).is_some());
            core.handle(Input::SendSettled { lease }).unwrap();
        } else {
            assert!(
                ready.contains(&Effect::Rebuild(SendKey::Build(build.key))),
                "{case}: round 2 must be refused"
            );
            assert!(
                core.current.is_none(),
                "{case}: revoked Key continuation fails instead of rebuilding"
            );
        }
    }
}

/// A validated `StepFailed` arriving while another drawing write is pending in the store queues
/// its cut behind that write, yet immediately removes the scene from outbound digests and closes
/// any open grant before the pending store write settles.
#[test]
fn a_pending_step_failed_cut_revokes_digest_before_store_write_settles() {
    use crate::connections::model::Lease as _;
    use crate::drawing::Input as DrawInput;
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, RunId, SceneFailure};
    use kumochi_scene::StopCut;
    use kumochi_scene::text::Bounded;

    let scratch = Scratch::new("pending-step-failed-revokes-digest");
    let (mut core, mut store, snapshot, recipient, scene_a) =
        seed_private_overlay_only(&scratch, AnthropicKey);
    let next = RunId(R.0 + 1);
    let key = begin_typed_key_turn(&mut core, &mut store, next);
    let first = built_key(
        &store,
        &snapshot,
        next,
        Some(&recipient),
        &[],
        core.drawing.digest(T),
    );
    let lease = grant_key_build(&mut core, key, &snapshot, first);
    let grant = core.picture_grant(lease).expect("digest grant");
    let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
    let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant.clone()));

    // Start a drawing store write and leave it pending so the upcoming StepFailed queues behind it.
    let pending_fx = core
        .handle(Input::Drawing(DrawInput::Call {
            thread: T,
            run: next,
            call: id("pending-write"),
            args: LATER_KEY_OVERLAY.into(),
        }))
        .unwrap();
    assert!(
        pending_fx
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. }))),
        "a store write is now pending"
    );
    assert!(
        grant.is_open(),
        "an unrelated pending write keeps the grant"
    );

    // Validated StepFailed on scene_a queues Job::Cut behind the pending write.
    core.handle(Input::Drawing(DrawInput::StepFailed {
        run: R,
        scene: scene_a,
        index: 0,
        reason: SceneFailure::PaintWork,
        cut: StopCut {
            activated: 0,
            revealed: Bounded::default(),
        },
    }))
    .unwrap();
    assert!(
        !core.drawing.visible_to(T, &recipient),
        "pending cut drops scene_a from outbound visibility immediately"
    );
    assert_eq!(
        core.drawing.digest(T).line_for(&recipient),
        None,
        "pending cut drops scene_a from outbound digest immediately"
    );
    assert!(
        !grant.is_open(),
        "open grant closes before the store write settles"
    );
    assert!(send.admit().is_none(), "write refused before store settle");

    let settled = core.handle(Input::SendUnsent { lease }).unwrap();
    let rebuilt = settled
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build),
            _ => None,
        })
        .expect("round 1 rebuilds cleanly");
    assert_eq!(rebuilt.drawings.line_for(&recipient), None);
}

/// Ending a run (such as on Stop) closes that run's digest-carrying grants immediately, so a
/// retained `SendLease` refuses any new write admission.
#[test]
fn stopping_a_run_closes_its_digest_grant() {
    use crate::connections::model::Lease as _;
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, RunId};

    let scratch = Scratch::new("stop-closes-digest-grant");
    let (mut core, mut store, snapshot, recipient, _) =
        seed_private_overlay_only(&scratch, AnthropicKey);
    let next = RunId(R.0 + 1);
    let key = begin_typed_key_turn(&mut core, &mut store, next);
    let first = built_key(
        &store,
        &snapshot,
        next,
        Some(&recipient),
        &[],
        core.drawing.digest(T),
    );
    let lease = grant_key_build(&mut core, key, &snapshot, first);
    let grant = core.picture_grant(lease).expect("digest grant");
    let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
    let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant.clone()));
    assert!(grant.is_open());

    core.handle(Input::Stop {
        run: next,
        revealed: Vec::new(),
    })
    .unwrap();
    assert!(
        !grant.is_open(),
        "Stop closes the ending run's digest grant"
    );
    assert!(
        send.admit().is_none(),
        "retained digest lease refuses writes after Stop"
    );
}

/// When a mixed picture+digest send is revoked solely because a drawing changed while the model
/// still accepts pictures, settling unsent rebuilds without the stale digest and keeps the valid
/// picture (no `ModelCantSee` notice or store row).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "seeds a private overlay then runs a mixed hold build through unsent rebuild"
)]
fn a_mixed_picture_and_digest_send_keeps_its_valid_picture_when_only_a_drawing_changes() {
    use super::super::routes::{built_for, choice, jpeg};
    use crate::connections::model::Lease as _;
    use crate::context::{InputItem, Screen};
    use crate::coordinator::{HoldKey, SendKey};
    use kumochi_protocol::message::{
        ConnectionKind::AnthropicKey, ControlToShell, GestureId, HeldScreen, RunId,
    };
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let scratch = Scratch::new("mixed-picture-digest-keeps-picture");
    let (mut core, _store, snapshot, _recipient, scene_a) =
        seed_private_overlay_only(&scratch, AnthropicKey);
    let hold_key = HoldKey {
        epoch: 0,
        gesture: GestureId(2),
    };
    let Ok(StoreOp::ReadHold(read)) = core.prepare_hold(
        hold_key,
        Some(T),
        0,
        snapshot.clone(),
        Arc::new(AtomicBool::new(false)),
    ) else {
        panic!("hold prepared");
    };
    let mut hold = read.snapshot;
    hold.generation = Some(crate::store::Generation(0));
    core.submit_hold(hold, "mixed turn".into(), Some(Screen::Image(jpeg())))
        .unwrap();
    let next = RunId(R.0 + 1);
    let stored = core.handle(Input::TurnStored { run: next }).unwrap();
    let key = stored
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("mixed build");
    let model = choice(AnthropicKey, "m");
    let Input::ContextBuilt(mut context) = built_for(key, &model) else {
        unreachable!()
    };
    context.snapshot = Some(snapshot.clone());
    core.handle(Input::ContextBuilt(context)).unwrap();
    let lease = core
        .handle(Input::SendReady {
            key: SendKey::Build(key),
            hold: None,
        })
        .unwrap()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(lease),
            _ => None,
        })
        .expect("mixed send granted");
    let grant = core.picture_grant(lease).expect("mixed grant");
    let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
    let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant.clone()));

    // Clear only the drawing; the picture permission stays valid.
    core.handle(Input::Clear {
        scenes: vec![scene_a],
    })
    .unwrap();
    assert!(!grant.is_open());
    assert!(send.admit().is_none());

    let settled = core.handle(Input::SendUnsent { lease }).unwrap();
    assert!(
        !settled.iter().any(|effect| matches!(
            effect,
            Effect::Control(ControlToShell::HoldScreen {
                outcome: HeldScreen::NotIncluded(_),
                ..
            }) | Effect::Store(StoreOp::CantSeeScreen { .. })
        )),
        "valid picture must not be stripped as ModelCantSee: {settled:?}"
    );
    let again = settled
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("mixed send rebuilds");
    assert_ne!(again, key);
    let Input::ContextBuilt(mut rebuilt) = built_for(again, &model) else {
        unreachable!()
    };
    rebuilt.snapshot = Some(snapshot);
    let started = core.handle(Input::ContextBuilt(rebuilt)).unwrap();
    let request = started
        .iter()
        .find_map(|effect| match effect {
            Effect::StartEngine { request } => Some(request),
            _ => None,
        })
        .expect("rebuilt mixed send starts");
    assert!(
        request
            .request
            .input
            .iter()
            .any(|item| matches!(item, InputItem::Image(_))),
        "rebuilt request still carries the valid picture"
    );
}

/// A standalone Key inline compaction rewrite sends a fresh body without round 1's held prefix,
/// so it is neither failed by nor gated under `key_digest`, while preserving `key_digest` for the
/// true round-2 continuation that follows it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "drives round 1, inline rewrite, and round-2 continuation with and without revocation"
)]
fn a_standalone_key_inline_rewrite_succeeds_and_keeps_continuation_guard() {
    use crate::connections::model::Lease as _;
    use crate::context::{Built, ModelRequest};
    use crate::coordinator::{CompactBuilt, ContextBuilt, JobAsk, SendKey, Unbuilt};
    use crate::engines::anthropic::live::Motion;
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, EventId, RunId};

    for revoke_before_inline in [false, true] {
        let scratch = Scratch::new(&format!(
            "key-inline-preserves-guard-{revoke_before_inline}"
        ));
        let (mut core, mut store, snapshot, recipient, scene_a) =
            seed_private_overlay_only(&scratch, AnthropicKey);
        let next = RunId(R.0 + 1);
        let key = begin_typed_key_turn(&mut core, &mut store, next);
        let first = built_key(
            &store,
            &snapshot,
            next,
            Some(&recipient),
            &[],
            core.drawing.digest(T),
        );
        admit_key_build(&mut core, key, &snapshot, first);
        let call = key_call(
            &mut core,
            &mut store,
            (next, "inline-round1-call"),
            LATER_KEY_OVERLAY,
        );
        assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
        let effects = core
            .handle(Input::CallCommitted {
                key: call.key.clone(),
                outcome: Committed::Done,
                then: None,
            })
            .unwrap();
        let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
            panic!("round 1 items: {effects:?}");
        };
        store
            .append_key_round_events(
                (T, next),
                snapshot.connection,
                &[],
                &round.key_calls,
                &round.calls,
                round.pictured_key_model.as_ref(),
            )
            .unwrap();
        let effects = core
            .handle(Input::RoundStored {
                run: next,
                round: 1,
            })
            .unwrap();
        let [Effect::Store(StoreOp::BuildContext(round2_build))] = effects.as_slice() else {
            panic!("round 2 build: {effects:?}");
        };
        // Trigger inline compaction on round 2.
        let compact_fx = core
            .handle(Input::ContextBuilt(ContextBuilt {
                key: round2_build.key,
                snapshot: None,
                result: Err(Unbuilt::TooLarge { room: Some(500) }),
            }))
            .unwrap();
        let compact = compact_fx
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::CompactContext(compact)) => Some(compact.clone()),
                _ => None,
            })
            .expect("inline compaction started");
        if revoke_before_inline {
            core.handle(Input::Clear {
                scenes: vec![scene_a],
            })
            .unwrap();
        }
        let model = super::super::routes::choice(AnthropicKey, "m");
        let started_inline = core
            .handle(Input::CompactBuilt(CompactBuilt {
                key: compact.key,
                result: Ok(Some((
                    JobAsk {
                        generation: crate::store::Generation(1),
                        built: Built {
                            recovered_context: None,
                            request: ModelRequest {
                                choice: Some(model.clone()),
                                instructions: String::new(),
                                input: Vec::new(),
                                tools: Vec::new(),
                            },
                            sources: vec![T],
                        },
                        covers: Some((EventId(1), EventId(2))),
                    },
                    100,
                ))),
            }))
            .unwrap();
        let inline_req = started_inline
            .iter()
            .find_map(|effect| match effect {
                Effect::StartEngine { request } => Some(request.clone()),
                _ => None,
            })
            .expect("inline rewrite started");
        assert_eq!(
            core.key_motion(&inline_req),
            Some((snapshot.connection, Motion::Standalone))
        );
        let inline_grant = core
            .handle(Input::SendReady {
                key: SendKey::Build(compact.key),
                hold: None,
            })
            .unwrap();
        let inline_lease = inline_grant
            .iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(*lease),
                _ => None,
            })
            .expect("standalone inline rewrite succeeds regardless of key_digest");
        assert!(
            core.picture_grant(inline_lease).is_none(),
            "standalone inline rewrite carries no digest grant"
        );
        core.handle(Input::SendSettled {
            lease: inline_lease,
        })
        .unwrap();
        core.handle(Input::Engine {
            key: compact.key,
            event: TurnEvent::TextDelta("summary".into()),
        })
        .unwrap();
        core.handle(Input::Engine {
            key: compact.key,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .unwrap();
        let after_inline = core
            .handle(Input::SummaryStored {
                owner: SendKey::Build(compact.key),
                applied: true,
            })
            .unwrap();
        let cont_build = after_inline
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.clone()),
                _ => None,
            })
            .expect("normal round 2 continuation build after inline rewrite");
        let second = built_key(
            &store,
            &snapshot,
            next,
            Some(&recipient),
            &cont_build.key_results,
            core.drawing.digest(T),
        );
        let Input::ContextBuilt(mut context) =
            super::super::routes::built_for(cont_build.key, &model)
        else {
            unreachable!()
        };
        context.snapshot = None;
        context.result.as_mut().unwrap().request = second;
        let cont_start = core.handle(Input::ContextBuilt(context)).unwrap();
        let cont_req = cont_start
            .iter()
            .find_map(|effect| match effect {
                Effect::StartEngine { request } => Some(request.clone()),
                _ => None,
            })
            .expect("continuation started");
        assert_eq!(
            core.key_motion(&cont_req),
            Some((snapshot.connection, Motion::Continue))
        );
        let ready = core
            .handle(Input::SendReady {
                key: SendKey::Build(cont_build.key),
                hold: None,
            })
            .unwrap();
        if revoke_before_inline {
            assert!(ready.contains(&Effect::Rebuild(SendKey::Build(cont_build.key))));
            assert!(
                core.current.is_none(),
                "retained key_digest guard fails the true continuation"
            );
        } else {
            let cont_lease = ready
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::Grant(lease) => Some(lease),
                    _ => None,
                })
                .expect("true continuation granted");
            let grant = core
                .picture_grant(cont_lease)
                .expect("retained key_digest guard gates the true continuation");
            let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
            let send = crate::engines::SendLease::new(cont_lease, inputs)
                .with_picture(Some(grant.clone()));
            core.handle(Input::Clear {
                scenes: vec![scene_a],
            })
            .unwrap();
            assert!(!grant.is_open());
            assert!(send.admit().is_none());
        }
    }
}

/// `ShowScreenOff` carries through both an in-flight private overlay store write (acknowledged
/// after `ShowScreenOff`) and a late `draw` call from an already-sent protected run that had not
/// drawn before `ShowScreenOff`: both keep `protected: true` in storage and install as
/// `Author::Unknown` (locally visible, never public, never in outbound digests).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "checks both pending overlay ack and late draw from an already-sent protected run"
)]
fn show_screen_off_carries_through_pending_overlay_write_and_late_protected_draw() {
    use super::super::routes::{built_for, choice, held, jpeg, key, pictured};
    use crate::context::{PictureRecipient, Route, Screen};
    use crate::coordinator::{PictureRoutes, SendKey};
    use crate::drawing::{Input as DrawInput, Reply, Write};
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, SceneId};

    let model = choice(AnthropicKey, "m");
    let recipient = PictureRecipient {
        route: Route {
            kind: AnthropicKey,
            tag: crate::context::ConnectionTag(5),
        },
        model: model.model.clone(),
    };

    for late_after_off in [false, true] {
        let (mut core, hold) = held(
            vec![Input::Engines(AnthropicKey, pictured(5, &["m"]))],
            model.clone(),
            Some(T),
        );
        let snapshot = hold.defaults.clone();
        core.set_pictures(PictureRoutes::new(true, false));
        core.submit_hold(hold, "protected run".into(), Some(Screen::Image(jpeg())))
            .unwrap();
        core.handle(Input::TurnStored { run: R }).unwrap();
        let Input::ContextBuilt(mut context) = built_for(key(), &model) else {
            unreachable!()
        };
        context.result.as_mut().unwrap().request.tools = crate::context::tools(snapshot.origin);
        core.handle(Input::ContextBuilt(context)).unwrap();
        let lease = core
            .handle(Input::SendReady {
                key: SendKey::Build(key()),
                hold: None,
            })
            .unwrap()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(lease),
                _ => None,
            })
            .expect("protected run granted");
        core.handle(Input::SendSettled { lease }).unwrap();

        if late_after_off {
            // Turn off Show Screen before the protected run draws anything at all; a subsequent
            // author() call (as provider draw dispatch performs) must never re-promote it.
            core.handle(Input::ShowScreenOff).unwrap();
            core.drawing.author(R, Some(&recipient));
            let mut effects = core
                .handle(Input::Drawing(DrawInput::Call {
                    thread: T,
                    run: R,
                    call: id("late-draw"),
                    args: LATER_KEY_OVERLAY.into(),
                }))
                .unwrap();
            let mut saw_protected_step = false;
            while let Some((serial, write)) = effects.iter().find_map(|effect| match effect {
                Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
                _ => None,
            }) {
                let reply = match write {
                    Write::Lookup { .. } => Reply::Lookup(None),
                    Write::Step { protected, .. } => {
                        assert!(protected, "late draw keeps runs.protected = true");
                        saw_protected_step = true;
                        Reply::Step(SceneId(61))
                    }
                    Write::Cut { .. } => Reply::Cut,
                };
                effects = core
                    .handle(Input::DrawingWritten {
                        serial,
                        reply: Ok(reply),
                    })
                    .unwrap();
            }
            assert!(saw_protected_step);
            assert!(
                core.drawing.digest(T).as_str().contains("later_scene"),
                "late overlay stays visible locally"
            );
            assert!(
                !core.drawing.digest(T).off_screen().contains("later_scene"),
                "late overlay never downgrades to public"
            );
            assert!(
                !core.drawing.visible_to(T, &recipient),
                "late overlay never enters outbound visibility"
            );
            assert_eq!(core.drawing.digest(T).line_for(&recipient), None);
        } else {
            // Start an overlay draw, advance to the pending Write::Step, then turn Show Screen off
            // before acknowledging DrawingWritten.
            core.drawing.author(R, Some(&recipient));
            let mut effects = core
                .handle(Input::Drawing(DrawInput::Call {
                    thread: T,
                    run: R,
                    call: id("pending-overlay"),
                    args: COPIED_KEY_OVERLAY.into(),
                }))
                .unwrap();
            let mut pending_step = None;
            while let Some((serial, write)) = effects.iter().find_map(|effect| match effect {
                Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
                _ => None,
            }) {
                match write {
                    Write::Lookup { .. } => {
                        effects = core
                            .handle(Input::DrawingWritten {
                                serial,
                                reply: Ok(Reply::Lookup(None)),
                            })
                            .unwrap();
                    }
                    Write::Step { protected, .. } => {
                        assert!(protected, "pending overlay write has protected = true");
                        pending_step = Some(serial);
                        break;
                    }
                    Write::Cut { .. } => unreachable!(),
                }
            }
            let step_serial = pending_step.expect("step write is pending");
            core.handle(Input::ShowScreenOff).unwrap();
            core.handle(Input::DrawingWritten {
                serial: step_serial,
                reply: Ok(Reply::Step(SceneId(60))),
            })
            .unwrap();
            assert!(
                core.drawing
                    .digest(T)
                    .as_str()
                    .contains("digest_copy_private"),
                "acknowledged overlay stays visible locally"
            );
            assert!(
                !core
                    .drawing
                    .digest(T)
                    .off_screen()
                    .contains("digest_copy_private"),
                "acknowledged overlay never downgrades to public"
            );
            assert!(
                !core.drawing.visible_to(T, &recipient),
                "acknowledged overlay never enters outbound visibility"
            );
            assert_eq!(core.drawing.digest(T).line_for(&recipient), None);
        }
    }
}

/// A `Job::Unseen` queued by `StopScenes` behind a pending drawing store write immediately
/// excludes unnamed, unactivated private scenes of that run from outbound digests and closes any
/// open grant before the pending store write settles.
#[test]
fn a_queued_unseen_cut_revokes_private_digest_before_store_write_settles() {
    use crate::connections::model::Lease as _;
    use crate::drawing::Input as DrawInput;
    use kumochi_protocol::message::{ConnectionKind::AnthropicKey, RunId, SceneCuts};

    let scratch = Scratch::new("queued-unseen-revokes-digest");
    let (mut core, mut store, snapshot, recipient, _scene_a) =
        seed_private_overlay_only(&scratch, AnthropicKey);
    let next = RunId(R.0 + 1);
    let key = begin_typed_key_turn(&mut core, &mut store, next);
    let first = built_key(
        &store,
        &snapshot,
        next,
        Some(&recipient),
        &[],
        core.drawing.digest(T),
    );
    let lease = grant_key_build(&mut core, key, &snapshot, first);
    let grant = core.picture_grant(lease).expect("digest grant");
    let (inputs, _settled) = tokio::sync::mpsc::unbounded_channel();
    let send = crate::engines::SendLease::new(lease, inputs).with_picture(Some(grant.clone()));

    // Leave a drawing store write pending so StopScenes queues Job::Unseen behind it.
    let pending_fx = core
        .handle(Input::Drawing(DrawInput::Call {
            thread: T,
            run: next,
            call: id("pending-for-unseen"),
            args: LATER_KEY_OVERLAY.into(),
        }))
        .unwrap();
    assert!(
        pending_fx
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. }))),
        "a store write is now pending"
    );
    assert!(grant.is_open());

    // StopScenes for R with empty cuts queues Job::Unseen targeting R's unactivated scene_a.
    core.handle(Input::StopScenes {
        run: R,
        cuts: SceneCuts::default(),
    })
    .unwrap();
    assert!(
        !core.drawing.visible_to(T, &recipient),
        "queued Job::Unseen drops unactivated scene from outbound visibility immediately"
    );
    assert_eq!(
        core.drawing.digest(T).line_for(&recipient),
        None,
        "queued Job::Unseen drops unactivated scene from outbound digest immediately"
    );
    assert!(
        !grant.is_open(),
        "open grant closes before the store write settles"
    );
    assert!(send.admit().is_none());

    let settled = core.handle(Input::SendUnsent { lease }).unwrap();
    let rebuilt = settled
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build),
            _ => None,
        })
        .expect("round 1 rebuilds cleanly");
    assert_eq!(rebuilt.drawings.line_for(&recipient), None);
}

/// Admits build `key` on the pictured Key choice, sending `request` with the draw tool.
fn admit_key_build(
    core: &mut Coordinator,
    key: BuildKey,
    snapshot: &RunSnapshot,
    request: crate::context::ModelRequest,
) {
    let lease = grant_key_build(core, key, snapshot, request);
    core.handle(Input::SendSettled { lease }).unwrap();
}

/// Grants build `key` as [`admit_key_build`] does and returns its lease, unsettled.
fn grant_key_build(
    core: &mut Coordinator,
    key: BuildKey,
    snapshot: &RunSnapshot,
    mut request: crate::context::ModelRequest,
) -> crate::coordinator::LeaseId {
    use crate::coordinator::SendKey;
    use kumochi_protocol::message::ConnectionKind;

    let model = super::super::routes::choice(ConnectionKind::AnthropicKey, "m");
    let Input::ContextBuilt(mut built) = super::super::routes::built_for(key, &model) else {
        unreachable!()
    };
    built.snapshot = (key.round == 1).then(|| snapshot.clone());
    request.tools = crate::context::tools(snapshot.origin);
    built.result.as_mut().unwrap().request = request;
    core.handle(Input::ContextBuilt(built)).unwrap();
    core.handle(Input::SendReady {
        key: SendKey::Build(key),
        hold: None,
    })
    .unwrap()
    .into_iter()
    .find_map(|effect| match effect {
        Effect::Grant(lease) => Some(lease),
        _ => None,
    })
    .expect("the Key round entered write admission")
}

const FIRST_KEY_OVERLAY: &str = r#"{"records":[{"begin":{"v":1,"scene":"first_scene","surface":"overlay","alt":"first_alt","others":"keep"}},{"step":{"id":"d1","put":[{"shape":{"id":"first_object","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;
const LATER_KEY_OVERLAY: &str = r#"{"records":[{"begin":{"v":1,"scene":"later_scene","surface":"overlay","alt":"later_alt","others":"keep"}},{"step":{"id":"d2","put":[{"shape":{"id":"later_object","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;

/// A Key continuation resends its first request, so a digest only a rebuilt later round could
/// carry never went out: the run stays unmarked and each of its calls is answered as written.
#[test]
fn a_digest_eligible_only_in_a_later_key_round_leaves_the_run_unmarked() {
    use crate::coordinator::PictureRoutes;
    use kumochi_protocol::message::RunId;

    let scratch = Scratch::new("provider-draw-key-digest-later");
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    end_pictured_key_run(&mut core, &mut store, &snapshot);
    core.set_pictures(PictureRoutes::new(false, false));
    let next = RunId(R.0 + 1);
    let key = begin_typed_key_turn(&mut core, &mut store, next);
    let first = built_key(&store, &snapshot, next, None, &[], core.drawing.digest(T));
    let wire = key_wire(&first, snapshot.connection).to_string();
    assert!(wire.contains("typed follow-up") && !wire.contains("key_scene_private"));
    admit_key_build(&mut core, key, &snapshot, first);
    let call = key_call(
        &mut core,
        &mut store,
        (next, "first-draw"),
        FIRST_KEY_OVERLAY,
    );
    assert_eq!(call.output, ToolOutput::Text("drawn: [d1]".into()));
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    let effects = core
        .handle(Input::CallCommitted {
            key: call.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a completed round: {effects:?}");
    };
    assert_eq!(round.pictured_key_model, None);
    store
        .append_key_round_events(
            (T, next),
            snapshot.connection,
            &[],
            &round.key_calls,
            &round.calls,
            None,
        )
        .unwrap();

    core.set_pictures(PictureRoutes::new(true, false));
    let effects = core
        .handle(Input::RoundStored {
            run: next,
            round: 1,
        })
        .unwrap();
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("a continuation build: {effects:?}");
    };
    let rebuilt = built_key(
        &store,
        &snapshot,
        next,
        Some(&key_recipient(&snapshot)),
        &build.key_results,
        core.drawing.digest(T),
    );
    assert!(
        key_wire(&rebuilt, snapshot.connection)
            .to_string()
            .contains("key_scene_private")
    );
    admit_key_build(&mut core, build.key, &snapshot, rebuilt);
    let call = key_call(
        &mut core,
        &mut store,
        (next, "later-draw"),
        LATER_KEY_OVERLAY,
    );
    assert_eq!(call.output, ToolOutput::Text("drawn: [d2]".into()));
    assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
    let effects = core
        .handle(Input::CallCommitted {
            key: call.key.clone(),
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a completed round: {effects:?}");
    };
    assert_eq!(round.pictured_key_model, None);
}

#[test]
fn pictured_key_scene_metadata_reaches_only_its_picture_recipient() {
    assert_drawn_metadata_reaches_only_its_recipient(
        "provider-draw-key-picture-digest",
        KEY_MARKS,
        &[
            "key_scene_private",
            "key_title_private",
            "key_alt_private",
            "key_object_private",
            "1280.0,720.0",
        ],
    );
}

const KEY_OVERLAY: &str = r#"{"records":[{"begin":{"v":1,"scene":"key_overlay_private","surface":"overlay","title":"key_overlay_title_private","alt":"key_overlay_alt_private"}},{"step":{"id":"o1","put":[{"shape":{"id":"key_overlay_object_private","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;

/// An overlay drawn by a pictured turn is its picture recipient's, though it isn't on the screen.
#[test]
fn a_pictured_key_overlay_reaches_only_its_picture_recipient() {
    assert_drawn_metadata_reaches_only_its_recipient(
        "provider-draw-key-picture-overlay",
        KEY_OVERLAY,
        &[
            "key_overlay_private",
            "key_overlay_title_private",
            "key_overlay_alt_private",
            "key_overlay_object_private",
        ],
    );
}

/// A pictured Key turn draws `marks`; each later request's whole wire carries every `private`
/// marker only for the exact recipient that saw the picture.
fn assert_drawn_metadata_reaches_only_its_recipient(scratch: &str, marks: &str, private: &[&str]) {
    use crate::store::RunEnd;

    let scratch = Scratch::new(scratch);
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    let commit = key_call(&mut core, &mut store, (R, "pictured-draw"), marks);
    assert_eq!(store.commit_call(&commit).unwrap(), Committed::Done);
    store.end_run(T, R, &[], RunEnd::Done).unwrap();
    assert_next_wires_keep_only_for_recipient(&mut store, &core, &snapshot, private);
}

/// The next turn's whole wire, for the picture's exact recipient and for each other route, model
/// and the text-only fallback, carries every `private` marker only for that exact recipient.
fn assert_next_wires_keep_only_for_recipient(
    store: &mut Store,
    core: &Coordinator,
    snapshot: &RunSnapshot,
    private: &[&str],
) {
    use kumochi_protocol::message::{ConnectionKind, RunId};

    let drawings = core.drawing.digest(T);
    assert_eq!(drawings.recipients().count(), 1);
    let run = RunId(R.0 + 1);
    store.begin_turn(T, None, run, "read the drawing").unwrap();
    for (connection, tag, slug, capability, keeps) in [
        (ConnectionKind::AnthropicKey, 5, "m", true, true),
        (ConnectionKind::AnthropicKey, 5, "m", false, false),
        (ConnectionKind::AnthropicKey, 5, "other", true, false),
        (ConnectionKind::AnthropicKey, 6, "m", true, false),
        (ConnectionKind::ChatGpt, 5, "m", true, false),
    ] {
        let mut receiving = snapshot.clone();
        receiving.connection = ConnectionTag(tag);
        receiving.choice = Some(super::super::routes::choice(connection, slug));
        let pictured = capability.then(|| key_recipient(&receiving));
        let request = built_key(
            store,
            &receiving,
            run,
            pictured.as_ref(),
            &[],
            drawings.clone(),
        );
        let wire = if connection == ConnectionKind::ChatGpt {
            crate::engines::responses::body(&request)
                .unwrap()
                .to_string()
        } else {
            key_wire(&request, receiving.connection).to_string()
        };
        for mark in private {
            assert_eq!(
                wire.contains(mark),
                keeps,
                "{connection:?} {tag} {slug} {mark}"
            );
        }
        assert!(wire.contains("read the drawing"));
        assert!(!wire.contains("186,598,450,78"));
        assert!(!wire.contains("input_image"));
    }
}

/// A protected overlay step whose write is acknowledged only after its run was stopped keeps
/// its recipient: the settlement uses the recipient read when the step was admitted, not the
/// run's ledger, which the stop removed.
#[test]
fn a_protected_step_acknowledged_after_its_run_ends_reaches_only_its_recipient() {
    use crate::drawing::Write;
    use crate::engines::ToolName;
    use crate::store::{Interruption, RunEnd};

    let scratch = Scratch::new("provider-draw-late-acknowledgment");
    let mut store = store(&scratch);
    let (mut core, snapshot) = pictured_key();
    let send = |core: &mut Coordinator, event| {
        let key = super::super::engine_key(core, R);
        core.handle(Input::Engine { key, event })
            .expect("current provider event")
    };
    let name = id("late-draw");
    let started = TurnEvent::ToolCallStarted {
        call: name.clone(),
        name: ToolName::Draw,
    };
    assert_eq!(send(&mut core, started), Vec::new());
    let mut effects = send(
        &mut core,
        TurnEvent::ToolArgsDelta {
            call: name.clone(),
            delta: KEY_OVERLAY.into(),
        },
    );
    effects.extend(send(
        &mut core,
        TurnEvent::ToolCallReady {
            call: name,
            args: KEY_OVERLAY.into(),
        },
    ));
    while let Some((_, write_op)) = pending(&effects) {
        if matches!(write_op, Write::Step { .. }) {
            break;
        }
        effects = write(&mut core, &mut store, &effects);
    }
    assert!(
        matches!(
            pending(&effects),
            Some((
                _,
                Write::Step {
                    protected: true,
                    ..
                }
            ))
        ),
        "the protected step's write is held: {effects:?}"
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .unwrap();
    drain(&mut core, &mut store, effects);
    store
        .end_run(T, R, &[], RunEnd::Interrupted(Interruption::Stop))
        .unwrap();
    assert_next_wires_keep_only_for_recipient(
        &mut store,
        &core,
        &snapshot,
        &[
            "key_overlay_private",
            "key_overlay_title_private",
            "key_overlay_alt_private",
            "key_overlay_object_private",
        ],
    );
}

#[test]
fn key_rounds_reusing_a_draw_id_draw_twice_and_a_same_round_retry_draws_once() {
    let scratch = Scratch::new("provider-draw-key-rounds");
    let mut store = store(&scratch);
    let mut core = key_offered();
    let first = String::from_utf8(args(None)).expect("JSON fixture");
    let first_drawn = ToolOutput::Text("drawn: [s1, s2]".into());
    let ended = key_draw_round(&mut core, &mut store, &first, &first_drawn);
    assert_eq!(round_calls(&ended), [(id("reused"), first_drawn)]);

    let effects = core
        .handle(Input::RoundStored { run: R, round: 1 })
        .expect("current");
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("one build, not {effects:?}");
    };
    assert_eq!(build.key.round, 2);
    let started = key_build(&mut core, build.key, None);
    assert!(
        started
            .iter()
            .any(|effect| matches!(effect, Effect::StartEngine { .. })),
        "round 2 starts: {started:?}"
    );

    let second = r#"{"records":[{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"second"}},{"step":{"id":"t1","put":[{"shape":{"id":"c","kind":"rect","box":[40,0,10,10]}}]}},{"end":{}}]}"#;
    let second_drawn = ToolOutput::Text("drawn: [t1]".into());
    let ended = key_draw_round(&mut core, &mut store, second, &second_drawn);
    assert_eq!(round_calls(&ended), [(id("reused"), second_drawn.clone())]);
    let mut retried = Vec::new();
    let again = core.tool_call(
        Caller::ModelRound {
            run: R,
            round: 2,
            call: id("reused"),
        },
        crate::engines::ToolName::Draw,
        second.as_bytes(),
        &mut retried,
    );
    assert_eq!(
        (again, retried),
        (Dispatch::Answered(second_drawn.clone()), Vec::new())
    );

    let steps: Vec<_> = store
        .drawing_scenes()
        .expect("real durable scenes")
        .scenes
        .iter()
        .flat_map(|scene| scene.log.iter())
        .map(|event| match event {
            kumochi_scene::SceneEvent::Step { step, .. } => step.id.as_str().to_owned(),
            kumochi_scene::SceneEvent::Stop(_) => panic!("no cut was requested"),
        })
        .collect();
    assert_eq!(steps, ["s1", "s2", "t1"]);
    let db = rusqlite::Connection::open(scratch.db()).expect("read result rows");
    let rows: u32 = db
        .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
        .expect("result row count");
    assert_eq!(rows, 2);
}

/// After a restart, an overlay a protected run drew reaches no model, the one it was drawn for
/// included, on the recipient line and on the text-only fallback alike, while an overlay only
/// public runs drew still reaches every model.
#[test]
fn a_restart_keeps_a_protected_overlay_out_of_every_request() {
    use crate::drawing::{Drawing, Target, Write};
    use crate::store::RunEnd;
    use kumochi_protocol::message::{ConnectionKind, RunId};

    let scratch = Scratch::new("provider-draw-restored-authors");
    let mut store = store(&scratch);
    let drawn = RunId(R.0 + 1);
    store.begin_turn(T, None, drawn, "protected turn").unwrap();
    for (run, scene, object, protected) in [
        (R, "restored_public", "public_object", false),
        (drawn, "restored_private", "private_object", true),
    ] {
        store
            .drawing_write(&Write::Step {
                thread: T,
                run,
                target: Target::New(
                    serde_json::from_value(json!({
                        "begin": {"v":1,"scene":scene,"surface":"overlay","alt":format!("{scene}_alt"),"others":"keep"},
                        "layout_version": 2
                    }))
                    .unwrap(),
                ),
                clear: Vec::new(),
                revision: 1,
                layout_version: 2,
                step: std::sync::Arc::new(
                    serde_json::from_value(json!({
                        "id":"s1","put":[{"shape":{"id":object,"kind":"rect","box":[0,0,10,10]}}]
                    }))
                    .unwrap(),
                ),
                protected,
            })
            .unwrap();
        store.end_run(T, run, &[], RunEnd::Done).unwrap();
    }
    drop(store);

    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    let (drawing, skipped) = Drawing::recover(store.drawing_scenes().unwrap().scenes);
    assert_eq!(skipped, Vec::new());
    let run = RunId(R.0 + 2);
    store.begin_turn(T, None, run, "after restart").unwrap();
    let (_, snapshot) = pictured_key();
    for (connection, slug, capable) in [
        (ConnectionKind::AnthropicKey, "m", true),
        (ConnectionKind::AnthropicKey, "other-vision", true),
        (ConnectionKind::AnthropicKey, "text-only", false),
        (ConnectionKind::ChatGpt, "m", true),
    ] {
        let mut receiving = snapshot.clone();
        receiving.choice = Some(super::super::routes::choice(connection, slug));
        let pictured = capable.then(|| key_recipient(&receiving));
        let request = built_key(
            &store,
            &receiving,
            run,
            pictured.as_ref(),
            &[],
            drawing.digest(T),
        );
        let wire = if connection == ConnectionKind::ChatGpt {
            crate::engines::responses::body(&request)
                .unwrap()
                .to_string()
        } else {
            key_wire(&request, receiving.connection).to_string()
        };
        for public in ["restored_public", "restored_public_alt", "public_object"] {
            assert!(wire.contains(public), "{connection:?} {slug} {public}");
        }
        for private in ["restored_private", "private_object"] {
            assert!(!wire.contains(private), "{connection:?} {slug} {private}");
        }
        assert!(wire.contains("after restart"));
    }
}
