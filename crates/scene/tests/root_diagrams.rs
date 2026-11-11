//! Root membership, compiled edge ownership and authored-log replay.
use kumochi_scene::layout::{LayoutState, SurfaceGeometry};
use kumochi_scene::{Admission, Budget, ErrorCode, GlobalBudget, Record, SceneState, parse_record};
use serde_json::{Value, json};

#[path = "support/scene.rs"]
mod scene_support;
use scene_support::{id, new_scene, prepared};

fn step(value: &Value) -> kumochi_scene::Step {
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&json!({"step":value})).expect("JSON"))
            .expect("typed step")
    else {
        panic!("step record")
    };
    step
}

fn parent(scene: &SceneState, layout: &LayoutState, name: &str) -> Option<kumochi_scene::Handle> {
    layout
        .items()
        .iter()
        .find(|item| Some(item.handle) == scene.handle(&id(name)))
        .expect("prepared item")
        .parent
}

#[test]
fn root_nodes_and_edges_prepare_and_replay_without_rewriting_authored_steps() {
    let mut scene = new_scene("root");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let value = json!({"id":"first","put":[
        {"edge":{"id":"link","from":"a","to":"b"}},
        {"node":{"id":"a","label":"A"}},
        {"node":{"id":"b","label":"B"}}
    ]});
    let picture = prepared(&mut scene, &mut budget, &before, value.clone());
    assert_eq!(scene.object_count(), 3);
    for name in ["a", "b", "link"] {
        assert_eq!(parent(&scene, &picture.after, name), None);
    }
    assert_eq!(picture.step(), &step(&value));
    assert_eq!(scene.step(0), Some(&step(&value)));
    assert!(matches!(
        scene.admit(&step(&value), &budget),
        Ok(Admission::Duplicate)
    ));
    let continued = prepared(
        &mut scene,
        &mut budget,
        &picture.after,
        json!({"id":"second","put":[
            {"node":{"id":"c","label":"C"}},
            {"edge":{"id":"later","from":"b","to":"c"}}
        ]}),
    );
    assert_eq!(parent(&scene, &continued.after, "later"), None);
    let replay = SceneState::replay(scene.header().clone(), scene.recovery_log()).expect("replay");
    assert_eq!(replay.recovery_log(), scene.recovery_log());
    assert_eq!(replay.object_count(), 5);
    assert_eq!(replay.usage(), scene.usage());
}

#[test]
fn omitted_edge_parents_use_all_expanded_and_existing_endpoints() {
    let mut scene = new_scene("nested");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let authored = json!({"id":"first","put":[
        {"edge":{"id":"inner","from":"a","to":"b"}},
        {"edge":{"id":"outer","from":"a","to":"c"}},
        {"edge":{"id":"root","from":"a","to":"r"}},
        {"diagram":{"id":"d"}},
        {"mermaid":{"in":"d","src":"flowchart TB\nsubgraph g[Group]\na --> b\nend\nc\n"}},
        {"node":{"id":"r","label":"Root"}}
    ]});
    let first = prepared(&mut scene, &mut budget, &before, authored.clone());
    assert_eq!(
        parent(&scene, &first.after, "inner"),
        scene.handle(&id("g"))
    );
    assert_eq!(
        parent(&scene, &first.after, "outer"),
        parent(&scene, &first.after, "g")
    );
    assert_eq!(parent(&scene, &first.after, "root"), None);
    assert_eq!(scene.step(0), Some(&step(&authored)));
    let second = json!({"id":"second","put":[
        {"edge":{"id":"later","from":"a","to":"new"}},
        {"node":{"id":"new","in":"g","label":"New"}}
    ]});
    let next = prepared(&mut scene, &mut budget, &first.after, second.clone());
    assert_eq!(parent(&scene, &next.after, "later"), scene.handle(&id("g")));
    let remove = step(&json!({"id":"remove","remove":["g"]}));
    assert_eq!(
        scene
            .admit(&remove, &budget)
            .expect_err("parent remains referenced")
            .code,
        ErrorCode::Orphans
    );
    let mut explicit = second;
    explicit["put"][0]["edge"]["in"] = json!("g");
    let explicit = step(&explicit);
    assert_eq!(
        scene
            .admit(&explicit, &budget)
            .expect_err("authored duplicate differs")
            .code,
        ErrorCode::DuplicateStepConflict
    );
    let (mut replay, replay_before) = kumochi_scene::layout::recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &std::sync::Arc::new(kumochi_scene::theme::Theme::default()),
        &mut scene_support::Measure,
        &std::sync::atomic::AtomicBool::new(false),
        None,
    )
    .expect("replay and prepare");
    assert_eq!(replay_before.items(), next.after.items());
    let replay_next = prepared(
        &mut replay,
        &mut GlobalBudget::default(),
        &replay_before,
        json!({"id":"third","put":[{"edge":{"id":"replayed","from":"a","to":"new"}}]}),
    );
    assert_eq!(
        parent(&replay, &replay_next.after, "replayed"),
        replay.handle(&id("g"))
    );
}

#[test]
fn explicit_and_required_parents_and_missing_endpoints_stay_validated() {
    for (put, code, path) in [
        (
            json!([{"diagram":{"id":"d"}},{"node":{"id":"a","label":"A"}},
            {"edge":{"id":"e","in":"d","from":"a","to":"a"}}]),
            ErrorCode::OutOfRange,
            "put[2].edge.from",
        ),
        (
            json!([{"node":{"id":"a","label":"A","in":"absent"}}]),
            ErrorCode::UnknownRef,
            "put[0].node.in",
        ),
        (
            json!([{"edge":{"id":"e","from":"absent","to":"absent"}}]),
            ErrorCode::UnknownRef,
            "put[0].edge.from",
        ),
        (
            json!([{"row":{"id":"r","cells":[]}}]),
            ErrorCode::MissingField,
            "put[0].row.in",
        ),
        (
            json!([{"series":{"id":"s","data":[]}}]),
            ErrorCode::MissingField,
            "put[0].series.in",
        ),
    ] {
        let scene = new_scene("invalid");
        let error = scene
            .admit(
                &step(&json!({"id":"bad","put":put})),
                &GlobalBudget::default(),
            )
            .expect_err("invalid parent or endpoint");
        assert_eq!((error.code, error.path.as_str()), (code, path));
        assert_eq!(scene.object_count(), 0);
    }
}

#[test]
fn root_node_and_edge_caps_include_continued_scenes_and_are_atomic() {
    for (kind, cap, budget) in [
        ("node", 48, Budget::DiagramNodes),
        ("edge", 96, Budget::DiagramEdges),
    ] {
        let mut scene = new_scene(kind);
        let mut global = GlobalBudget::default();
        let mut put = Vec::new();
        if kind == "edge" {
            put.push(json!({"node":{"id":"n","label":"N"}}));
        }
        for n in 0..cap {
            put.push(if kind == "node" {
                json!({"node":{"id":format!("n{n}"),"label":"N"}})
            } else {
                json!({"edge":{"id":format!("e{n}"),"from":"n","to":"n"}})
            });
        }
        let first = step(&json!({"id":"limit","put":put}));
        let Admission::New(admitted) = scene.admit(&first, &global).expect("exact root cap") else {
            panic!("new step")
        };
        scene.commit(admitted, &mut global).expect("commit");
        assert_eq!(scene.object_count(), if kind == "node" { 48 } else { 97 });
        let extra = if kind == "node" {
            json!({"node":{"id":"extra","label":"N"}})
        } else {
            json!({"edge":{"id":"extra","from":"n","to":"n"}})
        };
        let before = (scene.usage(), global.usage(), global.generation());
        let error = scene
            .admit(&step(&json!({"id":"over","put":[extra]})), &global)
            .expect_err("one over root cap");
        assert_eq!(error.code, ErrorCode::OverBudget(budget));
        assert_eq!((scene.usage(), global.usage(), global.generation()), before);
        assert_eq!(scene.revision(), 1);
    }
}
