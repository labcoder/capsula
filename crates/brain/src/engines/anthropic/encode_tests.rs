//! The Messages body: exact bodies for synthetic conversations, the stable cached prefix, every
//! refusal and the size bound. No provider recording or credential.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use serde_json::{Value, json};

use super::{BodyError, body};
use crate::connections::anthropic::messages::MessagesBody;
use crate::context::{BOUND, InputItem, Jpeg, ModelRequest, Role, ToolSchema};
use crate::engines::{CallId, Opaque};

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

/// The body, once the transport's envelope has taken it.
#[track_caller]
fn built(request: &ModelRequest) -> Value {
    let value = body(request).expect("a body");
    MessagesBody::new(&value).expect("the envelope takes it");
    value
}

#[track_caller]
fn refused(input: Vec<InputItem>, error: BodyError) {
    assert_eq!(body(&request(input)), Err(error));
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
    assert_eq!(body(&none), Err(BodyError::NoModel));
    for connection in [ConnectionKind::ChatGpt, ConnectionKind::ClaudeCode] {
        let mut other = request(vec![message(Role::User, "hello")]);
        other.choice.as_mut().expect("choice").connection = connection;
        assert_eq!(body(&other), Err(BodyError::Route), "{connection:?}");
    }
    let mut bare = request(vec![message(Role::User, "hello")]);
    bare.instructions.clear();
    assert_eq!(body(&bare), Err(BodyError::NoInstructions));
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
            body(&request(vec![native(&item)])),
            Err(BodyError::Native),
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
        assert_eq!(body(&request(input)), Err(BodyError::Pairing), "{case}");
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
        body(&conversation(&format!("{fits}x"))),
        Err(BodyError::TooLarge)
    );
}
