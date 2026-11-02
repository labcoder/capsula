//! Scalar, source and list admission boundaries in both encodings.
use capsula_scene::{
    ErrorCode, Finite, Ms, Op, Record, Source, Step, StepError, WireId, parse_record,
    record::{Change, Start},
};
use serde::Serialize;
use serde_json::{Value, json};

fn parse(value: &Value) -> Result<Record, StepError> {
    parse_record(&serde_json::to_vec(value).expect("test fixture encodes"))
}

fn list_fixture(case: &str, count: usize) -> Value {
    let list = vec![json!(0); count];
    let op = match case {
        "cols" => json!({"table":{"id":"o","cols":vec!["";count]}}),
        "cells" => json!({"row":{"id":"o","cells":list}}),
        "via" => json!({"line":{"id":"o","from":[0,0],"to":[0,0],"via":vec![[0,0];count]}}),
        "points" => json!({"shape":{"id":"o","kind":"polygon","points":vec![[0,0];count]}}),
        "data" => json!({"series":{"id":"o","data":list}}),
        "keys" => json!({"anim":{"target":"o","track":"x","keys":vec![[0,0];count]}}),
        "categories" => json!({"chart":{"id":"o","categories":vec!["";count]}}),
        "put" => {
            return json!({"step":{"id":"s","put":vec![json!({"mermaid":{"in":"d","src":""}});count]}});
        }
        "set" => return json!({"step":{"id":"s","set":vec![json!({"target":"o"});count]}}),
        "remove" => return json!({"step":{"id":"s","remove":vec!["o";count]}}),
        "change_cells" => return json!({"step":{"id":"s","set":[{"target":"o","cells":list}]}}),
        "change_data" => return json!({"step":{"id":"s","set":[{"target":"o","data":list}]}}),
        _ => panic!("known boundary fixture"),
    };
    json!({"step":{"id":"s","put":[op]}})
}

fn list_len(record: &Record, case: &str) -> usize {
    let Record::Step(step) = record else {
        panic!("step fixture")
    };
    match case {
        "put" => step.put.len(),
        "set" => step.set.len(),
        "remove" => step.remove.len(),
        "change_cells" => step.set[0].cells.as_ref().expect("cells present").len(),
        "change_data" => step.set[0].data.as_ref().expect("data present").len(),
        _ => match &step.put[0] {
            Op::Table(table) => table.cols.len(),
            Op::Row(row) => row.cells.len(),
            Op::Line(line) => line.via.len(),
            Op::Shape(shape) => shape.points.len(),
            Op::Series(series) => series.data.len(),
            Op::Anim(anim) => anim.keys.len(),
            Op::Chart(chart) => chart.categories.len(),
            _ => panic!("known bounded operation"),
        },
    }
}

#[test]
fn every_list_accepts_its_exact_cap_and_rejects_the_next_element() {
    for (case, cap, path) in [
        ("cols", 8, "put[0].table.cols"),
        ("cells", 8, "put[0].row.cells"),
        ("via", 512, "put[0].line.via"),
        ("points", 512, "put[0].shape.points"),
        ("data", 500, "put[0].series.data"),
        ("keys", 32, "put[0].anim.keys"),
        ("categories", 40, "put[0].chart.categories"),
        ("put", 856, "put"),
        ("set", 600, "set"),
        ("remove", 600, "remove"),
        ("change_cells", 8, "set[0].cells"),
        ("change_data", 500, "set[0].data"),
    ] {
        let record = parse(&list_fixture(case, cap)).expect("at-cap list is valid");
        assert_eq!(list_len(&record, case), cap, "no elements are truncated");
        let failure = parse(&list_fixture(case, cap + 1)).expect_err("one excess element fails");
        assert_eq!(failure.code, ErrorCode::TooLong);
        assert_eq!(failure.path.as_str(), format!("{path}[{cap}]"));
        let bytes = postcard::to_allocvec(&record).expect("legal record encodes");
        let decoded = postcard::from_bytes::<Record>(&bytes).expect("legal cap decodes");
        assert_eq!(list_len(&decoded, case), cap, "binary cap is not truncated");
    }
}

#[test]
fn scalar_and_record_byte_edges_are_exact() {
    assert_eq!(
        WireId::try_from("x".repeat(40))
            .expect("id at cap")
            .as_str()
            .len(),
        40
    );
    for coordinate in [20_000.0_f32, -20_000.0] {
        assert_eq!(
            Finite::try_from(coordinate)
                .expect("coordinate at cap")
                .get()
                .to_bits(),
            coordinate.to_bits()
        );
        let outside = f32::from_bits(coordinate.to_bits() + 1);
        assert_eq!(Finite::try_from(outside).err(), Some(ErrorCode::OutOfRange));
    }
    let prefix = b"{\"end\":{";
    let mut bytes = prefix.to_vec();
    bytes.resize(32_768 - 2, b' ');
    bytes.extend(b"}}");
    assert!(
        matches!(parse_record(&bytes), Ok(Record::End(_))),
        "exact record byte cap"
    );
    bytes.insert(prefix.len(), b' ');
    assert_eq!(
        parse_record(&bytes).expect_err("one byte over cap").code,
        ErrorCode::TooLong
    );
    for (json, code, path) in [
        (
            r#"{"step":{"id":"s","put":[{"line":{"id":"o","from":[0,20001],"to":[0,0]}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].line.from[1]",
        ),
        (
            r#"{"step":{"id":"s","put":[{"anim":{"target":"o","track":"x","keys":[[60001,0]]}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].anim.keys[0][0]",
        ),
        (
            r#"{"step":{"id":"s","put":[{"anim":{"target":"o","track":"x","keys":[[0,20001]]}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].anim.keys[0][1]",
        ),
        (
            r#"{"step":{"id":"s","put":[{"node":{"id":"o"}}]}}"#,
            ErrorCode::MissingField,
            "put[0].node.label",
        ),
        (
            r#"{"step":{"id":"s","put":[{"unknown_op":{}}]}}"#,
            ErrorCode::UnknownOp,
            "put[0]",
        ),
        (
            r#"{"begin":{"v":1,"scene":"s","surface":"bad","alt":""}}"#,
            ErrorCode::OutOfRange,
            "surface",
        ),
        (
            r#"{"step":{"id":"s","put":[{"node":{"id":"o","label":"","delay":-5}}]}}"#,
            ErrorCode::OutOfRange,
            "put[0].node.delay",
        ),
    ] {
        let failure = parse_record(json.as_bytes()).expect_err("invalid typed fixture");
        assert_eq!(failure.code, code);
        assert_eq!(failure.path.as_str(), path);
    }
}

#[test]
fn times_and_source_validate_on_both_routes() {
    for value in [0, 60_000] {
        let ms = Ms::try_from(value).expect("legal millisecond boundary");
        assert_eq!(ms.get(), value);
        let bytes = postcard::to_allocvec(&value).expect("raw time encodes");
        assert_eq!(
            postcard::from_bytes::<Ms>(&bytes).expect("legal binary time"),
            ms
        );
    }
    let bytes = postcard::to_allocvec(&60_001_u32).expect("raw time encodes");
    assert_eq!(
        postcard::from_bytes::<Ms>(&bytes).err(),
        Some(postcard::Error::SerdeDeCustom)
    );
    assert_eq!(Ms::try_from(60_001).err(), Some(ErrorCode::OutOfRange));
    let one = Ms::try_from(1).expect("one millisecond");
    assert_eq!(one.checked_add(one).expect("bounded addition").get(), 2);
    assert_eq!(Ms::try_from(60_000).expect("cap").checked_add(one), None);
    for source in ["x".repeat(8192), "é".repeat(4096)] {
        let value = Source::try_from(source.clone()).expect("exact UTF-8 byte cap");
        assert_eq!(value.as_str().len(), 8192);
        let bytes = postcard::to_allocvec(&source).expect("raw source encodes");
        assert!(
            postcard::from_bytes::<Source>(&bytes).ok().as_ref() == Some(&value),
            "source parity"
        );
        for op in [
            json!({"path":{"id":"p","d":source}}),
            json!({"mermaid":{"in":"d","src":source}}),
        ] {
            assert!(
                parse(&json!({"step":{"id":"s","put":[op]}})).is_ok(),
                "source is not capped as display text"
            );
        }
        let excessive = format!("{source}x");
        assert_eq!(
            Source::try_from(excessive.clone()).err(),
            Some(ErrorCode::TooLong)
        );
        let bytes = postcard::to_allocvec(&excessive).expect("raw source encodes");
        assert_eq!(
            postcard::from_bytes::<Source>(&bytes).err(),
            Some(postcard::Error::SerdeDeCustom)
        );
        for (op, path) in [
            (json!({"path":{"id":"p","d":excessive}}), "put[0].path.d"),
            (
                json!({"mermaid":{"in":"d","src":excessive}}),
                "put[0].mermaid.src",
            ),
        ] {
            let failure =
                parse(&json!({"step":{"id":"s","put":[op]}})).expect_err("source exceeds bytes");
            assert_eq!(failure.code, ErrorCode::TooLong);
            assert_eq!(failure.path.as_str(), path);
        }
    }
}

#[derive(Serialize)]
struct RawStep {
    id: &'static str,
    say: Option<&'static str>,
    put: Vec<Op>,
    set: Vec<Change>,
    remove: Vec<WireId>,
    start: Start,
}

#[test]
fn binary_step_lists_cannot_expand_before_admission() {
    let Record::Step(seed) = parse(&list_fixture("put", 1)).expect("seed step") else {
        panic!("step fixture")
    };
    let op = seed.put[0].clone();
    let id = WireId::try_from("o".to_owned()).expect("valid id");
    let Record::Step(seed) = parse(&list_fixture("set", 1)).expect("seed change") else {
        panic!("step fixture")
    };
    let change = seed.set[0].clone();
    for (case, cap) in [("put", 856), ("set", 600), ("remove", 600)] {
        for count in [cap, cap + 1] {
            let raw = RawStep {
                id: "s",
                say: None,
                put: if case == "put" {
                    vec![op.clone(); count]
                } else {
                    Vec::new()
                },
                set: if case == "set" {
                    vec![change.clone(); count]
                } else {
                    Vec::new()
                },
                remove: if case == "remove" {
                    vec![id.clone(); count]
                } else {
                    Vec::new()
                },
                start: Start::Speech,
            };
            let bytes = postcard::to_allocvec(&raw).expect("raw step encodes");
            let result = postcard::from_bytes::<Step>(&bytes);
            if count == cap {
                let step = result.expect("binary cap accepted");
                assert_eq!(
                    match case {
                        "put" => step.put.len(),
                        "set" => step.set.len(),
                        _ => step.remove.len(),
                    },
                    cap
                );
            } else {
                assert_eq!(result.err(), Some(postcard::Error::SerdeDeCustom));
            }
        }
    }
}
