//! Screen scenes in the drawing system: each gate's fixed refusal, the stamp before a lookup or
//! a continuation, the stored forms without boxes, the layout refusal, and recovery's skip.

use serde_json::{Value, json};

use super::*;
use crate::context::{Geometry, NoCanvas};
use crate::drawing::stored_item;
use crate::engines::Opaque;
use kumochi_scene::record::{ScreenDisplay, Surface, Target};
use kumochi_scene::screen::PLACEHOLDER_BOX;
use kumochi_scene::{Finite, Op};

const NOT_KEPT: &str = "; the marks' positions are not kept after this turn";

/// A screen scene's begin, with `extra` fields after its alt.
fn screen_begin(scene: &str, extra: &str) -> String {
    format!(r#"{{"begin":{{"scene":"{scene}","surface":"screen","alt":"marks"{extra}}}}}"#)
}

/// A step marking a box and labelling another, as the drafted example does.
fn marked(step: &str, label: &str) -> String {
    format!(
        r#"{{"id":"{step}","say":"Here.","put":[{{"highlight":{{"id":"{step}-mark","target":{{"box":[186,598,450,78]}},"style":"box"}}}},{{"callout":{{"id":"{step}-label","target":{{"box":[201,561,438,12]}},"text":"{label}","side":"top"}}}}]}}"#
    )
}

fn screen_call(run: u64, name: &str, begin: &str, steps: &[String]) -> Input {
    call(run, name, envelope(begin, steps))
}

/// A drawing told run 1 may draw on `canvas`.
fn told(canvas: Result<Geometry, NoCanvas>) -> Drawing {
    let mut drawing = Drawing::default();
    drawing.canvas(RunId(1), canvas);
    drawing
}

fn boxes(step: &Step) -> Vec<[f32; 4]> {
    step.put
        .iter()
        .filter_map(|op| match op {
            Op::Highlight(mark) => Some(&mark.target),
            Op::Callout(label) => Some(&label.target),
            _ => None,
        })
        .filter_map(|target| match target {
            Target::Box(bounds) => Some(bounds.map(Finite::get)),
            Target::Anchor(_) => None,
        })
        .collect()
}

fn labels(step: &Step) -> Vec<String> {
    step.put
        .iter()
        .filter_map(|op| match op {
            Op::Callout(label) => Some(label.text.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

fn stored_step(store: &Store, scene: usize, index: usize) -> Step {
    match &store.scenes[scene].log[index] {
        SceneEvent::Step { step, .. } => Step::clone(step),
        SceneEvent::Stop(_) => panic!("a step, not a cut"),
    }
}

fn draw_item(args: &str) -> Opaque {
    let item =
        json!({"type":"function_call","id":"fc_1","call_id":"c1","name":"draw","arguments":args});
    Opaque::new(serde_json::to_vec(&item).expect("fixture")).expect("a small item")
}

fn arguments(item: &Opaque) -> Value {
    let item: Value = serde_json::from_slice(item.bytes()).expect("an item");
    serde_json::from_str(item["arguments"].as_str().expect("arguments")).expect("JSON arguments")
}

#[test]
fn a_screen_scene_is_stamped_with_the_picture_and_stored_without_its_boxes() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let out = store.run(
        &mut drawing,
        screen_call(
            1,
            "c",
            &screen_begin("marks", ""),
            &[marked("s1", "the question")],
        ),
    );
    assert_eq!(out.calls[0].tool_text(), format!("drawn: [s1]{NOT_KEPT}"));
    let header = &store.scenes[0].header.begin;
    assert_eq!(
        header.size.map(|size| size.map(Finite::get)),
        Some([1280.0, 720.0])
    );
    assert_eq!(
        header.display,
        Some(ScreenDisplay {
            origin: [0, 0],
            size: [2560, 1440],
            dpi: 96,
        })
    );
    let stored = stored_step(&store, 0, 0);
    assert_eq!(boxes(&stored), [PLACEHOLDER_BOX; 2]);
    assert_eq!(labels(&stored), ["the question"]);
    assert_eq!(
        stored.say.as_ref().map(kumochi_scene::Text::as_str),
        Some("Here.")
    );
    let real = [[186.0, 598.0, 450.0, 78.0], [201.0, 561.0, 438.0, 12.0]];
    let held = drawing.scenes[0].state.step(0).expect("the step");
    assert_eq!(boxes(held), real);
    let shown = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Data(DataToShell::SceneStepCommit { step, .. }) => Some(step),
            _ => None,
        })
        .expect("the step goes to the shell");
    assert_eq!(boxes(shown), real);
}

#[test]
fn an_overlay_scene_beside_it_is_stored_as_written() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let card = envelope(
        &begin("card", ""),
        &[r#"{"id":"c1","put":[{"callout":{"id":"l","target":{"box":[10,20,30,40]},"text":"x"}}]}"#.to_owned()],
    );
    let item = draw_item(std::str::from_utf8(&card).expect("UTF-8"));
    assert_eq!(stored_item(item.clone()).bytes(), item.bytes());
    let out = store.run(&mut drawing, call(1, "c", card));
    assert_eq!(out.calls[0].tool_text(), "drawn: [c1]");
    assert_eq!(
        boxes(&stored_step(&store, 0, 0)),
        [[10.0, 20.0, 30.0, 40.0]]
    );
    assert_eq!(store.scenes[0].header.begin.display, None);
}

#[test]
fn each_gate_refuses_a_screen_scene_with_its_fixed_reason() {
    for (canvas, reason) in [
        (
            NoCanvas::NoPicture,
            "surface: drawing.NoScreenPicture; no picture of the user's screen was sent in this turn, so the screen can't be marked; explain in words or draw a card",
        ),
        (
            NoCanvas::OtherDisplay,
            "surface: drawing.OtherDisplay; the picture shows a display that can't be marked; explain in words or draw a card",
        ),
        (
            NoCanvas::Off,
            "surface: drawing.ScreenOff; the user turned off sharing the screen; explain in words or draw a card",
        ),
    ] {
        let mut drawing = told(Err(canvas));
        let mut store = Store::default();
        let out = store.run(
            &mut drawing,
            screen_call(1, "c", &screen_begin("marks", ""), &[marked("s1", "x")]),
        );
        assert_eq!(
            out.calls[0].tool_text(),
            format!("drawn: []; stopped: {reason}")
        );
        assert!(store.scenes.is_empty());
        let card = store.run(&mut drawing, call(1, "d", args("card", &[text("a", "x")])));
        assert_eq!(card.calls[0].tool_text(), "drawn: [a]");
    }
}

#[test]
fn with_the_screen_marks_off_a_screen_scene_is_refused_as_before() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let out = store.run(
        &mut drawing,
        screen_call(1, "c", &screen_begin("marks", ""), &[marked("s1", "x")]),
    );
    assert_eq!(
        out.calls[0].tool_text(),
        format!("drawn: []; stopped: surface: scene.Unsupported{RETRY}")
    );
    assert!(store.scenes.is_empty());
}

#[test]
fn a_written_size_must_be_the_picture_s_and_a_written_display_is_refused() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let other = store.run(
        &mut drawing,
        screen_call(
            1,
            "a",
            &screen_begin("marks", r#","size":[1920,1080]"#),
            &[marked("s1", "x")],
        ),
    );
    assert_eq!(
        other.calls[0].tool_text(),
        format!(
            "drawn: []; stopped: size: scene.SizeNotPicture: leave size out; a screen scene's size is the picture's, stated beside it; the picture is 1280 by 720 pixels{RETRY}{NOT_KEPT}"
        )
    );
    let display = r#","display":{"origin":[0,0],"size":[2560,1440],"dpi":96}"#;
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let written = store.run(
        &mut drawing,
        screen_call(
            1,
            "b",
            &screen_begin("marks", display),
            &[marked("s1", "x")],
        ),
    );
    assert_eq!(
        written.calls[0].tool_text(),
        format!("drawn: []; stopped: display: scene.Unsupported{RETRY}")
    );
    assert!(store.scenes.is_empty());
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let same = store.run(
        &mut drawing,
        screen_call(
            1,
            "c",
            &screen_begin("marks", r#","size":[1280,720]"#),
            &[marked("s1", "x")],
        ),
    );
    assert_eq!(same.calls[0].tool_text(), format!("drawn: [s1]{NOT_KEPT}"));
}

#[test]
fn a_later_call_continues_the_stamped_scene_and_a_different_size_is_refused() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let begin = screen_begin("marks", "");
    store.run(
        &mut drawing,
        screen_call(1, "a", &begin, &[marked("s1", "x")]),
    );
    let next = store.run(
        &mut drawing,
        screen_call(1, "b", &begin, &[marked("s2", "y")]),
    );
    assert_eq!(next.calls[0].tool_text(), format!("drawn: [s2]{NOT_KEPT}"));
    assert_eq!(store.scenes.len(), 1);
    assert_eq!(store.scenes[0].log.len(), 2);
    let resized = store.run(
        &mut drawing,
        screen_call(
            1,
            "c",
            &screen_begin("marks", r#","size":[640,360]"#),
            &[marked("s3", "z")],
        ),
    );
    assert_eq!(error(&resized.calls[0]), ErrorCode::SizeNotPicture);
    assert_eq!(store.scenes[0].log.len(), 2);
}

#[test]
fn a_label_with_no_room_refuses_its_step_by_path() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let crowded = r#"{"id":"s1","put":[{"callout":{"id":"l","target":{"box":[0,0,1280,720]},"text":"everything"}}]}"#;
    let out = store.run(
        &mut drawing,
        screen_call(1, "c", &screen_begin("marks", ""), &[crowded.to_owned()]),
    );
    assert_eq!(
        out.calls[0].tool_text(),
        format!(
            "drawn: []; stopped: step s1: put[0].callout: scene.NoRoom: no free side outside the marks has room for this label{RETRY}{NOT_KEPT}"
        )
    );
    assert!(store.scenes.is_empty());
}

#[test]
fn recovery_leaves_out_a_screen_scene_and_restores_the_scene_beside_it() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    store.run(
        &mut drawing,
        screen_call(1, "a", &screen_begin("marks", ""), &[marked("s1", "x")]),
    );
    store.run(&mut drawing, call(1, "b", args("card", &[text("a", "x")])));
    let (recovered, skipped) = Drawing::recover(store.visible());
    assert_eq!(skipped, [SceneId(1)]);
    assert_eq!(
        recovered
            .snapshot()
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(2)]
    );
}

#[test]
fn a_screen_call_s_native_item_keeps_its_records_without_boxes() {
    let args = String::from_utf8(envelope(
        &screen_begin("marks", ""),
        &[marked("s1", "the question")],
    ))
    .expect("JSON");
    let stored = arguments(&stored_item(draw_item(&args)));
    let records = stored["records"].as_array().expect("records");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["begin"]["surface"], "screen");
    assert_eq!(records[0]["begin"].get("display"), None);
    let put = &records[1]["step"]["put"];
    assert_eq!(
        put[0]["highlight"]["target"],
        json!({"box":[0.0, 0.0, 1.0, 1.0]})
    );
    assert_eq!(
        put[1]["callout"]["target"],
        json!({"box":[0.0, 0.0, 1.0, 1.0]})
    );
    assert_eq!(put[1]["callout"]["text"], "the question");
    assert_eq!(records[1]["step"]["say"], "Here.");
    let item: Value = serde_json::from_slice(stored_item(draw_item(&args)).bytes()).expect("item");
    assert_eq!(
        (&item["call_id"], &item["name"]),
        (&json!("c1"), &json!("draw"))
    );
}

#[test]
fn other_items_and_overlay_calls_are_stored_as_they_came() {
    let overlay = String::from_utf8(args("card", &[text("a", "x")])).expect("JSON");
    let item = draw_item(&overlay);
    assert_eq!(stored_item(item.clone()), item);
    let lookup = json!({"type":"function_call","call_id":"c2","name":"lookup","arguments":"{\"box\":[1,2,3,4]}"});
    let item = Opaque::new(serde_json::to_vec(&lookup).expect("fixture")).expect("small");
    assert_eq!(stored_item(item.clone()), item);
    let said = Opaque::new(br#"{"type":"message","role":"assistant","content":"x"}"#.to_vec())
        .expect("small");
    assert_eq!(stored_item(said.clone()), said);
}

#[test]
fn a_screen_call_whose_records_can_t_be_read_keeps_none() {
    let begin = screen_begin("marks", "");
    for args in [
        format!(r#"{{"records":[{begin},{{"step":{{"id":"s1","put":[{{"highlight":{{"id":"d","target":{{"box":[186,598,4"#),
        r#"{"records":[{"begin":{"scene":"marks","surf"#.to_owned(),
        format!(r#"{{"records":[{begin},{{"step":{{"id":"s1","typo":[186,598,450,78]}}}},{{"end":{{}}}}]}}"#),
        r#"{"records":[{"step":{"id":"s1","put":[{"highlight":{"id":"d","target":{"box":[186,598,450,78]},"style":"box"}}]}}]}"#.to_owned(),
    ] {
        assert_eq!(arguments(&stored_item(draw_item(&args))), json!({"records":[]}), "{args}");
    }
}

#[test]
fn no_mark_s_box_or_display_reaches_a_debug_a_result_or_a_stored_box() {
    const LABEL: &str = "zq-label-7319";
    const BOX: &str = "917.375";
    const DISPLAY: [&str; 2] = ["2731", "1537"];
    let geometry = Geometry {
        display: ScreenDisplay {
            origin: [0, 0],
            size: [2731, 1537],
            dpi: 97,
        },
        picture: [1280, 720],
    };
    let step = format!(
        r#"{{"id":"s1","put":[{{"highlight":{{"id":"m","target":{{"box":[{BOX},300,50,40]}},"style":"box"}}}},{{"callout":{{"id":"l","target":{{"box":[{BOX},300,50,40]}},"text":"{LABEL}","side":"top"}}}}]}}"#
    );
    let args = envelope(&screen_begin("marks", ""), &[step]);
    let mut drawing = told(Ok(geometry));
    let mut store = Store::default();
    let out = store.run(&mut drawing, call(1, "c", args.clone()));
    assert_eq!(out.calls[0].drawn, [wire("s1")]);
    let absent = |text: &str, marker: &str| {
        let bytes = format!("{:?}", marker.as_bytes());
        !text.contains(marker) && !text.contains(bytes.trim_matches(['[', ']']))
    };
    let held = drawing.scenes[0].state.step(0).expect("the step");
    let debugs = [
        format!("{held:?}"),
        format!("{:?}", out.effects),
        format!("{:?}", out.calls),
        format!("{:?}", store.scenes),
        out.calls[0].tool_text(),
    ];
    for text in &debugs {
        for marker in [LABEL, BOX, DISPLAY[0], DISPLAY[1]] {
            assert!(absent(text, marker), "{marker} in {text}");
        }
    }
    let row = serde_json::to_string(&stored_step(&store, 0, 0)).expect("JSON");
    let item = String::from_utf8(
        stored_item(draw_item(&String::from_utf8(args).expect("JSON")))
            .bytes()
            .to_vec(),
    )
    .expect("UTF-8");
    for stored in [&row, &item] {
        assert!(stored.contains(LABEL), "{stored}");
        assert!(absent(stored, BOX), "{stored}");
        assert!(
            DISPLAY.iter().all(|marker| absent(stored, marker)),
            "{stored}"
        );
    }
    let header = serde_json::to_string(&store.scenes[0].header).expect("JSON");
    assert!(absent(&header, BOX) && absent(&header, LABEL), "{header}");
    assert!(
        DISPLAY.iter().all(|marker| header.contains(marker)),
        "{header}"
    );
}

#[test]
fn a_label_with_no_room_after_a_mark_names_its_own_put_index() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let crowded = r#"{"id":"s1","put":[{"highlight":{"id":"m","target":{"box":[40,40,20,20]},"style":"box"}},{"callout":{"id":"l","target":{"box":[0,0,1280,720]},"text":"everything"}}]}"#;
    let out = store.run(
        &mut drawing,
        screen_call(1, "c", &screen_begin("marks", ""), &[crowded.to_owned()]),
    );
    assert_eq!(
        out.calls[0].tool_text(),
        format!(
            "drawn: []; stopped: step s1: put[1].callout: scene.NoRoom: no free side outside the marks has room for this label{RETRY}{NOT_KEPT}"
        )
    );
    assert!(store.scenes.is_empty());
}

const OFF: &str = "surface: drawing.ScreenOff; the user turned off sharing the screen; explain in words or draw a card";

/// The screen scene headers and steps `out` sends the shell.
fn shown(out: &Output) -> usize {
    out.effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Data(DataToShell::SceneBegin { .. } | DataToShell::SceneStepCommit { .. })
            )
        })
        .count()
}

fn texts(outs: &[&Output]) -> Vec<String> {
    outs.iter()
        .flat_map(|out| out.calls.iter().map(CallEnd::tool_text))
        .collect()
}

#[test]
fn turning_sharing_off_ends_an_open_screen_call_and_refuses_its_later_steps_and_continuations() {
    const KEEP: &str = r#","others":"keep""#;
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "card", args("card", &[text("a", "x")])),
    );
    store.run(
        &mut drawing,
        record_of(1, "c", 0, &screen_begin("marks", KEEP)),
    );
    let first = store.run(
        &mut drawing,
        record_of(1, "c", 1, &step(&marked("s1", "x"))),
    );
    assert_eq!(shown(&first), 2);
    let off = store.run(&mut drawing, Input::ScreenOff);
    assert_eq!(
        texts(&[&off]),
        [format!("drawn: [s1]; stopped: {OFF}{NOT_KEPT}")]
    );
    let later = drawing
        .handle(record_of(1, "c", 2, &step(&marked("s2", "y"))))
        .map(|out| out.effects.len());
    assert_eq!(later, Err(crate::coordinator::Stale));
    let again = store.run(
        &mut drawing,
        screen_call(1, "d", &screen_begin("marks", KEEP), &[marked("s3", "z")]),
    );
    assert_eq!(texts(&[&again]), [format!("drawn: []; stopped: {OFF}")]);
    assert_eq!(shown(&again), 0);
    let live: Vec<_> = drawing
        .scenes
        .iter()
        .filter(|held| !held.gone)
        .map(|held| held.state.header().surface)
        .collect();
    assert_eq!(live, [Surface::Overlay]);
    let card = store.run(&mut drawing, call(1, "e", args("card2", &[text("b", "y")])));
    assert_eq!(texts(&[&card]), ["drawn: [b]"]);
}

#[test]
fn a_screen_call_waiting_on_its_lookup_its_layout_or_its_write_when_sharing_goes_off_shows_nothing()
{
    for wait in ["lookup", "layout", "write"] {
        let mut drawing = told(Ok(Geometry::FIXTURE));
        drawing.defer_preparation = wait == "layout";
        let mut store = Store::default();
        let lookup = drawing
            .handle(screen_call(
                1,
                "c",
                &screen_begin("marks", ""),
                &[marked("s1", "x")],
            ))
            .expect("a call");
        assert_eq!(trace(&lookup), ["lookup"]);
        let (off, settled) = match wait {
            "lookup" => {
                let off = drawing.handle(Input::ScreenOff).expect("the setting");
                let found = reply(&mut drawing, &lookup, Ok(Reply::Lookup(None)));
                (off, store.settle(&mut drawing, found))
            }
            "layout" => {
                let laid = reply(&mut drawing, &lookup, Ok(Reply::Lookup(None)));
                let job = laid
                    .effects
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::PrepareDrawing(job) => Some(job.clone()),
                        _ => None,
                    })
                    .expect("a step to lay out");
                let off = drawing.handle(Input::ScreenOff).expect("the setting");
                let prepared = drawing.handle(job.run()).expect("the pending preparation");
                (off, store.settle(&mut drawing, prepared))
            }
            _ => {
                let written = reply(&mut drawing, &lookup, Ok(Reply::Lookup(None)));
                assert_eq!(trace(&written), ["write step 1"]);
                let off = drawing.handle(Input::ScreenOff).expect("the setting");
                let mut both = written;
                merge(
                    &mut both,
                    Output {
                        effects: off.effects.clone(),
                        calls: Vec::new(),
                    },
                );
                (off, store.settle(&mut drawing, both))
            }
        };
        let drawn = if wait == "write" { "s1" } else { "" };
        assert_eq!(
            texts(&[&off, &settled]),
            [format!("drawn: [{drawn}]; stopped: {OFF}{NOT_KEPT}")],
            "{wait}"
        );
        assert_eq!(shown(&off) + shown(&settled), 0, "{wait}");
        assert!(drawing.scenes.iter().all(|held| held.gone), "{wait}");
    }
}

#[test]
fn a_held_screen_scene_s_step_being_written_when_sharing_goes_off_shows_nothing() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    let first = store.run(
        &mut drawing,
        screen_call(1, "b", &screen_begin("marks", ""), &[marked("s1", "x")]),
    );
    assert_eq!(shown(&first), 2);
    let written = drawing
        .handle(screen_call(
            1,
            "c",
            &screen_begin("marks", ""),
            &[marked("s2", "y")],
        ))
        .expect("a call");
    assert_eq!(trace(&written), ["write step 2"]);
    let off = drawing.handle(Input::ScreenOff).expect("the setting");
    let mut both = written;
    merge(
        &mut both,
        Output {
            effects: off.effects.clone(),
            calls: Vec::new(),
        },
    );
    let settled = store.settle(&mut drawing, both);
    assert_eq!(
        texts(&[&off, &settled]),
        [format!("drawn: [s2]; stopped: {OFF}{NOT_KEPT}")]
    );
    assert_eq!(shown(&off) + shown(&settled), 0);
    assert!(drawing.scenes.iter().all(|held| held.gone));
}

const CHANGED: &str = "surface: drawing.ScreenChanged; the user's screen may have changed after the picture was taken, so these marks weren't shown and the screen can't be marked again in this turn; explain in words or draw a card";

/// The visible screen scene.
fn screen_scene(drawing: &Drawing) -> SceneId {
    drawing
        .scenes
        .iter()
        .find(|held| !held.gone && held.state.header().surface == Surface::Screen)
        .map(|held| held.scene)
        .expect("a visible screen scene")
}

#[test]
fn a_skipped_screen_scene_ends_its_open_call_as_changed_and_closes_its_run_s_screen() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    store.run(
        &mut drawing,
        record_of(1, "c", 0, &screen_begin("marks", "")),
    );
    let first = store.run(
        &mut drawing,
        record_of(1, "c", 1, &step(&marked("s1", "x"))),
    );
    assert_eq!(shown(&first), 2);
    let scene = screen_scene(&drawing);
    assert_eq!(scene, SceneId(1));
    let skipped = store.run(&mut drawing, Input::SceneSkipped { scene });
    assert_eq!(
        texts(&[&skipped]),
        [format!("drawn: [s1]; stopped: {CHANGED}{NOT_KEPT}")]
    );
    assert_eq!(trace(&skipped), ["end 1 false", "revoke 1", "clear 1"]);
    assert!(drawing.scenes.iter().all(|held| held.gone));
    assert!(
        !drawing.drew_nothing(RunId(1)),
        "the skipped step stays drawn"
    );
    let late = store.run(&mut drawing, Input::SceneSkipped { scene });
    assert_eq!((late.effects.len(), late.calls.len()), (0, 0));
    let again = store.run(
        &mut drawing,
        screen_call(1, "d", &screen_begin("again", ""), &[marked("s2", "y")]),
    );
    assert_eq!(texts(&[&again]), [format!("drawn: []; stopped: {CHANGED}")]);
    assert_eq!(shown(&again), 0);
    let forward = r#"{"id":"a","start":{"after":"later","ms":0}}"#.to_owned();
    let failed = store.run(&mut drawing, call(1, "e", args("card", &[forward])));
    assert_eq!(error(&failed.calls[0]), ErrorCode::ForwardRef);
    let repair = store.run(&mut drawing, call(1, "f", args("card2", &[text("b", "y")])));
    assert_eq!(
        texts(&[&repair]),
        ["drawn: [b]"],
        "neither screen ending opened the repair round"
    );
}

#[test]
fn a_skip_closes_the_screen_for_a_later_run_continuing_the_scene_before_its_first_step() {
    let mut drawing = told(Ok(Geometry::FIXTURE));
    let mut store = Store::default();
    store.run(
        &mut drawing,
        screen_call(1, "b", &screen_begin("marks", ""), &[marked("s1", "x")]),
    );
    store.run(
        &mut drawing,
        Input::RunClosed {
            run: RunId(1),
            loss: Loss::Drop,
        },
    );
    drawing.canvas(RunId(2), Ok(Geometry::FIXTURE));
    let scene = screen_scene(&drawing);
    let begun = store.run(
        &mut drawing,
        record_of(2, "c", 0, &screen_begin("marks", "")),
    );
    assert_eq!(begun.calls, []);
    let skipped = store.run(&mut drawing, Input::SceneSkipped { scene });
    assert_eq!(
        texts(&[&skipped]),
        [format!("drawn: []; stopped: {CHANGED}{NOT_KEPT}")]
    );
    let again = store.run(
        &mut drawing,
        screen_call(2, "d", &screen_begin("again", ""), &[marked("s2", "y")]),
    );
    assert_eq!(texts(&[&again]), [format!("drawn: []; stopped: {CHANGED}")]);
}

/// Arguments with a screen scene after an ordinary records array: a second envelope, and a second
/// `records` member of the same object. The reducer never reads it.
const SUFFIXED: [&str; 2] = [
    r#"{"records":[{"begin":{"scene":"card","surface":"overlay","alt":"a"}},{"end":{}}]} {"records":[{"begin":{"scene":"marks","surface":"screen","alt":"a"}},{"step":{"id":"s1","put":[{"highlight":{"id":"m","target":{"box":[917.375,300,50,40]}}}]}},{"end":{}}]}"#,
    r#"{"records":[{"begin":{"scene":"card","surface":"overlay","alt":"a"}},{"end":{}}],"records":[{"begin":{"scene":"marks","surface":"screen","alt":"a","display":{"origin":[0,0],"size":[2731,1537],"dpi":97}}},{"step":{"id":"s1","put":[{"highlight":{"id":"m","target":{"box":[917.375,300,50,40]}}}]}},{"end":{}}]}"#,
];

/// A screen call keeps no records when its arguments say anything after the records array, or its
/// envelope names the screen anywhere but isn't one screen scene the surface admits, so none of
/// its numbers reach the stored item.
#[test]
fn a_mixed_or_forbidden_screen_call_is_stored_with_no_records() {
    const OVERLAY: &str = r#"{"begin":{"scene":"card","surface":"overlay","alt":"a"}}"#;
    const WRITTEN: &str = r#"{"begin":{"scene":"marks","surface":"screen","alt":"marks","display":{"origin":[0,0],"size":[2731,1537],"dpi":97}}}"#;
    const END: &str = r#"{"end":{}}"#;
    let one = |put: &str| format!(r#"{{"step":{{"id":"s1","put":[{put}]}}}}"#);
    let screen = screen_begin("marks", "");
    let mark =
        one(r#"{"highlight":{"id":"m","target":{"box":[917.375,300,50,40]},"style":"box"}}"#);
    let forbidden = [
        one(r##"{"shape":{"id":"m","kind":"rect","box":[917.375,300,50,40],"fill":"#ffffff"}}"##),
        one(r#"{"highlight":{"id":"m","target":[917.375,300],"style":"box"}}"#),
        one(
            r#"{"callout":{"id":"l","target":{"box":[10,10,20,20]},"text":"here","at":[917.375,300]}}"#,
        ),
    ];
    for step in &forbidden {
        assert!(
            kumochi_scene::parse_screen_draw_record(step.as_bytes()).is_ok(),
            "{step}"
        );
    }
    let mut envelopes = vec![
        vec![
            OVERLAY.to_owned(),
            screen.clone(),
            mark.clone(),
            END.to_owned(),
        ],
        vec![OVERLAY.to_owned(), mark.clone(), WRITTEN.to_owned()],
        vec![screen.clone(), mark.clone(), WRITTEN.to_owned()],
        vec![screen.clone(), END.to_owned(), mark.clone(), END.to_owned()],
        vec![screen.clone(), mark],
        vec![END.to_owned()],
    ];
    envelopes.extend(
        forbidden
            .iter()
            .map(|step| vec![screen.clone(), step.clone(), END.to_owned()]),
    );
    let mut calls: Vec<String> = envelopes
        .iter()
        .map(|records| format!(r#"{{"records":[{}]}}"#, records.join(",")))
        .collect();
    calls.extend(SUFFIXED.map(str::to_owned));
    for args in calls {
        let stored = stored_item(draw_item(&args));
        assert_eq!(arguments(&stored), json!({"records": []}), "{args}");
        let text = String::from_utf8(stored.bytes().to_vec()).expect("UTF-8");
        for marker in ["917.375", "2731"] {
            let listed = format!("{:?}", marker.as_bytes());
            assert!(!text.contains(marker), "{text}");
            assert!(!text.contains(listed.trim_matches(['[', ']'])), "{text}");
        }
    }
}
