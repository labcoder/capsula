use super::new_scene;
use kumochi_scene::{Record, SceneState, Step, parse_record};
use serde_json::{Value, json};
use std::fmt::Write as _;

pub(super) fn scene() -> SceneState {
    new_scene("mermaid")
}

pub(super) fn typed(value: &Value) -> Step {
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step record");
    };
    step
}

pub(super) fn graph(source: &str) -> Step {
    typed(&json!({"id":"graph","put":[
        {"diagram":{"id":"target"}},
        {"mermaid":{"in":"target","src":source}}
    ]}))
}

pub(super) fn connected_full_graph() -> String {
    let mut source = String::from("flowchart TB\n");
    let mut first = 0;
    for (group, nodes) in [48, 48, 48, 48, 48, 48, 11].into_iter().enumerate() {
        writeln!(source, "subgraph g{group}").expect("source string");
        for node in first..first + nodes - 1 {
            writeln!(source, "n{node} --> n{}", node + 1).expect("source string");
        }
        source.push_str("end\n");
        first += nodes;
    }
    source
}
