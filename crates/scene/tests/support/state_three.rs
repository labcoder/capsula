use kumochi_scene::{Admission, Committed, GlobalBudget, Record, SceneState, Step, parse_record};

pub(super) fn header() -> kumochi_scene::Begin {
    let source = br#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#;
    let Ok(Record::Begin(header)) = parse_record(source) else {
        panic!("begin fixture");
    };
    header
}

pub(super) fn step(json: &str) -> Step {
    let record = format!(r#"{{"step":{json}}}"#);
    match parse_record(record.as_bytes()) {
        Ok(Record::Step(step)) => step,
        other => panic!("step fixture: {other:?}"),
    }
}

pub(super) fn put(name: &str, ops: &[String]) -> String {
    format!(r#"{{"id":"{name}","put":[{}]}}"#, ops.join(","))
}

pub(super) fn view(name: &str) -> String {
    format!(r#"{{"view3d":{{"id":"{name}"}}}}"#)
}

/// A 12-triangle box in view v.
pub(super) fn cube(name: &str, extra: &str) -> String {
    format!(r#"{{"solid":{{"in":"v","id":"{name}","shape":"box","size":[2,2,2]{extra}}}}}"#)
}

/// A lathe of radius 1 with 2 × segments × (points − 1) triangles.
pub(super) fn lathe(name: &str, points: usize, segments: u8) -> String {
    let profile: Vec<String> = (0..points).map(|i| format!("[1,{i}]")).collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"lathe","points":[{}],"segments":{segments}}}}}"#,
        profile.join(",")
    )
}

pub(super) fn scene() -> SceneState {
    SceneState::begin(header()).expect("valid header")
}

pub(super) fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Committed {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state.commit(admitted, global).expect("fresh admission")
}

/// `tests/mesh_size.rs`'s maximum outline: 64 retained perimeter points, 252 triangles.
pub(super) fn compact(name: &str) -> String {
    let points: Vec<String> = (0..16)
        .map(|i| format!("[{i},0]"))
        .chain((0..16).map(|i| format!("[16,{i}]")))
        .chain((0..16).map(|i| format!("[{},16]", 16 - i)))
        .chain((0..16).map(|i| format!("[0,{}]", 16 - i)))
        .collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"extrude","points":[{}],"depth":2}}}}"#,
        points.join(",")
    )
}
