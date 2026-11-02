//! Scene-format behavior and privacy regressions.
use capsula_scene::record::{Anchor, Cell, Color, KeyValue, Point, Start};
use capsula_scene::{ErrorCode, Finite, StepError, Text, WireId, parse_record};
use serde::{Serialize, de::DeserializeOwned};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq>(value: &T) {
    let json = serde_json::to_vec(value).expect("valid fixture serializes");
    let decoded = serde_json::from_slice::<T>(&json);
    assert!(decoded.is_ok(), "valid JSON round trip");
    assert!(
        decoded.ok().as_ref() == Some(value),
        "JSON preserves the value"
    );
    let binary = postcard::to_allocvec(value).expect("valid fixture serializes");
    let decoded = postcard::from_bytes::<T>(&binary);
    assert!(decoded.is_ok(), "valid binary round trip");
    assert!(
        decoded.ok().as_ref() == Some(value),
        "binary preserves the value"
    );
}
fn error(json: &str) -> StepError {
    match parse_record(json.as_bytes()) {
        Err(error) => error,
        Ok(_) => panic!("invalid fixture must be rejected"),
    }
}
#[test]
fn strict_records() {
    assert_eq!(
        error(r#"{"begin":{"v":2,"scene":"s","surface":"overlay","alt":""}}"#).code,
        ErrorCode::WrongVersion
    );
    assert_eq!(error(r#"{"view3d":{}}"#).code, ErrorCode::UnknownOp);
    assert_eq!(
        error(r#"{"end":{"extra":0}}"#).code,
        ErrorCode::UnknownField
    );
    assert_eq!(
        error(r#"{"step":{"id":"s","id":"t"}}"#).code,
        ErrorCode::DuplicateKey
    );
    assert_eq!(error(r#"{"step":{"id":""}}"#).code, ErrorCode::OutOfRange);
    assert_eq!(
        error(&format!(r#"{{"step":{{"id":"{}"}}}}"#, "x".repeat(41))).code,
        ErrorCode::TooLong
    );
    assert_eq!(error(r#"{"end":{}} trailing"#).code, ErrorCode::Malformed);
    assert_eq!(
        error(r#"{"end":{},"step":{"id":"s"}}"#).code,
        ErrorCode::Malformed
    );
    let missing = error(r#"{"begin":{"v":1,"scene":"s","surface":"overlay"}}"#);
    assert_eq!(missing.code, ErrorCode::MissingField);
    assert_eq!(missing.path.as_str(), "alt");
}
#[test]
fn text_and_list_boundaries_are_not_truncated() {
    let accepted = Text::try_from("é".repeat(400));
    assert!(accepted.is_ok(), "400 Unicode scalars are legal");
    assert_eq!(
        Text::try_from("é".repeat(401)).err(),
        Some(ErrorCode::TooLong)
    );
}
#[test]
fn mixed_syntax_round_trips() {
    for json in [r#""speech""#, r#""now""#, r#"{"after":"s1","ms":200}"#] {
        let value: Start = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
    for json in ["[1,-2]", r#"{"ref":"n"}"#] {
        let value: Anchor = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
    for json in [r#""blue""#, r##""#01a2ff""##] {
        let value: Color = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
    for json in [r#""cell""#, "3.5", "true"] {
        let value: Cell = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
    for json in ["3.5", "[1,2]"] {
        let value: Point = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
    for json in ["0.5", "[1,2]", r#""accent""#] {
        let value: KeyValue = serde_json::from_str(json).expect("valid static fixture");
        round_trip(&value);
    }
}
#[test]
fn mixed_maps_are_strict() {
    for (json, code, path) in [
        (
            r#"{"step":{"id":"s","put":[{"text":{"id":"t","at":{"ref":"a","ref":"b"},"text":""}}]}}"#,
            ErrorCode::DuplicateKey,
            "put[0].text.at",
        ),
        (
            r#"{"step":{"id":"s","put":[{"text":{"id":"t","at":{"ref":"a","other":0},"text":""}}]}}"#,
            ErrorCode::UnknownField,
            "put[0].text.at",
        ),
        (
            r#"{"step":{"id":"s","start":{"after":"s","ms":1,"x":0}}}"#,
            ErrorCode::UnknownField,
            "start.x",
        ),
        (
            r#"{"step":{"id":"s","start":{"after":"s","ms":1,"ms":2}}}"#,
            ErrorCode::DuplicateKey,
            "start",
        ),
        (
            r##"{"step":{"id":"s","put":[{"node":{"id":"n","label":"","color":"#fffffff"}}]}}"##,
            ErrorCode::OutOfRange,
            "put[0].node.color",
        ),
        (
            r##"{"step":{"id":"s","put":[{"node":{"id":"n","label":"","color":"#zz0000"}}]}}"##,
            ErrorCode::OutOfRange,
            "put[0].node.color",
        ),
    ] {
        let failure = error(json);
        assert_eq!(failure.code, code);
        assert_eq!(failure.path.as_str(), path);
    }
}

#[test]
fn binary_payloads_cannot_bypass_validation() {
    for number in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 20_001.0] {
        let bytes = postcard::to_allocvec(&number).expect("raw number encodes");
        assert!(
            postcard::from_bytes::<Finite>(&bytes).err() == Some(postcard::Error::SerdeDeCustom),
            "invalid finite number fails"
        );
    }
    // The raw discriminant is written explicitly to select binary Hex (variant 1).
    let mut bytes = vec![1];
    bytes.extend(postcard::to_allocvec(&0x0100_0000_u32).expect("raw integer encodes"));
    assert!(
        postcard::from_bytes::<Color>(&bytes).err() == Some(postcard::Error::SerdeDeCustom),
        "binary RGB remains 24 bit"
    );
    let mut bytes = vec![1]; // Cell::Number
    bytes.extend(postcard::to_allocvec(&f64::NAN).expect("raw number encodes"));
    assert!(
        postcard::from_bytes::<Cell>(&bytes).err() == Some(postcard::Error::SerdeDeCustom),
        "binary cell rejects NaN"
    );
    let mut bytes = vec![0]; // KeyValue::Num
    bytes.extend(postcard::to_allocvec(&f32::NAN).expect("raw number encodes"));
    assert!(
        postcard::from_bytes::<KeyValue>(&bytes).err() == Some(postcard::Error::SerdeDeCustom),
        "binary key rejects NaN"
    );
    for id in ["", &"x".repeat(41), "invalid/id"] {
        let bytes = postcard::to_allocvec(id).expect("raw string encodes");
        assert!(
            postcard::from_bytes::<WireId>(&bytes).err() == Some(postcard::Error::SerdeDeCustom),
            "binary ids remain validated"
        );
    }
}
#[test]
fn errors_and_debug_do_not_echo_drawing_content() {
    let sentinel = "private-label-sentinel".repeat(25);
    let json = serde_json::json!({"step":{"id":"private-id-sentinel","put":[{"node":{"id":"n","label":sentinel}}]}});
    let bytes = serde_json::to_vec(&json).expect("fixture encodes");
    let mut failure = parse_record(&bytes).expect_err("long label fails");
    assert_eq!(failure.code, ErrorCode::TooLong);
    assert_eq!(failure.path.as_str(), "put[0].node.label");
    failure.step = Some(WireId::try_from("private-id-sentinel".to_owned()).expect("valid id"));
    let safe = format!("{failure} {failure:?}");
    assert!(!safe.contains("private"), "diagnostics redact ids and text");
    assert!(
        failure.tool_text().contains("private-id-sentinel"),
        "only the model reply includes its step id"
    );
    assert!(
        !failure.tool_text().contains("private-label"),
        "model reply never echoes labels"
    );
    let text = Text::try_from("private-text-sentinel".to_owned()).expect("valid text");
    let id = WireId::try_from("private-id-sentinel".to_owned()).expect("valid id");
    assert!(
        !format!("{text:?} {id:?}").contains("private"),
        "debug is length only"
    );
    for (json, code, path) in [
        (
            r#"{"step":{"id":"s","private-key-sentinel":"private-value-sentinel"}}"#,
            ErrorCode::UnknownField,
            "",
        ),
        (
            r#"{"private-op-sentinel":{"text":"private-value-sentinel"}}"#,
            ErrorCode::UnknownOp,
            "",
        ),
        (
            r#"{"step":{"id":"s","put":[{"text":{"id":"t","at":[0,0],"text":"","size":"private-value-sentinel"}}]}}"#,
            ErrorCode::Malformed,
            "put[0].text.size",
        ),
        (
            r#"{"step":{"id":"s","put":[{"node":{"id":"n","label":"","shape":"private-value-sentinel"}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].node.shape",
        ),
        (
            r##"{"step":{"id":"s","put":[{"node":{"id":"n","label":"","color":"#private-value-sentinel"}}]}}"##,
            ErrorCode::OutOfRange,
            "put[0].node.color",
        ),
        (
            r#"{"step":{"id":"s","put":[{"node":{"id":"n","label":"","color":"private-value-sentinel"}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].node.color",
        ),
    ] {
        let failure = error(json);
        assert_eq!(failure.code, code);
        assert_eq!(failure.path.as_str(), path);
        let safe = format!("{failure} {failure:?} {}", failure.tool_text());
        assert!(
            !safe.contains("private"),
            "parser messages and unknown keys are discarded"
        );
        assert!(
            !safe.contains("unknown field"),
            "raw serde wording is discarded"
        );
    }
    let bytes = postcard::to_allocvec("private-value-sentinel/").expect("raw string encodes");
    let failure = postcard::from_bytes::<WireId>(&bytes).expect_err("invalid binary id");
    assert_eq!(failure, postcard::Error::SerdeDeCustom);
    assert!(
        !format!("{failure} {failure:?}").contains("private"),
        "binary errors discard custom text"
    );
}
#[test]
fn every_operation_round_trips_as_a_record() {
    let fixtures = [
        r#"{"line":{"id":"l","from":[0,0],"to":[1,2]}}"#,
        r#"{"arrow":{"id":"a","from":{"ref":"n"},"to":[1,2]}}"#,
        r#"{"path":{"id":"p","d":"M0 0 L1 1"}}"#,
        r#"{"shape":{"id":"s","kind":"rect","box":[0,0,4,5]}}"#,
        r#"{"text":{"id":"t","at":[0,0],"text":"hello"}}"#,
        r#"{"callout":{"id":"c","target":{"ref":"n"},"text":"hello"}}"#,
        r#"{"highlight":{"id":"h","target":{"ref":"n"}}}"#,
        r#"{"diagram":{"id":"d","box":[0,0,100,100]}}"#,
        r#"{"node":{"id":"n","in":"d","label":"node"}}"#,
        r#"{"edge":{"id":"e","in":"d","from":"n","to":"n2","flow":500}}"#,
        r#"{"table":{"id":"t","cols":["a","b"]}}"#,
        r#"{"row":{"id":"r","in":"t","cells":["cell",1,true]}}"#,
        r#"{"chart":{"id":"c","kind":"line","categories":["a"],"x":{"scale":"linear"}}}"#,
        r#"{"series":{"id":"s","in":"c","data":[1,[2,3]]}}"#,
        r#"{"anim":{"target":"s","track":"pos","keys":[[0,[0,0]],[100,[2,3]]]}}"#,
        r#"{"mermaid":{"in":"d","src":"flowchart LR; a-->b"}}"#,
    ];
    for op in fixtures {
        let json = format!(r#"{{"step":{{"id":"s","put":[{op}]}}}}"#);
        let record = parse_record(json.as_bytes()).expect("valid static fixture");
        round_trip(&record);
        let json = serde_json::to_vec(&record).expect("record encodes");
        assert!(
            parse_record(&json).ok().as_ref() == Some(&record),
            "typed parse preserves every operation"
        );
    }
    for json in [
        r#"{"begin":{"v":1,"scene":"s","surface":"inline","size":[320,240],"title":"t","alt":"description"}}"#,
        r#"{"end":{}}"#,
    ] {
        let record = parse_record(json.as_bytes()).expect("valid static fixture");
        round_trip(&record);
    }
}
