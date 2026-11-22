//! Scene-format behavior and privacy regressions.
use kumochi_scene::record::{
    Anchor, Cell, Color, KeyValue, Others, Point, Rgb, Start, Surface, Token, Version,
};
use kumochi_scene::{
    Begin, ErrorCode, Finite, Record, StepError, Text, WireId, parse_draw_record, parse_record,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

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
        error(r#"{"step":{"id":"s","set":[{"target":"series","opacity":0.25}]}}"#).code,
        ErrorCode::UnknownField
    );
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
    let inline = br#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":""}}"#;
    let expected = Begin {
        v: Version::V1,
        scene: WireId::try_from("s".to_owned()).expect("valid id"),
        surface: Surface::Inline,
        size: None,
        title: None,
        alt: Text::try_from(String::new()).expect("empty alt is valid"),
        others: Others::Clear,
        display: None,
    };
    assert_eq!(
        parse_record(inline).expect("generic inline begin"),
        Record::Begin(expected.clone())
    );
    let unsupported = parse_draw_record(inline).expect_err("draw tool rejects inline begin");
    assert_eq!(unsupported.step, None);
    assert_eq!(unsupported.code, ErrorCode::Unsupported);
    assert_eq!(unsupported.path.as_str(), "surface");
    assert_eq!(unsupported.tool_text(), "surface: scene.Unsupported");
    assert_eq!(
        parse_draw_record(br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":""}}"#)
            .expect("draw tool overlay begin"),
        Record::Begin(Begin {
            surface: Surface::Overlay,
            ..expected
        })
    );
}
#[test]
fn the_draw_tool_defaults_only_omitted_begin_surface_and_version() {
    let explicit = parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"a"}}"#)
        .expect("explicit header");
    for (json, missing) in [
        (r#"{"begin":{"scene":"s","alt":"a"}}"#, None),
        (r#"{"begin":{"v":1,"scene":"s","alt":"a"}}"#, None),
        (
            r#"{"begin":{"scene":"s","surface":"overlay","alt":"a"}}"#,
            Some("v"),
        ),
    ] {
        assert_eq!(
            parse_draw_record(json.as_bytes()).expect("draw defaults"),
            explicit
        );
        let error = parse_record(json.as_bytes()).expect_err("generic parsing stays strict");
        assert_eq!(error.code, ErrorCode::MissingField, "{json}");
        if let Some(path) = missing {
            assert_eq!(error.path.as_str(), path, "{json}");
        }
    }
    for (json, path, code) in [
        (r#"{"begin":{"scene":"s"}}"#, "alt", ErrorCode::MissingField),
        (
            r#"{"begin":{"scene":"s","surface":"overlay"}}"#,
            "alt",
            ErrorCode::MissingField,
        ),
        (
            r#"{"begin":{"v":2,"scene":"s","surface":"overlay","alt":"a"}}"#,
            "v",
            ErrorCode::WrongVersion,
        ),
        (
            r#"{"begin":{"v":2,"scene":"s","alt":"a"}}"#,
            "v",
            ErrorCode::WrongVersion,
        ),
        (
            r#"{"begin":{"scene":"s","surface":"inline","alt":"a"}}"#,
            "surface",
            ErrorCode::Unsupported,
        ),
        (
            r#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":"a"}}"#,
            "surface",
            ErrorCode::Unsupported,
        ),
    ] {
        let error =
            parse_draw_record(json.as_bytes()).expect_err("no content or explicit override");
        assert_eq!((error.path.as_str(), error.code), (path, code));
    }
    for (json, path, code) in [
        (
            r#"{"begin":{"scene":"s","surface":null,"alt":"a"}}"#,
            "surface",
            ErrorCode::Malformed,
        ),
        (
            r#"{"begin":{"scene":"s","surface":"bad","alt":"a"}}"#,
            "surface",
            ErrorCode::OutOfRange,
        ),
        (
            r#"{"begin":{"v":null,"scene":"s","alt":"a"}}"#,
            "v",
            ErrorCode::Malformed,
        ),
        (
            r#"{"begin":{"v":1,"scene":"s","alt":"a","alt":"b"}}"#,
            "",
            ErrorCode::DuplicateKey,
        ),
        (
            r#"{"begin":{"v":1,"scene":"s","alt":"a"}"#,
            "",
            ErrorCode::Malformed,
        ),
    ] {
        let error = parse_draw_record(json.as_bytes()).expect_err("invalid header");
        assert_eq!((error.path.as_str(), error.code), (path, code), "{json}");
    }
}

#[test]
fn draw_defaults_reject_duplicate_outer_begins_instead_of_picking_the_last() {
    for json in [
        r#"{"begin":{"v":1,"scene":"first","alt":"a"},"begin":{"v":1,"scene":"second","surface":"overlay","alt":"b"}}"#,
        r#"{"begin":{"scene":"first","surface":"overlay","alt":"a"},"begin":{"v":1,"scene":"second","surface":"overlay","alt":"b"}}"#,
        r#"{"begin":{"scene":"first","alt":"a"},"begin":{"v":1,"scene":"second","surface":"overlay","alt":"b"}}"#,
    ] {
        let error = parse_draw_record(json.as_bytes()).expect_err("duplicate outer begin");
        assert_eq!(
            (error.code, error.path.as_str()),
            (ErrorCode::DuplicateKey, "")
        );
    }
}

#[test]
fn text_and_list_boundaries_are_not_truncated() {
    let exact = "é".repeat(400);
    let accepted = Text::try_from(exact.clone()).expect("400 Unicode scalars are legal");
    assert_eq!(accepted.as_str(), exact);
    assert_eq!(
        Text::try_from("é".repeat(401)).err(),
        Some(ErrorCode::TooLong)
    );
}
#[test]
fn mixed_syntax_round_trips() {
    fn check<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
        json: &str,
        expected: &T,
    ) {
        let value: T = serde_json::from_str(json).expect("valid static fixture");
        assert_eq!(&value, expected);
        round_trip(&value);
    }
    let id = |name: &str| WireId::try_from(name.to_owned()).expect("valid ID");
    let finite = |value| Finite::try_from(value).expect("finite");
    let number = |value| kumochi_scene::text::Number::try_from(value).expect("valid number");
    check(r#""speech""#, &Start::Speech);
    check(r#""now""#, &Start::Now);
    check(
        r#"{"after":"s1","ms":200}"#,
        &Start::After {
            step: id("s1"),
            ms: 200.try_into().expect("valid milliseconds"),
        },
    );
    check("[1,-2]", &Anchor::At([1.0, -2.0].map(finite)));
    check(r#"{"ref":"n"}"#, &Anchor::Ref(id("n")));
    check(r#""blue""#, &Color::Token(Token::Blue));
    check(
        r##""#01a2ff""##,
        &Color::Hex(Rgb::try_from(0x01_a2_ff).expect("valid RGB")),
    );
    check(
        r#""cell""#,
        &Cell::Text(Text::try_from("cell".to_owned()).expect("valid text")),
    );
    check("3.5", &Cell::Number(number(3.5)));
    check("true", &Cell::Bool(true));
    check("3.5", &Point::Value(number(3.5)));
    check("[1,2]", &Point::Pair([1.0, 2.0].map(number)));
    check("0.5", &KeyValue::Num(finite(0.5)));
    check("[1,2]", &KeyValue::V2([1.0, 2.0].map(finite)));
    check(r#""accent""#, &KeyValue::Color(Color::Token(Token::Accent)));
}
fn omits_bytes(output: &str, marker: &[u8]) -> bool {
    let bytes = format!("{marker:?}");
    !output.contains(bytes.trim_matches(['[', ']']))
}

fn assert_redacted(output: &str, markers: &[&str]) {
    for marker in markers {
        assert!(
            !output.contains(marker) && omits_bytes(output, marker.as_bytes()),
            "output must not contain a private marker"
        );
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
        postcard::from_bytes::<WireId>(&bytes).expect_err("binary ids remain validated");
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
    assert_redacted(&safe, &["private"]);
    let tool_text = failure.tool_text();
    assert!(
        tool_text.contains("private-id-sentinel"),
        "only the model reply includes its step id"
    );
    assert!(
        !tool_text.contains("private-label"),
        "model reply never echoes labels"
    );
    assert_redacted(&tool_text, &["private-label-sentinel"]);
    assert!(omits_bytes(&tool_text, b"private-id-sentinel"));
    let text = Text::try_from("private-text-sentinel".to_owned()).expect("valid text");
    let id = WireId::try_from("private-id-sentinel".to_owned()).expect("valid id");
    assert_redacted(&format!("{text:?} {id:?}"), &["private"]);
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
        assert_redacted(&safe, &["private"]);
    }
    let bytes = postcard::to_allocvec("private-value-sentinel/").expect("raw string encodes");
    let failure = postcard::from_bytes::<WireId>(&bytes).expect_err("invalid binary id");
    assert_redacted(&format!("{failure} {failure:?}"), &["private"]);
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
    let expected = [
        ("line", "to", json!([1.0, 2.0])),
        ("arrow", "from", json!({"ref":"n"})),
        ("path", "d", json!("M0 0 L1 1")),
        ("shape", "box", json!([0.0, 0.0, 4.0, 5.0])),
        ("text", "text", json!("hello")),
        ("callout", "target", json!({"ref":"n"})),
        ("highlight", "target", json!({"ref":"n"})),
        ("diagram", "box", json!([0.0, 0.0, 100.0, 100.0])),
        ("node", "label", json!("node")),
        ("edge", "flow", json!(500)),
        ("table", "cols", json!(["a", "b"])),
        ("row", "cells", json!(["cell", 1.0, true])),
        ("chart", "/x/scale", json!("linear")),
        ("series", "data", json!([1.0, [2.0, 3.0]])),
        ("anim", "keys", json!([[0, [0.0, 0.0]], [100, [2.0, 3.0]]])),
        ("mermaid", "src", json!("flowchart LR; a-->b")),
    ];
    for (op, (tag, field, payload)) in fixtures.into_iter().zip(expected) {
        let json = format!(r#"{{"step":{{"id":"s","put":[{op}]}}}}"#);
        let record = parse_record(json.as_bytes()).expect("valid static fixture");
        let Record::Step(step) = &record else {
            panic!("step")
        };
        assert_eq!(step.id.as_str(), "s");
        assert_eq!(step.put.len(), 1);
        let encoded = serde_json::to_value(&step.put[0]).expect("operation encodes");
        let tagged = encoded.get(tag).expect("authored operation tag");
        assert_eq!(
            tagged.pointer(&if field.starts_with('/') {
                field.to_owned()
            } else {
                format!("/{field}")
            }),
            Some(&payload),
            "{tag}.{field}"
        );
        round_trip(&record);
        let json = serde_json::to_vec(&record).expect("record encodes");
        assert!(
            parse_record(&json).ok().as_ref() == Some(&record),
            "typed parse preserves every operation"
        );
    }
    let begin = parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"inline","size":[320,240],"title":"t","alt":"description"}}"#)
        .expect("valid begin");
    let Record::Begin(header) = &begin else {
        panic!("begin")
    };
    assert_eq!(header.scene.as_str(), "s");
    assert_eq!(header.surface, Surface::Inline);
    assert_eq!(
        serde_json::to_value(header.size).expect("size encodes"),
        json!([320.0, 240.0])
    );
    assert_eq!(header.title.as_ref().map(Text::as_str), Some("t"));
    assert_eq!(header.alt.as_str(), "description");
    round_trip(&begin);
    let end = parse_record(br#"{"end":{}}"#).expect("valid end");
    assert!(matches!(end, Record::End(_)));
    round_trip(&end);
}

mod three {
    use super::*;
    use kumochi_scene::{
        Budget, Record,
        record::{Geometry, ShapeError, Solid, View3d},
    };
    use serde_json::{Value, json};

    fn solid(value: &Value) -> Solid {
        serde_json::from_value(value.clone()).expect("decoded solid")
    }
    fn exact_round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let json = serde_json::to_vec(value).expect("JSON");
        assert_eq!(
            &serde_json::from_slice::<T>(&json).expect("JSON decode"),
            value,
            "JSON preserves every field"
        );
        let bytes = postcard::to_allocvec(value).expect("postcard");
        assert_eq!(
            &postcard::from_bytes::<T>(&bytes).expect("postcard decode"),
            value,
            "postcard preserves every field"
        );
    }
    fn fields() -> Vec<Value> {
        vec![
            json!({"shape":"box","size":[1,2,3]}),
            json!({"shape":"sphere","r":2}),
            json!({"shape":"cylinder","r":2,"h":3}),
            json!({"shape":"cone","r":2,"r2":0,"h":3}),
            json!({"shape":"torus","r":2,"r2":1}),
            json!({"shape":"extrude","points":[[0,0],[1,0],[0,1]],"depth":3}),
            json!({"shape":"lathe","points":[[0,0],[1,2]]}),
            json!({"shape":"tube","path":[[0,0,0],[1,2,3]],"r2":1}),
        ]
    }
    fn fixture(mut value: Value) -> Value {
        value["in"] = json!("v");
        value["id"] = json!("s");
        value
    }
    fn shape_error(value: &Value, field: &'static str, code: ErrorCode) {
        let value = solid(value);
        assert_eq!(
            value.geometry().expect_err("invalid geometry"),
            ShapeError { field, code },
            "JSON shape validation reports the exact field and code"
        );
        let bytes = postcard::to_allocvec(&value).expect("decoded but invalid geometry");
        let decoded: Solid =
            postcard::from_bytes(&bytes).expect("shape rules are not decode bounds");
        assert_eq!(
            decoded.geometry().expect_err("invalid binary geometry"),
            ShapeError { field, code },
            "postcard shape validation reports the same field and code"
        );
    }

    #[test]
    fn all_shapes_resolve_exact_fields_and_round_trip_full_records() {
        let expected = [
            Geometry::Box {
                size: [1.0, 2.0, 3.0],
            },
            Geometry::Sphere { r: 2.0 },
            Geometry::Cylinder { r: 2.0, h: 3.0 },
            Geometry::Cone {
                r: 2.0,
                top: 0.0,
                h: 3.0,
            },
            Geometry::Torus {
                ring: 2.0,
                tube: 1.0,
            },
            Geometry::Extrude {
                outline: &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]
                    .map(|p| p.map(|v| Finite::try_from(v).expect("finite"))),
                depth: 3.0,
            },
            Geometry::Lathe {
                profile: &[[0.0, 0.0], [1.0, 2.0]]
                    .map(|p| p.map(|v| Finite::try_from(v).expect("finite"))),
            },
            Geometry::Tube {
                path: &[[0.0, 0.0, 0.0], [1.0, 2.0, 3.0]]
                    .map(|p| p.map(|v| Finite::try_from(v).expect("finite"))),
                thickness: 1.0,
            },
        ];
        for (index, fields) in fields().into_iter().enumerate() {
            let mut value = fixture(fields);
            value.as_object_mut().expect("object").extend(json!({
                "pos":[12.875,-7.625,23.125],"rot":[4,5,6],"scale":2,"attach":"a",
                "label":"private-label-sentinel","info":"private-info-sentinel","color":"accent","opacity":0.5
            }).as_object().expect("object").clone());
            if !matches!(index, 0 | 5) {
                value["segments"] = json!(12);
            }
            let input = json!({"step":{"id":"s","put":[{"solid":value}]}});
            let record =
                parse_record(&serde_json::to_vec(&input).expect("fixture")).expect("record");
            let Record::Step(step) = &record else {
                panic!("step")
            };
            let [kumochi_scene::Op::Solid(decoded)] = &*step.put else {
                panic!("one solid")
            };
            assert_eq!(decoded.geometry().expect("geometry"), expected[index]);
            let position: [f32; 3] = [12.875, -7.625, 23.125];
            assert_eq!(
                decoded.pos.map(|v| v.map(|v| v.get().to_bits())),
                Some(position.map(f32::to_bits))
            );
            assert_eq!(
                decoded.rot.map(|v| v.map(|v| v.get().to_bits())),
                Some([4.0_f32, 5.0, 6.0].map(f32::to_bits))
            );
            assert_eq!(
                decoded.scale.map(|v| v.get().to_bits()),
                Some(2.0_f32.to_bits())
            );
            assert_eq!(decoded.attach.as_ref().map(WireId::as_str), Some("a"));
            assert_eq!(
                decoded.label.as_ref().map(Text::as_str),
                Some("private-label-sentinel")
            );
            assert_eq!(
                decoded.info.as_ref().map(Text::as_str),
                Some("private-info-sentinel")
            );
            assert_eq!(decoded.color, Some(Color::Token(Token::Accent)));
            assert_eq!(
                decoded.opacity.map(|v| v.get().to_bits()),
                Some(0.5_f32.to_bits())
            );
            exact_round_trip(&record);
            let debug = format!("{decoded:?}");
            assert_redacted(&debug, &["private", "12.875"]);
            assert!(omits_bytes(&debug, &12.875_f32.to_le_bytes()));
        }
        let mut cone = fixture(json!({"shape":"cone","r":2,"h":3}));
        assert_eq!(
            solid(&cone).geometry().expect("default top"),
            Geometry::Cone {
                r: 2.0,
                top: 0.0,
                h: 3.0
            }
        );
        cone["r2"] = json!(1);
        assert_eq!(
            solid(&cone).geometry().expect("truncated cone"),
            Geometry::Cone {
                r: 2.0,
                top: 1.0,
                h: 3.0
            }
        );
    }

    #[test]
    fn viewport_defaults_and_full_fields_are_exact_and_redacted() {
        let empty: View3d = serde_json::from_str(r#"{"id":"v"}"#).expect("empty viewport");
        assert_eq!(empty.id.as_str(), "v");
        assert_eq!(
            (
                empty.bounds,
                empty.camera,
                empty.light,
                empty.orbit,
                empty.spin
            ),
            (None, None, None, true, None)
        );
        let full: View3d = serde_json::from_value(json!({
            "id":"private-id-sentinel","box":[1,2,303.625,400],
            "camera":{"yaw":30,"pitch":45,"dist":12,"fov":60,"target":[1,2,3]},
            "light":{"dir":[4,5,6],"ambient":0.25},"orbit":false,"spin":-90
        }))
        .expect("full viewport");
        assert_eq!(full.id.as_str(), "private-id-sentinel");
        assert_eq!(
            full.bounds.map(|v| v.map(|v| v.get().to_bits())),
            Some([1.0_f32, 2.0, 303.625, 400.0].map(f32::to_bits))
        );
        exact_round_trip(&full);
        let encoded = serde_json::to_value(&full).expect("encoded viewport");
        assert_eq!(
            encoded["camera"],
            json!({"yaw":30.0,"pitch":45.0,"dist":12.0,"fov":60.0,"target":[1.0,2.0,3.0]})
        );
        assert_eq!(
            encoded["light"],
            json!({"dir":[4.0,5.0,6.0],"ambient":0.25})
        );
        assert_eq!(encoded["orbit"], false);
        assert_eq!(encoded["spin"], -90.0);
        let debug = format!("{full:?}");
        assert_redacted(&debug, &["private", "303.625"]);
        assert!(omits_bytes(&debug, &303.625_f32.to_le_bytes()));
        let camera: kumochi_scene::record::Camera = serde_json::from_str("{}").expect("camera");
        assert_eq!(
            (
                camera.yaw,
                camera.pitch,
                camera.dist,
                camera.fov,
                camera.target
            ),
            (None, None, None, None, None)
        );
    }

    #[test]
    fn each_shape_requires_its_fields_before_rejecting_unused_fields() {
        let required: &[&[&str]] = &[
            &["size"],
            &["r"],
            &["r", "h"],
            &["r", "h"],
            &["r", "r2"],
            &["points", "depth"],
            &["points"],
            &["path", "r2"],
        ];
        for (value, required) in fields().into_iter().zip(required) {
            for &field in *required {
                let mut value = fixture(value.clone());
                value.as_object_mut().expect("object").remove(field);
                shape_error(&value, field, ErrorCode::MissingField);
            }
        }
        shape_error(
            &fixture(json!({"shape":"box","r":1})),
            "size",
            ErrorCode::MissingField,
        );
        let sphere = fixture(json!({"shape":"sphere","r":1,"points":[],"path":[]}));
        assert_eq!(
            solid(&sphere).geometry().expect("unused empty lists"),
            Geometry::Sphere { r: 1.0 }
        );
    }

    #[test]
    fn every_shape_refuses_each_field_it_does_not_use() {
        let all = [
            ("size", json!([1, 1, 1])),
            ("r", json!(1)),
            ("r2", json!(0.5)),
            ("h", json!(1)),
            ("depth", json!(1)),
            ("points", json!([[0, 0], [1, 0], [0, 1]])),
            ("path", json!([[0, 0, 0], [1, 1, 1]])),
            ("segments", json!(3)),
        ];
        let used: [&[&str]; 8] = [
            &["size"],
            &["r", "segments"],
            &["r", "h", "segments"],
            &["r", "r2", "h", "segments"],
            &["r", "r2", "segments"],
            &["points", "depth"],
            &["points", "segments"],
            &["path", "r2", "segments"],
        ];
        for (fields, used) in fields().into_iter().zip(used) {
            for (field, value) in &all {
                if used.contains(field) {
                    continue;
                }
                let mut solid = fixture(fields.clone());
                solid[*field] = value.clone();
                shape_error(&solid, field, ErrorCode::UnknownField);
            }
        }
    }

    #[test]
    fn list_presence_counts_and_radius_relations_are_shape_dependent() {
        for (fields, field, min, max) in [
            (json!({"shape":"extrude","depth":1}), "points", 3, 64),
            (json!({"shape":"lathe"}), "points", 2, 512),
            (json!({"shape":"tube","r2":1}), "path", 2, 512),
        ] {
            let mut value = fixture(fields);
            shape_error(&value, field, ErrorCode::MissingField);
            value[field] = json!([]);
            shape_error(&value, field, ErrorCode::MissingField);
            for count in 1..min {
                value[field] = if field == "path" {
                    json!(vec![[0, 0, 0]; count])
                } else {
                    json!(vec![[0, 0]; count])
                };
                shape_error(&value, field, ErrorCode::OutOfRange);
            }
            for count in [min, max] {
                value[field] = if field == "path" {
                    json!(vec![[0, 0, 0]; count])
                } else {
                    json!(vec![[0, 0]; count])
                };
                let solid = solid(&value);
                let count = match solid.geometry().expect("exact count boundary") {
                    Geometry::Extrude { outline, .. } => outline.len(),
                    Geometry::Lathe { profile } => profile.len(),
                    Geometry::Tube { path, .. } => path.len(),
                    _ => panic!("list shape"),
                };
                assert_eq!(count, value[field].as_array().expect("list").len());
                exact_round_trip(&solid);
            }
            if field == "points" && min == 3 {
                value[field] = json!(vec![[0, 0]; 65]);
                shape_error(&value, field, ErrorCode::OverBudget(Budget::PathPoints));
            }
        }
        for tube in [0.0, 2.0, 2.001] {
            shape_error(
                &fixture(json!({"shape":"torus","r":2,"r2":tube})),
                "r2",
                ErrorCode::OutOfRange,
            );
        }
        for tube in [0.001_f32, f32::from_bits(2.0_f32.to_bits() - 1)] {
            let value = fixture(json!({"shape":"torus","r":2,"r2":tube}));
            assert_eq!(
                solid(&value).geometry().expect("legal torus radius edges"),
                Geometry::Torus { ring: 2.0, tube }
            );
        }
        let value = fixture(json!({"shape":"tube","r2":0.001,"path":[[0,0,0],[1,0,0]]}));
        assert_eq!(
            solid(&value).geometry().expect("minimum thickness"),
            Geometry::Tube {
                path: &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]]
                    .map(|p| p.map(|v| Finite::try_from(v).expect("finite"))),
                thickness: 0.001,
            }
        );
        shape_error(
            &fixture(json!({"shape":"tube","r2":0,"path":[[0,0,0],[1,0,0]]})),
            "r2",
            ErrorCode::OutOfRange,
        );
        shape_error(
            &fixture(json!({"shape":"lathe","points":[[-0.001,0],[1,1]]})),
            "points",
            ErrorCode::OutOfRange,
        );
    }

    #[test]
    fn three_dimensional_malformed_records_have_exact_paths() {
        for (op, code, path) in [
            (
                r#"{"solid":{"id":"s","in":"v","shape":"unknown"}}"#,
                ErrorCode::OutOfRange,
                "solid.shape",
            ),
            (
                r#"{"solid":{"id":"s","shape":"box"}}"#,
                ErrorCode::MissingField,
                "solid.in",
            ),
            (
                r#"{"solid":{"id":"s","in":"v","shape":"box","pos":[1,2]}}"#,
                ErrorCode::Malformed,
                "solid.pos",
            ),
            (
                r#"{"solid":{"id":"s","in":"v","shape":"box","pos":[1,2,3,4]}}"#,
                ErrorCode::Malformed,
                "solid.pos",
            ),
            (
                r#"{"solid":{"id":"s","id":"t","in":"v","shape":"box"}}"#,
                ErrorCode::DuplicateKey,
                "solid",
            ),
            (
                r#"{"solid":{"id":"s","in":"v","shape":"box","unknown":0}}"#,
                ErrorCode::UnknownField,
                "solid",
            ),
            (
                r#"{"view3d":{"id":"v","unknown":0}}"#,
                ErrorCode::UnknownField,
                "view3d",
            ),
            (
                r#"{"view3d":{"id":"v","camera":{"unknown":0}}}"#,
                ErrorCode::UnknownField,
                "view3d.camera",
            ),
            (
                r#"{"view3d":{"id":"v","light":{"unknown":0}}}"#,
                ErrorCode::UnknownField,
                "view3d.light",
            ),
        ] {
            let failure = error(&format!(r#"{{"step":{{"id":"s","put":[{op}]}}}}"#));
            assert_eq!(failure.code, code);
            assert_eq!(failure.path.as_str(), format!("put[0].{path}"));
        }
        for key in ["[]", "[1]", "[1,2,3,4]"] {
            let failure = error(&format!(
                r#"{{"step":{{"id":"s","put":[{{"anim":{{"target":"s","track":"pos3","keys":[[0,{key}]]}}}}]}}}}"#
            ));
            assert_eq!(failure.code, ErrorCode::Malformed);
            assert_eq!(failure.path.as_str(), "put[0].anim.keys[0][1]");
        }
        let v3: KeyValue = serde_json::from_str("[1,2,3]").expect("V3");
        assert_eq!(
            v3,
            KeyValue::V3([1.0, 2.0, 3.0].map(|v| Finite::try_from(v).expect("finite")))
        );
        exact_round_trip(&v3);
        let Record::Step(step) =
            parse_record(br#"{"step":{"id":"s","put":[{"view3d":{"id":"v"}}]}}"#)
                .expect("empty view")
        else {
            panic!("step")
        };
        assert_eq!(step.put.len(), 1);
    }

    #[test]
    fn three_dimensional_binary_records_reject_nonfinite_and_truncated_inputs() {
        let solid_with_position = |position: [f32; 3]| {
            let mut bytes = vec![1, b'v', 1, b's', 0, 0, 0, 0, 0, 0, 0, 0, 1];
            bytes.extend(position.into_iter().flat_map(f32::to_le_bytes));
            bytes.extend([0; 8]);
            bytes
        };
        let key = |position: [f32; 3]| {
            [3].into_iter()
                .chain(position.into_iter().flat_map(f32::to_le_bytes))
                .collect::<Vec<_>>()
        };
        let solid_bytes = solid_with_position([1.0, 2.0, 3.0]);
        let key_bytes = key([1.0, 2.0, 3.0]);
        let control = postcard::from_bytes::<Solid>(&solid_bytes).expect("fixed-wire control");
        assert_eq!(
            control
                .pos
                .expect("authored position")
                .map(Finite::get)
                .map(f32::to_bits),
            [1.0_f32, 2.0, 3.0].map(f32::to_bits)
        );
        assert_eq!(
            postcard::from_bytes::<KeyValue>(&key_bytes).expect("fixed-wire key control"),
            KeyValue::V3([1.0, 2.0, 3.0].map(|v| Finite::try_from(v).expect("finite")))
        );
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            postcard::from_bytes::<KeyValue>(&key([value, 2.0, 3.0])).expect_err("nonfinite V3");
            postcard::from_bytes::<Solid>(&solid_with_position([value, 2.0, 3.0]))
                .expect_err("nonfinite position");
        }
        postcard::from_bytes::<Solid>(&solid_bytes[..solid_bytes.len() - 1])
            .expect_err("truncated solid");
        postcard::from_bytes::<KeyValue>(&key_bytes[..key_bytes.len() - 1])
            .expect_err("truncated V3");
    }
}
