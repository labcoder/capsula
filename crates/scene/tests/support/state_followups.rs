use kumochi_scene::{
    Admission, Begin, GlobalBudget, Handle, Record, SceneState, Step, parse_record,
};

pub(super) fn record(json: &str) -> Record {
    parse_record(json.as_bytes()).expect("valid fixture")
}

pub(super) fn header() -> Begin {
    let Record::Begin(header) =
        record(r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#)
    else {
        panic!("begin fixture");
    };
    header
}

pub(super) fn scene() -> SceneState {
    SceneState::begin(header()).expect("valid header")
}

pub(super) fn step(json: &str) -> Step {
    let Record::Step(step) = record(&format!(r#"{{"step":{json}}}"#)) else {
        panic!("step fixture");
    };
    step
}

pub(super) fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Vec<Handle> {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state
        .commit(admitted, global)
        .expect("fresh candidate")
        .handles
        .into_iter()
        .map(|(_, h)| h)
        .collect()
}
