//! The Messages body: exact bodies for synthetic conversations, the stable cached prefix, every
//! refusal and the size bound. No provider recording or credential.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use serde_json::{Value, json};

use super::{BodyError, Conversation, Round, body, next};
use crate::context::{BOUND, ConnectionTag, InputItem, Jpeg, ModelRequest, Role, ToolSchema};
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
    body(request, TAG).expect("a body").1.body
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

fn item(block: &Value) -> Opaque {
    Opaque::new(serde_json::to_vec(block).expect("json")).expect("small item")
}

fn slug(model: &str) -> ModelSlug {
    ModelSlug::new(model).expect("slug")
}

fn first() -> Conversation {
    body(&request(vec![message(Role::User, "draw a box")]), TAG)
        .expect("a body")
        .1
}

/// A Sonnet 5.5 reply with omitted thinking: a signed empty block before each call, as the
/// migration guide describes, and a redacted one.
fn signed_reply() -> Vec<Value> {
    vec![
        json!({"type": "thinking", "thinking": "", "signature": "c2lnbmVkLTE="}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {"ops": [1]}}),
        json!({"type": "thinking", "thinking": "Drew one.", "signature": "c2lnbmVkLTI="}),
        json!({"type": "redacted_thinking", "data": "ZW5jcnlwdGVk"}),
        json!({"type": "text", "text": "And the second."}),
        json!({"type": "tool_use", "id": "toolu_2", "name": "draw", "input": {"ops": [2]}}),
    ]
}

fn round(answered: &Conversation, blocks: &[Value], results: &[&str]) -> Round {
    Round {
        answered: answered.prefix(),
        blocks: blocks.iter().map(item).collect(),
        results: results
            .iter()
            .map(|id| {
                (
                    CallId::new(id).expect("ascii id"),
                    format!("{{\"done\":\"{id}\"}}"),
                )
            })
            .collect(),
    }
}

#[test]
fn a_signed_reply_continues_the_exact_request_with_its_blocks_unchanged() {
    let first = first();
    let sent = first.body.clone();
    let reply = signed_reply();
    let answer = round(&first, &reply, &["toolu_2", "toolu_1"]);
    let (_, second) = next(first, TAG, &slug("claude-sonnet-5-5"), &answer).expect("continues");
    let mut expected = sent;
    let messages = expected["messages"].as_array_mut().expect("messages");
    messages.push(json!({"role": "assistant", "content": reply}));
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"done\":\"toolu_2\"}"},
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"done\":\"toolu_1\"}"},
    ]}));
    assert_eq!(second.body, expected);
    assert_eq!(breakpoints(&second.body), ["system/0"]);
    let later = [
        json!({"type": "thinking", "thinking": "", "signature": "c2lnbmVkLTM="}),
        json!({"type": "tool_use", "id": "toolu_3", "name": "draw", "input": {}}),
    ];
    let answer = round(&second, &later, &["toolu_3"]);
    let prefix = second.body["messages"].clone();
    let (_, third) = next(second, TAG, &slug("claude-sonnet-5-5"), &answer).expect("continues");
    let messages = third.body["messages"].as_array().expect("messages");
    assert_eq!(messages[..3], prefix.as_array().expect("messages")[..]);
    assert_eq!(messages[3]["content"], json!(later));
}

#[test]
fn a_reply_to_another_request_sign_in_or_model_continues_nothing() {
    let reply = signed_reply();
    let results = ["toolu_1", "toolu_2"];
    let sonnet = slug("claude-sonnet-5-5");
    let answered = first();
    // The same input rebuilt, and one whose drawing digest moved: either is another prefix.
    for rebuilt in [
        request(vec![message(Role::User, "draw a box")]),
        request(vec![
            message(Role::Developer, "Drawings: one box"),
            message(Role::User, "draw a box"),
        ]),
    ] {
        let rebuilt = body(&rebuilt, TAG).expect("a body").1;
        assert_eq!(
            next(rebuilt, TAG, &sonnet, &round(&answered, &reply, &results)).err(),
            Some(BodyError::Prefix)
        );
    }
    let conversation = first();
    let answer = round(&conversation, &reply, &results);
    assert_eq!(
        next(conversation, ConnectionTag(42), &sonnet, &answer).err(),
        Some(BodyError::Prefix)
    );
    let conversation = first();
    let answer = round(&conversation, &reply, &results);
    assert_eq!(
        next(conversation, TAG, &slug("claude-opus-5-5"), &answer).err(),
        Some(BodyError::Prefix)
    );
}

#[test]
fn a_reply_outside_the_signed_set_or_unpaired_continues_nothing() {
    let call = json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}});
    let sonnet = slug("claude-sonnet-5-5");
    for (block, error) in [
        (
            json!({"type": "thinking", "thinking": "", "signature": ""}),
            BodyError::Native,
        ),
        (
            json!({"type": "thinking", "thinking": ""}),
            BodyError::Native,
        ),
        (
            json!({"type": "thinking", "thinking": 7, "signature": "s"}),
            BodyError::Native,
        ),
        (
            json!({"type": "thinking", "thinking": "", "signature": "s", "cache_control": {}}),
            BodyError::Native,
        ),
        (
            json!({"type": "redacted_thinking", "data": ""}),
            BodyError::Native,
        ),
        (
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}}),
            BodyError::Native,
        ),
        (
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
            BodyError::Native,
        ),
    ] {
        let conversation = first();
        let answer = round(&conversation, &[block.clone(), call.clone()], &["toolu_1"]);
        assert_eq!(
            next(conversation, TAG, &sonnet, &answer).err(),
            Some(error),
            "{block}"
        );
    }
    let thinking = json!({"type": "thinking", "thinking": "", "signature": "s"});
    for (blocks, results) in [
        (vec![thinking.clone()], vec![]),
        (vec![thinking, call.clone()], vec![]),
        (vec![call.clone()], vec!["toolu_2"]),
        (vec![call.clone()], vec!["toolu_1", "toolu_1"]),
    ] {
        let conversation = first();
        let answer = round(&conversation, &blocks, &results);
        assert_eq!(
            next(conversation, TAG, &sonnet, &answer).err(),
            Some(BodyError::Pairing),
            "{blocks:?} {results:?}"
        );
    }
    let conversation = first();
    let answer = round(&conversation, std::slice::from_ref(&call), &["toolu_1"]);
    let (_, second) = next(conversation, TAG, &sonnet, &answer).expect("continues");
    let answer = round(&second, &[call], &["toolu_1"]);
    assert_eq!(
        next(second, TAG, &sonnet, &answer).err(),
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
fn built_and_refused_bodies_print_no_content() {
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
    let conversation = first();
    let answer = round(
        &conversation,
        &[json!({"type": "thinking", "thinking": marker, "signature": marker})],
        &[],
    );
    let debug = format!("{built:?} {refused:?} {conversation:?} {answer:?}");
    assert!(!debug.contains(marker), "text leaked: {debug}");
    assert!(
        !debug.contains(&format!("{:?}", marker.as_bytes())),
        "bytes leaked: {debug}"
    );
}
