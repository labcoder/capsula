//! The offered JSON contract agrees with the independent validator and typed decoder.

use kumochi_scene::{
    Admission, ErrorCode, GlobalBudget, Record, SceneState, draw_schema, parse_record,
};
use serde_json::{Value, json};

fn valid(record: &Value) -> bool {
    let schema = draw_schema();
    let validator = jsonschema::validator_for(&schema).expect("local drawing schema compiles");
    validator.is_valid(&json!({"records": [record]}))
}

#[test]
fn draw_schema_stays_within_its_reviewed_growth_budget() {
    let bytes = serde_json::to_vec(&draw_schema())
        .expect("serialized schema")
        .len();
    assert!(
        bytes <= 32 * 1024,
        "draw schema grew past its 32 KiB review budget: {bytes}"
    );
}

#[test]
fn every_two_dimensional_operation_and_mixed_form_fits_the_schema() {
    let operations = [
        json!({"line": {"id": "l", "from": [1, 2], "to": {"ref": "n"}, "color": "#01a2ff"}}),
        json!({"arrow": {"id": "a", "from": [1, 2], "to": [3, 4]}}),
        json!({"path": {"id": "p", "d": "M0 0 L1 1"}}),
        json!({"shape": {"id": "s", "kind": "circle", "r": 7}}),
        json!({"text": {"id": "t", "at": [0, 0], "text": "é", "color": "blue"}}),
        json!({"callout": {"id": "c", "target": {"ref": "n"}, "text": "note"}}),
        json!({"highlight": {"id": "h", "target": [0, 0]}}),
        json!({"diagram": {"id": "d"}}),
        json!({"node": {"id": "n", "in": "d", "label": "Node"}}),
        json!({"edge": {"id": "e", "in": "d", "from": "n", "to": "m"}}),
        json!({"table": {"id": "table", "cols": ["x"]}}),
        json!({"row": {"id": "row", "cells": ["cell", 3.5, true]}}),
        json!({"chart": {"id": "chart"}}),
        json!({"series": {"id": "series", "data": [1, [2, 3]]}}),
        json!({"anim": {"target": "n", "track": "x", "keys": [[0, 0], [250, [1, 2]], [500, "blue"]]}}),
        json!({"mermaid": {"in": "d", "src": "flowchart LR\nA-->B"}}),
    ];
    let validator = jsonschema::validator_for(&draw_schema()).expect("local drawing schema");
    for operation in operations {
        for start in [
            json!("speech"),
            json!("now"),
            json!({"after": "earlier", "ms": 200}),
        ] {
            let record = json!({"step": {"id": "step", "put": [operation], "start": start}});
            let parsed = parse_record(&serde_json::to_vec(&record).expect("fixture JSON"))
                .expect("fixture uses the decoder's JSON form");
            assert!(validator.is_valid(&json!({"records": [record]})));
            assert!(validator.is_valid(&json!({"records": [parsed]})));
        }
    }
    assert!(valid(
        &json!({"begin": {"v": 1, "scene": "scene", "surface": "overlay", "alt": ""}})
    ));
    assert!(valid(
        &json!({"begin": {"scene": "scene", "surface": "overlay", "alt": ""}})
    ));
    assert!(valid(&json!({"end": {}})));
    assert!(valid(&json!({"begin": {"scene":"root", "alt":"diagram"}})));
    assert!(valid(
        &json!({"step": {"id": "s", "set": [{"target": "n", "text": "renamed", "color": "green"}], "remove": ["old"]}})
    ));
    assert!(valid(
        &json!({"step": {"id": "s", "set": [{"target": "sun", "behind": "cloud"}]}})
    ));
    assert!(!valid(
        &json!({"step": {"id": "s", "set": [{"target": "sun", "behind": "two words"}]}})
    ));
}

#[test]
fn scalar_dimension_minimums_match_admission() {
    let Record::Begin(header) =
        parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":""}}"#)
            .expect("valid begin")
    else {
        panic!("not a begin");
    };
    let scene = SceneState::begin(header).expect("valid scene");
    let global = GlobalBudget::default();
    let check = |operation: Value, path: &str, expected: bool| {
        let record = json!({"step": {"id": "s", "put": [operation]}});
        assert_eq!(valid(&record), expected, "{path}");
        let Record::Step(step) = parse_record(&serde_json::to_vec(&record).expect("fixture JSON"))
            .expect("dimension values decode for admission")
        else {
            panic!("not a step");
        };
        if expected {
            assert!(
                matches!(scene.admit(&step, &global), Ok(Admission::New(_))),
                "{path}"
            );
        } else {
            let error = scene.admit(&step, &global).expect_err("invalid dimension");
            assert_eq!(error.code, ErrorCode::OutOfRange);
            assert_eq!(error.path.as_str(), format!("put[0].{path}"));
        }
    };
    for field in ["width", "radius"] {
        for (value, expected) in [(json!(0), true), (json!(-1), false)] {
            let mut shape = json!({"id": "s", "kind": "rect", "box": [0, 0, 4, 5]});
            shape[field] = value;
            check(json!({"shape": shape}), &format!("shape.{field}"), expected);
        }
    }
    for (value, expected) in [(json!(0.5), true), (json!(0), false)] {
        for (operation, path) in [
            (
                json!({"line": {"id": "l", "from": [0, 0], "to": [1, 1], "width": value}}),
                "line.width",
            ),
            (
                json!({"arrow": {"id": "a", "from": [0, 0], "to": [1, 1], "width": value}}),
                "arrow.width",
            ),
            (
                json!({"path": {"id": "p", "d": "M0 0 L1 1", "width": value}}),
                "path.width",
            ),
            (
                json!({"path": {"id": "p", "d": "M0 0 L1 1", "scale": value}}),
                "path.scale",
            ),
            (
                json!({"shape": {"id": "s", "kind": "circle", "at": [0, 0], "r": value}}),
                "shape.r",
            ),
            (
                json!({"text": {"id": "t", "at": [0, 0], "text": "", "size": value}}),
                "text.size",
            ),
            (
                json!({"text": {"id": "t", "at": [0, 0], "text": "", "max_width": value}}),
                "text.max_width",
            ),
            (
                json!({"callout": {"id": "c", "target": [0, 0], "text": "", "max_width": value}}),
                "callout.max_width",
            ),
            (
                json!({"highlight": {"id": "h", "target": [0, 0], "pad": value}}),
                "highlight.pad",
            ),
        ] {
            check(operation, path, expected);
        }
    }
}

#[test]
fn schema_is_closed_and_excludes_every_three_dimensional_form() {
    for record in [
        json!({"step": {"id": "s", "put": [{"view3d": {"id": "v"}}]}}),
        json!({"step": {"id": "s", "put": [{"solid": {"in": "v", "id": "s", "shape": "sphere", "r": 1}}]}}),
        json!({"step": {"id": "s", "put": [{"anim": {"target": "v", "track": "pos3", "keys": []}}]}}),
        json!({"step": {"id": "s", "put": [{"anim": {"target": "v", "track": "rot3", "keys": []}}]}}),
        json!({"step": {"id": "s", "put": [{"anim": {"target": "v", "track": "pos", "keys": [[0, [1, 2, 3]]]}}]}}),
        json!({"step": {"id": "s", "set": [{"target": "s", "info": "3d-only"}]}}),
        json!({"step": {"id": "s", "set": [{"target": "s", "info": null}]}}),
        json!({"step": {"id": "s", "unknown": 1}}),
        json!({"end": {"unknown": 1}}),
        json!({"begin": {"v": 2, "scene": "scene", "surface": "overlay", "alt": ""}}),
        json!({"step": {"id": "s", "start": {"after": "first", "ms": 0, "unknown": 1}}}),
    ] {
        assert!(!valid(&record));
    }
    let schema = draw_schema();
    let validator = jsonschema::validator_for(&schema).expect("local drawing schema");
    assert!(!validator.is_valid(&json!({"records": [], "unknown": 1})));
    assert!(!validator.is_valid(&json!({})));
}

#[test]
fn scalar_and_list_boundaries_are_expressed_without_truncation() {
    for (id, expected) in [
        ("x".repeat(40), true),
        ("x".repeat(41), false),
        (String::new(), false),
        ("x y".into(), false),
    ] {
        assert_eq!(valid(&json!({"step": {"id": id}})), expected, "id boundary");
    }
    for (length, expected) in [(400, true), (401, false)] {
        assert_eq!(
            valid(
                &json!({"begin": {"v": 1, "scene": "s", "surface": "inline", "alt": "é".repeat(length)}})
            ),
            expected,
            "Unicode scalar boundary"
        );
    }
    for (coordinate, expected) in [
        (20_000, true),
        (-20_000, true),
        (20_001, false),
        (-20_001, false),
    ] {
        assert_eq!(
            valid(
                &json!({"step": {"id": "s", "put": [{"text": {"id": "t", "at": [coordinate, 0], "text": ""}}]}})
            ),
            expected,
            "coordinate boundary"
        );
    }
    for (ms, expected) in [(60_000, true), (60_001, false)] {
        assert_eq!(
            valid(&json!({"step": {"id": "s", "start": {"after": "first", "ms": ms}}})),
            expected,
            "duration boundary"
        );
    }
    for (length, expected) in [(600, true), (601, false)] {
        assert_eq!(
            valid(&json!({"step": {"id": "s", "remove": vec!["x"; length]}})),
            expected,
            "remove-list boundary"
        );
    }
    for (length, expected) in [(856, true), (857, false)] {
        assert_eq!(
            valid(
                &json!({"step": {"id": "s", "put": vec![json!({"diagram": {"id": "d"}}); length]}})
            ),
            expected,
            "put-list boundary"
        );
    }
    let validator = jsonschema::validator_for(&draw_schema()).expect("local drawing schema");
    assert!(validator.is_valid(&json!({"records": vec![json!({"end": {}}); 66]})));
    assert!(!validator.is_valid(&json!({"records": vec![json!({"end": {}}); 67]})));
}

#[test]
fn source_byte_limit_remains_a_decoder_check() {
    let record =
        json!({"step": {"id": "s", "put": [{"mermaid": {"in": "d", "src": "é".repeat(4097)}}]}});
    assert!(valid(&record));
    let error = parse_record(&serde_json::to_vec(&record).expect("fixture JSON"))
        .expect_err("8194 source bytes exceed the decoder limit");
    assert_eq!(error.code, kumochi_scene::ErrorCode::TooLong);
}

#[test]
fn handwritten_forms_agree_on_near_misses_with_the_typed_decoder() {
    let validator = jsonschema::validator_for(&draw_schema()).expect("local drawing schema");
    for (kind, value, accepted) in [
        ("Start", json!("now"), true),
        ("Start", json!("noww"), false),
        ("Start", json!({"after":"s","ms":60000}), true),
        ("Start", json!({"after":"s","ms":60001}), false),
        ("Start", json!({"after":"s","ms":0,"extra":0}), false),
        ("Anchor", json!([20000, -20000]), true),
        ("Anchor", json!([20001, 0]), false),
        ("Anchor", json!({"ref":"n"}), true),
        ("Anchor", json!({"ref":"n","extra":0}), false),
        ("Color", json!("#aBc123"), true),
        ("Color", json!("#aBc12"), false),
        ("Color", json!("#gg0000"), false),
        ("Color", json!({"Rgb":0}), false),
        ("Cell", json!(""), true),
        ("Cell", json!(true), true),
        ("Cell", Value::Null, false),
        ("Cell", json!([]), false),
        ("Point", json!(1), true),
        ("Point", json!([1, 2]), true),
        ("Point", json!([1]), false),
        ("Point", json!([1, 2, 3]), false),
        ("KeyValue", json!([20000, -20000]), true),
        ("KeyValue", json!([20001, 0]), false),
        ("KeyValue", json!([0, 0, 0, 0]), false),
        ("KeyValue", json!({"color":"blue"}), false),
    ] {
        let step = match kind {
            "Start" => json!({"id":"s","start":value}),
            "Anchor" => json!({"id":"s","put":[{"line":{"id":"n","from":value,"to":[0,0]}}]}),
            "Color" => {
                json!({"id":"s","put":[{"text":{"id":"n","at":[0,0],"text":"","color":value}}]})
            }
            "Cell" => json!({"id":"s","put":[{"row":{"id":"n","cells":[value]}}]}),
            "Point" => json!({"id":"s","put":[{"series":{"id":"n","data":[value]}}]}),
            "KeyValue" => {
                json!({"id":"s","put":[{"anim":{"target":"n","track":"x","keys":[[0,value]]}}]})
            }
            _ => unreachable!("closed fixture kinds"),
        };
        let record = json!({"step":step});
        assert_eq!(
            validator.is_valid(&json!({"records":[record]})),
            accepted,
            "{kind} schema"
        );
        assert_eq!(
            parse_record(&serde_json::to_vec(&record).expect("fixture")).is_ok(),
            accepted,
            "{kind} decoder"
        );
    }
}
