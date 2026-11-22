//! Synthetic envelope fixtures; no provider recording or credentials.

use super::Failure;
use super::MessagesBody;
use crate::connections::model::MAX_REQUEST_BYTES;
use serde_json::{Value, json};
use zeroize::Zeroizing;

fn request() -> Value {
    json!({"model":"claude-sonnet-5-5","messages":[{"role":"user","content":"hello"}],
        "stream":true,"max_tokens":2048,
        "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral"}}]})
}

#[track_caller]
fn refused(value: &Value) {
    assert!(
        matches!(MessagesBody::new(value), Err(Failure::Request)),
        "invalid envelope is refused"
    );
}

#[test]
fn rejects_missing_mistyped_and_unsupported_fields() {
    for key in ["model", "messages", "stream", "max_tokens"] {
        let mut value = request();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            matches!(MessagesBody::new(&value), Err(Failure::Request)),
            "missing field: {key}"
        );
        value[key] = Value::Null;
        assert!(
            matches!(MessagesBody::new(&value), Err(Failure::Request)),
            "null field: {key}"
        );
    }
    for (case, (key, bad)) in [
        ("model", json!("Bad Model")),
        ("model", json!("")),
        ("messages", json!([])),
        ("messages", json!({})),
        ("messages", json!([{"role":"system","content":"hello"}])),
        ("messages", json!([{"role":"user","content":7}])),
        ("messages", json!([{"role":"user","content":["text"]}])),
        ("messages", json!([{"role":"user","content":[{"type":7}]}])),
        ("stream", json!(false)),
        ("stream", json!("true")),
        ("max_tokens", json!(2049)),
        ("max_tokens", json!(2048.0)),
        ("max_tokens", json!("2048")),
        ("store", json!(false)),
        ("thinking", json!({"type":"enabled"})),
        ("unknown", json!(true)),
        ("output_config", json!({})),
        ("output_config", json!({"effort":"extreme"})),
        ("output_config", json!({"effort":1})),
        ("output_config", json!({"effort":"high","extra":false})),
        ("system", json!("instructions")),
        ("system", json!([])),
        ("tools", json!({})),
        ("tools", json!([false])),
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = request();
        value[key] = bad;
        assert!(
            matches!(MessagesBody::new(&value), Err(Failure::Request)),
            "invalid envelope case {case}, field {key}"
        );
    }
    refused(&json!([]));
}

#[test]
fn accepts_only_client_tools() {
    for discriminator in [None, Some(json!("custom"))] {
        let mut value = request();
        value["tools"] = json!([{"name":"draw","input_schema":{"type":"object"}}]);
        if let Some(discriminator) = discriminator {
            value["tools"][0]["type"] = discriminator;
        }
        let body = MessagesBody::new(&value).expect("client tool");
        assert_eq!(
            serde_json::from_slice::<Value>(body.bytes()).unwrap(),
            value
        );
    }
    for discriminator in [
        json!("web_search_20250305"),
        json!("bash_20250124"),
        json!(""),
        json!(null),
        json!(1),
    ] {
        let mut value = request();
        value["tools"] = json!([{"type":discriminator,"name":"draw"}]);
        refused(&value);
    }
}

#[test]
fn cache_belongs_only_to_the_last_stable_prefix_block() {
    let mut message_cache = request();
    message_cache["messages"] = json!([
        {"role":"user","content":"first"},
        {"role":"assistant","content":"second","cache_control":{"type":"ephemeral"}},
        {"role":"user","content":"third"}
    ]);
    refused(&message_cache);
    let mut value = request();
    value["tools"] = json!([{"name":"first"},{"name":"last","cache_control":{"type":"ephemeral"}}]);
    refused(&value);
    value.as_object_mut().unwrap().remove("system");
    let body = MessagesBody::new(&value).expect("last tool is stable prefix end");
    assert_eq!(
        serde_json::from_slice::<Value>(body.bytes()).unwrap(),
        value
    );
    value["tools"][0]["cache_control"] = json!({"type":"ephemeral"});
    refused(&value);
    value["tools"][1]
        .as_object_mut()
        .unwrap()
        .remove("cache_control");
    refused(&value);
    value.as_object_mut().unwrap().remove("tools");
    refused(&value);

    for cache in [
        json!({"type":"persistent"}),
        json!({"type":"ephemeral","ttl":"1h"}),
        json!(null),
    ] {
        let mut value = request();
        value["system"][0]["cache_control"] = cache;
        refused(&value);
    }
    let mut value = request();
    value["system"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"text","text":"suffix"}));
    refused(&value);
    value["system"][1]["cache_control"] = json!({"type":"ephemeral"});
    refused(&value);
    for dynamic in [
        json!([{"type":"text","text":"dynamic","cache_control":{"type":"ephemeral"}}]),
        json!([{"type":"tool_result","tool_use_id":"one","content":[{"type":"text","text":"dynamic","cache_control":{"type":"ephemeral"}}]}]),
    ] {
        let mut value = request();
        value["messages"][0]["content"] = dynamic;
        refused(&value);
    }
}

#[test]
fn serialized_request_bound_is_inclusive_and_debug_is_redacted() {
    let mut value = request();
    value["messages"][0]["content"] = json!("");
    let overhead = serde_json::to_vec(&value).unwrap().len();
    value["messages"][0]["content"] = json!("x".repeat(MAX_REQUEST_BYTES - overhead));
    let body = MessagesBody::new(&value).expect("exact bound");
    assert_eq!(body.bytes().len(), 16 * 1024 * 1024);
    value["messages"][0]["content"] = json!("x".repeat(MAX_REQUEST_BYTES - overhead + 1));
    refused(&value);
    value["messages"][0]["content"] = json!("PRIVATE_REQUEST_MARKER");
    let body = MessagesBody::new(&value).expect("small envelope");
    let formatted = format!("{body:?}");
    assert_eq!(
        formatted,
        format!("MessagesBody {{ bytes: {} }}", body.bytes().len())
    );
    assert!(
        !formatted.contains("PRIVATE_REQUEST_MARKER"),
        "request text is redacted"
    );
    let byte_marker = format!("{:?}", b"PRIVATE_REQUEST_MARKER");
    assert!(
        !formatted.contains(byte_marker.trim_matches(['[', ']'])),
        "request bytes are redacted"
    );
}

#[test]
fn json_is_checked_then_sent_as_written_up_to_the_bound() {
    let written = br#"{"model":"claude-sonnet-5-5","stream":true,"max_tokens":2048,"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"draw","input":{"n": 123456789012345678901234567890, "d": 0.10000000000000000000000001}}]},{"role":"user","content":"next"}]}"#;
    let mut padded = written.to_vec();
    padded.resize(MAX_REQUEST_BYTES, b' ');
    let body = MessagesBody::from_json(Zeroizing::new(padded.clone())).expect("at the bound");
    assert_eq!(body.bytes(), padded.as_slice());
    padded.push(b' ');
    assert!(matches!(
        MessagesBody::from_json(Zeroizing::new(padded)),
        Err(Failure::Request)
    ));

    let mut value = request();
    value["tools"] = json!([{"name":"example","input_schema":{"type":"object","properties":{
        "cache_control":{"type":"string"}}}}]);
    value["messages"] = json!([
        {"role":"user","content":"first"},
        {"role":"assistant","content":[{"type":"tool_use","id":"one","name":"example",
            "input":{"cache_control":{"type":"persistent"},"content":[{"cache_control":false}]}}]},
        {"role":"user","content":"next"}
    ]);
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        value["output_config"] = json!({"effort":effort});
        let written = serde_json::to_vec(&value).expect("fixture JSON");
        let body =
            MessagesBody::from_json(Zeroizing::new(written.clone())).expect("supported envelope");
        assert_eq!(body.bytes(), written.as_slice());
    }

    let uncapped = String::from_utf8_lossy(written).replace("2048", "2049");
    for refused in [uncapped.as_bytes(), &written[..written.len() - 1]] {
        assert!(matches!(
            MessagesBody::from_json(Zeroizing::new(refused.to_vec())),
            Err(Failure::Request)
        ));
    }
}

#[test]
fn json_is_read_as_written_with_no_name_repeated_at_any_depth() {
    let body = |messages: &str| {
        format!(
            r#"{{"model":"claude-sonnet-5-5","stream":true,"max_tokens":2048,"system":[{{"type":"text","text":"s","cache_control":{{"type":"ephemeral"}}}}],"messages":{messages}}}"#
        )
        .into_bytes()
    };
    let sound = body(
        r#"[{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x"}],"note":{"$serde_json::private::RawValue":"1"}}]"#,
    );
    assert_eq!(
        MessagesBody::from_json(Zeroizing::new(sound.clone()))
            .expect("a literal envelope")
            .bytes(),
        sound.as_slice()
    );
    for (case, messages) in [
        (
            "a repeated top name",
            r#"[{"role":"user","content":"a"}],"messages":[{"role":"user","content":"b"}]"#,
        ),
        (
            "a repeated deep name",
            r#"[{"role":"user","content":[{"type":"text","text":"a","text":"b"}]}]"#,
        ),
        (
            "an embedded raw value",
            r#"[{"$serde_json::private::RawValue":"{\"role\":\"user\",\"content\":\"hi\"}"}]"#,
        ),
    ] {
        assert!(
            matches!(
                MessagesBody::from_json(Zeroizing::new(body(messages))),
                Err(Failure::Request)
            ),
            "{case}"
        );
    }
}

#[test]
fn members_are_read_in_order_each_value_exactly_as_written() {
    let object = r#" { "a" : 1.50 , "b\"c":"x\\\"}" ,"d":[{"e":[true]},null], "f":{}, "g":-2e3 } "#;
    assert_eq!(
        super::members(object),
        Some(vec![
            ("a".to_owned(), "1.50"),
            ("b\"c".to_owned(), r#""x\\\"}""#),
            ("d".to_owned(), r#"[{"e":[true]},null]"#),
            ("f".to_owned(), "{}"),
            ("g".to_owned(), "-2e3"),
        ])
    );
    assert_eq!(super::members("{}"), Some(Vec::new()));
    for refused in ["[1]", "\"a\"", r#"{"a":1} x"#, r#"{"a":1"#, ""] {
        assert_eq!(super::members(refused), None, "{refused}");
    }
}

/// A splice fills exactly its range of one new buffer, and refuses before writing a place past
/// the body or a total over the request cap; a failed write is a request failure.
#[test]
fn a_splice_writes_only_inside_the_body_and_the_request_cap() {
    let body = MessagesBody::new(&request()).expect("envelope");
    let old = body.bytes().to_vec();
    let at = old.len() - 2;
    let filled = |out: &mut [u8]| {
        out.fill(b'x');
        Ok::<(), ()>(())
    };
    let spliced = body.spliced(at, 3, filled).expect("spliced");
    assert_eq!(
        spliced.bytes(),
        [&old[..at], b"xxx", &old[at..]].concat().as_slice()
    );
    assert!(spliced.pictured() && !body.pictured());
    let room = MAX_REQUEST_BYTES - old.len();
    let full = body.spliced(at, room, filled).expect("at the cap");
    assert_eq!(full.bytes().len(), MAX_REQUEST_BYTES);
    let unwritten = |_: &mut [u8]| -> Result<(), ()> { panic!("written past a refusal") };
    assert!(matches!(
        body.spliced(at, room + 1, unwritten),
        Err(Failure::Request)
    ));
    assert!(matches!(
        body.spliced(old.len() + 1, 0, unwritten),
        Err(Failure::Request)
    ));
    assert!(matches!(
        body.spliced(at, 3, |_| Err(())),
        Err(Failure::Request)
    ));
}
