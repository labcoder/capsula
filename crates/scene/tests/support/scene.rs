use kumochi_scene::{
    Admission, GlobalBudget, Record, SceneState, WireId,
    display::{MeasureError, TextMeasure, TextStyle},
    layout::{LayoutState, PrepareInput, PreparedStep, SurfaceGeometry, prepare},
    parse_record,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) struct Measure;

impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let width =
            f32::from(u16::try_from(text.chars().count()).expect("bounded fixture text")) * 8.0;
        Ok([max_width.map_or(width, |limit| width.min(limit)), 10.0])
    }
}

pub(super) fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}

pub(super) fn new_scene(name: &str) -> SceneState {
    let bytes = serde_json::to_vec(&json!({"begin":{
        "v":1,"scene":name,"surface":"inline","alt":""
    }}))
    .expect("header JSON");
    let Record::Begin(begin) = parse_record(&bytes).expect("begin record") else {
        panic!("begin fixture");
    };
    SceneState::begin(begin).expect("scene")
}

pub(super) fn prepared(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
) -> PreparedStep {
    prepared_on(scene, budget, before, value, SurfaceGeometry::default())
}

pub(super) fn prepared_on(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
    surface: SurfaceGeometry,
) -> PreparedStep {
    prepared_with(scene, budget, before, value, surface, &mut Measure)
}

pub(super) fn prepared_with(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
    surface: SurfaceGeometry,
    measure: &mut dyn TextMeasure,
) -> PreparedStep {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let bytes = serde_json::to_vec(&envelope).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted step") else {
        panic!("new step");
    };
    let committed = scene.commit(candidate, budget).expect("fresh admission");
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        surface,
        Arc::new(Theme::default()),
    )
    .expect("consecutive preparation");
    let prepared = prepare(input, measure, &AtomicBool::new(false)).expect("picture");
    prepared.validate(scene, before).expect("current picture");
    prepared
}
