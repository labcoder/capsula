//! The screen surface: its stamp, what it admits, its dressing, its placement and its stored form.
use kumochi_scene::{
    Begin, ErrorCode, GlobalBudget, Op, Record, SceneState, Step, StepError,
    geom::Affine,
    parse_draw_record, parse_screen_draw_record,
    record::ScreenDisplay,
    screen::{self, PLACEHOLDER_BOX},
};
use serde_json::{Value, json};

const DISPLAY: ScreenDisplay = ScreenDisplay {
    origin: [0, 0],
    size: [2560, 1440],
    dpi: 96,
};
const PICTURE: [u32; 2] = [1280, 720];

fn record(value: &Value) -> Result<Record, StepError> {
    parse_screen_draw_record(&serde_json::to_vec(value).expect("fixture JSON"))
}
fn model_header() -> Begin {
    let Ok(Record::Begin(begin)) =
        record(&json!({"begin":{"scene":"marks","surface":"screen","alt":""}}))
    else {
        panic!("screen header");
    };
    begin
}
fn header() -> Begin {
    let mut begin = model_header();
    screen::stamp(&mut begin, PICTURE, DISPLAY).expect("stamped");
    begin
}
fn step(put: &Value) -> Step {
    let Ok(Record::Step(step)) = record(&json!({"step":{"id":"s","put":put}})) else {
        panic!("screen step");
    };
    step
}
fn refusal(put: &Value) -> (String, ErrorCode) {
    let scene = SceneState::begin(header()).expect("scene");
    let error = scene
        .admit(&step(put), &GlobalBudget::default())
        .expect_err("refused");
    (error.path.as_str().to_owned(), error.code)
}

#[test]
fn a_screen_header_carries_the_brains_stamp_and_only_a_screen_header_may() {
    let unstamped = SceneState::begin(model_header()).expect_err("no stamp");
    assert_eq!(
        (unstamped.path.as_str(), unstamped.code),
        ("display", ErrorCode::Unsupported)
    );
    let Ok(Record::Begin(mut overlay)) = record(&json!({"begin":{"scene":"o","alt":""}})) else {
        panic!("overlay header");
    };
    let json = serde_json::to_string(&overlay).expect("overlay JSON");
    assert_eq!(
        json,
        r#"{"v":1,"scene":"o","surface":"overlay","size":null,"title":null,"alt":"","others":"clear"}"#
    );
    assert_eq!(
        serde_json::from_str::<Begin>(&json).ok(),
        Some(overlay.clone())
    );
    overlay.display = Some(DISPLAY);
    let misplaced = SceneState::begin(overlay).expect_err("stamp on an overlay");
    assert_eq!(
        (misplaced.path.as_str(), misplaced.code),
        ("display", ErrorCode::Unsupported)
    );
    let stamped = header();
    assert_eq!(
        stamped.size.map(|size| size.map(f32::from)),
        Some([1280.0, 720.0])
    );
    assert_eq!(stamped.display, Some(DISPLAY));
    let json = serde_json::to_string(&stamped).expect("stamped JSON");
    assert!(json.contains(r#""display":{"#), "{json}");
    assert_eq!(
        serde_json::from_str::<Begin>(&json).ok(),
        Some(stamped.clone())
    );
    SceneState::begin(stamped).expect("a stamped screen scene");
}

#[test]
fn the_model_may_write_the_pictures_size_but_never_another_or_the_display() {
    let mut same =
        record(&json!({"begin":{"scene":"m","surface":"screen","alt":"","size":[1280,720]}}));
    let Ok(Record::Begin(begin)) = &mut same else {
        panic!("header with size");
    };
    screen::stamp(begin, PICTURE, DISPLAY).expect("the picture's own size");
    let Ok(Record::Begin(mut other)) =
        record(&json!({"begin":{"scene":"m","surface":"screen","alt":"","size":[1280,721]}}))
    else {
        panic!("header with size");
    };
    let error = screen::stamp(&mut other, PICTURE, DISPLAY).expect_err("another size");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("size", ErrorCode::SizeNotPicture)
    );
    let written = json!({"begin":{"scene":"m","surface":"screen","alt":"",
        "display":{"origin":[0,0],"size":[2560,1440],"dpi":96}}});
    for error in [
        record(&written).expect_err("screen parse"),
        parse_draw_record(&serde_json::to_vec(&written).expect("JSON")).expect_err("draw parse"),
    ] {
        assert_eq!(
            (error.path.as_str(), error.code),
            ("display", ErrorCode::Unsupported)
        );
    }
    let Ok(Record::Begin(mut overlay)) = record(&json!({"begin":{"scene":"o","alt":""}})) else {
        panic!("overlay header");
    };
    let error = screen::stamp(&mut overlay, PICTURE, DISPLAY).expect_err("an overlay");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("surface", ErrorCode::Unsupported)
    );
    let mut stamped = header();
    let error = screen::stamp(&mut stamped, PICTURE, DISPLAY).expect_err("stamped twice");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("display", ErrorCode::Unsupported)
    );
}

#[test]
fn draw_still_refuses_the_screen_surface_and_box_targets_and_does_not_offer_them() {
    let begin = br#"{"begin":{"scene":"m","surface":"screen","alt":""}}"#;
    let error = parse_draw_record(begin).expect_err("dormant surface");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("surface", ErrorCode::Unsupported)
    );
    let boxed =
        br#"{"step":{"id":"s","put":[{"highlight":{"id":"h","target":{"box":[1,2,3,4]}}}]}}"#;
    let error = parse_draw_record(boxed).expect_err("dormant box");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("put[0].target", ErrorCode::Unsupported)
    );
    let offered = |schema: &Value| {
        let targets = schema["$defs"]["Target"]["anyOf"]
            .as_array()
            .expect("targets");
        let ops = schema["$defs"]["Op"]["oneOf"].as_array().expect("ops");
        (
            schema["$defs"]["Surface"].to_string().contains("screen"),
            targets
                .iter()
                .any(|target| !target["properties"]["box"].is_null()),
            schema.to_string().contains("display"),
            ops.iter()
                .filter(|op| {
                    !op["properties"]["line"].is_null() || !op["properties"]["arrow"].is_null()
                })
                .count(),
        )
    };
    assert_eq!(
        offered(&kumochi_scene::draw_schema()),
        (false, false, false, 2)
    );
    assert_eq!(
        offered(&kumochi_scene::screen_draw_schema()),
        (true, true, false, 0)
    );
}

#[test]
fn the_screen_surface_admits_only_marks_that_never_cover_their_target() {
    let target = json!({"box":[10,10,20,20]});
    for (put, path) in [
        (
            json!([{"text":{"id":"t","at":[10,10],"text":"hi"}}]),
            "put[0]",
        ),
        (
            json!([{"shape":{"id":"s","kind":"rect","box":[0,0,10,10]}}]),
            "put[0]",
        ),
        (json!([{"path":{"id":"p","d":"M0 0 L10 10"}}]), "put[0]"),
        (json!([{"diagram":{"id":"d"}}]), "put[0]"),
        (
            json!([{"highlight":{"id":"h","target":[10,10]}}]),
            "put[0].target",
        ),
        (
            json!([{"highlight":{"id":"h","in":"x","target":target}}]),
            "put[0].in",
        ),
        (
            json!([{"callout":{"id":"c","target":target,"text":"a","at":[50,50]}}]),
            "put[0].at",
        ),
        (
            json!([{"callout":{"id":"c","target":[1,1],"text":"a"}}]),
            "put[0].target",
        ),
        (
            json!([{"line":{"id":"l","from":{"ref":"a"},"to":{"ref":"b"}}}]),
            "put[0]",
        ),
        (
            json!([{"arrow":{"id":"a","from":{"ref":"a"},"to":{"ref":"b"}}}]),
            "put[0]",
        ),
    ] {
        assert_eq!(
            refusal(&put),
            (path.to_owned(), ErrorCode::Unsupported),
            "{path}"
        );
    }
    let Ok(Record::Step(set)) =
        record(&json!({"step":{"id":"s","set":[{"target":"h","color":"red"}]}}))
    else {
        panic!("set step");
    };
    let error = SceneState::begin(header())
        .expect("scene")
        .admit(&set, &GlobalBudget::default())
        .expect_err("set refused");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("set", ErrorCode::Unsupported)
    );
}

#[test]
fn a_box_must_be_non_empty_and_inside_the_picture() {
    for empty in [json!([0, 0, 0, 10]), json!([0, 0, 10, -1])] {
        let error = record(
            &json!({"step":{"id":"s","put":[{"highlight":{"id":"h","target":{"box":empty}}}]}}),
        )
        .expect_err("empty box");
        assert_eq!(error.code, ErrorCode::OutOfRange);
    }
    for outside in [[-1, 0, 10, 10], [1271, 0, 10, 10], [0, 711, 10, 10]] {
        assert_eq!(
            refusal(&json!([{"highlight":{"id":"h","target":{"box":outside}}}])),
            ("put[0].target".to_owned(), ErrorCode::OutOfRange)
        );
    }
    let edge = step(&json!([{"highlight":{"id":"h","target":{"box":[1270,710,10,10]}}}]));
    SceneState::begin(header())
        .expect("scene")
        .admit(&edge, &GlobalBudget::default())
        .expect("a box touching the picture's corner");
}

#[test]
fn a_label_is_one_line_of_at_most_its_cap_and_the_refusal_states_the_cap() {
    let label =
        |text: &str| json!([{"callout":{"id":"c","target":{"box":[10,10,20,20]},"text":text}}]);
    let longest = "a".repeat(24);
    SceneState::begin(header())
        .expect("scene")
        .admit(&step(&label(&longest)), &GlobalBudget::default())
        .expect("24 characters");
    for text in ["a".repeat(25), "two\nlines".to_owned()] {
        assert_eq!(
            refusal(&label(&text)),
            ("put[0].text".to_owned(), ErrorCode::LabelTooLong)
        );
    }
    assert!(
        ErrorCode::LabelTooLong
            .to_string()
            .contains("at most 24 characters")
    );
}

#[test]
fn the_canvas_maps_onto_its_own_display_in_logical_pixels_and_nowhere_else() {
    let size = [1280.0, 720.0];
    assert_eq!(
        screen::placement(DISPLAY, size, DISPLAY),
        Some(Affine([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]))
    );
    let scaled = ScreenDisplay {
        dpi: 144,
        ..DISPLAY
    };
    let Some(Affine([x, 0.0, 0.0, y, 0.0, 0.0])) = screen::placement(scaled, size, scaled) else {
        panic!("150% placement");
    };
    assert!((x - 4.0 / 3.0).abs() < 1e-6 && (y - 4.0 / 3.0).abs() < 1e-6);
    let portrait = ScreenDisplay {
        size: [1440, 2560],
        ..DISPLAY
    };
    assert_eq!(
        screen::placement(portrait, [405.0, 720.0], portrait),
        Some(Affine([1440.0 / 405.0, 0.0, 0.0, 2560.0 / 720.0, 0.0, 0.0]))
    );
    assert_eq!(
        screen::placement(DISPLAY, size, scaled),
        None,
        "another scaling"
    );
    let moved = ScreenDisplay {
        origin: [2560, 0],
        ..DISPLAY
    };
    assert_eq!(
        screen::placement(DISPLAY, size, moved),
        None,
        "another display"
    );
    assert_eq!(
        screen::placement(DISPLAY, [320.0, 180.0], DISPLAY),
        None,
        "over the scale cap"
    );
    assert_eq!(
        screen::placement(
            ScreenDisplay { dpi: 0, ..DISPLAY },
            size,
            ScreenDisplay { dpi: 0, ..DISPLAY }
        ),
        None
    );
}

#[test]
fn no_box_or_display_number_reaches_a_debug_output_or_the_stored_step() {
    let marker = "zq-sentinel";
    let planted = 777.25;
    let mut begin = header();
    begin.display = Some(ScreenDisplay {
        origin: [77_725, 77_726],
        size: [2560, 1440],
        dpi: 77_727,
    });
    let authored = step(&json!([
        {"highlight":{"id":"h","target":{"box":[planted,planted,planted,planted]}}},
        {"callout":{"id":"c","target":{"box":[planted,10,20,20]},"text":marker}}
    ]));
    let stored = screen::without_boxes(&authored);
    let mut outputs = vec![
        format!("{authored:?}"),
        format!("{stored:?}"),
        format!("{begin:?}"),
        format!("{:?}", Record::Begin(begin)),
    ];
    outputs.extend(
        authored
            .put
            .iter()
            .chain(stored.put.iter())
            .map(|op| format!("{op:?}")),
    );
    for output in &outputs {
        for absent in ["777", "77725", "77727", marker] {
            assert!(!output.contains(absent), "{absent} in {output}");
            assert!(
                !output.contains(&format!("{:?}", absent.as_bytes())),
                "{absent} bytes"
            );
        }
    }
    let bytes = serde_json::to_string(&stored).expect("stored JSON");
    assert!(!bytes.contains("777"), "a box in the stored step");
    assert!(bytes.contains(marker), "labels are kept");
    for op in stored.put.iter() {
        let target = match op {
            Op::Highlight(mark) => &mark.target,
            Op::Callout(label) => &label.target,
            _ => panic!("only marks"),
        };
        let kumochi_scene::record::Target::Box(bounds) = target else {
            panic!("a box target");
        };
        assert_eq!(
            bounds.map(|value| f32::from(value).to_bits()),
            PLACEHOLDER_BOX.map(f32::to_bits)
        );
    }
}
