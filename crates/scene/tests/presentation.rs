//! Canvas coordinates and host-side placement, independent of screen resolution.
#[path = "support/allocation.rs"]
mod allocation;
use kumochi_scene::{
    Admission, Begin, Frame, GlobalBudget, Record, SceneState, SceneView, StopCut,
    display::{MeasureError, TextMeasure, TextStyle},
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

fn picture(header: Begin, put: serde_json::Value) -> LayoutState {
    let mut scene = SceneState::begin(header).expect("scene");
    let before = LayoutState::new(&scene);
    let mut budget = GlobalBudget::default();
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&json!({"step":{"id":"one","put":put}})).expect("json"))
            .expect("step")
    else {
        panic!("step")
    };
    prepared(&mut scene, &mut budget, before, step).after
}

fn prepared(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: LayoutState,
    step: kumochi_scene::Step,
) -> PreparedStep {
    let Admission::New(candidate) = scene.admit(&step, budget).expect("admit") else {
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
    .expect("input");
    prepare(input, &mut Measure, &AtomicBool::new(false)).expect("prepare")
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
    let step = prepared(&mut scene, &mut budget, before, step);
    assert_eq!(step.after.fit_extent(), rect(16.0, -14.0, 473.0, 448.0));
    let mut frame = Frame::new();
    view.activate(step, 0, 800, &mut frame).expect("activate");
    view.frame(1000, &mut frame).expect("sample");
    let expected = Affine::pose(2.0, 0.0, Vec2::new(775.0, 354.0));
    assert_eq!(frame.items().len(), 7);
    assert!(frame.items().iter().all(|item| item.transform == expected));
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
                revealed: Bounded::new(view.stop(1000)).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let kept = view.layout().cut(&scene).expect("cut layout");
    let mut stopped = SceneView::new(&kept, scene.activated());
    stopped.set_stage(stage);
    let mut rest_frame = Frame::new();
    stopped.frame(0, &mut rest_frame).expect("stopped frame");
    assert!(
        rest_frame
            .items()
            .iter()
            .all(|item| item.transform == expected)
    );
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
    assert!(
        recovered_frame
            .items()
            .iter()
            .all(|item| item.transform == expected)
    );
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
    assert!(
        recovered_frame
            .items()
            .iter()
            .all(|item| item.transform == Affine::pose(1.0, 0.0, Vec2::new(-6.0, 34.0)))
    );
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
        json!([{"chart":{"id":"c","kind":"bar"}}]),
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
        json!([
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
        picture(header(None), json!([])).fit_extent(),
        rect(0.0, 0.0, 960.0, 600.0)
    );
    assert_eq!(
        picture(
            header(None),
            json!([{"line":{"id":"line","from":[10,20],"to":[30,20]}}])
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
            json!([{"line":{"id":"line","from":[90,90],"to":[120,150]}}])
        )
        .fit_extent(),
        rect(0.0, 0.0, 120.0, 150.0),
    );
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
        let layout = picture(header(None), put);
        assert_eq!(layout.presentation_scale(), 1.0);
    }
}

#[test]
fn curve_subdivision_reserves_the_presentation_scale() {
    let layout = picture(
        header(None),
        json!([{"path":{"id":"p","d":"M0 0 Q4 8 8 0"}}]),
    );
    let items = layout.items();
    let kumochi_scene::display::Prim::Line { points, .. } = &items[0].primitives[0] else {
        panic!("curve")
    };
    assert_eq!(points.len(), 9);
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
            2.0,
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
        let step = prepared(&mut scene, &mut budget, layout, step);
        assert_eq!(step.after.fit_extent(), extent);
        layout = step.after.clone();
        view.activate(step, 0, 800, &mut frame).expect("activate");
        view.frame(0, &mut frame).expect("frame");
        assert_eq!(frame.items()[0].transform.0[0], scale);
    }
}
