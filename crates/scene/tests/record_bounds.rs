//! Scalar, source and list admission boundaries in both encodings.
use kumochi_scene::{
    ErrorCode, Finite, Ms, Op, Record, Source, Step, StepError, WireId, parse_record,
    record::{Change, Start},
};
use serde::Serialize;
use serde_json::{Value, json};

mod three {
    use super::*;
    use kumochi_scene::{
        record::{Fov, Length, Pitch, Segments, Size, Spin, Unit},
        text::Bounded,
    };
    use serde::de::DeserializeOwned;

    fn scalar<
        T: TryFrom<f32, Error = ErrorCode> + Into<f32> + DeserializeOwned + std::fmt::Debug,
    >(
        valid: &[f32],
        invalid: &[f32],
    ) {
        for &raw in valid {
            let value = T::try_from(raw).expect("exact legal boundary");
            assert_eq!(
                Into::<f32>::into(value).to_bits(),
                raw.to_bits(),
                "constructor retains the exact boundary"
            );
            let bytes = postcard::to_allocvec(&raw).expect("raw scalar");
            let decoded: T = postcard::from_bytes(&bytes).expect("binary exact boundary");
            assert_eq!(
                Into::<f32>::into(decoded).to_bits(),
                raw.to_bits(),
                "postcard retains the exact boundary"
            );
        }
        for &raw in invalid {
            assert_eq!(
                T::try_from(raw).expect_err("outside boundary"),
                ErrorCode::OutOfRange,
                "constructor rejects the first outside fixture"
            );
            let bytes = postcard::to_allocvec(&raw).expect("raw scalar");
            assert_eq!(
                postcard::from_bytes::<T>(&bytes).expect_err("binary range check"),
                postcard::Error::SerdeDeCustom,
                "postcard cannot bypass range validation"
            );
        }
        for raw in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                T::try_from(raw).expect_err("nonfinite"),
                ErrorCode::NotFinite,
                "nonfinite values have their own exact code"
            );
            let bytes = postcard::to_allocvec(&raw).expect("raw scalar");
            assert_eq!(
                postcard::from_bytes::<T>(&bytes).expect_err("binary finite check"),
                postcard::Error::SerdeDeCustom,
                "postcard cannot bypass finite validation"
            );
        }
    }

    #[test]
    fn bounded_3d_scalars_check_raw_postcard_and_constructor_edges() {
        scalar::<Size>(&[0.001, 20_000.0], &[0.0, 0.000_99, 20_000.5]);
        scalar::<Length>(&[0.0, 0.001, 20_000.0], &[-0.001, 0.000_99, 20_000.5]);
        scalar::<Unit>(&[0.0, 1.0], &[-0.0001, 1.0001]);
        scalar::<Fov>(&[10.0, 120.0], &[9.99, 120.01]);
        scalar::<Pitch>(&[-85.0, 85.0], &[-85.01, 85.01]);
        scalar::<Spin>(&[-360.0, 360.0], &[-360.5, 360.5]);
        for raw in [3_u8, 48] {
            assert_eq!(Segments::try_from(raw).expect("legal segments").get(), raw);
            let bytes = postcard::to_allocvec(&raw).expect("raw segments");
            assert_eq!(
                postcard::from_bytes::<Segments>(&bytes)
                    .expect("binary segments")
                    .get(),
                raw
            );
        }
        for raw in [2_u8, 49] {
            assert_eq!(
                Segments::try_from(raw).expect_err("outside segments"),
                ErrorCode::OutOfRange
            );
            let bytes = postcard::to_allocvec(&raw).expect("raw segments");
            assert_eq!(
                postcard::from_bytes::<Segments>(&bytes).expect_err("binary segments"),
                postcard::Error::SerdeDeCustom
            );
        }
    }

    fn check(op: &Value, field: &str, raw: Value, code: Option<ErrorCode>) {
        let input = json!({"step":{"id":"s","put":[op]}});
        if let Some(code) = code {
            let error = parse(&input).expect_err("JSON boundary failure");
            assert_eq!(error.code, code, "JSON boundary code");
            assert_eq!(
                error.path.as_str(),
                format!("put[0].{field}"),
                "JSON boundary path"
            );
        } else {
            let record = parse(&input).expect("JSON exact boundary");
            let output = serde_json::to_value(&record).expect("serialized record");
            let mut value = &output["step"]["put"][0];
            for key in field.split('.') {
                value = &value[key];
            }
            assert_eq!(
                serde_json::from_value::<f32>(value.clone())
                    .expect("numeric boundary")
                    .to_bits(),
                serde_json::from_value::<f32>(raw)
                    .expect("raw boundary")
                    .to_bits(),
                "JSON preserves the checked scalar boundary"
            );
        }
    }

    #[test]
    fn solid_scalars_have_json_limit_and_one_over_paths() {
        for field in ["r", "h", "depth", "scale"] {
            for (raw, code) in [
                (0.001, None),
                (20_000.0, None),
                (0.000_99, Some(ErrorCode::OutOfRange)),
                (20_000.5, Some(ErrorCode::OutOfRange)),
            ] {
                let mut solid = json!({"in":"v","id":"s","shape":"sphere"});
                solid[field] = json!(raw);
                check(
                    &json!({"solid":solid}),
                    &format!("solid.{field}"),
                    json!(raw),
                    code,
                );
            }
        }
        for (field, values) in [
            (
                "r2",
                vec![
                    (0.0, None),
                    (0.001, None),
                    (20_000.0, None),
                    (-0.001, Some(ErrorCode::OutOfRange)),
                    (0.000_99, Some(ErrorCode::OutOfRange)),
                    (20_000.5, Some(ErrorCode::OutOfRange)),
                ],
            ),
            (
                "opacity",
                vec![
                    (0.0, None),
                    (1.0, None),
                    (-0.0001, Some(ErrorCode::OutOfRange)),
                    (1.0001, Some(ErrorCode::OutOfRange)),
                ],
            ),
        ] {
            for (raw, code) in values {
                let mut solid = json!({"in":"v","id":"s","shape":"cone"});
                solid[field] = json!(raw);
                check(
                    &json!({"solid":solid}),
                    &format!("solid.{field}"),
                    json!(raw),
                    code,
                );
            }
        }
    }

    #[test]
    fn camera_and_light_scalars_have_json_limit_and_one_over_paths() {
        for (field, valid, invalid) in [
            ("fov", [10.0, 120.0], [9.99, 120.01]),
            ("pitch", [-85.0, 85.0], [-85.01, 85.01]),
            ("dist", [0.001, 20_000.0], [0.000_99, 20_000.5]),
        ] {
            for (values, code) in [(valid, None), (invalid, Some(ErrorCode::OutOfRange))] {
                for raw in values {
                    let mut camera = json!({});
                    camera[field] = json!(raw);
                    check(
                        &json!({"view3d":{"id":"v","camera":camera}}),
                        &format!("view3d.camera.{field}"),
                        json!(raw),
                        code,
                    );
                }
            }
        }
        for (raw, code) in [
            (-360.0, None),
            (360.0, None),
            (-360.5, Some(ErrorCode::OutOfRange)),
            (360.5, Some(ErrorCode::OutOfRange)),
        ] {
            check(
                &json!({"view3d":{"id":"v","spin":raw}}),
                "view3d.spin",
                json!(raw),
                code,
            );
        }
        for (raw, code) in [
            (0.0, None),
            (1.0, None),
            (-0.0001, Some(ErrorCode::OutOfRange)),
            (1.0001, Some(ErrorCode::OutOfRange)),
        ] {
            check(
                &json!({"view3d":{"id":"v","light":{"ambient":raw}}}),
                "view3d.light.ambient",
                json!(raw),
                code,
            );
        }
        for raw in [json!([0, 0, 0]), json!([-0.0, 0.0, 0.0])] {
            check(
                &json!({"view3d":{"id":"v","light":{"dir":raw.clone()}}}),
                "view3d.light.dir",
                raw,
                Some(ErrorCode::OutOfRange),
            );
        }
        for (raw, expected) in [
            (json!([0, 0, 0.5]), json!([0.0, 0.0, 0.5])),
            (json!([0, -1, 0]), json!([0.0, -1.0, 0.0])),
        ] {
            let input =
                json!({"step":{"id":"s","put":[{"view3d":{"id":"v","light":{"dir":raw}}}]}});
            let record = parse(&input).expect("nonzero direction");
            let output = serde_json::to_value(&record).expect("serialized record");
            assert_eq!(output["step"]["put"][0]["view3d"]["light"]["dir"], expected);
        }
        let zero = postcard::to_allocvec(&[0.0_f32; 3]).expect("raw direction");
        assert_eq!(
            postcard::from_bytes::<kumochi_scene::record::LightDir>(&zero)
                .expect_err("binary direction check"),
            postcard::Error::SerdeDeCustom,
            "postcard cannot bypass the zero-direction check"
        );
        let up = postcard::to_allocvec(&[0.0_f32, 1.0, 0.0]).expect("raw direction");
        assert_eq!(
            postcard::from_bytes::<kumochi_scene::record::LightDir>(&up)
                .expect("nonzero binary direction")
                .get()
                .map(|c| c.get().to_bits()),
            [0.0_f32, 1.0, 0.0].map(f32::to_bits)
        );
    }

    #[test]
    fn segments_and_box_dimensions_have_json_limit_and_one_over_paths() {
        for (raw, code) in [
            (json!(3), None),
            (json!(48), None),
            (json!(2), Some(ErrorCode::OutOfRange)),
            (json!(49), Some(ErrorCode::OutOfRange)),
            (json!(48.5), Some(ErrorCode::Malformed)),
            (json!("48"), Some(ErrorCode::Malformed)),
        ] {
            check(
                &json!({"solid":{"in":"v","id":"s","shape":"sphere","segments":raw}}),
                "solid.segments",
                raw,
                code,
            );
        }
        for (raw, code) in [
            (0.001, None),
            (20_000.0, None),
            (0.000_99, Some(ErrorCode::OutOfRange)),
            (20_000.5, Some(ErrorCode::OutOfRange)),
        ] {
            for axis in 0..3 {
                let mut size = [1.0_f32; 3];
                size[axis] = raw;
                let input = json!({"step":{"id":"s","put":[{"solid":{"in":"v","id":"s","shape":"box","size":size}}]}});
                if let Some(code) = code {
                    let error = parse(&input).expect_err("size component out of bounds");
                    assert_eq!(error.code, code);
                    assert_eq!(error.path.as_str(), format!("put[0].solid.size[{axis}]"));
                } else {
                    let Record::Step(step) = parse(&input).expect("size boundary") else {
                        panic!("step")
                    };
                    let Op::Solid(solid) = &step.put[0] else {
                        panic!("solid")
                    };
                    assert_eq!(
                        solid.size.expect("size")[axis].get().to_bits(),
                        raw.to_bits()
                    );
                }
            }
        }
    }

    #[test]
    fn solid_text_and_list_decode_caps_apply_in_both_formats() {
        for count in [400, 401] {
            let info = "é".repeat(count);
            let input = json!({"step":{"id":"s","put":[{"solid":{"id":"s","in":"v","shape":"box","info":info}}]}});
            let bytes = postcard::to_allocvec(&info).expect("raw text");
            if count == 400 {
                let Record::Step(step) = parse(&input).expect("exact info cap") else {
                    panic!("step")
                };
                let Op::Solid(solid) = &step.put[0] else {
                    panic!("solid")
                };
                assert_eq!(solid.info.as_ref().expect("info").as_str(), info);
                assert_eq!(
                    postcard::from_bytes::<kumochi_scene::Text>(&bytes)
                        .expect("binary text cap")
                        .as_str(),
                    info
                );
            } else {
                let error = parse(&input).expect_err("excess info");
                assert_eq!(error.code, ErrorCode::TooLong);
                assert_eq!(error.path.as_str(), "put[0].solid.info");
                assert_eq!(
                    postcard::from_bytes::<kumochi_scene::Text>(&bytes)
                        .expect_err("binary text cap"),
                    postcard::Error::SerdeDeCustom
                );
            }
        }
        for field in ["points", "path"] {
            for count in [512, 513] {
                let list = if field == "points" {
                    json!(vec![[0.0_f32; 2]; count])
                } else {
                    json!(vec![[0.0_f32; 3]; count])
                };
                let mut solid = json!({"id":"s","in":"v","shape":"box"});
                solid[field] = list;
                let input = json!({"step":{"id":"s","put":[{"solid":solid}]}});
                if count == 512 {
                    let record = parse(&input).expect("exact list cap");
                    let bytes = postcard::to_allocvec(&record).expect("binary list");
                    assert_eq!(
                        postcard::from_bytes::<Record>(&bytes).expect("exact binary list"),
                        record
                    );
                } else {
                    let error = parse(&input).expect_err("excess list");
                    assert_eq!(error.code, ErrorCode::TooLong);
                    assert_eq!(error.path.as_str(), format!("put[0].solid.{field}[512]"));
                }
                if field == "points" {
                    let bytes =
                        postcard::to_allocvec(&vec![[0.0_f32; 2]; count]).expect("raw outline");
                    let decoded = postcard::from_bytes::<Bounded<[Finite; 2], 512>>(&bytes);
                    if count == 512 {
                        assert_eq!(decoded.expect("binary cap").len(), 512);
                    } else {
                        assert_eq!(
                            decoded.expect_err("binary over cap"),
                            postcard::Error::SerdeDeCustom
                        );
                    }
                } else {
                    let bytes =
                        postcard::to_allocvec(&vec![[0.0_f32; 3]; count]).expect("raw path");
                    let decoded = postcard::from_bytes::<Bounded<[Finite; 3], 512>>(&bytes);
                    if count == 512 {
                        assert_eq!(decoded.expect("binary cap").len(), 512);
                    } else {
                        assert_eq!(
                            decoded.expect_err("binary over cap"),
                            postcard::Error::SerdeDeCustom
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn programmatically_built_lists_reject_excess_values() {
    let at_cap = kumochi_scene::text::Bounded::<u8, 2>::new(vec![1, 2]).expect("exact cap");
    assert_eq!(&*at_cap, &[1, 2]);
    assert_eq!(
        kumochi_scene::text::Bounded::<u8, 2>::new(vec![1, 2, 3]).err(),
        Some(ErrorCode::TooLong)
    );
}

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
        for (op, kind, field) in [
            (json!({"path":{"id":"p","d":source}}), "path", "d"),
            (json!({"mermaid":{"in":"d","src":source}}), "mermaid", "src"),
        ] {
            let record = parse(&json!({"step":{"id":"s","put":[op]}})).expect("at source cap");
            let encoded = serde_json::to_value(record).expect("serializable record");
            assert_eq!(
                encoded["step"]["put"][0][kind][field].as_str(),
                Some(source.as_str())
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
