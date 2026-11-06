//! Synthetic envelope fixtures; no provider recording or credentials.

use super::Failure;
use super::MessagesBody;
use crate::connections::model::MAX_REQUEST_BYTES;
use serde_json::{Value, json};

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
fn preserves_structured_input_and_all_supported_efforts() {
    let mut value = request();
    value["tools"] = json!([{"name":"example","input_schema":{"type":"object","properties":{
        "cache_control":{"type":"string"}}}}]);
    value["messages"][0]["content"] = json!([{"type":"tool_use","id":"one","name":"example",
        "input":{"cache_control":{"type":"persistent"},"content":[{"cache_control":false}]}}]);
    let structured = value["messages"][0]["content"].clone();
    value["messages"] = json!([
        {"role":"user","content":"first"},
        {"role":"assistant","content":structured},
        {"role":"user","content":"next"}
    ]);
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        value["output_config"] = json!({"effort":effort});
        let body = MessagesBody::new(&value).expect("supported envelope");
        assert_eq!(
            serde_json::from_slice::<Value>(body.bytes()).unwrap(),
            value
        );
    }
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
