use serde_json::{Value, json};

use super::{Progress, Reader, StopReason};
use crate::connections::anthropic::error::{Failure, Kind};
use crate::connections::model::framer::{MAX_EVENT_BYTES, MAX_LINE_BYTES, MAX_STREAM_BYTES};
use crate::connections::model::sse::MAX_TEXT_BYTES;

const REPLAY: &[u8] =
    include_bytes!("../../../../fixtures/anthropic/synthetic-text-tool-usage.sse");
const ERROR: &[u8] =
    include_bytes!("../../../../fixtures/anthropic/synthetic-error-after-text.sse");

fn event(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn ending(reason: &str) -> Vec<u8> {
    let mut bytes = event(&json!({"type":"message_delta","delta":{"stop_reason":reason}}));
    bytes.extend(event(&json!({"type":"message_stop"})));
    bytes
}

fn read(bytes: &[u8], size: usize) -> (Vec<Value>, Result<StopReason, Failure>) {
    let mut reader = Reader::default();
    let mut seen = Vec::new();
    for chunk in bytes.chunks(size) {
        match reader.feed(chunk, &mut |event| seen.push(event.value().clone())) {
            Ok(Progress::More) => {}
            Ok(Progress::Completed) => break,
            Err(error) => return (seen, Err(error)),
        }
    }
    (seen, reader.finish())
}

#[test]
fn text_tool_and_usage_replay_in_order_across_utf8_and_crlf_splits() {
    let expected = vec![
        json!({"type":"message_start","message":{"id":"msg_synthetic","type":"message","role":"assistant","model":"synthetic-model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"wörld 🙂"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_synthetic","name":"synthetic_lookup","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"synthetic\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":19}}),
        json!({"type":"message_stop"}),
    ];
    let crlf = std::str::from_utf8(REPLAY).unwrap().replace('\n', "\r\n");
    for bytes in [REPLAY, crlf.as_bytes()] {
        for size in [bytes.len(), 1, 7] {
            let (seen, end) = read(bytes, size);
            assert_eq!(seen, expected, "chunks of {size}");
            assert_eq!(end.unwrap(), StopReason::ToolUse);
        }
    }
}

#[test]
fn each_successful_stop_reason_needs_message_stop() {
    for (reason, expected) in [
        ("end_turn", StopReason::EndTurn),
        ("tool_use", StopReason::ToolUse),
        ("stop_sequence", StopReason::StopSequence),
    ] {
        let mut reader = Reader::default();
        let delta = json!({"type":"message_delta","delta":{"stop_reason":reason}});
        let stop = json!({"type":"message_stop"});
        let mut seen = Vec::new();
        assert_eq!(
            reader
                .feed(&event(&delta), &mut |e| seen.push(e.value().clone()))
                .unwrap(),
            Progress::More
        );
        assert_eq!(seen.as_slice(), std::slice::from_ref(&delta));
        assert_eq!(
            reader
                .feed(&event(&stop), &mut |e| seen.push(e.value().clone()))
                .unwrap(),
            Progress::Completed
        );
        assert_eq!(seen, [delta, stop]);
        assert_eq!(reader.finish().unwrap(), expected);
    }
}

#[test]
fn eof_before_terminal_is_truncated_and_stays_failed() {
    let text = event(
        &json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"Partial"}}),
    );
    let delta = event(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}));
    let terminal = ending("end_turn");
    for bytes in [
        b"".as_slice(),
        text.as_slice(),
        delta.as_slice(),
        &terminal[..terminal.len() - 1],
    ] {
        let mut reader = Reader::default();
        assert_eq!(reader.feed(bytes, &mut |_| {}).unwrap(), Progress::More);
        for _ in 0..2 {
            assert!(matches!(reader.finish(), Err(Failure::Truncated)));
            assert!(matches!(
                reader.feed(&terminal, &mut |_| panic!("EOF cannot be rescued")),
                Err(Failure::Truncated)
            ));
        }
    }
}

#[test]
fn refusal_and_exhaustion_fail_before_a_valid_terminal_can_rescue_them() {
    for reason in ["max_tokens", "model_context_window_exceeded", "refusal"] {
        let mut reader = Reader::default();
        let mut bytes = ending(reason);
        bytes.extend(ending("end_turn"));
        let mut forwarded = Vec::new();
        let error = reader.feed(&bytes, &mut |event| forwarded.push(event.value().clone()));
        assert_eq!(forwarded, [json!({"type":"message_delta","delta":{"stop_reason":reason}})]);
        if reason == "refusal" {
            assert!(matches!(error, Err(Failure::Refused)));
            assert!(matches!(reader.finish(), Err(Failure::Refused)));
            assert!(matches!(
                reader.feed(&ending("end_turn"), &mut |_| {}),
                Err(Failure::Refused)
            ));
        } else {
            assert!(matches!(error, Err(Failure::Exhausted)));
            assert!(matches!(reader.finish(), Err(Failure::Exhausted)));
            assert!(matches!(
                reader.feed(&ending("end_turn"), &mut |_| {}),
                Err(Failure::Exhausted)
            ));
        }
    }
}

#[test]
fn nonterminal_message_deltas_preserve_the_recorded_stop_reason() {
    for update in [
        json!({"type":"message_delta","usage":{"output_tokens":2}}),
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":3}}),
        json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":4}}),
    ] {
        let reason = json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}});
        let stop = json!({"type":"message_stop"});
        let expected = vec![update.clone(), reason, update.clone(), stop];
        let bytes: Vec<_> = expected.iter().flat_map(event).collect();
        for size in [bytes.len(), 1, 7] {
            let (seen, result) = read(&bytes, size);
            assert_eq!(seen, expected);
            assert_eq!(result.unwrap(), StopReason::ToolUse);
        }

        let mut reader = Reader::default();
        let mut seen = Vec::new();
        assert_eq!(
            reader
                .feed(&event(&update), &mut |e| seen.push(e.value().clone()))
                .unwrap(),
            Progress::More
        );
        assert_eq!(seen, [update]);
        assert!(matches!(
            reader.feed(&event(&json!({"type":"message_stop"})), &mut |_| {
                panic!("a stop without a recorded reason is not forwarded");
            }),
            Err(Failure::Protocol)
        ));
        assert!(matches!(
            reader.feed(&ending("end_turn"), &mut |_| panic!("failure is sticky")),
            Err(Failure::Protocol)
        ));
    }
}

#[test]
fn malformed_events_and_missing_mistyped_or_unsupported_stop_reasons_fail_closed() {
    for value in [
        json!(null),
        json!({}),
        json!({"type":7}),
        json!({"type":"content_block_delta","delta":{}}),
        json!({"type":"content_block_delta","delta":{"type":"text_delta","text":7}}),
        json!({"type":"message_delta","delta":null}),
        json!({"type":"message_delta","delta":7}),
        json!({"type":"message_delta","delta":"mistyped"}),
        json!({"type":"message_delta","delta":{"stop_reason":7}}),
        json!({"type":"message_delta","delta":{"stop_reason":"future_reason"}}),
        json!({"type":"message_stop"}),
    ] {
        let mut reader = Reader::default();
        let mut bytes = event(&value);
        bytes.extend(ending("end_turn"));
        assert!(
            matches!(reader.feed(&bytes, &mut |_| {}), Err(Failure::Protocol)),
            "{value}"
        );
        assert!(matches!(reader.finish(), Err(Failure::Protocol)));
        assert!(matches!(
            reader.feed(&ending("end_turn"), &mut |_| panic!("failure is sticky")),
            Err(Failure::Protocol)
        ));
    }
    assert!(matches!(read(b"data: {\n\n", 1).1, Err(Failure::Protocol)));
}

#[test]
fn error_after_text_stays_failed_on_valid_stop_and_repeated_feed() {
    for size in [ERROR.len(), 1, 7] {
        let (seen, end) = read(ERROR, size);
        assert_eq!(
            seen,
            [
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}})
            ]
        );
        let Failure::Provider(error) = end.unwrap_err() else {
            panic!("the streamed error must retain its provider category");
        };
        assert_eq!(error.kind(), Kind::Overloaded);
        assert_eq!(error.status(), 200);
    }
    let mut reader = Reader::default();
    assert!(matches!(
        reader.feed(ERROR, &mut |_| {}),
        Err(Failure::Provider(_))
    ));
    let stop = ending("end_turn");
    for bytes in [stop.as_slice(), ERROR, b""] {
        let Failure::Provider(error) = reader
            .feed(bytes, &mut |_| panic!("failure emits nothing else"))
            .unwrap_err()
        else {
            panic!("failure must stay failed");
        };
        assert_eq!(error.kind(), Kind::Overloaded);
        assert_eq!(error.status(), 200);
    }
    let Failure::Provider(error) = reader.finish().unwrap_err() else {
        panic!("finish must preserve the provider error");
    };
    assert_eq!(error.kind(), Kind::Overloaded);
}

#[test]
fn completion_ignores_trailing_bytes_and_every_later_feed() {
    let mut reader = Reader::default();
    let mut bytes = ending("stop_sequence");
    bytes.extend_from_slice(ERROR);
    let mut seen = Vec::new();
    assert_eq!(
        reader
            .feed(&bytes, &mut |e| seen.push(e.value().clone()))
            .unwrap(),
        Progress::Completed
    );
    assert_eq!(
        seen,
        [
            json!({"type":"message_delta","delta":{"stop_reason":"stop_sequence"}}),
            json!({"type":"message_stop"})
        ]
    );
    for bytes in [ERROR, b"data: not JSON\n\n", b""] {
        assert_eq!(
            reader
                .feed(bytes, &mut |_| panic!("completion emits nothing else"))
                .unwrap(),
            Progress::Completed
        );
        assert_eq!(reader.finish().unwrap(), StopReason::StopSequence);
    }
}

#[test]
fn reader_enforces_line_and_multiline_event_caps() {
    for extra in [0, 1] {
        let mut line = vec![b'a'; MAX_LINE_BYTES + extra];
        line[0] = b':';
        line.push(b'\n');
        line.extend(ending("end_turn"));
        let first = 600_000;
        let header = b"{\"type\":\"ping\"}";
        let mut joined = b"data: ".to_vec();
        joined.extend_from_slice(header);
        joined.resize(6 + first, b' ');
        joined.extend_from_slice(b"\ndata: ");
        joined.extend(vec![b' '; MAX_EVENT_BYTES + extra - first - 1]);
        joined.extend_from_slice(b"\n\n");
        joined.extend(ending("end_turn"));
        for bytes in [&line, &joined] {
            for size in [bytes.len(), 64 * 1024] {
                let (seen, end) = read(bytes, size);
                if extra == 0 {
                    assert_eq!(end.unwrap(), StopReason::EndTurn);
                    assert_eq!(
                        seen,
                        [
                            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                            json!({"type":"message_stop"})
                        ]
                    );
                } else {
                    assert!(matches!(end, Err(Failure::Protocol)));
                    assert_eq!(seen, Vec::<Value>::new());
                }
            }
        }
    }
}

#[test]
fn ignored_ping_and_unknown_events_still_count_toward_stream_cap() {
    for kind in ["ping", "synthetic_future_event"] {
        let mut padding = event(&json!({"type":kind}));
        padding.resize(padding.len() - 2, b' ');
        padding.extend(vec![b' '; 64 * 1024 - padding.len() - 2]);
        padding.extend_from_slice(b"\n\n");
        let stop = ending("end_turn");
        let mut bytes = padding.repeat((MAX_STREAM_BYTES - stop.len()) / padding.len());
        let remaining = MAX_STREAM_BYTES - stop.len() - bytes.len();
        bytes.extend(vec![b'\n'; remaining]);
        bytes.extend_from_slice(&stop);
        assert_eq!(bytes.len(), MAX_STREAM_BYTES);
        for size in [bytes.len(), 64 * 1024] {
            let (seen, end) = read(&bytes, size);
            assert_eq!(end.unwrap(), StopReason::EndTurn);
            assert_eq!(
                seen,
                [
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                    json!({"type":"message_stop"})
                ]
            );
        }
        bytes.insert(0, b'\n');
        for size in [bytes.len(), 64 * 1024] {
            let (seen, end) = read(&bytes, size);
            assert!(matches!(end, Err(Failure::Protocol)));
            assert_eq!(
                seen,
                [json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}})]
            );
        }
    }
}

#[test]
fn only_text_deltas_spend_the_visible_text_cap() {
    for extra in [0, 1] {
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend(event(&json!({"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"x".repeat(MAX_TEXT_BYTES / 2)}})));
        }
        for text in [
            "é".repeat(MAX_TEXT_BYTES / 4),
            "a".repeat(MAX_TEXT_BYTES / 2 + extra),
        ] {
            bytes.extend(event(
                &json!({"type":"content_block_delta","delta":{"type":"text_delta","text":text}}),
            ));
        }
        bytes.extend(ending("end_turn"));
        for size in [bytes.len(), 64 * 1024] {
            let (seen, end) = read(&bytes, size);
            if extra == 0 {
                assert_eq!(end.unwrap(), StopReason::EndTurn);
                assert_eq!(seen.len(), 7);
                assert_eq!(
                    seen[3]["delta"]["text"].as_str().unwrap().len(),
                    MAX_TEXT_BYTES / 2
                );
                assert_eq!(
                    seen[4]["delta"]["text"].as_str().unwrap().len(),
                    MAX_TEXT_BYTES / 2
                );
            } else {
                assert!(matches!(end, Err(Failure::Protocol)));
                assert_eq!(seen.len(), 4);
            }
        }
    }
}

#[test]
fn outer_reader_debug_never_exposes_unknown_types_or_error_bodies() {
    let marker = "PRIVATE_READER_MARKER";
    let unknown = event(&json!({"type":marker,"payload":marker}));
    let partial = format!("data: {{\"type\":\"{marker}");
    let mut reader = Reader::default();
    assert_eq!(
        reader
            .feed(&unknown, &mut |_| panic!("unknown types are ignored"))
            .unwrap(),
        Progress::More
    );
    let open = format!("{reader:?}");
    assert_eq!(
        reader.feed(partial.as_bytes(), &mut |_| {}).unwrap(),
        Progress::More
    );
    let buffered = format!("{reader:?}");
    let mut reader = Reader::default();
    let body = event(
        &json!({"type":"error","error":{"type":marker,"message":marker},"request_id":marker}),
    );
    let error = reader.feed(&body, &mut |_| {}).unwrap_err();
    assert!(matches!(&error, Failure::Provider(error) if error.kind() == Kind::Other));
    let bytes = format!("{:?}", marker.as_bytes());
    for formatted in [
        open,
        buffered,
        format!("{reader:?}"),
        format!("{error:?}"),
        format!("{error}"),
    ] {
        assert!(!formatted.contains(marker), "provider text is redacted");
        assert!(
            !formatted.contains(bytes.trim_matches(['[', ']'])),
            "provider bytes are redacted"
        );
    }
}
