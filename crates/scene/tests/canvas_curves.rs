//! Offline timing of the SVG preparation envelope used by overlay presentation.
use kumochi_scene::{
    Admission, GlobalBudget, Record, SceneState,
    display::{MeasureError, TextMeasure, TextStyle},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    parse_record,
    theme::Theme,
};
use std::{
    hint::black_box,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

struct NoText;
impl TextMeasure for NoText {
    fn measure(&mut self, _: &str, _: TextStyle, _: Option<f32>) -> Result<[f32; 2], MeasureError> {
        Err(MeasureError)
    }
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn measure_prepare_canvas_curves() {
    let Record::Begin(header) =
        parse_record(br#"{"begin":{"v":1,"scene":"curves","surface":"overlay","alt":""}}"#)
            .expect("begin")
    else {
        panic!("begin")
    };
    let paths: Vec<_> = (0..256).map(|index| serde_json::json!({"path":{"id":format!("p{index}"),"d":"M0 0 Q4 8 8 0","scale":4}})).collect();
    let source =
        serde_json::to_vec(&serde_json::json!({"step":{"id":"curves","put":paths}})).expect("json");
    let Record::Step(step) = parse_record(&source).expect("step") else {
        panic!("step")
    };
    let mut samples = Vec::with_capacity(64);
    let mut resident = 0;
    let mut segments = 0;
    for iteration in 0..69 {
        let mut scene = SceneState::begin(header.clone()).expect("scene");
        let mut budget = GlobalBudget::default();
        let layout = LayoutState::new(&scene);
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admit") else {
            panic!("new")
        };
        let ticket = scene
            .commit(candidate, &mut budget)
            .expect("commit")
            .preparation;
        let input = PrepareInput::new(
            ticket,
            layout,
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("input");
        let start = Instant::now();
        let prepared =
            prepare(black_box(input), &mut NoText, &AtomicBool::new(false)).expect("prepare");
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        assert_eq!(prepared.after.items().len(), 256);
        resident = prepared.resident_bytes;
        segments = scene.usage().segments;
        if iteration >= 5 {
            samples.push(elapsed);
        }
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "prepare_canvas_curves paths=256 samples=64 warmups=5 segments={segments} resident_bytes={resident} p50_us={} p95_us={} max_us={}; SVG preparation only, no admission, text shaping, painter or presentation",
        samples[31], samples[60], samples[63]
    );
}
