//! The screen surface: its stamp, what it admits, its dressing, its placement and its stored form.
use kumochi_scene::{
    Admission, Begin, ErrorCode, Frame, GlobalBudget, Op, Record, SceneState, SceneView, Step,
    StepError, WireId,
    display::{Item, MeasureError, Paint, Prim, TextMeasure, TextStyle},
    geom::{Affine, Rect, Vec2},
    layout::{LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare},
    limits::{MAX_MARKER_OPACITY, MAX_SPOTLIGHT_DIM},
    parse_draw_record, parse_screen_draw_record,
    record::{ScreenDisplay, Surface},
    screen::{self, MARK_RGB, PLACEHOLDER_BOX, SPOTLIGHT_RGB},
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

const DISPLAY: ScreenDisplay = ScreenDisplay {
    origin: [0, 0],
    size: [2560, 1440],
    dpi: 96,
};
const PICTURE: [u32; 2] = [1280, 720];

struct Measure;
impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _: TextStyle,
        _: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let chars = u16::try_from(text.chars().count()).expect("fixture text");
        Ok([f32::from(chars) * 8.0, 10.0])
    }
}

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
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}
fn refusal(put: &Value) -> (String, ErrorCode) {
    let scene = SceneState::begin(header()).expect("scene");
    let error = scene
        .admit(&step(put), &GlobalBudget::default())
        .expect_err("refused");
    (error.path.as_str().to_owned(), error.code)
}
#[expect(
    clippy::panic_in_result_fn,
    reason = "fixture steps always parse and admit; only preparation's result is under test"
)]
fn prepared(steps: &[Value]) -> Result<PreparedStep, PrepareError> {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut layout = LayoutState::new(&scene);
    let mut last = None;
    for (index, put) in steps.iter().enumerate() {
        let mut body = if put.is_array() {
            json!({"put":put})
        } else {
            put.clone()
        };
        body["id"] = json!(format!("s{index}"));
        let value = json!({"step":body});
        let Ok(Record::Step(step)) = record(&value) else {
            panic!("screen step");
        };
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admitted") else {
            panic!("new step");
        };
        let committed = scene.commit(candidate, &mut budget).expect("committed");
        let input = PrepareInput::new(
            committed.preparation,
            layout.clone(),
            SurfaceGeometry::Overlay {
                default_canvas: [960.0, 600.0],
            },
            Arc::new(Theme::default()),
        )
        .map_err(|(error, _)| error)?;
        let result = prepare(input, &mut Measure, &AtomicBool::new(false))?;
        layout = result.after.clone();
        last = Some(result);
    }
    Ok(last.expect("at least one step"))
}
fn item(result: &PreparedStep, name: &str) -> Item {
    let handle = result.after.handle(&id(name)).expect("placed");
    result
        .after
        .items()
        .into_iter()
        .find(|item| item.handle == handle)
        .expect("item")
}
type Drawn = (Rect, f32, Option<Paint>, Option<(Paint, f32)>);
fn rects(item: &Item) -> Vec<Drawn> {
    item.primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Rect {
                rect,
                radius,
                fill,
                border,
            } => Some((*rect, *radius, *fill, *border)),
            _ => None,
        })
        .collect()
}
fn near(actual: Rect, expected: [f32; 4]) -> bool {
    let got = [
        actual.origin.x,
        actual.origin.y,
        actual.size.x,
        actual.size.y,
    ];
    got.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-3)
}
fn rect(value: [f32; 4]) -> Rect {
    Rect {
        origin: Vec2::new(value[0], value[1]),
        size: Vec2::new(value[2], value[3]),
    }
}
/// Whether every point of `target` lies inside the hole of a rounded outline.
fn clears(target: Rect, outline: Drawn) -> bool {
    let (outer, radius, _, border) = outline;
    let width = border.expect("an outline").1;
    let hole = Rect {
        origin: outer.origin + Vec2::new(width, width),
        size: outer.size - Vec2::new(width * 2.0, width * 2.0),
    };
    let inner = radius - width;
    [
        target.origin,
        target.max(),
        Vec2::new(target.origin.x, target.max().y),
        Vec2::new(target.max().x, target.origin.y),
    ]
    .into_iter()
    .all(|corner| {
        let core = Vec2::new(
            corner
                .x
                .min(hole.max().x - inner)
                .max(hole.origin.x + inner),
            corner
                .y
                .min(hole.max().y - inner)
                .max(hole.origin.y + inner),
        );
        (corner.x - core.x).hypot(corner.y - core.y) <= inner + 1e-3
    })
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
fn screen_labels_refuse_hidden_formatting_and_line_separators_at_admission() {
    let scene = SceneState::begin(header()).expect("scene");
    let budget = GlobalBudget::default();
    let controls = [
        '\u{ad}',
        '\u{34f}',
        '\u{61c}',
        '\u{115f}',
        '\u{1160}',
        '\u{17b4}',
        '\u{17b5}',
        '\u{180e}',
        '\u{200b}',
        '\u{200c}',
        '\u{200d}',
        '\u{200e}',
        '\u{200f}',
        '\u{2028}',
        '\u{2029}',
        '\u{202a}',
        '\u{202b}',
        '\u{202c}',
        '\u{202d}',
        '\u{202e}',
        '\u{2060}',
        '\u{2061}',
        '\u{2064}',
        '\u{2065}',
        '\u{2066}',
        '\u{2067}',
        '\u{2068}',
        '\u{2069}',
        '\u{206a}',
        '\u{206f}',
        '\u{3164}',
        '\u{feff}',
        '\u{ffa0}',
        '\u{fff0}',
        '\u{fff8}',
        '\u{1bca0}',
        '\u{1bca3}',
        '\u{1d173}',
        '\u{1d17a}',
        '\u{e0000}',
        '\u{e0001}',
        '\u{e0020}',
        '\u{e0061}',
        '\u{e007f}',
        '\u{e00ff}',
        '\u{e01f0}',
        '\u{e0fff}',
    ];
    let missing: Vec<_> = controls
        .into_iter()
        .filter_map(|control| {
            let text = format!("a{control}b");
            let parsed =
                step(&json!([{"callout":{"id":"c","target":{"box":[10,10,20,20]},"text":text}}]));
            let refused = scene.admit(&parsed, &budget).err().is_some_and(|error| {
                error.path.as_str() == "put[0].text" && error.code == ErrorCode::LabelTooLong
            });
            (!refused).then_some(u32::from(control))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "wrong or missing refusals: {missing:X?}"
    );
    assert_eq!(
        ErrorCode::LabelTooLong.to_string(),
        "scene.LabelTooLong: a label on the screen is one line of at most 24 characters, without hidden formatting controls"
    );
}

#[test]
fn screen_labels_admit_writing_marks_and_variation_selectors() {
    let scene = SceneState::begin(header()).expect("scene");
    let budget = GlobalBudget::default();
    for text in [
        "Cafe\u{301}",
        "שָׁלוֹם",
        "عَرَبِيّ",
        "हिन्दी",
        "☀\u{fe0f}",
        "ᠠ\u{180b}\u{180d}\u{180f}",
        "禰\u{e0100}\u{e01ef}",
    ] {
        let parsed =
            step(&json!([{"callout":{"id":"c","target":{"box":[10,10,20,20]},"text":text}}]));
        assert!(matches!(
            scene.admit(&parsed, &budget).expect("valid label"),
            Admission::New(_)
        ));
    }
}

#[test]
fn screen_labels_refuse_a_joined_emoji_before_commit_but_overlays_admit_it() {
    let text = "👩\u{200d}💻";
    let mut overlay_header = model_header();
    overlay_header.surface = Surface::Overlay;
    let overlay = SceneState::begin(overlay_header).expect("overlay");
    let ordinary = json!({"step":{"id":"overlay","put":[
        {"text":{"id":"anchor","text":"Anchor","at":[0,0]}},
        {"callout":{"id":"label","target":{"ref":"anchor"},"text":text}}
    ]}});
    let bytes = serde_json::to_vec(&ordinary).expect("fixture JSON");
    let Record::Step(ordinary) = parse_draw_record(&bytes).expect("ordinary draw record") else {
        panic!("ordinary draw step");
    };
    let mut budget = GlobalBudget::default();
    assert!(matches!(
        overlay.admit(&ordinary, &budget).expect("ordinary label"),
        Admission::New(_)
    ));

    let mut scene = SceneState::begin(header()).expect("screen");
    let first = step(&json!([{"highlight":{"id":"first","target":{"box":[10,10,20,20]}}}]));
    let Admission::New(first) = scene.admit(&first, &budget).expect("first admitted") else {
        panic!("first step");
    };
    scene.commit(first, &mut budget).expect("first committed");
    let mut later = step(&json!([
        {"highlight":{"id":"later","target":{"box":[30,30,20,20]}}},
        {"callout":{"id":"label","target":{"box":[30,30,20,20]},"text":text}}
    ]));
    later.id = id("bad");
    let error = scene
        .admit(&later, &budget)
        .err()
        .expect("screen label refused");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("put[1].text", ErrorCode::LabelTooLong)
    );
    assert_eq!(scene.committed(), 1);
    assert!(scene.handle(&id("first")).is_some());
    assert_eq!(scene.handle(&id("later")), None);
    assert_eq!(scene.handle(&id("label")), None);
}

#[test]
fn a_box_mark_is_a_two_tone_outline_outside_the_padded_box_in_logical_pixels() {
    let result = prepared(&[
        json!([{"highlight":{"id":"h","target":{"box":[100,100,40,20]},"style":"box"}}]),
    ])
    .expect("prepared");
    let drawn = rects(&item(&result, "h"));
    assert_eq!(drawn.len(), 2);
    // Two logical pixels per picture pixel: pad 2, edge 0.5, stroke 1, corner 3 grown by 3(1 - 1/√2).
    let (outer, radius, fill, border) = drawn[0];
    assert!(
        near(outer, [95.121_32, 95.121_32, 49.757_36, 29.757_36]),
        "{outer:?}"
    );
    assert!((radius - 5.0).abs() < 1e-4 && fill.is_none());
    let (paint, width) = border.expect("white edge");
    assert_eq!(
        (paint.rgb.get(), paint.opacity, width),
        (0x00FF_FFFF, 1.0, 2.0)
    );
    let (inner, radius, _, border) = drawn[1];
    assert!(
        near(inner, [95.621_32, 95.621_32, 48.757_36, 28.757_36]),
        "{inner:?}"
    );
    assert!((radius - 4.5).abs() < 1e-4);
    let (paint, width) = border.expect("stroke");
    assert_eq!((paint.rgb.get(), width), (MARK_RGB, 1.0));
}

#[test]
fn rings_underlines_markers_and_spotlights_keep_their_numbers() {
    let mark = |style: &str| {
        let result = prepared(&[
            json!([{"highlight":{"id":"h","target":{"box":[100,100,40,20]},"style":style}}]),
        ])
        .expect("prepared");
        rects(&item(&result, "h"))
    };
    let ring = mark("ring");
    assert!(
        near(ring[0].0, [91.029_44, 91.029_44, 57.941_12, 37.941_12]),
        "{:?}",
        ring[0].0
    );
    assert!((ring[0].1 - 18.970_56).abs() < 1e-3, "a loop: round ends");
    let underline = mark("underline");
    assert!(near(underline[0].0, [100.0, 122.0, 40.0, 2.0]));
    assert!(near(underline[1].0, [100.5, 122.5, 39.0, 1.0]));
    assert_eq!((underline[0].1, underline[1].1), (1.0, 0.5));
    let marker = mark("marker");
    assert_eq!(marker.len(), 1);
    assert!(near(marker[0].0, [98.0, 98.0, 44.0, 24.0]));
    assert_eq!(
        marker[0].2.map(|paint| paint.opacity),
        Some(MAX_MARKER_OPACITY)
    );
    let spotlight = mark("spotlight");
    let dims: Vec<_> = spotlight.iter().filter(|drawn| drawn.3.is_none()).collect();
    assert_eq!(dims.len(), 4);
    for dim in dims {
        let paint = dim.2.expect("dimming");
        assert_eq!(
            (paint.rgb.get(), paint.opacity),
            (SPOTLIGHT_RGB, MAX_SPOTLIGHT_DIM)
        );
    }
}

#[test]
fn no_outline_ever_covers_its_target_whatever_its_shape_or_pad() {
    for target in [
        [600.0, 300.0, 4.0, 4.0],
        [100.0, 100.0, 400.0, 12.0],
        [200.0, 200.0, 300.0, 200.0],
    ] {
        for pad in [None, Some(0.1)] {
            for style in ["box", "ring", "spotlight"] {
                let mut highlight = json!({"id":"h","target":{"box":target},"style":style});
                if let Some(pad) = pad {
                    highlight["pad"] = json!(pad);
                }
                let result = prepared(&[json!([{ "highlight": highlight }])]).expect("prepared");
                let outlines: Vec<_> = rects(&item(&result, "h"))
                    .into_iter()
                    .filter(|drawn| drawn.3.is_some())
                    .collect();
                assert_eq!(outlines.len(), 2);
                for outline in outlines {
                    assert!(clears(rect(target), outline), "{style} {target:?} {pad:?}");
                }
            }
            let mut underline = json!({"id":"h","target":{"box":target},"style":"underline"});
            if let Some(pad) = pad {
                underline["pad"] = json!(pad);
            }
            let result = prepared(&[json!([{ "highlight": underline }])]).expect("prepared");
            for (bar, ..) in rects(&item(&result, "h")) {
                assert!(
                    bar.origin.y > target[1] + target[3],
                    "an underline below its words"
                );
            }
        }
    }
}

#[test]
fn a_label_sits_on_the_named_side_outside_its_mark_or_the_next_free_side() {
    let label = |target: [u32; 4], side: &str, others: Value| {
        let result = prepared(&[
            others,
            json!([{"callout":{"id":"c","target":{"box":target},"text":"Save","side":side}}]),
        ])
        .expect("prepared");
        item(&result, "c").bounds
    };
    let marks = |target: [u32; 4]| json!([{"highlight":{"id":"h","target":{"box":target}}}]);
    // Label 40 by 15, 3 canvas units (the 6 px gap, halved) off the ring, whose outline reaches
    // 5.757 out: pad 2, the 6-unit corner's growth 1.757 and outline 2.
    let top = label([600, 300, 80, 40], "top", marks([600, 300, 80, 40]));
    assert!(near(top, [620.0, 276.243, 40.0, 15.0]), "{top:?}");
    let left = label([600, 300, 80, 40], "left", marks([600, 300, 80, 40]));
    assert!(near(left, [551.243, 312.5, 40.0, 15.0]), "{left:?}");
    let bottom = label([600, 2, 80, 40], "top", marks([600, 2, 80, 40]));
    assert!(near(bottom, [620.0, 50.757, 40.0, 15.0]), "{bottom:?}");
    let blocked = json!([
        {"highlight":{"id":"h","target":{"box":[600,2,80,40]}}},
        {"highlight":{"id":"k","target":{"box":[600,50,80,20]}}}
    ]);
    let right = label([600, 2, 80, 40], "top", blocked);
    assert!(near(right, [688.757, 14.5, 40.0, 15.0]), "{right:?}");
}

#[test]
fn a_label_on_a_looped_mark_s_own_box_sits_beside_the_mark() {
    let result = prepared(&[json!([
        {"highlight":{"id":"h","target":{"box":[100,100,200,20]},"style":"box"}},
        {"callout":{"id":"c","target":{"box":[100,100,200,20]},"text":"Save"}}
    ])])
    .expect("prepared");
    // The round-ended outline reaches 8.97 units out (pad 2, the radius's growth 4.97, outline
    // 2), past the label's fixed 7; the label keeps its 3-unit gap from the outline instead.
    let top = item(&result, "c").bounds;
    assert!(near(top, [180.0, 73.029, 40.0, 15.0]), "{top:?}");
}

#[test]
fn a_label_keeps_off_every_mark_of_its_step_and_moves_off_a_later_one() {
    let callout = json!({"callout":{"id":"c","target":{"box":[600,300,80,40]},"text":"Save"}});
    // Its round-ended outline reaches 6.9 units out, onto the top label; the box itself doesn't.
    let mark = json!({"highlight":{"id":"h","target":{"box":[610,262,60,10]},"style":"box"}});
    let other = json!({"callout":{"id":"d","target":{"box":[610,270,60,10]},"text":"Here"}});
    let far = json!({"highlight":{"id":"f","target":{"box":[10,10,20,20]}}});
    let alone = item(&prepared(&[json!([callout])]).expect("prepared"), "c").bounds;
    assert!(near(alone, [620.0, 278.0, 40.0, 15.0]), "{alone:?}");
    let kept = item(
        &prepared(&[json!([callout]), json!([far])]).expect("prepared"),
        "c",
    )
    .bounds;
    assert!(near(kept, [620.0, 278.0, 40.0, 15.0]), "{kept:?}");
    for steps in [
        vec![json!([callout, mark])],
        vec![json!([callout, other])],
        vec![json!([callout]), json!([mark])],
    ] {
        let label = item(&prepared(&steps).expect("prepared"), "c").bounds;
        assert!(
            near(label, [620.0, 347.0, 40.0, 15.0]),
            "{steps:?}: {label:?}"
        );
    }
}

#[test]
fn an_earlier_label_with_no_side_left_sits_out_until_one_frees() {
    let callout = json!([{"callout":{"id":"c","target":{"box":[600,300,80,40]},"text":"Save"}}]);
    // The eight sides' labels: one row above, one below, and one each to the left and right.
    let fence = json!([
        {"highlight":{"id":"t","target":{"box":[550,270,180,10]}}},
        {"highlight":{"id":"b","target":{"box":[550,355,180,10]}}},
        {"highlight":{"id":"l","target":{"box":[560,315,10,10]}}},
        {"highlight":{"id":"r","target":{"box":[700,315,10,10]}}}
    ]);
    let fenced = prepared(&[callout.clone(), fence.clone()]).expect("the fencing step stands");
    assert!(
        item(&fenced, "c").primitives.is_empty(),
        "the label sits out"
    );
    let freed = prepared(&[callout, fence, json!({"remove":["t"]})]).expect("prepared");
    let label = item(&freed, "c");
    assert_eq!(label.primitives.len(), 2, "the label is back");
    assert!(
        near(label.bounds, [620.0, 278.0, 40.0, 15.0]),
        "{:?}",
        label.bounds
    );
}

#[test]
fn a_label_that_sits_out_leaves_its_place_to_a_later_label() {
    let callout = json!([{"callout":{"id":"c","target":{"box":[600,300,80,40]},"text":"Save"}}]);
    let fence = json!([
        {"highlight":{"id":"t","target":{"box":[550,270,180,10]}}},
        {"highlight":{"id":"b","target":{"box":[550,355,180,10]}}},
        {"highlight":{"id":"l","target":{"box":[560,315,10,10]}}},
        {"highlight":{"id":"r","target":{"box":[700,315,10,10]}}}
    ]);
    // c sat out at its old centre (640, 285.5), which d's named side covers.
    let later = json!({"remove":["t"],"put":[
        {"callout":{"id":"d","target":{"box":[610,255,60,10]},"text":"Open","side":"bottom"}}
    ]});
    let result = prepared(&[callout, fence, later]).expect("prepared");
    let label = item(&result, "d");
    assert!(
        near(label.bounds, [620.0, 272.0, 40.0, 15.0]),
        "{:?}",
        label.bounds
    );
}

#[test]
fn a_screen_scene_holds_one_spotlight() {
    let spot = |id: &str, at: u32| json!({"highlight":{"id":id,"target":{"box":[at,10,20,20]},"style":"spotlight"}});
    assert_eq!(
        refusal(&json!([spot("a", 10), spot("b", 40)])),
        ("put[1].style".to_owned(), ErrorCode::Unsupported)
    );
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let Admission::New(first) = scene
        .admit(&step(&json!([spot("a", 10)])), &budget)
        .expect("one spotlight")
    else {
        panic!("new step");
    };
    scene.commit(first, &mut budget).expect("committed");
    let later = |put: Value| {
        let Ok(Record::Step(step)) = record(&json!({"step":{"id":"t","put":put}})) else {
            panic!("screen step");
        };
        step
    };
    let error = scene
        .admit(&later(json!([spot("b", 40)])), &budget)
        .expect_err("a second spotlight");
    assert_eq!(
        (error.path.as_str(), error.code),
        ("put[0].style", ErrorCode::Unsupported)
    );
    let boxed = json!([{"highlight":{"id":"b","target":{"box":[40,10,20,20]}}}]);
    scene
        .admit(&later(boxed), &budget)
        .expect("a box after the spotlight");
}

#[test]
fn a_label_with_no_legal_side_is_refused() {
    let error = prepared(&[json!([
        {"callout":{"id":"c","target":{"box":[0,0,1280,720]},"text":"Everything"}}
    ])])
    .expect_err("no room");
    let PrepareError::Layout(refusal) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        (
            refusal.step.as_ref().map(WireId::as_str),
            refusal.path.as_str(),
            refusal.code
        ),
        (Some("s0"), "put[0].callout", ErrorCode::NoRoom)
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

#[test]
fn a_screen_view_sits_on_its_own_display_keeps_off_the_stage_and_paints_nothing_unplaced() {
    let marked = prepared(&[json!([
        {"highlight":{"id":"h","target":{"box":[100,100,200,50]}}}
    ])])
    .expect("prepared");
    let painted = |view: &SceneView| {
        let mut frame = Frame::new();
        view.frame(u64::MAX, &mut frame).expect("sampled");
        frame
            .items()
            .iter()
            .map(|item| item.transform)
            .collect::<Vec<_>>()
    };
    let resting = painted(&SceneView::new(&marked.after, 1));
    assert!(!resting.is_empty());
    let twice = Affine([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]);
    let on_screen: Vec<Affine> = resting.iter().map(|at| twice.compose(*at)).collect();
    let mut view = SceneView::new(&marked.after, 1);
    let moved = ScreenDisplay {
        origin: [2560, 0],
        ..DISPLAY
    };
    for (header, overlay) in [
        (header(), None),
        (header(), Some(moved)),
        (model_header(), Some(DISPLAY)),
    ] {
        assert!(!view.set_screen(&header, overlay));
        assert_eq!(painted(&view), [], "{overlay:?}");
    }
    assert!(view.set_screen(&header(), Some(DISPLAY)));
    assert_eq!(painted(&view), on_screen);
    assert!(!view.set_stage(rect([0.0, 0.0, 300.0, 200.0])));
    assert_eq!(painted(&view), on_screen, "the stage never moves it");
    let card = Begin {
        surface: Surface::Overlay,
        ..header()
    };
    let mut ordinary = SceneView::new(&marked.after, 1);
    assert!(!ordinary.set_screen(&card, Some(DISPLAY)));
    assert_eq!(painted(&ordinary), resting);
    assert!(ordinary.set_stage(rect([0.0, 0.0, 300.0, 200.0])));
    let scene = SceneState::begin(header()).expect("scene");
    let mut begun = SceneView::new(&LayoutState::new(&scene), scene.activated());
    for overlay in [Some(DISPLAY), None] {
        assert_eq!(begun.set_screen(&header(), overlay), overlay.is_some());
        assert_eq!(painted(&begun), [], "a begun screen scene with no step");
    }
}
