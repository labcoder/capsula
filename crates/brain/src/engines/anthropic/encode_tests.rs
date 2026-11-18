//! The Messages body: exact bodies for synthetic conversations, the stable cached prefix, every
//! refusal and the size bound. No provider recording or credential.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use serde_json::{Value, json};

use super::{BodyError, Conversation, body, next};
use crate::connections::model::sse::{Progress, Reader};
use crate::context::{BOUND, ConnectionTag, InputItem, Jpeg, ModelRequest, Role, ToolSchema};
use crate::engines::anthropic::{Mapper, Reply};
use crate::engines::{CallId, Opaque};

const TAG: ConnectionTag = ConnectionTag(41);

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

fn call(id: &str, name: &str, arguments: &str) -> InputItem {
    InputItem::FunctionCall {
        call: CallId::new(id).expect("ascii id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn output(id: &str, output: &str) -> InputItem {
    InputItem::FunctionOutput {
        call: CallId::new(id).expect("ascii id"),
        output: output.to_owned(),
    }
}

fn native(block: &Value) -> InputItem {
    InputItem::Native(Opaque::new(serde_json::to_vec(block).expect("json")).expect("small item"))
}

fn draw_use(id: &str) -> InputItem {
    native(&json!({"type": "tool_use", "id": id, "name": "draw", "input": {"ops": [id]}}))
}

fn request(input: Vec<InputItem>) -> ModelRequest {
    ModelRequest {
        choice: Some(ModelChoice {
            model: ModelSlug::new("claude-sonnet-5-5").expect("slug"),
            effort: Effort::Low,
            connection: ConnectionKind::AnthropicKey,
        }),
        instructions: "rules".to_owned(),
        input,
        tools: vec![ToolSchema {
            name: "draw",
            description: "Draws.",
            parameters: json!({"type": "object", "properties": {"cache_control": {"type": "string"}}}),
        }],
    }
}

/// The body's JSON, once the transport's envelope has taken it.
#[track_caller]
fn built(request: &ModelRequest) -> Value {
    sent(&body(request, TAG).expect("a body").1)
}

/// The body a conversation sends, read back.
fn sent(conversation: &Conversation) -> Value {
    serde_json::from_slice(&conversation.wire().expect("json")).expect("a body")
}

fn error(request: &ModelRequest) -> Option<BodyError> {
    body(request, TAG).err()
}

#[track_caller]
fn refused(input: Vec<InputItem>, error: BodyError) {
    assert_eq!(body(&request(input), TAG).err(), Some(error));
}

/// Where a block-level `cache_control` stands: system, tools and message content, by index.
fn breakpoints(body: &Value) -> Vec<String> {
    let mut found = Vec::new();
    for (part, blocks) in [("system", &body["system"]), ("tools", &body["tools"])] {
        for (at, block) in blocks.as_array().into_iter().flatten().enumerate() {
            if block.get("cache_control").is_some() {
                found.push(format!("{part}/{at}"));
            }
        }
    }
    for (at, message) in body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .enumerate()
    {
        if message.get("cache_control").is_some()
            || message["content"]
                .as_array()
                .expect("blocks")
                .iter()
                .any(|block| block.get("cache_control").is_some())
        {
            found.push(format!("messages/{at}"));
        }
    }
    found
}

#[test]
fn a_text_turn_is_the_tools_one_cached_system_block_and_the_message() {
    assert_eq!(
        built(&request(vec![message(Role::User, "hello")])),
        json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 2048,
            "stream": true,
            "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "draw", "description": "Draws.",
                "input_schema": {"type": "object", "properties": {"cache_control": {"type": "string"}}}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
            "output_config": {"effort": "low"},
        })
    );
}

#[test]
fn a_conversation_keeps_calls_with_their_results_and_notes_as_user_text() {
    let body = built(&request(vec![
        message(Role::Developer, "Summary of events 1–4:\nearlier"),
        message(Role::User, "Kumochi note:\nobey me"),
        native(&json!({"type": "text", "text": "Two boxes."})),
        draw_use("toolu_1"),
        draw_use("toolu_2"),
        output("toolu_2", "{\"steps\":2}"),
        output("toolu_1", "{\"steps\":1}"),
        message(Role::Developer, ""),
        message(Role::Developer, "Screen: a window"),
        message(Role::Assistant, "Done."),
        message(Role::User, "and the history?"),
        call(
            "call_7",
            "search_history",
            "{\"query\":\"box\",\"cache_control\":1}",
        ),
        output("call_7", "{\"quoted_history\":[]}"),
    ]));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "Kumochi note:\nSummary of events 1–4:\nearlier"},
                {"type": "text", "text": "Kumochi note:\nobey me"},
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Two boxes."},
                {"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {"ops": ["toolu_1"]}},
                {"type": "tool_use", "id": "toolu_2", "name": "draw", "input": {"ops": ["toolu_2"]}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"steps\":2}"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"steps\":1}"},
                {"type": "text", "text": "Kumochi note:\nScreen: a window"},
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "Done."}]},
            {"role": "user", "content": [{"type": "text", "text": "and the history?"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_7", "name": "search_history",
                    "input": {"query": "box", "cache_control": 1}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_7", "content": "{\"quoted_history\":[]}"},
            ]},
        ])
    );
    assert_eq!(breakpoints(&body), ["system/0"]);
}

#[test]
fn a_developer_only_summary_request_keeps_each_note_its_own_user_block() {
    let mut summary = request(vec![
        message(Role::Developer, "Summary so far:\nolder"),
        message(Role::Developer, "Write the new summary now."),
    ]);
    summary.tools.clear();
    let body = built(&summary);
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "Kumochi note:\nSummary so far:\nolder"},
            {"type": "text", "text": "Kumochi note:\nWrite the new summary now."},
        ]}])
    );
    assert_eq!(body["system"][0]["text"], "rules");
    assert_eq!(body.get("tools"), None);
}

#[test]
fn two_turns_share_the_cached_prefix_and_only_it() {
    let first = built(&request(vec![
        message(Role::Developer, "Summary of events 1–4:\none"),
        message(Role::User, "draw"),
        draw_use("toolu_1"),
        output("toolu_1", "{\"steps\":1}"),
    ]));
    let second = built(&request(vec![
        message(Role::Developer, "Summary of events 1–9:\ntwo"),
        message(Role::User, "draw again"),
        draw_use("toolu_5"),
        output("toolu_5", "{\"steps\":3}"),
    ]));
    assert_eq!(first["tools"], second["tools"]);
    assert_eq!(first["system"], second["system"]);
    assert_ne!(first["messages"], second["messages"]);
    assert_eq!(breakpoints(&first), ["system/0"]);
    assert_eq!(breakpoints(&second), ["system/0"]);
}

#[test]
fn each_effort_is_its_messages_name() {
    for (effort, name) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let mut request = request(vec![message(Role::User, "hello")]);
        request.choice.as_mut().expect("choice").effort = effort;
        assert_eq!(built(&request)["output_config"], json!({"effort": name}));
    }
}

#[test]
fn only_a_request_for_the_key_with_instructions_builds() {
    let mut none = request(vec![message(Role::User, "hello")]);
    none.choice = None;
    assert_eq!(error(&none), Some(BodyError::NoModel));
    for connection in [ConnectionKind::ChatGpt, ConnectionKind::ClaudeCode] {
        let mut other = request(vec![message(Role::User, "hello")]);
        other.choice.as_mut().expect("choice").connection = connection;
        assert_eq!(error(&other), Some(BodyError::Route), "{connection:?}");
    }
    let mut bare = request(vec![message(Role::User, "hello")]);
    bare.instructions.clear();
    assert_eq!(error(&bare), Some(BodyError::NoInstructions));
}

#[test]
fn arguments_names_and_native_items_outside_the_closed_set_build_nothing() {
    for arguments in ["", "{", "{} x", "[1]", "\"text\"", "null", "7"] {
        refused(
            vec![
                call("call_1", "search_history", arguments),
                output("call_1", "ok"),
            ],
            BodyError::Arguments,
        );
    }
    for id in ["call.1", "call 1", "call/1"] {
        refused(vec![call(id, "search_history", "{}")], BodyError::Name);
        refused(vec![output(id, "ok")], BodyError::Name);
    }
    let long = "a".repeat(65);
    for id in ["call.1", "call 1", long.as_str()] {
        refused(
            vec![native(
                &json!({"type": "tool_use", "id": id, "name": "draw", "input": {}}),
            )],
            BodyError::Name,
        );
    }
    let longest = "a".repeat(64);
    built(&request(vec![
        message(Role::User, "draw"),
        native(&json!({"type": "tool_use", "id": longest, "name": "draw", "input": {}})),
        output(&longest, "ok"),
    ]));
    refused(
        vec![call("call_1", "search.history", "{}")],
        BodyError::Name,
    );
    refused(
        vec![native(
            &json!({"type": "tool_use", "id": "toolu_1", "name": "", "input": {}}),
        )],
        BodyError::Name,
    );
    for item in [
        json!({"type": "thinking", "thinking": "hm", "signature": "sig"}),
        json!({"type": "redacted_thinking", "data": "x"}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "x"}}),
        json!({"type": "text", "text": "hi", "citations": []}),
        json!({"type": "text", "text": ""}),
        json!({"type": "text", "text": 7}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": []}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "draw"}),
        json!({"text": "no type"}),
        json!({"type": 7}),
        json!(["text"]),
    ] {
        assert_eq!(
            error(&request(vec![native(&item)])),
            Some(BodyError::Native),
            "{item}"
        );
    }
    let image = Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xE0]).expect("a JPEG's start");
    refused(
        vec![
            message(Role::User, "draw"),
            draw_use("toolu_1"),
            output("toolu_1", "ok"),
            message(Role::User, "look"),
            InputItem::Image(image),
        ],
        BodyError::Image,
    );
}

#[test]
fn a_call_without_exactly_its_result_first_in_the_next_user_message_builds_nothing() {
    let cases: [(&str, Vec<InputItem>); 7] = [
        (
            "orphan result",
            vec![message(Role::User, "hi"), output("toolu_1", "ok")],
        ),
        (
            "missing result",
            vec![draw_use("toolu_1"), message(Role::User, "next")],
        ),
        (
            "unanswered at the end",
            vec![message(Role::User, "hi"), draw_use("toolu_1")],
        ),
        (
            "a note before the result",
            vec![
                draw_use("toolu_1"),
                message(Role::Developer, "note"),
                output("toolu_1", "ok"),
            ],
        ),
        (
            "a turn between",
            vec![
                draw_use("toolu_1"),
                message(Role::User, "wait"),
                message(Role::Assistant, "ok"),
                output("toolu_1", "ok"),
            ],
        ),
        (
            "the same id twice",
            vec![
                draw_use("toolu_1"),
                output("toolu_1", "ok"),
                message(Role::Assistant, "again"),
                draw_use("toolu_1"),
                output("toolu_1", "ok"),
            ],
        ),
        (
            "one answer twice, the other none",
            vec![
                draw_use("toolu_1"),
                draw_use("toolu_2"),
                output("toolu_1", "ok"),
                output("toolu_1", "ok"),
            ],
        ),
    ];
    for (case, input) in cases {
        assert_eq!(error(&request(input)), Some(BodyError::Pairing), "{case}");
    }
    refused(
        vec![draw_use("toolu_1"), output("toolu_2", "ok")],
        BodyError::Pairing,
    );
}

#[test]
fn the_bound_takes_its_exact_serialized_size_and_refuses_one_byte_more() {
    let conversation = |filler: &str| {
        request(vec![
            message(Role::User, filler),
            draw_use("toolu_1"),
            output("toolu_1", "{\"steps\":1}"),
        ])
    };
    let size = |value: &Value| serde_json::to_vec(value).expect("json").len();
    let base = size(&built(&conversation("x"))) - 1;
    let room = BOUND - base;
    // A quote is two bytes once escaped, so the filler is mostly escapes.
    let fits = format!("{}{}", "\"".repeat(room / 2), "x".repeat(room % 2));
    let at_bound = built(&conversation(&fits));
    assert_eq!(size(&at_bound), BOUND);
    assert_eq!(
        error(&conversation(&format!("{fits}x"))),
        Some(BodyError::TooLarge)
    );
}

#[test]
fn an_empty_conversation_or_one_ending_with_the_assistant_builds_nothing() {
    refused(Vec::new(), BodyError::Ending);
    refused(vec![message(Role::Developer, "")], BodyError::Ending);
    refused(
        vec![
            message(Role::User, "question"),
            message(Role::Assistant, "partial"),
        ],
        BodyError::Ending,
    );
    refused(
        vec![
            message(Role::User, "question"),
            message(Role::Assistant, "partial"),
            message(Role::User, ""),
        ],
        BodyError::Ending,
    );
}

fn slug(model: &str) -> ModelSlug {
    ModelSlug::new(model).expect("slug")
}

fn first(ask: &str) -> Conversation {
    body(&request(vec![message(Role::User, ask)]), TAG)
        .expect("a body")
        .1
}

/// Feeds `events` to `mapper` as one stream's callbacks.
fn feed(mapper: &mut Mapper, events: &[Value]) {
    for event in events {
        let wire = format!("data: {event}\n\n");
        assert_eq!(
            Reader::default().feed(wire.as_bytes(), &mut |event| mapper.event(event)),
            Ok(Progress::More)
        );
    }
}

fn start(index: u32, block: &Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn delta(index: u32, delta: &Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn stop(index: u32) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn opened() -> Value {
    json!({"type": "message_start", "message": {"usage": {}}})
}

/// The tool input exactly as streamed: a 30-digit integer and a 26-place decimal, which a
/// binary64 parse would round.
const PRECISE: &str = r#"{"n": 123456789012345678901234567890, "d": 0.10000000000000000000000001}"#;

/// A Sonnet 5.5 round, block 1 stopping after block 2: signed empty thinking, a call whose input
/// streams in two raw fragments, a progress note, redacted thinking, text and a second call.
fn signed_round() -> Vec<Value> {
    let (head, tail) = PRECISE.split_at(20);
    vec![
        opened(),
        start(0, &json!({"type": "thinking", "thinking": ""})),
        delta(
            0,
            &json!({"type": "signature_delta", "signature": "c2lnbmVkLTE="}),
        ),
        stop(0),
        start(
            1,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
        ),
        delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": head}),
        ),
        start(
            2,
            &json!({"type": "thinking", "thinking": "Drew one.", "signature": "c2lnbmVkLTI="}),
        ),
        delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": tail}),
        ),
        stop(2),
        stop(1),
        start(3, &json!({"type": "redacted_thinking", "data": "ZW5j"})),
        stop(3),
        start(4, &json!({"type": "text", "text": "And the "})),
        delta(4, &json!({"type": "text_delta", "text": "second."})),
        stop(4),
        start(
            5,
            &json!({"type": "tool_use", "id": "toolu_2", "name": "draw", "input": {}}),
        ),
        stop(5),
    ]
}

/// The reply the events make for a conversation's request, if its mapper yields one.
fn replied(conversation: &Conversation, events: &[Value]) -> Option<Reply> {
    let mut mapper = Mapper::answering(conversation);
    feed(&mut mapper, events);
    mapper.finish(Ok(Progress::Completed));
    mapper.reply()
}

fn results(ids: &[&str]) -> Vec<(CallId, String)> {
    ids.iter()
        .map(|id| {
            (
                CallId::new(id).expect("ascii id"),
                format!("{{\"done\":\"{id}\"}}"),
            )
        })
        .collect()
}

#[test]
fn a_mapped_reply_continues_its_request_with_every_block_as_it_came() {
    let first = first("draw a box");
    let before = first.messages[0].get().to_owned();
    let reply = replied(&first, &signed_round()).expect("a whole reply");
    let sonnet = slug("claude-sonnet-5-5");
    let (outgoing, second) = next(
        first,
        TAG,
        &sonnet,
        reply,
        &results(&["toolu_2", "toolu_1"]),
    )
    .expect("continues");
    let expected = format!(
        concat!(
            r#"{{"role":"assistant","content":["#,
            r#"{{"type":"thinking","thinking":"","signature":"c2lnbmVkLTE="}},"#,
            r#"{{"type":"tool_use","id":"toolu_1","name":"draw","input":{}}},"#,
            r#"{{"type":"thinking","thinking":"Drew one.","signature":"c2lnbmVkLTI="}},"#,
            r#"{{"type":"redacted_thinking","data":"ZW5j"}},"#,
            r#"{{"type":"text","text":"And the second."}},"#,
            r#"{{"type":"tool_use","id":"toolu_2","name":"draw","input":{{}}}}]}}"#,
        ),
        PRECISE
    );
    assert_eq!(second.messages.len(), 3);
    assert_eq!(second.messages[0].get(), before);
    assert_eq!(second.messages[1].get(), expected);
    let wire = String::from_utf8(second.wire().expect("json")).expect("utf-8");
    assert_eq!(
        format!("{outgoing:?}"),
        format!("MessagesBody {{ bytes: {} }}", wire.len())
    );
    assert!(wire.contains(&expected), "{wire}");
    let body = sent(&second);
    assert_eq!(
        body["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"done\":\"toolu_2\"}"},
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"done\":\"toolu_1\"}"},
        ]})
    );
    assert_eq!(breakpoints(&body), ["system/0"]);
    let later = [
        opened(),
        start(
            0,
            &json!({"type": "thinking", "thinking": "", "signature": "c2lnbmVkLTM="}),
        ),
        stop(0),
        start(
            1,
            &json!({"type": "tool_use", "id": "toolu_3", "name": "draw", "input": {}}),
        ),
        stop(1),
    ];
    let reply = replied(&second, &later).expect("a whole reply");
    let kept: Vec<String> = second.messages.iter().map(|m| m.get().to_owned()).collect();
    let (_, third) = next(second, TAG, &sonnet, reply, &results(&["toolu_3"])).expect("continues");
    let now: Vec<&str> = third.messages.iter().map(|m| m.get()).collect();
    assert_eq!(
        now[..3],
        kept.iter().map(String::as_str).collect::<Vec<_>>()[..]
    );
    assert_eq!(
        now[3],
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"c2lnbmVkLTM="},{"type":"tool_use","id":"toolu_3","name":"draw","input":{}}]}"#
    );
}

#[test]
fn interleaved_requests_on_one_sign_in_and_model_each_take_only_their_own_reply() {
    let sonnet = slug("claude-sonnet-5-5");
    let call = |id: &str| {
        vec![
            start(
                0,
                &json!({"type": "tool_use", "id": id, "name": "draw", "input": {}}),
            ),
            stop(0),
        ]
    };
    let both = |a: &Conversation, b: &Conversation| {
        let (mut ma, mut mb) = (Mapper::answering(a), Mapper::answering(b));
        let (sa, sb) = (call("toolu_a"), call("toolu_b"));
        feed(&mut ma, &[opened()]);
        feed(&mut mb, &[opened()]);
        for (ea, eb) in sa.iter().zip(&sb) {
            feed(&mut mb, std::slice::from_ref(eb));
            feed(&mut ma, std::slice::from_ref(ea));
        }
        mb.finish(Ok(Progress::Completed));
        ma.finish(Ok(Progress::Completed));
        (ma.reply().expect("a"), mb.reply().expect("b"))
    };
    let (a, b) = (first("one"), first("two"));
    let (reply_a, reply_b) = both(&a, &b);
    assert_eq!(
        next(b, TAG, &sonnet, reply_a, &results(&["toolu_a"])).err(),
        Some(BodyError::Prefix)
    );
    assert_eq!(
        next(a, TAG, &sonnet, reply_b, &results(&["toolu_b"])).err(),
        Some(BodyError::Prefix)
    );
    let (a, b) = (first("one"), first("two"));
    let (reply_a, reply_b) = both(&a, &b);
    let (_, a) = next(a, TAG, &sonnet, reply_a, &results(&["toolu_a"])).expect("A's own");
    let (_, b) = next(b, TAG, &sonnet, reply_b, &results(&["toolu_b"])).expect("B's own");
    assert_eq!(sent(&a)["messages"][1]["content"][0]["id"], "toolu_a");
    assert_eq!(sent(&b)["messages"][1]["content"][0]["id"], "toolu_b");
    // A rebuild of the same input is another request; so is another sign-in or model.
    let answered = first("one");
    let reply = replied(&answered, &signed_round()).expect("a whole reply");
    assert_eq!(
        next(
            first("one"),
            TAG,
            &sonnet,
            reply,
            &results(&["toolu_1", "toolu_2"])
        )
        .err(),
        Some(BodyError::Prefix)
    );
    for (tag, model) in [
        (ConnectionTag(42), "claude-sonnet-5-5"),
        (TAG, "claude-opus-5-5"),
    ] {
        let conversation = first("one");
        let reply = replied(&conversation, &signed_round()).expect("a whole reply");
        assert_eq!(
            next(
                conversation,
                tag,
                &slug(model),
                reply,
                &results(&["toolu_1", "toolu_2"])
            )
            .err(),
            Some(BodyError::Prefix)
        );
    }
}

#[test]
fn a_failed_partial_or_unrepresentable_stream_yields_no_reply() {
    let call = [
        start(
            0,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
        ),
        stop(0),
    ];
    let with = |blocks: &[Value]| {
        let mut events = vec![opened()];
        events.extend_from_slice(blocks);
        events.extend_from_slice(&call);
        events
    };
    for (case, events) in [
        (
            "unknown block",
            with(&[start(1, &json!({"type": "fallback"})), stop(1)]),
        ),
        (
            "empty text",
            with(&[start(1, &json!({"type": "text", "text": ""})), stop(1)]),
        ),
        (
            "unsigned thinking",
            with(&[
                start(1, &json!({"type": "thinking", "thinking": ""})),
                stop(1),
            ]),
        ),
        (
            "a start-object input",
            with(&[
                start(
                    1,
                    &json!({"type": "tool_use", "id": "toolu_2", "name": "draw", "input": {"n": 1}}),
                ),
                stop(1),
            ]),
        ),
        (
            "a duplicated call",
            with(&[
                start(
                    1,
                    &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
                ),
                stop(1),
            ]),
        ),
        (
            "a block left open",
            with(&[start(1, &json!({"type": "text", "text": "hi"}))]),
        ),
        (
            "a block never seen",
            with(&[start(2, &json!({"type": "text", "text": "hi"})), stop(2)]),
        ),
    ] {
        assert!(replied(&first("draw"), &events).is_none(), "{case}");
    }
    let conversation = first("draw");
    let events = with(&[]);
    let mut partial = Mapper::answering(&conversation);
    feed(&mut partial, &events);
    assert!(partial.reply().is_none(), "before completion");
    assert!(
        partial
            .finish(Err(
                crate::connections::anthropic::error::Failure::Truncated
            ))
            .is_some()
    );
    assert!(partial.reply().is_none(), "after a failure");
    let mut unbound = Mapper::default();
    feed(&mut unbound, &events);
    unbound.finish(Ok(Progress::Completed));
    assert!(unbound.reply().is_none(), "a mapper bound to no request");
    let mut once = Mapper::answering(&conversation);
    feed(&mut once, &events);
    once.finish(Ok(Progress::Completed));
    assert!(once.reply().is_some());
    assert!(once.reply().is_none(), "a reply is taken once");
}

#[test]
fn a_reply_without_a_call_or_its_results_continues_nothing() {
    let sonnet = slug("claude-sonnet-5-5");
    let words = [
        opened(),
        start(0, &json!({"type": "text", "text": "done"})),
        stop(0),
    ];
    let conversation = first("draw");
    let reply = replied(&conversation, &words).expect("a whole reply");
    assert_eq!(
        next(conversation, TAG, &sonnet, reply, &[]).err(),
        Some(BodyError::Pairing)
    );
    let call = [
        opened(),
        start(
            0,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
        ),
        stop(0),
    ];
    for answers in [&[][..], &["toolu_2"], &["toolu_1", "toolu_1"]] {
        let conversation = first("draw");
        let reply = replied(&conversation, &call).expect("a whole reply");
        assert_eq!(
            next(conversation, TAG, &sonnet, reply, &results(answers)).err(),
            Some(BodyError::Pairing),
            "{answers:?}"
        );
    }
    let conversation = first("draw");
    let reply = replied(&conversation, &call).expect("a whole reply");
    let (_, second) = next(conversation, TAG, &sonnet, reply, &results(&["toolu_1"])).expect("ok");
    let reply = replied(&second, &call).expect("a whole reply");
    assert_eq!(
        next(second, TAG, &sonnet, reply, &results(&["toolu_1"])).err(),
        Some(BodyError::Pairing),
        "an id called in an earlier round"
    );
}

#[test]
fn a_completed_run_replays_without_thinking() {
    let body = built(&request(vec![
        message(Role::User, "draw a box"),
        native(&json!({"type": "text", "text": "Here it is."})),
        draw_use("toolu_1"),
        output("toolu_1", "{\"steps\":1}"),
        message(Role::Assistant, "Done."),
        message(Role::User, "and another"),
    ]));
    let sent = serde_json::to_string(&body).expect("json");
    assert!(!sent.contains("thinking"), "{sent}");
    assert!(!sent.contains("signature"), "{sent}");
}

#[test]
fn built_refused_and_replied_carriers_print_no_content() {
    let marker = "sensitive-sentinel";
    let conversation = request(vec![
        message(Role::User, marker),
        call(
            "call_1",
            "search_history",
            &format!("{{\"query\":\"{marker}\"}}"),
        ),
        output("call_1", marker),
    ]);
    let built = body(&conversation, TAG);
    assert!(built.is_ok());
    let mut refused = conversation;
    refused.input.push(message(Role::Assistant, marker));
    let refused = body(&refused, TAG);
    assert_eq!(refused.as_ref().err(), Some(&BodyError::Ending));
    let conversation = first(marker);
    let mut mapper = Mapper::answering(&conversation);
    feed(
        &mut mapper,
        &[
            opened(),
            start(
                0,
                &json!({"type": "thinking", "thinking": marker, "signature": marker}),
            ),
            stop(0),
            start(1, &json!({"type": "text", "text": marker})),
            stop(1),
            start(
                2,
                &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
            ),
            delta(
                2,
                &json!({"type": "input_json_delta", "partial_json": format!("{{\"m\":\"{marker}\"}}")}),
            ),
            stop(2),
        ],
    );
    mapper.finish(Ok(Progress::Completed));
    let streaming = format!("{mapper:?}");
    let reply = mapper.reply().expect("a whole reply");
    let debug = format!("{built:?} {refused:?} {conversation:?} {streaming} {reply:?}");
    let continued = next(
        conversation,
        TAG,
        &slug("claude-sonnet-5-5"),
        reply,
        &results(&["toolu_1"]),
    );
    let debug = format!("{debug} {continued:?}");
    assert!(continued.is_ok());
    assert!(!debug.contains(marker), "text leaked: {debug}");
    assert!(
        !debug.contains(&format!("{:?}", marker.as_bytes())),
        "bytes leaked: {debug}"
    );
}
