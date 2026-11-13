//! The Responses body as data, and recorded rounds mapped to turn events.

use kumochi_protocol::message::{Effort, ModelChoice, ModelSlug};
use serde_json::{Value, json};

use super::{BodyError, ROUND_BYTES, ROUND_ITEMS, Round, body};
use crate::connections::model::error::{Code, Failure, ProviderError};
use crate::connections::model::sse::Reader;
use crate::context::{BOUND, InputItem, ModelRequest, Role, ToolSchema};
use crate::engines::{CallId, EngineFailure, Finish, Opaque, ToolName, TurnEvent};

const TOOL_ROUND: &[u8] = include_bytes!("../../../fixtures/responses/tool_round.sse");
const TEXT: &[u8] = include_bytes!("../../../fixtures/responses/text.sse");
const FAILED: &[u8] = include_bytes!("../../../fixtures/responses/failed_usage_limit.sse");

fn call_id(id: &str) -> CallId {
    CallId::new(id).expect("short ASCII")
}

fn request(instructions: &str, input: Vec<InputItem>) -> ModelRequest {
    ModelRequest {
        choice: Some(ModelChoice {
            model: ModelSlug::new("gpt-fixture").expect("slug"),
            effort: Effort::XHigh,
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        }),
        instructions: instructions.to_owned(),
        input,
        tools: vec![ToolSchema {
            name: "draw",
            description: "Draws on the overlay.",
            parameters: json!({ "type": "object" }),
        }],
    }
}

#[test]
fn the_body_is_the_request_as_data() {
    let reasoning = br#"{"type":"reasoning","id":"rs_1","encrypted_content":"e30="}"#;
    let input = vec![
        InputItem::Message {
            role: Role::Developer,
            text: "Ground rules".to_owned(),
        },
        InputItem::Message {
            role: Role::User,
            text: "Draw \"a\"".to_owned(),
        },
        InputItem::Native(Opaque::new(reasoning.to_vec()).expect("small")),
        InputItem::FunctionCall {
            call: call_id("call_1"),
            name: "draw".to_owned(),
            arguments: "{}".to_owned(),
        },
        InputItem::FunctionOutput {
            call: call_id("call_1"),
            output: "drawn".to_owned(),
        },
        InputItem::Message {
            role: Role::Assistant,
            text: "Done.".to_owned(),
        },
    ];
    let expected = json!({
        "model": "gpt-fixture",
        "instructions": "Be brief.",
        "input": [
            { "type": "message", "role": "developer", "content": "Ground rules" },
            { "type": "message", "role": "user", "content": "Draw \"a\"" },
            { "type": "reasoning", "id": "rs_1", "encrypted_content": "e30=" },
            { "type": "function_call", "call_id": "call_1", "name": "draw", "arguments": "{}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "drawn" },
            { "type": "message", "role": "assistant", "content": "Done." },
        ],
        "tools": [{
            "type": "function",
            "name": "draw",
            "description": "Draws on the overlay.",
            "parameters": { "type": "object" },
            "strict": false,
        }],
        "reasoning": { "effort": "xhigh" },
        "include": ["reasoning.encrypted_content"],
        "store": false,
        "stream": true,
    });
    assert_eq!(body(&request("Be brief.", input)), Ok(expected));
}

/// A text-only body is pinned byte for byte, so image input can't change what text turns send.
#[test]
fn a_text_only_body_keeps_its_exact_bytes() {
    let input = vec![
        InputItem::Message {
            role: Role::User,
            text: "Hi \"you\"".to_owned(),
        },
        InputItem::Message {
            role: Role::Assistant,
            text: "Hello.".to_owned(),
        },
    ];
    let mut request = request("Be brief.", input);
    request.tools.clear();
    assert_eq!(
        body(&request).expect("small").to_string(),
        concat!(
            r#"{"include":["reasoning.encrypted_content"],"#,
            r#""input":[{"content":"Hi \"you\"","role":"user","type":"message"},"#,
            r#"{"content":"Hello.","role":"assistant","type":"message"}],"#,
            r#""instructions":"Be brief.","model":"gpt-fixture","reasoning":{"effort":"xhigh"},"#,
            r#""store":false,"stream":true,"tools":[]}"#,
        )
    );
}

#[test]
fn the_offered_draw_schema_is_serialized_non_strict_without_changing_other_tools() {
    let mut request = request("", Vec::new());
    request.tools = crate::context::tools(crate::context::Origin::Kumo);
    request.tools.push(ToolSchema {
        name: "focus",
        description: "focus",
        parameters: json!({"type":"object"}),
    });
    let bytes =
        serde_json::to_vec(&body(&request).expect("bounded actual draw schema")).expect("JSON");
    let sent: Value = serde_json::from_slice(&bytes).expect("request body");
    assert_eq!(sent["tools"][0]["name"], json!("draw"));
    assert_eq!(sent["tools"][0]["strict"], json!(false));
    assert_eq!(
        sent["tools"][0]["parameters"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(sent["tools"][1].get("strict"), None);
}

#[test]
fn each_effort_has_its_wire_name() {
    for (effort, name) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let mut request = request("", Vec::new());
        if let Some(choice) = request.choice.as_mut() {
            choice.effort = effort;
        }
        let sent = body(&request).expect("small");
        assert_eq!(sent["reasoning"], json!({ "effort": name }));
    }
}

#[test]
fn a_body_at_the_bound_is_sent_and_one_byte_over_is_refused() {
    let empty = body(&request("", Vec::new()))
        .expect("small")
        .to_string()
        .len();
    let fits = "a".repeat(BOUND - empty);
    assert_eq!(
        body(&request(&fits, Vec::new())).map(|sent| sent.to_string().len()),
        Ok(BOUND)
    );
    let over = "a".repeat(BOUND - empty + 1);
    assert_eq!(body(&request(&over, Vec::new())), Err(BodyError::TooLarge));
}

#[test]
fn a_request_with_no_model_is_refused() {
    let mut request = request("", Vec::new());
    request.choice = None;
    assert_eq!(body(&request), Err(BodyError::NoModel));
}

#[test]
fn a_stored_reasoning_item_that_isnt_json_is_refused() {
    for bad in [&b""[..], b"{\"type\":", b"not json"] {
        let input = vec![InputItem::Native(Opaque::new(bad.to_vec()).expect("small"))];
        assert_eq!(body(&request("", input)), Err(BodyError::Native), "{bad:?}");
    }
}

/// Feeds `stream` in pieces of `chunk` bytes, as a transport would, until the round ends.
fn play(stream: &[u8], chunk: usize) -> Vec<TurnEvent> {
    let mut reader = Reader::default();
    let mut round = Round::default();
    let mut events = Vec::new();
    for bytes in stream.chunks(chunk) {
        let read = reader.feed(bytes, &mut |event| round.event(event));
        events.append(&mut round.take());
        if let Some(end) = round.end(read) {
            events.push(end);
            return events;
        }
    }
    events
}

#[test]
fn wire_synthetic_history_fixture_preserves_the_catalog_call_and_arguments() {
    let stream = include_bytes!("../../../fixtures/responses/history.sse");
    for chunk in [1, 7, 1024] {
        assert_eq!(
            play(stream, chunk),
            vec![
                TurnEvent::ToolCallStarted {
                    call: call_id("history"),
                    name: ToolName::SearchHistory
                },
                TurnEvent::ToolArgsDelta {
                    call: call_id("history"),
                    delta: r#"{"query":"small"}"#.into()
                },
                TurnEvent::ToolCallReady {
                    call: call_id("history"),
                    args: r#"{"query":"small"}"#.into()
                },
                item(
                    0,
                    &json!({"type":"function_call", "id":"fc_history", "call_id":"history", "name":"search_history", "arguments":r#"{"query":"small"}"#})
                ),
                finished(Finish::Completed),
            ]
        );
    }
}

fn item(index: u32, item: &Value) -> TurnEvent {
    TurnEvent::OutputItem {
        index,
        item: Opaque::new(serde_json::to_vec(item).expect("json")).expect("small"),
    }
}

fn text(delta: &str) -> TurnEvent {
    TurnEvent::TextDelta(delta.to_owned())
}

fn finished(finish: Finish) -> TurnEvent {
    TurnEvent::Finished(finish)
}

fn protocol() -> TurnEvent {
    finished(Finish::Failed(EngineFailure::Stream(Failure::Protocol)))
}

#[test]
fn the_fixture_tool_round_maps_in_order_with_the_name_before_any_delta() {
    let call = call_id("call_fixture");
    let expected = vec![
        item(
            0,
            &json!({
                "type": "reasoning",
                "id": "rs_fixture",
                "summary": [],
                "encrypted_content": "ZW5jcnlwdGVkLWZpeHR1cmU=",
            }),
        ),
        TurnEvent::ToolCallStarted {
            call: call.clone(),
            name: ToolName::Draw,
        },
        TurnEvent::ToolArgsDelta {
            call: call.clone(),
            delta: "{\"steps\":".to_owned(),
        },
        TurnEvent::ToolArgsDelta {
            call: call.clone(),
            delta: "[]}".to_owned(),
        },
        TurnEvent::ToolCallReady {
            call,
            args: "{\"steps\":[]}".to_owned(),
        },
        item(
            1,
            &json!({
                "type": "function_call",
                "id": "fc_fixture",
                "call_id": "call_fixture",
                "name": "draw",
                "arguments": "{\"steps\":[]}",
                "status": "completed",
            }),
        ),
        text("Done"),
        text("."),
        item(
            2,
            &json!({
                "type": "message",
                "id": "msg_fixture",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "Done.", "annotations": [] }],
            }),
        ),
        finished(Finish::Completed),
    ];
    for chunk in [TOOL_ROUND.len(), 1, 7] {
        assert_eq!(play(TOOL_ROUND, chunk), expected, "{chunk}-byte reads");
    }
}

#[test]
fn a_text_round_streams_its_text_and_completes() {
    assert_eq!(
        play(TEXT, TEXT.len()),
        [
            text("Hello, "),
            text("w\u{f6}rld \u{1f642}"),
            finished(Finish::Completed)
        ]
    );
}

#[test]
fn a_failed_response_after_text_fails_the_round_with_its_code() {
    let refused = ProviderError {
        status: 200,
        code: Some(Code::UsageLimit),
        param: None,
    };
    assert_eq!(
        play(FAILED, FAILED.len()),
        [
            text("Partial"),
            finished(Finish::Failed(EngineFailure::Stream(Failure::Provider(
                refused
            ))))
        ]
    );
}

#[test]
fn a_stream_cut_before_its_end_has_no_ending_of_its_own() {
    let cut = TOOL_ROUND.len() - 40;
    let events = play(&TOOL_ROUND[..cut], cut);
    assert_eq!(events.len(), 9, "everything up to the cut");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::Finished(_)))
    );
}

/// A stream of `events`, each one `data` line.
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|event| format!("data: {event}\n\n").into_bytes())
        .collect()
}

fn added_as(item: &str, call: &str, name: &str, index: u32) -> Value {
    json!({
        "type": "response.output_item.added",
        "output_index": index,
        "item": {
            "type": "function_call", "id": item, "call_id": call, "name": name, "arguments": "",
        },
    })
}

fn added(item: &str, call: &str) -> Value {
    added_as(item, call, "look", 0)
}

fn args_delta(item: &str, delta: &str) -> Value {
    json!({ "type": "response.function_call_arguments.delta", "item_id": item, "delta": delta })
}

fn args_done(item: &str, arguments: &str) -> Value {
    json!({
        "type": "response.function_call_arguments.done", "item_id": item, "arguments": arguments,
    })
}

fn call_done_as(item: &str, call: &str, name: &str, arguments: &str, index: u32) -> Value {
    json!({
        "type": "response.output_item.done",
        "output_index": index,
        "item": {
            "type": "function_call", "id": item, "call_id": call, "name": name,
            "arguments": arguments,
        },
    })
}

fn call_done(item: &str, call: &str, arguments: &str) -> Value {
    call_done_as(item, call, "look", arguments, 0)
}

fn ready(call: &str, args: &str) -> TurnEvent {
    TurnEvent::ToolCallReady {
        call: call_id(call),
        args: args.to_owned(),
    }
}

fn args_of(call: &str, delta: &str) -> TurnEvent {
    TurnEvent::ToolArgsDelta {
        call: call_id(call),
        delta: delta.to_owned(),
    }
}

fn completed() -> Value {
    json!({ "type": "response.completed", "response": { "status": "completed" } })
}

fn started(call: &str) -> TurnEvent {
    TurnEvent::ToolCallStarted {
        call: call_id(call),
        name: ToolName::Unknown,
    }
}

#[test]
fn an_unknown_tool_name_is_started_as_unknown_and_its_round_completes() {
    let stream = sse(&[
        added("fc_1", "call_1"),
        args_delta("fc_1", "{}"),
        args_done("fc_1", "{}"),
        call_done("fc_1", "call_1", "{}"),
        completed(),
    ]);
    let events = play(&stream, stream.len());
    assert_eq!(events[0], started("call_1"));
    assert_eq!(
        events[2],
        TurnEvent::ToolCallReady {
            call: call_id("call_1"),
            args: "{}".to_owned()
        }
    );
    assert_eq!(events.last(), Some(&finished(Finish::Completed)));
}

#[test]
fn calls_that_break_the_stream_rules_fail_the_round() {
    let longest = "c".repeat(CallId::MAX);
    let too_long = "c".repeat(CallId::MAX + 1);
    let cases: Vec<(&str, Vec<Value>, Vec<TurnEvent>)> = vec![
        (
            "a delta for an item never started",
            vec![args_delta("fc_9", "{}")],
            vec![],
        ),
        (
            "an item id started twice",
            vec![added("fc_1", "call_1"), added("fc_1", "call_2")],
            vec![started("call_1")],
        ),
        (
            "a call id started twice",
            vec![added("fc_1", "call_1"), added("fc_2", "call_1")],
            vec![started("call_1")],
        ),
        (
            "a call id one byte over the limit",
            vec![added("fc_1", &too_long)],
            vec![],
        ),
        (
            "arguments done that differ from the deltas",
            vec![
                added("fc_1", &longest),
                args_delta("fc_1", "{}"),
                args_done("fc_1", "{ }"),
            ],
            vec![
                started(&longest),
                TurnEvent::ToolArgsDelta {
                    call: call_id(&longest),
                    delta: "{}".to_owned(),
                },
            ],
        ),
        (
            "a done call naming another call id",
            vec![added("fc_1", "call_1"), call_done("fc_1", "call_2", "")],
            vec![started("call_1")],
        ),
        (
            "a call done twice",
            vec![
                added("fc_1", "call_1"),
                call_done("fc_1", "call_1", ""),
                call_done("fc_1", "call_1", ""),
            ],
            vec![
                started("call_1"),
                TurnEvent::ToolCallReady {
                    call: call_id("call_1"),
                    args: String::new(),
                },
                item(0, &call_done("fc_1", "call_1", "")["item"]),
            ],
        ),
        (
            "a delta after the call was done",
            vec![
                added("fc_1", "call_1"),
                call_done("fc_1", "call_1", ""),
                args_delta("fc_1", "{}"),
            ],
            vec![
                started("call_1"),
                TurnEvent::ToolCallReady {
                    call: call_id("call_1"),
                    args: String::new(),
                },
                item(0, &call_done("fc_1", "call_1", "")["item"]),
            ],
        ),
        (
            "a completed response with a call still open",
            vec![added("fc_1", "call_1"), completed()],
            vec![started("call_1")],
        ),
        (
            "a done item with no output index",
            vec![json!({ "type": "response.output_item.done", "item": { "type": "reasoning" } })],
            vec![],
        ),
        (
            "a delta that isn't text",
            vec![json!({ "type": "response.output_text.delta", "delta": 7 })],
            vec![],
        ),
    ];
    for (case, events, mut expected) in cases {
        let mut stream = sse(&events);
        stream.extend(sse(&[completed()]));
        expected.push(protocol());
        assert_eq!(play(&stream, stream.len()), expected, "{case}");
    }
}

fn reasoning_done(index: u32) -> Value {
    json!({
        "type": "response.output_item.done",
        "output_index": index,
        "item": { "type": "reasoning", "id": "rs" },
    })
}

#[test]
fn a_round_holds_at_most_its_item_budget() {
    let mut fits: Vec<Value> = (0..u32::try_from(ROUND_ITEMS).expect("small"))
        .map(reasoning_done)
        .collect();
    fits.push(completed());
    let events = play(&sse(&fits), usize::MAX);
    assert_eq!(events.len(), ROUND_ITEMS + 1);
    assert_eq!(events.last(), Some(&finished(Finish::Completed)));
    let one_over = fits.len() - 1;
    fits.insert(one_over, reasoning_done(64));
    let events = play(&sse(&fits), usize::MAX);
    assert_eq!(events.len(), ROUND_ITEMS + 1);
    assert_eq!(events.last(), Some(&protocol()));
}

#[test]
fn a_round_holds_at_most_its_byte_budget() {
    let delta = "a".repeat(ROUND_BYTES / 2);
    let stream = sse(&[
        added("fc_1", "call_1"),
        args_delta("fc_1", &delta),
        args_delta("fc_1", &delta),
        completed(),
    ]);
    let events = play(&stream, usize::MAX);
    assert_eq!(events.len(), 3, "the start and the delta that fit");
    assert_eq!(events.last(), Some(&protocol()));
}

#[test]
fn an_item_over_the_item_cap_fails_the_round() {
    let big = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": { "type": "reasoning", "encrypted_content": "a".repeat(Opaque::MAX) },
    });
    let stream = sse(&[big, completed()]);
    assert_eq!(play(&stream, usize::MAX), [protocol()]);
}

#[test]
fn interleaved_calls_keep_their_own_arguments_and_indexes() {
    let stream = sse(&[
        added_as("fc_a", "call_a", "draw", 0),
        added_as("fc_b", "call_b", "look", 1),
        args_delta("fc_a", "{\"x\":"),
        args_delta("fc_b", "[1"),
        args_delta("fc_a", "1}"),
        args_delta("fc_b", "]"),
        args_done("fc_a", "{\"x\":1}"),
        args_done("fc_b", "[1]"),
        call_done_as("fc_b", "call_b", "look", "[1]", 1),
        call_done_as("fc_a", "call_a", "draw", "{\"x\":1}", 0),
        completed(),
    ]);
    let expected = vec![
        TurnEvent::ToolCallStarted {
            call: call_id("call_a"),
            name: ToolName::Draw,
        },
        started("call_b"),
        args_of("call_a", "{\"x\":"),
        args_of("call_b", "[1"),
        args_of("call_a", "1}"),
        args_of("call_b", "]"),
        ready("call_b", "[1]"),
        item(
            1,
            &json!({
                "type": "function_call", "id": "fc_b", "call_id": "call_b", "name": "look",
                "arguments": "[1]",
            }),
        ),
        ready("call_a", "{\"x\":1}"),
        item(
            0,
            &json!({
                "type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "draw",
                "arguments": "{\"x\":1}",
            }),
        ),
        finished(Finish::Completed),
    ];
    for chunk in [stream.len(), 1, 5] {
        assert_eq!(play(&stream, chunk), expected, "{chunk}-byte reads");
    }
}

#[test]
fn a_call_done_under_another_name_is_refused_before_it_is_ready() {
    let stream = sse(&[
        added_as("fc_1", "call_1", "draw", 0),
        call_done_as("fc_1", "call_1", "look", "", 0),
        completed(),
    ]);
    assert_eq!(
        play(&stream, stream.len()),
        [
            TurnEvent::ToolCallStarted {
                call: call_id("call_1"),
                name: ToolName::Draw,
            },
            protocol()
        ]
    );
}

#[test]
fn a_done_item_that_isnt_a_typed_object_fails_the_round() {
    let shapes = [
        None,
        Some(Value::Null),
        Some(json!(7)),
        Some(json!("reasoning")),
        Some(json!([])),
        Some(json!({})),
        Some(json!({ "type": 5 })),
    ];
    for shape in shapes {
        let mut done = json!({ "type": "response.output_item.done", "output_index": 0 });
        if let Some(item) = shape.clone() {
            done["item"] = item;
        }
        let stream = sse(&[done, completed()]);
        assert_eq!(play(&stream, stream.len()), [protocol()], "{shape:?}");
    }
}

/// `item`'s done event with a `pad` field sized so the item serialises to exactly `size` bytes.
fn padded(mut done: Value, size: usize) -> Value {
    done["item"]["pad"] = json!("");
    let base = serde_json::to_vec(&done["item"]).expect("json").len();
    done["item"]["pad"] = json!("a".repeat(size - base));
    assert_eq!(serde_json::to_vec(&done["item"]).expect("json").len(), size);
    done
}

#[test]
fn a_call_done_item_at_the_item_cap_is_ready_and_one_byte_over_is_refused_whole() {
    let fits = padded(call_done("fc_1", "call_1", ""), Opaque::MAX);
    let stream = sse(&[added("fc_1", "call_1"), fits.clone(), completed()]);
    assert_eq!(
        play(&stream, usize::MAX),
        [
            started("call_1"),
            ready("call_1", ""),
            item(0, &fits["item"]),
            finished(Finish::Completed)
        ]
    );
    let over = padded(call_done("fc_1", "call_1", ""), Opaque::MAX + 1);
    let stream = sse(&[added("fc_1", "call_1"), over, completed()]);
    assert_eq!(play(&stream, usize::MAX), [started("call_1"), protocol()]);
}

#[test]
fn a_call_done_item_is_refused_whole_once_the_round_has_no_item_left() {
    for (others, fits) in [(ROUND_ITEMS - 2, true), (ROUND_ITEMS - 1, false)] {
        let mut events = vec![added("fc_1", "call_1")];
        let mut expected = vec![started("call_1")];
        for index in 1..=u32::try_from(others).expect("small") {
            events.push(reasoning_done(index));
            expected.push(item(index, &reasoning_done(index)["item"]));
        }
        events.push(call_done("fc_1", "call_1", ""));
        events.push(completed());
        if fits {
            expected.push(ready("call_1", ""));
            expected.push(item(0, &call_done("fc_1", "call_1", "")["item"]));
            expected.push(finished(Finish::Completed));
        } else {
            expected.push(protocol());
        }
        assert_eq!(
            play(&sse(&events), usize::MAX),
            expected,
            "{others} other items"
        );
    }
}

#[test]
fn a_call_done_item_is_refused_whole_once_it_would_pass_the_byte_budget() {
    // What the start keeps: "fc_1", "call_1" and "look".
    let started_bytes = 14;
    let call_bytes = serde_json::to_vec(&call_done("fc_1", "call_1", "")["item"])
        .expect("json")
        .len();
    let fill = ROUND_BYTES - started_bytes - call_bytes;
    for (extra, fits) in [(0, true), (1, false)] {
        let mut events = vec![added("fc_1", "call_1")];
        let mut expected = vec![started("call_1")];
        for index in 1..=5 {
            let size = fill / 5 + if index == 5 { fill % 5 + extra } else { 0 };
            let done = padded(reasoning_done(index), size);
            expected.push(item(index, &done["item"]));
            events.push(done);
        }
        events.push(call_done("fc_1", "call_1", ""));
        events.push(completed());
        if fits {
            expected.push(ready("call_1", ""));
            expected.push(item(0, &call_done("fc_1", "call_1", "")["item"]));
            expected.push(finished(Finish::Completed));
        } else {
            expected.push(protocol());
        }
        assert_eq!(play(&sse(&events), usize::MAX), expected, "{extra} over");
    }
}
