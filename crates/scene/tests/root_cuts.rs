//! Previously admitted survivors stay replayable when Stop removes their containers.
use kumochi_scene::{
    Admission, Budget, ErrorCode, GlobalBudget, Record, SceneState, StopCut, StopOutcome,
    parse_record,
};
use kumochi_scene::{
    layout::{LayoutState, SurfaceGeometry, recover},
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[path = "support/scene.rs"]
mod scene_support;
use scene_support::{id, new_scene, prepared};

fn step(value: &Value) -> kumochi_scene::Step {
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&json!({"step":value})).expect("JSON"))
            .expect("typed step")
    else {
        panic!("step")
    };
    step
}

fn detached(kind: &str) -> (SceneState, GlobalBudget, usize) {
    let mut scene = new_scene("cut");
    let mut global = GlobalBudget::default();
    let count = match kind {
        "node" => 25,
        "edge" => 49,
        "row" => 21,
        "series" => 5,
        _ => unreachable!(),
    };
    let mut put = Vec::new();
    let mut names = Vec::new();
    for group in 0..2 {
        let parent = format!("g{group}");
        put.push(match kind {
            "node" | "edge" => json!({"diagram":{"id":parent}}),
            "row" => json!({"table":{"id":parent,"cols":["A"]}}),
            "series" => json!({"chart":{"id":parent}}),
            _ => unreachable!(),
        });
        if kind == "edge" {
            put.push(json!({"node":{"id":format!("n{group}"),"in":parent,"label":"N"}}));
        }
        for index in 0..count {
            let name = format!("o{group}_{index}");
            put.push(match kind {
                "node" => json!({"node":{"id":name,"in":parent,"label":"N"}}),
                "edge" => json!({"edge":{"id":name,"in":parent,"from":format!("n{group}"),"to":format!("n{group}")}}),
                "row" => json!({"row":{"id":name,"in":parent,"cells":["x"]}}),
                "series" => json!({"series":{"id":name,"in":parent,"data":[1,2]}}),
                _ => unreachable!(),
            });
            names.push(name);
        }
    }
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut global,
        &before,
        json!({"id":"first","put":put}),
    );
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::new(
            names
                .iter()
                .map(|name| scene.handle(&id(name)).expect("survivor"))
                .collect(),
        )
        .expect("bounded cut"),
    };
    assert_eq!(
        scene.stop(&cut, &mut global),
        Ok(StopOutcome::Kept),
        "{kind} cut"
    );
    assert_eq!(scene.object_count(), 2 * count, "{kind} survivor count");
    let cut_layout = picture.after.cut(&scene).expect("detached layout");
    let replay = SceneState::replay(scene.header().clone(), scene.recovery_log())
        .expect("old cut log replays");
    assert_eq!(
        replay.recovery_log(),
        scene.recovery_log(),
        "{kind} authored log"
    );
    assert_eq!(replay.usage(), scene.usage(), "{kind} replay usage");
    let (mut recovered, layout) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut scene_support::Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("recovery prepares old cut");
    assert_eq!(layout.items(), cut_layout.items(), "{kind} replay layout");
    let next = prepared(&mut scene, &mut global, &cut_layout, json!({"id":"next"}));
    assert_eq!(
        next.after.items(),
        cut_layout.items(),
        "{kind} no-op layout"
    );
    scene.retire(1, &mut global).expect("continuation retires");
    assert_eq!(global.usage(), scene.usage(), "{kind} retired usage");
    let recovered_next = prepared(
        &mut recovered,
        &mut GlobalBudget::default(),
        &layout,
        json!({"id":"next"}),
    );
    assert_eq!(
        recovered_next.after.items(),
        next.after.items(),
        "{kind} replay continuation"
    );
    (scene, global, 2 * count)
}

#[test]
fn detached_nodes_edges_rows_and_series_survive_cut_replay_and_continuation() {
    for kind in ["node", "edge", "row", "series"] {
        let (scene, _, count) = detached(kind);
        assert_eq!(scene.object_count(), count);
        assert_eq!(scene.revision(), 2);
    }
}

#[test]
fn new_root_growth_cannot_bypass_caps_after_an_oversized_detached_cut() {
    for (kind, cap) in [
        ("node", Budget::DiagramNodes),
        ("edge", Budget::DiagramEdges),
    ] {
        let (scene, global, count) = detached(kind);
        let error = scene
            .admit(
                &step(&json!({"id":"growth","put":[{"node":{"id":"extra","label":"N"}}]})),
                &global,
            )
            .expect_err("root growth remains capped");
        assert_eq!(
            (error.code, error.path.as_str()),
            (ErrorCode::OverBudget(cap), "")
        );
        assert_eq!(scene.object_count(), count);
        if kind == "node" {
            let error = scene.admit(&step(&json!({"id":"edge_growth","put":[{"edge":{"id":"extra","from":"o0_0","to":"o1_0"}}]})), &global)
                .expect_err("an edge also grows the oversized root diagram");
            assert_eq!(error.code, ErrorCode::OverBudget(Budget::DiagramNodes));
        }
        assert!(matches!(
            scene.admit(
                &step(&json!({"id":"unrelated","put":[{"shape":{"id":"shape","kind":"rect"}}]})),
                &global
            ),
            Ok(Admission::New(_))
        ));
    }
}
