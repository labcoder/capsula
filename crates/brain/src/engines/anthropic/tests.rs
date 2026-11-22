use serde_json::{Value, json};

use super::{Block, Bounded, Mapper};
use crate::{
    connections::{
        anthropic::{error::Failure, sse_map::Reader},
        model::sse::Progress,
    },
    engines::{CallId, EngineFailure, Finish, Opaque, Tokens, ToolName, TurnEvent, Usage},
};

#[expect(
    clippy::needless_pass_by_value,
    reason = "fixture builders take temporary JSON values"
)]
fn send(mapper: &mut Mapper, value: Value) {
    let wire = format!("data: {value}\n\n");
    let result = crate::connections::model::sse::Reader::default()
        .feed(wire.as_bytes(), &mut |event| mapper.event(event));
    assert_eq!(result, Ok(Progress::More));
}

fn started() -> Mapper {
    let mut mapper = Mapper::default();
    send(
        &mut mapper,
        json!({"type":"message_start", "message":{"usage":{}}}),
    );
    mapper
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "fixture builders take temporary JSON values"
)]
fn start(mapper: &mut Mapper, index: u32, block: Value) {
    send(
        mapper,
        json!({"type":"content_block_start", "index":index, "content_block":block}),
    );
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "fixture builders take temporary JSON values"
)]
fn delta(mapper: &mut Mapper, index: u32, delta: Value) {
    send(
        mapper,
        json!({"type":"content_block_delta", "index":index, "delta":delta}),
    );
}

fn stop(mapper: &mut Mapper, index: u32) {
    send(mapper, json!({"type":"content_block_stop", "index":index}));
}

fn call(id: &str) -> CallId {
    CallId::new(id).unwrap()
}

fn protocol(mapper: &mut Mapper) {
    assert_eq!(
        mapper.finish(Ok(Progress::Completed)),
        Some(Finish::Failed(EngineFailure::Anthropic(Failure::Protocol)))
    );
    start(
        mapper,
        999,
        json!({"type":"text", "text":"must not escape"}),
    );
    assert_eq!(mapper.finish(Ok(Progress::Completed)), None);
    assert_eq!(mapper.take(), Vec::new());
}

#[test]
fn interleaved_blocks_keep_exact_call_and_private_thinking_events() {
    let mut m = started();
    start(&mut m, 0, json!({"type":"text", "text":"hi"}));
    start(
        &mut m,
        4,
        json!({"type":"thinking", "thinking":"private", "signature":""}),
    );
    start(
        &mut m,
        7,
        json!({"type":"tool_use", "id":"a", "name":"not_offered", "input":{}}),
    );
    delta(
        &mut m,
        7,
        json!({"type":"input_json_delta", "partial_json":"{\"x\":"}),
    );
    delta(&mut m, 0, json!({"type":"text_delta", "text":"!"}));
    delta(
        &mut m,
        4,
        json!({"type":"thinking_delta", "thinking":" thought"}),
    );
    delta(
        &mut m,
        4,
        json!({"type":"signature_delta", "signature":"sig"}),
    );
    delta(
        &mut m,
        4,
        json!({"type":"signature_delta", "signature":"nature"}),
    );
    delta(
        &mut m,
        7,
        json!({"type":"input_json_delta", "partial_json":"1}"}),
    );
    stop(&mut m, 7);
    stop(&mut m, 4);
    stop(&mut m, 0);
    let opaque = Opaque::new(
        br#"{"type":"thinking","thinking":"private thought","signature":"signature"}"#.to_vec(),
    )
    .unwrap();
    assert_eq!(
        m.take(),
        vec![
            TurnEvent::TextDelta("hi".into()),
            TurnEvent::ToolCallStarted {
                call: call("a"),
                name: ToolName::Unknown
            },
            TurnEvent::ToolArgsDelta {
                call: call("a"),
                delta: "{\"x\":".into()
            },
            TurnEvent::TextDelta("!".into()),
            TurnEvent::ToolArgsDelta {
                call: call("a"),
                delta: "1}".into()
            },
            TurnEvent::ToolCallReady {
                call: call("a"),
                args: "{\"x\":1}".into()
            },
            TurnEvent::OutputItem {
                index: 4,
                item: opaque
            }
        ]
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
}

#[test]
fn wire_catalog_names_are_recognized_without_offering_or_activating_them() {
    for (name, expected) in [
        ("draw", ToolName::Draw),
        ("search_history", ToolName::SearchHistory),
        ("read_history", ToolName::ReadHistory),
        ("list_tasks", ToolName::ListTasks),
        ("create_task", ToolName::CreateTask),
        ("focus", ToolName::Focus),
        ("unknown-wire-tool", ToolName::Unknown),
    ] {
        let mut mapper = started();
        start(
            &mut mapper,
            0,
            json!({"type":"tool_use", "id":"wire", "name":name, "input":{}}),
        );
        stop(&mut mapper, 0);
        assert_eq!(
            mapper.take(),
            vec![
                TurnEvent::ToolCallStarted {
                    call: call("wire"),
                    name: expected
                },
                TurnEvent::ToolCallReady {
                    call: call("wire"),
                    args: "{}".into()
                },
            ]
        );
        assert_eq!(
            mapper.finish(Ok(Progress::Completed)),
            Some(Finish::Completed)
        );
    }
}

#[test]
fn empty_tool_fragments_preserve_start_until_nonempty_json_arrives() {
    for (empty_deltas, fragment) in [(false, None), (true, None), (true, Some("{\"x\":1}"))] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"fallback", "data":"ignored"}));
        stop(&mut m, 0);
        start(
            &mut m,
            1,
            json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
        );
        if empty_deltas {
            delta(
                &mut m,
                1,
                json!({"type":"input_json_delta", "partial_json":""}),
            );
        }
        let mut expected = vec![TurnEvent::ToolCallStarted {
            call: call("a"),
            name: ToolName::Draw,
        }];
        if let Some(args) = fragment {
            delta(
                &mut m,
                1,
                json!({"type":"input_json_delta", "partial_json":args}),
            );
            expected.push(TurnEvent::ToolArgsDelta {
                call: call("a"),
                delta: args.into(),
            });
        }
        if empty_deltas {
            delta(
                &mut m,
                1,
                json!({"type":"input_json_delta", "partial_json":""}),
            );
        }
        stop(&mut m, 1);
        expected.push(TurnEvent::ToolCallReady {
            call: call("a"),
            args: fragment.unwrap_or("{}").into(),
        });
        assert_eq!(m.take(), expected);
        assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
        assert_eq!(m.take(), []);
    }
}

#[test]
fn empty_initial_text_emits_nothing_until_visible_delta() {
    let mut m = started();
    start(&mut m, 0, json!({"type":"text", "text":""}));
    assert_eq!(m.take(), vec![]);
    delta(&mut m, 0, json!({"type":"text_delta", "text":"visible"}));
    stop(&mut m, 0);
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    assert_eq!(m.take(), vec![TurnEvent::TextDelta("visible".into())]);
}

#[test]
fn absent_initial_signature_accepts_signed_thinking_but_unsigned_stop_fails() {
    let mut m = started();
    start(&mut m, 0, json!({"type":"thinking", "thinking":"private"}));
    delta(
        &mut m,
        0,
        json!({"type":"signature_delta", "signature":"sig"}),
    );
    stop(&mut m, 0);
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    assert_eq!(
        m.take(),
        vec![TurnEvent::OutputItem {
            index: 0,
            item: Opaque::new(
                br#"{"type":"thinking","thinking":"private","signature":"sig"}"#.to_vec()
            )
            .unwrap(),
        }]
    );
    let mut m = started();
    start(&mut m, 0, json!({"type":"thinking", "thinking":"private"}));
    assert_eq!(m.finish(Ok(Progress::More)), None);
    stop(&mut m, 0);
    protocol(&mut m);
}

#[test]
fn invalid_block_transitions_fail_sticky() {
    for bad in [
        json!({"type":"content_block_delta", "index":9, "delta":{"type":"text_delta", "text":"bad"}}),
        json!({"type":"content_block_stop", "index":9}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":"duplicate"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{}"}}),
        json!({"type":"content_block_delta", "index":"0", "delta":{"type":"text_delta", "text":"bad"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":false}}),
    ] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"text", "text":""}));
        assert_eq!(m.take(), vec![]);
        send(&mut m, bad);
        protocol(&mut m);
    }
    let mut m = started();
    start(&mut m, 0, json!({"type":"fallback"}));
    stop(&mut m, 0);
    delta(&mut m, 0, json!({"type":"future_delta", "data":"bad"}));
    protocol(&mut m);

    let mut m = started();
    start(
        &mut m,
        0,
        json!({"type":"thinking", "thinking":"private", "signature":""}),
    );
    stop(&mut m, 0);
    protocol(&mut m);

    let mut m = started();
    start(&mut m, 0, json!({"type":"fallback"}));
    protocol(&mut m);
}

#[test]
fn duplicate_ids_and_invalid_arguments_never_become_ready() {
    for args in ["{", "[]", "{} trailing", "null"] {
        let mut m = started();
        start(
            &mut m,
            0,
            json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
        );
        delta(
            &mut m,
            0,
            json!({"type":"input_json_delta", "partial_json":args}),
        );
        assert_eq!(
            m.take(),
            vec![
                TurnEvent::ToolCallStarted {
                    call: call("a"),
                    name: ToolName::Draw
                },
                TurnEvent::ToolArgsDelta {
                    call: call("a"),
                    delta: args.into()
                }
            ]
        );
        stop(&mut m, 0);
        protocol(&mut m);
    }
    let mut m = started();
    start(
        &mut m,
        0,
        json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
    );
    stop(&mut m, 0);
    assert_eq!(
        m.take(),
        vec![
            TurnEvent::ToolCallStarted {
                call: call("a"),
                name: ToolName::Draw
            },
            TurnEvent::ToolCallReady {
                call: call("a"),
                args: "{}".into()
            }
        ]
    );
    start(
        &mut m,
        1,
        json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
    );
    protocol(&mut m);
}

fn usage_start(m: &mut Mapper) {
    send(
        m,
        json!({"type":"message_start", "message":{"usage":{"input_tokens":11, "output_tokens":1,
        "cache_read_input_tokens":22, "cache_creation_input_tokens":33}}}),
    );
}

#[test]
fn literal_final_usage_precedes_exhaustion_and_eof_has_none() {
    let mut m = Mapper::default();
    usage_start(&mut m);
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{}, "usage":{"output_tokens":4}}),
    );
    let result = Reader::default().feed(
        b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\n",
        &mut |event| m.event(event),
    );
    assert_eq!(result, Err(Failure::Exhausted));
    assert_eq!(
        m.finish(result),
        Some(Finish::Failed(EngineFailure::Anthropic(Failure::Exhausted)))
    );
    let whole = Tokens {
        input: 11,
        output: 9,
        cache_read: 22,
        cache_write: 33,
    };
    assert_eq!(
        m.take(),
        vec![TurnEvent::Usage(Usage {
            whole,
            first_request: Some(whole)
        })]
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), None);
    let mut m = Mapper::default();
    usage_start(&mut m);
    assert_eq!(
        m.finish(Err(Failure::Truncated)),
        Some(Finish::Failed(EngineFailure::Anthropic(Failure::Truncated)))
    );
    assert_eq!(m.take(), vec![]);
    let mut m = started();
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":9}}),
    );
    assert_eq!(
        m.finish(Err(Failure::Truncated)),
        Some(Finish::Failed(EngineFailure::Anthropic(Failure::Truncated)))
    );
    assert_eq!(m.take(), vec![]);
}

#[test]
fn semantic_reader_failures_beat_truncated_arguments_and_preserve_final_usage() {
    for (reason, failure) in [
        ("max_tokens", Failure::Exhausted),
        ("refusal", Failure::Refused),
    ] {
        let mut m = started();
        start(
            &mut m,
            0,
            json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
        );
        delta(
            &mut m,
            0,
            json!({"type":"input_json_delta", "partial_json":"{\"x\":"}),
        );
        stop(&mut m, 0);
        let wire = format!(
            "data: {}\n\n",
            json!({"type":"message_delta", "delta":{"stop_reason":reason}, "usage":{"output_tokens":9}})
        );
        let read = Reader::default().feed(wire.as_bytes(), &mut |event| m.event(event));
        assert_eq!(read, Err(failure.clone()));
        assert_eq!(
            m.finish(read),
            Some(Finish::Failed(EngineFailure::Anthropic(failure)))
        );
        let whole = Tokens {
            output: 9,
            ..Tokens::default()
        };
        assert_eq!(
            m.take(),
            vec![
                TurnEvent::ToolCallStarted {
                    call: call("a"),
                    name: ToolName::Draw
                },
                TurnEvent::ToolArgsDelta {
                    call: call("a"),
                    delta: "{\"x\":".into()
                },
                TurnEvent::Usage(Usage {
                    whole,
                    first_request: Some(whole)
                }),
            ]
        );
    }
    let mut m = started();
    delta(&mut m, 99, json!({"type":"text_delta", "text":"invalid"}));
    let error = json!({"type":"error", "error":{"type":"overloaded_error", "message":"synthetic"}});
    let expected = Failure::streamed(&error);
    let wire = format!("data: {error}\n\n");
    let read = Reader::default().feed(wire.as_bytes(), &mut |event| m.event(event));
    assert_eq!(read, Err(expected.clone()));
    assert_eq!(
        m.finish(read),
        Some(Finish::Failed(EngineFailure::Anthropic(expected)))
    );
    assert_eq!(m.take(), vec![]);
}

#[test]
fn optional_null_usage_counts_are_absent_but_output_is_strict() {
    let mut m = Mapper::default();
    send(
        &mut m,
        json!({"type":"message_start", "message":{"usage":{
        "input_tokens":null, "cache_read_input_tokens":null, "cache_creation_input_tokens":null, "output_tokens":1}}}),
    );
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":9}}),
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    let whole = Tokens {
        output: 9,
        ..Tokens::default()
    };
    assert_eq!(
        m.take(),
        vec![TurnEvent::Usage(Usage {
            whole,
            first_request: Some(whole)
        })]
    );
    let mut m = Mapper::default();
    usage_start(&mut m);
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{
        "input_tokens":null, "cache_read_input_tokens":null, "cache_creation_input_tokens":null, "output_tokens":9}}),
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    let whole = Tokens {
        input: 11,
        output: 9,
        cache_read: 22,
        cache_write: 33,
    };
    assert_eq!(
        m.take(),
        vec![TurnEvent::Usage(Usage {
            whole,
            first_request: Some(whole)
        })]
    );
    let mut m = Mapper::default();
    send(
        &mut m,
        json!({"type":"message_start", "message":{"usage":{"output_tokens":null}}}),
    );
    protocol(&mut m);
}

#[test]
fn bad_usage_counts_fail_instead_of_clamping_or_resetting() {
    for count in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("4"),
        json!(4_294_967_296_u64),
        Value::Null,
    ] {
        let mut m = Mapper::default();
        usage_start(&mut m);
        send(
            &mut m,
            json!({"type":"message_delta", "delta":{}, "usage":{"output_tokens":count}}),
        );
        protocol(&mut m);
    }
}

#[test]
fn item_name_id_and_byte_bounds_are_exact() {
    for count in [64, 65] {
        let mut m = started();
        for index in 0..count {
            start(&mut m, index, json!({"type":"fallback"}));
            stop(&mut m, index);
        }
        if count == 64 {
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
            assert_eq!(m.take(), vec![]);
        } else {
            protocol(&mut m);
        }
    }
    for (id, name, good) in [
        ("i".repeat(64), "n".repeat(64), true),
        ("i".repeat(65), "n".into(), false),
        (String::new(), "n".into(), false),
        ("é".into(), "n".into(), false),
        ("i".into(), "n".repeat(65), false),
    ] {
        let mut m = started();
        start(
            &mut m,
            0,
            json!({"type":"tool_use", "id":id, "name":name, "input":{}}),
        );
        if good {
            stop(&mut m, 0);
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
            assert_eq!(
                m.take(),
                vec![
                    TurnEvent::ToolCallStarted {
                        call: call(&id),
                        name: ToolName::Unknown
                    },
                    TurnEvent::ToolCallReady {
                        call: call(&id),
                        args: "{}".into()
                    },
                ]
            );
        } else {
            protocol(&mut m);
        }
    }
}

#[test]
fn opaque_limit_and_debug_redaction_cover_the_outer_carriers() {
    for extra in [0, 1] {
        let mut m = started();
        let data = "x".repeat(262_106 + extra);
        start(&mut m, 5, json!({"type":"redacted_thinking", "data":data}));
        stop(&mut m, 5);
        if extra == 0 {
            let expected = format!("{{\"type\":\"redacted_thinking\",\"data\":\"{data}\"}}");
            assert_eq!(expected.len(), 262_144);
            assert_eq!(
                m.take(),
                vec![TurnEvent::OutputItem {
                    index: 5,
                    item: Opaque::new(expected.into_bytes()).unwrap(),
                }]
            );
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
        } else {
            protocol(&mut m);
        }
    }
    let marker = "sensitive-sentinel";
    let mut m = started();
    start(
        &mut m,
        0,
        json!({"type":"thinking", "thinking":marker, "signature":marker}),
    );
    stop(&mut m, 0);
    let debug = format!(
        "{m:?} {:?} {:?} {:?} {:?} {:?}",
        m.events,
        Block::RedactedThinking(marker.into()),
        Block::Thinking {
            text: marker.into(),
            signature: marker.into()
        },
        Bounded {
            bytes: marker.as_bytes().to_vec(),
            cap: 100
        },
        super::Native::Thinking {
            thinking: marker,
            signature: marker
        }
    );
    assert!(!debug.contains(marker), "text leaked: {debug}");
    assert!(
        !debug.contains(&format!("{:?}", marker.as_bytes())),
        "bytes leaked: {debug}"
    );
}

#[test]
fn queued_and_retained_arguments_share_the_byte_budget_and_take_releases_ownership() {
    for drain in [false, true] {
        let mut m = started();
        start(
            &mut m,
            0,
            json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
        );
        assert_eq!(
            m.take(),
            vec![TurnEvent::ToolCallStarted {
                call: call("a"),
                name: ToolName::Draw
            }]
        );
        let padding = " ".repeat(524_221);
        delta(
            &mut m,
            0,
            json!({"type":"input_json_delta", "partial_json":padding}),
        );
        let first = TurnEvent::ToolArgsDelta {
            call: call("a"),
            delta: padding.clone(),
        };
        if drain {
            assert_eq!(m.take(), vec![first.clone()]);
        }
        delta(
            &mut m,
            0,
            json!({"type":"input_json_delta", "partial_json":"{}"}),
        );
        stop(&mut m, 0);
        if drain {
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
            assert_eq!(
                m.take(),
                vec![
                    TurnEvent::ToolArgsDelta {
                        call: call("a"),
                        delta: "{}".into()
                    },
                    TurnEvent::ToolCallReady {
                        call: call("a"),
                        args: format!("{padding}{{}}")
                    },
                ]
            );
        } else {
            assert_eq!(m.take(), vec![first]);
            protocol(&mut m);
        }
    }
}

#[test]
fn absent_usage_fields_preserve_literal_counts_on_success_and_refusal() {
    for failure in [None, Some(Failure::Refused)] {
        let mut m = Mapper::default();
        usage_start(&mut m);
        send(
            &mut m,
            json!({"type":"message_delta", "delta":{}, "usage":{"output_tokens":u32::MAX}}),
        );
        send(
            &mut m,
            json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{}}),
        );
        let expected = failure.clone().map_or(Finish::Completed, |f| {
            Finish::Failed(EngineFailure::Anthropic(f))
        });
        assert_eq!(
            m.finish(failure.map_or(Ok(Progress::Completed), Err)),
            Some(expected)
        );
        let whole = Tokens {
            input: 11,
            output: u32::MAX,
            cache_read: 22,
            cache_write: 33,
        };
        assert_eq!(
            m.take(),
            vec![TurnEvent::Usage(Usage {
                whole,
                first_request: Some(whole)
            })]
        );
    }
}

#[test]
fn mistyped_starts_and_duplicate_stops_fail_closed() {
    for block in [
        json!({"type":"text", "text":null}),
        json!({"type":"tool_use", "id":"a", "name":"draw", "input":[]}),
        json!({"type":"tool_use", "id":8, "name":"draw", "input":{}}),
        json!({"type":"tool_use", "id":"a", "name":false, "input":{}}),
        json!({"type":"thinking", "thinking":"private", "signature":null}),
        json!({"type":"redacted_thinking", "data":42}),
        json!({"type":5}),
    ] {
        let mut m = started();
        start(&mut m, 0, block);
        protocol(&mut m);
    }
    let mut m = started();
    start(&mut m, 0, json!({"type":"fallback"}));
    stop(&mut m, 0);
    stop(&mut m, 0);
    protocol(&mut m);
}

#[test]
fn failure_is_sticky_before_finish() {
    let mut m = started();
    delta(&mut m, 0, json!({"type":"text_delta", "text":"unknown"}));
    start(
        &mut m,
        0,
        json!({"type":"text", "text":"valid but too late"}),
    );
    assert_eq!(m.take(), vec![]);
    protocol(&mut m);
}

#[test]
fn dense_single_sixteen_kib_read_delivers_all_130_text_deltas() {
    let mut wire = String::from("data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}

data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

");
    for _ in 0..130 {
        wire.push_str("event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}

");
    }
    assert!(wire.len() <= 16_384, "dense fixture must fit a read");
    wire.push_str(
        &"
"
        .repeat(16_384 - wire.len()),
    );
    let mut m = Mapper::default();
    assert_eq!(
        Reader::default().feed(wire.as_bytes(), &mut |event| m.event(event)),
        Ok(Progress::More)
    );
    assert_eq!(m.take(), vec![TurnEvent::TextDelta("a".into()); 130]);
    stop(&mut m, 0);
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
}

#[test]
fn blocks_and_completion_require_message_start() {
    for block in [
        json!({"type":"text", "text":"must not escape"}),
        json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
        json!({"type":"thinking", "thinking":"private", "signature":"sig"}),
        json!({"type":"redacted_thinking", "data":"private"}),
        json!({"type":"fallback"}),
    ] {
        let mut m = Mapper::default();
        start(&mut m, 0, block);
        send(
            &mut m,
            json!({"type":"message_start", "message":{"usage":{}}}),
        );
        assert_eq!(m.take(), vec![]);
        protocol(&mut m);
    }
    protocol(&mut Mapper::default());
    let mut m = started();
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    assert_eq!(m.take(), vec![]);
}

#[test]
fn optional_deltas_preserve_the_final_usage_marker_and_latest_literal_counts() {
    let mut m = Mapper::default();
    usage_start(&mut m);
    send(
        &mut m,
        json!({"type":"message_delta", "usage":{"output_tokens":4}}),
    );
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":9}}),
    );
    send(
        &mut m,
        json!({"type":"message_delta", "delta":{"stop_reason":null}, "usage":{"output_tokens":10}}),
    );
    send(
        &mut m,
        json!({"type":"message_delta", "usage":{"output_tokens":12}}),
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
    let whole = Tokens {
        input: 11,
        output: 12,
        cache_read: 22,
        cache_write: 33,
    };
    assert_eq!(
        m.take(),
        vec![TurnEvent::Usage(Usage {
            whole,
            first_request: Some(whole)
        })]
    );
    assert_eq!(m.finish(Ok(Progress::Completed)), None);
}

#[test]
fn queued_bytes_include_event_overhead_and_reserved_final_usage_at_exact_limit() {
    for (extra, with_usage) in [(0, false), (0, true), (1, true)] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"text", "text":""}));
        let chunk = "x".repeat(65_536);
        for _ in 0..15 {
            delta(&mut m, 0, json!({"type":"text_delta", "text":chunk}));
        }
        let last = "x".repeat(64_448 + extra);
        delta(&mut m, 0, json!({"type":"text_delta", "text":last}));
        if extra == 0 {
            start(
                &mut m,
                1,
                json!({"type":"fallback", "data":"s".repeat(4_096)}),
            );
            delta(
                &mut m,
                1,
                json!({"type":"future_delta", "data":"s".repeat(4_096)}),
            );
            stop(&mut m, 1);
        }
        stop(&mut m, 0);
        if with_usage {
            send(
                &mut m,
                json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":7}}),
            );
        }
        let mut expected = vec![TurnEvent::TextDelta(chunk); 15];
        if extra == 0 {
            expected.push(TurnEvent::TextDelta(last));
            if with_usage {
                let whole = Tokens {
                    output: 7,
                    ..Tokens::default()
                };
                expected.push(TurnEvent::Usage(Usage {
                    whole,
                    first_request: Some(whole),
                }));
            }
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
            assert_eq!(m.take(), expected);
        } else {
            assert_eq!(m.take(), expected);
            protocol(&mut m);
        }
    }
}

#[test]
fn zero_byte_deltas_are_bounded_by_fixed_event_overhead() {
    for extra in [0, 1] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"text", "text":""}));
        for _ in 0..16_383 + extra {
            delta(&mut m, 0, json!({"type":"text_delta", "text":""}));
        }
        stop(&mut m, 0);
        send(
            &mut m,
            json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":7}}),
        );
        let mut expected = vec![TurnEvent::TextDelta(String::new()); 16_383];
        if extra == 0 {
            let whole = Tokens {
                output: 7,
                ..Tokens::default()
            };
            expected.push(TurnEvent::Usage(Usage {
                whole,
                first_request: Some(whole),
            }));
            assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
            assert_eq!(m.take(), expected);
        } else {
            assert_eq!(m.take(), expected);
            protocol(&mut m);
        }
    }
}

#[test]
fn final_failure_usage_has_reserved_space_even_after_queue_budget_failure() {
    for (reason, failure) in [
        ("max_tokens", Failure::Exhausted),
        ("refusal", Failure::Refused),
    ] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"text", "text":""}));
        let chunk = "x".repeat(65_536);
        for _ in 0..15 {
            delta(&mut m, 0, json!({"type":"text_delta", "text":chunk}));
        }
        let last = "x".repeat(64_448);
        delta(&mut m, 0, json!({"type":"text_delta", "text":last}));
        delta(
            &mut m,
            0,
            json!({"type":"text_delta", "text":"one more byte fails"}),
        );
        let wire = format!(
            "data: {}\n\n",
            json!({"type":"message_delta", "delta":{"stop_reason":reason}, "usage":{"output_tokens":9}})
        );
        let read = Reader::default().feed(wire.as_bytes(), &mut |event| m.event(event));
        assert_eq!(read, Err(failure.clone()));
        assert_eq!(
            m.finish(read),
            Some(Finish::Failed(EngineFailure::Anthropic(failure)))
        );
        let mut expected = vec![TurnEvent::TextDelta(chunk); 15];
        expected.push(TurnEvent::TextDelta(last));
        let whole = Tokens {
            output: 9,
            ..Tokens::default()
        };
        expected.push(TurnEvent::Usage(Usage {
            whole,
            first_request: Some(whole),
        }));
        assert_eq!(m.take(), expected);
        assert_eq!(m.finish(Ok(Progress::Completed)), None);
        assert_eq!(m.take(), vec![]);
    }
}
