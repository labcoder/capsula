//! Canvas coordinates and host-side placement, independent of screen resolution.
#[path = "support/allocation.rs"]
mod allocation;
use kumochi_scene::{
    Admission, Begin, Frame, GlobalBudget, Record, SceneState, SceneView, StopCut,
    display::{MeasureError, PaintWork, TextMeasure, TextStyle, fallback_corner_segments},
    geom::{Affine, Rect, Vec2},
    layout::{LayoutState, PrepareInput, PreparedStep, SurfaceGeometry, place, prepare, recover},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};

struct Measure;
impl TextMeasure for Measure {
    fn measure(
        &mut self,
        _text: &str,
        _style: TextStyle,
        _width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        Ok([40.0, 20.0])
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        origin: Vec2::new(x, y),
        size: Vec2::new(w, h),
    }
}

fn header(size: Option<[f32; 2]>) -> Begin {
    let Record::Begin(header) = parse_record(
        &serde_json::to_vec(
            &json!({"begin":{"v":1,"scene":"canvas","surface":"overlay","size":size,"alt":""}}),
        )
        .expect("json"),
    )
    .expect("begin") else {
        panic!("begin")
    };
    header
}

fn picture(header: Begin, put: &serde_json::Value) -> LayoutState {
    let mut scene = SceneState::begin(header).expect("scene");
    let before = LayoutState::new(&scene);
    let mut budget = GlobalBudget::default();
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&json!({"step":{"id":"one","put":put}})).expect("json"))
            .expect("step")
    else {
        panic!("step")
    };
    prepared(&mut scene, &mut budget, before, &step).after
}

/// Commits and prepares `step` under the paint cap the shell passes.
fn prepared(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: LayoutState,
    step: &kumochi_scene::Step,
) -> PreparedStep {
    let Admission::New(candidate) = scene.admit(step, budget).expect("admit") else {
        panic!("new")
    };
    let ticket = scene.commit(candidate, budget).expect("commit").preparation;
    let input = PrepareInput::new(
        ticket,
        before,
        SurfaceGeometry::Overlay {
            default_canvas: [960.0, 600.0],
        },
        Arc::new(Theme::default()),
    )
    .expect("input")
    .with_paint_cap(Some(PaintWork::LIMITS));
    prepare(input, &mut Measure, &AtomicBool::new(false)).expect("prepare")
}

fn assert_graph_placement(frame: &Frame, expected: Affine) {
    assert_eq!(
        frame.items().len(),
        7,
        "the recorded graph keeps every item"
    );
    assert!(
        frame.items().iter().all(|item| item.transform == expected),
        "every recorded graph item uses the fitted transform"
    );
}

#[test]
fn recorded_graph_stays_centered_through_activation_stop_recovery_and_stage_changes() {
    let mut fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/canvas-graph.json")).expect("recorded call");
    fixture["records"][0]["begin"]["v"] = json!(1);
    let Record::Begin(begin) =
        parse_record(&serde_json::to_vec(&fixture["records"][0]).expect("json")).expect("header")
    else {
        panic!("begin")
    };
    assert_eq!(begin.size, None);
    let mut scene = SceneState::begin(begin.clone()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut view = SceneView::new(&before, 0);
    let stage = rect(32.0, 180.0, 2496.0, 1188.0);
    view.set_stage(stage);
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&fixture["records"][1]).expect("json")).expect("step")
    else {
        panic!("step")
    };
    let step = prepared(&mut scene, &mut budget, before, &step);
    assert_eq!(step.after.fit_extent(), rect(16.0, -14.0, 473.0, 448.0));
    let mut frame = Frame::new();
    view.activate(step, 0, 800, &mut frame).expect("activate");
    // Default entrances pace the recorded objects, so sample once the last has entered.
    let entered = view.entrance_end().expect("the step puts objects");
    view.frame(entered, &mut frame).expect("sample");
    let expected = Affine::pose(2.0, 0.0, Vec2::new(775.0, 354.0));
    assert_graph_placement(&frame, expected);
    let ((), activity) = allocation::activity(|| {
        for now in [0, 100, 400, 800, 1000, 60_000] {
            view.frame(now, &mut frame).expect("placed frame");
        }
    });
    assert_eq!(activity, [0, 0]);
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(view.stop(entered)).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let kept = view.layout().cut(&scene).expect("cut layout");
    let mut stopped = SceneView::new(&kept, scene.activated());
    stopped.set_stage(stage);
    let mut rest_frame = Frame::new();
    stopped.frame(0, &mut rest_frame).expect("stopped frame");
    assert_graph_placement(&rest_frame, expected);
    let (replayed, recovered) = recover(
        begin,
        scene.recovery_log(),
        SurfaceGeometry::Overlay {
            default_canvas: [960.0, 600.0],
        },
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("replay");
    let mut recovered_view = SceneView::new(&recovered, replayed.activated());
    recovered_view.set_stage(stage);
    let mut recovered_frame = Frame::new();
    recovered_view
        .frame(0, &mut recovered_frame)
        .expect("recovered frame");
    assert_graph_placement(&recovered_frame, expected);
    let sources: Vec<_> = recovered
        .items()
        .iter()
        .map(|item| Arc::clone(&item.primitives))
        .collect();
    let (changed, allocations) =
        allocation::count(|| recovered_view.set_stage(rect(10.0, 20.0, 473.0, 448.0)));
    assert!(changed);
    assert_eq!(allocations, 0);
    recovered_view
        .frame(0, &mut recovered_frame)
        .expect("new stage");
    assert_graph_placement(
        &recovered_frame,
        Affine::pose(1.0, 0.0, Vec2::new(-6.0, 34.0)),
    );
    assert_eq!(recovered_view.layout().items().len(), sources.len());
    assert!(
        recovered_view
            .layout()
            .items()
            .iter()
            .zip(&sources)
            .all(|(item, source)| Arc::ptr_eq(&item.primitives, source))
    );
}

#[test]
fn placement_centers_and_caps_upscaling_but_has_no_lower_floor() {
    assert_eq!(
        place(
            rect(10.0, 20.0, 100.0, 50.0),
            rect(100.0, 200.0, 400.0, 300.0),
            2.0
        ),
        Affine::pose(2.0, 0.0, Vec2::new(180.0, 260.0))
    );
    assert_eq!(
        place(
            rect(0.0, 0.0, 20_000.0, 10_000.0),
            rect(0.0, 0.0, 100.0, 100.0),
            2.0
        ),
        Affine::pose(0.005, 0.0, Vec2::new(0.0, 25.0))
    );
    assert_eq!(
        place(
            rect(0.0, 0.0, 100.0, 100.0),
            rect(0.0, 0.0, 400.0, 400.0),
            1.0
        ),
        Affine::pose(1.0, 0.0, Vec2::new(150.0, 150.0))
    );
}

#[test]
fn overlay_layout_uses_the_authored_canvas_instead_of_the_host_size() {
    let layout = picture(
        header(Some([320.0, 200.0])),
        &json!([{"chart":{"id":"c","kind":"bar"}}]),
    );
    let bounds = layout
        .bounds(&"c".to_owned().try_into().expect("id"))
        .expect("bounds")
        .expect("chart");
    assert_eq!(bounds, rect(0.0, 0.0, 320.0, 200.0));
    assert_eq!(layout.fit_extent(), rect(0.0, 0.0, 320.0, 200.0));
}

#[test]
fn world_extent_includes_ancestor_and_rest_transforms_with_padding() {
    let layout = picture(
        header(None),
        &json!([
            {"diagram":{"id":"d","box":[100,200,100,100]}},
            {"shape":{"id":"r","in":"d","kind":"rect","box":[10,20,20,30]}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[0,0]],[100,[50,60]]]}}
        ]),
    );
    let world = layout
        .bounds(&"r".to_owned().try_into().expect("id"))
        .expect("bounds")
        .expect("rect");
    let extent = layout.fit_extent();
    assert_eq!(world, rect(60.0, 80.0, 20.0, 30.0));
    assert_eq!(extent, rect(36.0, 56.0, 68.0, 78.0));
    let mut view = SceneView::new(&layout, 1);
    view.set_stage(rect(0.0, 0.0, 136.0, 156.0));
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("frame");
    let handle = layout
        .handle(&"r".to_owned().try_into().expect("id"))
        .expect("child");
    let child = frame
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .expect("child frame");
    assert_eq!(
        child.transform,
        Affine::pose(2.0, 0.0, Vec2::new(28.0, 8.0))
    );
}

#[test]
fn empty_content_falls_back_to_the_canvas() {
    assert_eq!(
        picture(header(None), &json!([])).fit_extent(),
        rect(0.0, 0.0, 960.0, 600.0)
    );
    assert_eq!(
        picture(
            header(None),
            &json!([{"line":{"id":"line","from":[10,20],"to":[30,20]}}])
        )
        .fit_extent(),
        rect(0.0, 0.0, 960.0, 600.0),
    );
}

#[test]
fn overflow_grows_a_declared_canvas_instead_of_clipping_to_it() {
    assert_eq!(
        picture(
            header(Some([100.0, 100.0])),
            &json!([{"line":{"id":"line","from":[90,90],"to":[120,150]}}])
        )
        .fit_extent(),
        rect(0.0, 0.0, 120.0, 150.0),
    );
}

#[test]
fn axis_aligned_strokes_grow_the_declared_canvas_before_fitting() {
    for (from, to, extent, translation) in [
        (
            [90, 90],
            [120, 90],
            rect(0.0, 0.0, 120.0, 100.0),
            [0.0, 25.0 / 3.0],
        ),
        (
            [90, 90],
            [90, 120],
            rect(0.0, 0.0, 100.0, 120.0),
            [25.0 / 3.0, 0.0],
        ),
        (
            [-20, 90],
            [10, 90],
            rect(-20.0, 0.0, 120.0, 100.0),
            [50.0 / 3.0, 25.0 / 3.0],
        ),
        (
            [90, -20],
            [90, 10],
            rect(0.0, -20.0, 100.0, 120.0),
            [25.0 / 3.0, 50.0 / 3.0],
        ),
    ] {
        let layout = picture(
            header(Some([100.0, 100.0])),
            &json!([{"line":{"id":"line","from":from,"to":to}}]),
        );
        assert_eq!(layout.fit_extent(), extent, "stroke {from:?} to {to:?}");
        let actual = place(layout.fit_extent(), rect(0.0, 0.0, 100.0, 100.0), 2.0);
        let expected = [
            5.0 / 6.0,
            0.0,
            0.0,
            5.0 / 6.0,
            translation[0],
            translation[1],
        ];
        for (actual, expected) in actual.0.into_iter().zip(expected) {
            assert!(
                (actual - expected).abs() < 0.00001,
                "{actual} != {expected}"
            );
        }
    }
}

#[test]
fn presentation_respects_the_remaining_composed_scale_envelope() {
    for put in [
        json!([
            {"diagram":{"id":"d","box":[0,0,1,1]}},
            {"shape":{"id":"r","in":"d","kind":"rect","box":[0,0,1,1]}},
            {"anim":{"target":"d","track":"scale","keys":[[0,64]]}}
        ]),
        json!([{"path":{"id":"p","d":"M0 0 Q1 1 2 0","scale":64}}]),
    ] {
        let layout = picture(header(None), &put);
        assert_eq!(layout.presentation_scale().to_bits(), 1.0_f32.to_bits());
    }
}

#[test]
fn curve_subdivision_reserves_the_presentation_scale() {
    let layout = picture(
        header(None),
        &json!([{"path":{"id":"p","d":"M0 0 Q4 8 8 0"}}]),
    );
    let items = layout.items();
    let kumochi_scene::display::Prim::Line { points, .. } = &items[0].primitives[0] else {
        panic!("curve")
    };
    assert_eq!(points.len(), 9);
}

#[test]
fn a_small_overlay_diagram_of_rounded_nodes_fits_the_paint_budgets() {
    // The shape of a recorded call the shell refused for the paint work of its rounded rects.
    let work = picture(
        header(Some([520.0, 320.0])),
        &json!([
            {"node":{"id":"a","label":"Alpha \u{2600}\u{fe0f}","shape":"circle","color":"amber"}},
            {"node":{"id":"b","label":"Bravo","sub":"second","shape":"round","color":"blue"}},
            {"node":{"id":"c","label":"Charlie","sub":"third","shape":"round","color":"gray"}},
            {"node":{"id":"d","label":"Delta","sub":"fourth","shape":"pill","color":"teal"}},
            {"edge":{"id":"ab","from":"a","to":"b","label":"one"}},
            {"edge":{"id":"bc","from":"b","to":"c","label":"two"}},
            {"edge":{"id":"cd","from":"c","to":"d","label":"three"}},
            {"edge":{"id":"db","from":"d","to":"b","label":"four"}}
        ]),
    )
    .paint_work();
    // At the overlay's charge of 2 the radius-40 circle needs 10 pieces per corner (4 · 11 + 1
    // points), the radius-12 rounds 6 (29) and the radius-32 pill 9 (41), filled and bordered.
    // Each edge adds a 3-point arrowhead and a radius-4 label chip of 21 fill points.
    assert_eq!(
        work,
        PaintWork {
            dash_pieces: 0,
            fill_vertices: 45 + 2 * 29 + 41 + 4 * (3 + 21),
            largest_fill: 45,
            outline_points: 45 + 2 * 29 + 41,
            largest_outline: 45,
        }
    );
}

#[test]
fn four_plain_nodes_and_three_labelled_edges_fit_the_paint_budgets() {
    let work = picture(
        header(Some([520.0, 320.0])),
        &json!([
            {"node":{"id":"a","label":"Alpha"}},
            {"node":{"id":"b","label":"Bravo"}},
            {"node":{"id":"c","label":"Charlie"}},
            {"node":{"id":"d","label":"Delta"}},
            {"edge":{"id":"ab","from":"a","to":"b","label":"one"}},
            {"edge":{"id":"bc","from":"b","to":"c","label":"two"}},
            {"edge":{"id":"cd","from":"c","to":"d","label":"three"}}
        ]),
    )
    .paint_work();
    // Four radius-4 boxes at the overlay's charge of 2: 4 pieces per corner, so 4 · 5 + 1 points,
    // filled and bordered. Each edge adds a 3-point arrowhead and a label chip of 21.
    assert_eq!(
        work,
        PaintWork {
            dash_pieces: 0,
            fill_vertices: 4 * 21 + 3 * (3 + 21),
            largest_fill: 21,
            outline_points: 4 * 21,
            largest_outline: 21,
        }
    );
}

#[test]
fn the_drift_allowance_comes_before_the_64_clamp() {
    // g = 32 on the overlay reaches the 64 clamp. Radius 1.2671660184860229 sits just under the
    // ten-piece threshold there, so each filled, bordered rect costs 4 · 11 + 1 points; applying
    // the allowance after the clamp would cross it, to 49 each and 539 in all.
    let mut put = vec![
        json!({"diagram":{"id":"d"}}),
        json!({"anim":{"target":"d","track":"scale","keys":[[0,32]]}}),
    ];
    put.extend((0..11).map(|index| {
        json!({"shape":{"id":format!("r{index}"),"in":"d","kind":"rect","box":[index * 4,0,3,3],"radius":1.267_166_f32,"fill":"blue"}})
    }));
    let work = picture(header(Some([520.0, 320.0])), &json!(put)).paint_work();
    assert_eq!(
        work,
        PaintWork {
            dash_pieces: 0,
            fill_vertices: 11 * 45,
            largest_fill: 45,
            outline_points: 11 * 45,
            largest_outline: 45,
        }
    );
}

#[test]
fn a_rotated_chain_at_the_depth_limit_stays_within_its_charge() {
    // Fifteen turned diagrams scale the rect by 16 at the 16-object depth limit, and its radius
    // puts the overlay's 32 just under the ten-piece threshold. The frame's composed matrix may
    // land on either side of it, so the allowance charges eleven.
    let radius = 2.534_332_f32;
    let mut put = vec![json!({"diagram":{"id":"g0"}})];
    put.extend(
        (1..15).map(|n| json!({"diagram":{"id":format!("g{n}"),"in":format!("g{}", n - 1)}})),
    );
    put.extend(
        (0..15).map(|n| json!({"anim":{"target":format!("g{n}"),"track":"rotate","keys":[[0,7]]}})),
    );
    put.extend(
        (0..4).map(|n| json!({"anim":{"target":format!("g{n}"),"track":"scale","keys":[[0,2]]}})),
    );
    put.push(json!({"shape":{"id":"r","in":"g14","kind":"rect","box":[0,0,8,8],"radius":radius,"fill":"blue"}}));
    let layout = picture(header(None), &json!(put));
    let mut view = SceneView::new(&layout, 0);
    view.set_stage(rect(0.0, 0.0, 2560.0, 1440.0));
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("a fresh frame");
    let item = frame
        .items()
        .iter()
        .find(|item| !item.primitives.is_empty())
        .expect("the rect");
    let [a, b, c, d, ..] = item.transform.0;
    let posed = (a * d - b * c).abs().sqrt();
    assert!((posed - 32.0).abs() < 1e-3, "posed at {posed}");
    assert!(b.abs() > 1.0, "turned, so the painter takes the fallback");
    let bounds = rect(0.0, 0.0, 8.0, 8.0);
    assert_eq!(fallback_corner_segments(bounds, radius, 32.0), 10);
    assert_eq!(
        fallback_corner_segments(bounds, radius, item.paint_scale),
        11
    );
    assert!(fallback_corner_segments(bounds, radius, posed) <= 11);
}

#[test]
fn a_growing_step_replaces_the_fit_once_and_removal_does_not_shrink_it() {
    let mut scene = SceneState::begin(header(None)).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut layout = LayoutState::new(&scene);
    let mut view = SceneView::new(&layout, 0);
    view.set_stage(rect(0.0, 0.0, 116.0, 116.0));
    let mut frame = Frame::new();
    for (source, extent, scale) in [
        (
            r#"{"step":{"id":"first","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}}"#,
            rect(-24.0, -24.0, 58.0, 58.0),
            2.0_f32,
        ),
        (
            r#"{"step":{"id":"grow","put":[{"shape":{"id":"b","kind":"rect","box":[100,0,10,10]}}]}}"#,
            rect(-24.0, -24.0, 158.0, 58.0),
            116.0 / 158.0,
        ),
        (
            r#"{"step":{"id":"remove","remove":["b"]}}"#,
            rect(-24.0, -24.0, 158.0, 58.0),
            116.0 / 158.0,
        ),
    ] {
        let Record::Step(step) = parse_record(source.as_bytes()).expect("step") else {
            panic!("step")
        };
        let step = prepared(&mut scene, &mut budget, layout, &step);
        assert_eq!(step.after.fit_extent(), extent);
        layout = step.after.clone();
        view.activate(step, 0, 800, &mut frame).expect("activate");
        view.frame(0, &mut frame).expect("frame");
        assert_eq!(frame.items()[0].transform.0[0].to_bits(), scale.to_bits());
    }
}

/// Ordinary illustrations of the kind the 512-point caps refused (OSC-688) fit the paint caps.
#[test]
fn ordinary_illustrations_fit_the_paint_caps() {
    let round = |id: String, at: [f32; 4]| json!({"shape":{"id":id,"kind":"ellipse","box":at,"fill":"#d5dde9","width":0}});
    // A night sky: its background, a two-circle moon and six stars, then a cloud of eight
    // ellipses. Every filled ellipse is a 48-gon whatever its size: 5 + 16 · 48 = 773.
    let mut sky = vec![
        json!({"shape":{"id":"sky","kind":"rect","box":[0,0,960,600],"fill":"#0b1a3a","width":0}}),
    ];
    sky.extend((0..16_u16).map(|index| {
        let x = f32::from(index) * 55.0;
        round(format!("round{index}"), [x, 100.0, 40.0 + x / 4.0, 30.0])
    }));
    // A detailed cartoon mouse: 21 filled round parts and a radius-12 shirt, filled and bordered,
    // whose fallback costs 29 points: 21 · 48 + 29 = 1,037.
    let mut mouse = vec![
        json!({"shape":{"id":"shirt","kind":"rect","box":[225,455,200,145],"radius":12,"fill":"red","color":"fg","width":2}}),
    ];
    mouse.extend((0..21_u16).map(|index| {
        let x = f32::from(index) * 30.0;
        round(format!("part{index}"), [x, 200.0, 24.0, 18.0])
    }));
    // A 30-step flowchart with labelled arrows: 17 points per node, and per arrow 3 for its head
    // and 17 for its label's chip: 30 · 17 + 29 · 20 = 1,090 fill and 30 · 17 = 510 outline.
    let mut flow = vec![json!({"diagram":{"id":"d","dir":"down"}})];
    flow.extend(
        (0..30).map(|index| json!({"node":{"id":format!("n{index}"),"in":"d","label":"Step"}})),
    );
    flow.extend((1..30).map(|index| {
        json!({"edge":{"id":format!("e{index}"),"from":format!("n{}", index - 1),"to":format!("n{index}"),"label":"then"}})
    }));
    for (put, fill, outline) in [(sky, 773, 0), (mouse, 1_037, 29), (flow, 1_090, 510)] {
        let work = picture(header(None), &serde_json::Value::Array(put)).paint_work();
        assert_eq!((work.fill_vertices, work.outline_points), (fill, outline));
        assert!(work.within_limits());
    }
}
