//! Canvas coordinates and host-side placement, independent of screen resolution.
use kumochi_scene::{
    Admission, Begin, GlobalBudget, Record, SceneState,
    display::{MeasureError, TextMeasure, TextStyle},
    geom::{Affine, Rect, Vec2},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, place, prepare},
    parse_record,
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
    let Admission::New(candidate) = scene.admit(&step, &budget).expect("admit") else {
        panic!("new")
    };
    let ticket = scene
        .commit(candidate, &mut budget)
        .expect("commit")
        .preparation;
    let input = PrepareInput::new(
        ticket,
        before,
        SurfaceGeometry::Overlay {
            default_canvas: [960.0, 600.0],
        },
        Arc::new(Theme::default()),
    )
    .expect("input");
    prepare(input, &mut Measure, &AtomicBool::new(false))
        .expect("prepare")
        .after
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
    assert!(extent.origin.x <= world.origin.x - 24.0 && extent.origin.y <= world.origin.y - 24.0);
    assert!(extent.max().x >= world.max().x + 24.0 && extent.max().y >= world.max().y + 24.0);
}

#[test]
fn empty_content_falls_back_to_the_canvas() {
    assert_eq!(
        picture(header(None), json!([])).fit_extent(),
        rect(0.0, 0.0, 960.0, 600.0)
    );
}
