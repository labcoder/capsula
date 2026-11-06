use serde_json::{Value, json};

use super::{BYTES, Block, Bounded, ITEMS, Mapper};
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
        format!("{:?}", mapper.finish(Ok(Progress::Completed))),
        "Some(Failed(Anthropic(Protocol)))"
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
    assert_eq!(m.bytes, 1);
}

#[test]
fn no_fragments_use_the_start_object_and_fallback_is_skipped() {
    let mut m = started();
    start(&mut m, 0, json!({"type":"fallback", "data":"ignored"}));
    stop(&mut m, 0);
    start(
        &mut m,
        1,
        json!({"type":"tool_use", "id":"a", "name":"draw", "input":{}}),
    );
    stop(&mut m, 1);
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
    assert_eq!(m.finish(Ok(Progress::Completed)), Some(Finish::Completed));
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
        assert_eq!(m.take(), vec![TurnEvent::TextDelta(String::new())]);
        send(&mut m, bad);
        protocol(&mut m);
    }
    let mut m = started();
    start(&mut m, 0, json!({"type":"fallback"}));
    stop(&mut m, 0);
    delta(&mut m, 0, json!({"type":"future_delta", "data":"bad"}));
    protocol(&mut m);
}

#[test]
fn duplicate_ids_and_invalid_arguments_never_become_ready() {
    for args in ["", "{", "[]", "{} trailing", "null"] {
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

#[test]
fn missing_signature_and_open_blocks_refuse_completion() {
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
    let mut m = started();
    for index in 0..64 {
        start(&mut m, index, json!({"type":"fallback"}));
        stop(&mut m, index);
    }
    assert_eq!(m.blocks.len(), ITEMS);
    start(&mut m, 64, json!({"type":"fallback"}));
    protocol(&mut m);
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
        assert_eq!(m.broken, !good);
    }
    let mut m = started();
    assert_eq!(
        m.start(0, &json!({"type":"text", "text":"x".repeat(1_048_576)})),
        Ok(())
    );
    assert_eq!(m.bytes, 1_048_576);
    assert_eq!(
        m.delta(0, &json!({"type":"text_delta", "text":"!"})),
        Err(())
    );
    assert_eq!(m.charge(usize::MAX), Err(()));
}

#[test]
fn opaque_limit_and_debug_redaction_cover_the_outer_carriers() {
    for extra in [0, 1] {
        let mut m = started();
        let overhead = b"{\"data\":\"\",\"type\":\"redacted_thinking\"}".len();
        let data = "x".repeat(Opaque::MAX - overhead + extra);
        assert_eq!(
            m.start(5, &json!({"type":"redacted_thinking", "data":data})),
            Ok(())
        );
        assert_eq!(m.stop(5), if extra == 0 { Ok(()) } else { Err(()) });
        if extra == 0 {
            let events = m.take();
            assert_eq!(events.len(), 1);
            match &events[0] {
                TurnEvent::OutputItem { index, item } => {
                    assert_eq!(*index, 5);
                    assert_eq!(item.size(), Opaque::MAX);
                }
                other => panic!("unexpected {other:?}"),
            }
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
fn queue_ownership_is_released_and_retained_arguments_are_charged() {
    let mut m = started();
    assert_eq!(
        m.start(0, &json!({"type":"text", "text":"x".repeat(BYTES)})),
        Ok(())
    );
    assert_eq!(m.take(), vec![TurnEvent::TextDelta("x".repeat(BYTES))]);
    assert_eq!(m.bytes, 0);
    assert_eq!(
        m.delta(0, &json!({"type":"text_delta", "text":"next"})),
        Ok(())
    );
    assert_eq!(m.take(), vec![TurnEvent::TextDelta("next".into())]);
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
    let args = " ".repeat((BYTES - 5) / 2);
    assert_eq!(
        m.delta(0, &json!({"type":"input_json_delta", "partial_json":args})),
        Ok(())
    );
    assert_eq!(m.bytes, BYTES - 3);
    assert_eq!(
        m.delta(0, &json!({"type":"input_json_delta", "partial_json":"xx"})),
        Err(())
    );
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
fn failure_is_sticky_before_finish_and_the_queue_has_a_hard_item_cap() {
    let mut m = started();
    delta(&mut m, 0, json!({"type":"text_delta", "text":"unknown"}));
    start(
        &mut m,
        0,
        json!({"type":"text", "text":"valid but too late"}),
    );
    assert_eq!(m.take(), vec![]);
    protocol(&mut m);
    let mut m = started();
    start(&mut m, 0, json!({"type":"text", "text":"a"}));
    for _ in 1..ITEMS {
        delta(&mut m, 0, json!({"type":"text_delta", "text":"a"}));
    }
    assert_eq!(m.events, vec![TurnEvent::TextDelta("a".into()); ITEMS]);
    delta(
        &mut m,
        0,
        json!({"type":"text_delta", "text":"must not queue"}),
    );
    assert_eq!(m.take(), vec![TurnEvent::TextDelta("a".into()); ITEMS]);
    protocol(&mut m);
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
fn final_usage_obeys_the_same_queue_limit_as_content() {
    for count in [ITEMS - 1, ITEMS] {
        let mut m = started();
        start(&mut m, 0, json!({"type":"text", "text":"a"}));
        for _ in 1..count {
            delta(&mut m, 0, json!({"type":"text_delta", "text":"a"}));
        }
        stop(&mut m, 0);
        send(
            &mut m,
            json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":7}}),
        );
        let mut expected = vec![TurnEvent::TextDelta("a".into()); count];
        let ending = if count == ITEMS - 1 {
            let whole = Tokens {
                output: 7,
                ..Tokens::default()
            };
            expected.push(TurnEvent::Usage(Usage {
                whole,
                first_request: Some(whole),
            }));
            Finish::Completed
        } else {
            Finish::Failed(EngineFailure::Anthropic(Failure::Protocol))
        };
        assert_eq!(m.finish(Ok(Progress::Completed)), Some(ending));
        assert_eq!(m.take(), expected);
        assert_eq!(m.finish(Ok(Progress::Completed)), None);
        assert_eq!(m.take(), vec![]);
    }
}
