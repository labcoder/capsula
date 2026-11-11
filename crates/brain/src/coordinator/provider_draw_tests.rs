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
        let expected = ToolOutput::Text(
            "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef".into(),
        );
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
        let expected = ToolOutput::Text(format!("drawn: [s1]; stopped: {stopped}"));
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
                ToolOutput::Text(
                    "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef".into()
                )
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
fn a_refused_draw_logs_one_warning_with_only_its_code_and_field() {
    let logs = crate::test_support::Captured::default();
    let _logging = logs.install();
    let mut core = offered(vec![T]);
    let inline = PREFIX.replace("\"surface\":\"overlay\"", "\"surface\":\"inline\"");
    start(&mut core, "inline");
    let refused = result(&delta(&mut core, "inline", &inline));
    assert_eq!(
        refused.output,
        ToolOutput::Text("drawn: []; stopped: surface: scene.Unsupported".into())
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
            ""
        } else if byte_bound {
            "; stopped: drawing exceeds byte limit"
        } else {
            "; stopped: drawing.BudgetSpent"
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
            "canonical" => "drawn: [s1, s2]; stopped: step s3: remove[0]: scene.UnknownRef".into(),
            "malformed" => format!(
                "drawn: [s1]; stopped: malformed drawing at byte {}",
                PREFIX.len() + 1
            ),
            _ => "drawn: [s1]; stopped: truncated drawing".into(),
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
    let expected = ToolOutput::Text("drawn: []; stopped: surface: scene.Unsupported".into());
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
