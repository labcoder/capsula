//! Exact geometry for the diagram look review's visible-link and clearance regressions.
use kumochi_scene::{
    GlobalBudget, Handle, StopCut,
    display::Prim,
    geom::{Rect, Vec2},
    layout::{LayoutState, SurfaceGeometry},
    text::Bounded,
};
use serde_json::json;

#[path = "support/scene.rs"]
mod scene_support;
use scene_support::{id, new_scene, prepared, prepared_on};

fn chip(layout: &LayoutState, handle: Handle) -> Rect {
    layout
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .expect("edge")
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Rect { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("chip")
}

#[test]
fn two_way_labels_have_distinct_opposite_arcs() {
    let mut scene = new_scene("two_way");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"pair","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"yes"}},
            {"edge":{"id":"e2","in":"diagram","from":"b","to":"a","label":"retry"}}
        ]}),
    );
    for (name, y, width, start, end) in [
        ("e1", -3.0, 32.0, 144.0, 216.0),
        ("e2", 61.0, 48.0, 216.0, 144.0),
    ] {
        let handle = scene.handle(&id(name)).expect("edge");
        assert_eq!(
            chip(&picture.after, handle),
            Rect {
                origin: Vec2::new(180.0 - width * 0.5, y),
                size: Vec2::new(width, 14.0),
            }
        );
        let items = picture.after.items();
        let edge = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("edge");
        let Prim::Line { points, .. } = &edge.primitives[0] else {
            panic!("curve")
        };
        assert_eq!(points[0], Vec2::new(start, 36.0));
        assert_eq!(points[8], Vec2::new(180.0, y + 7.0));
        assert_eq!(points[16], Vec2::new(end, 36.0));
    }
}

#[test]
fn skipped_label_bows_over_the_chain_without_widening_ranks() {
    let mut scene = new_scene("skip_arc");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"skip","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,600,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"node":{"id":"c","in":"diagram","label":"C"}},
            {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"x"}},
            {"edge":{"id":"e2","in":"diagram","from":"b","to":"c","label":"x"}},
            {"edge":{"id":"e3","in":"diagram","from":"a","to":"c","label":"shared"}}
        ]}),
    );
    assert_eq!(
        picture.after.bounds(&id("c")).expect("last rank"),
        Some(Rect {
            origin: Vec2::new(412.0, 16.0),
            size: Vec2::new(80.0, 40.0),
        })
    );
    assert_eq!(
        chip(&picture.after, scene.handle(&id("e3")).expect("skip")),
        Rect {
            origin: Vec2::new(272.0, -3.0),
            size: Vec2::new(56.0, 14.0),
        }
    );
}

#[test]
fn neighbouring_chips_keep_an_exact_six_pixel_world_gap() {
    let mut scene = new_scene("chip_gap");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"parents","put":[
            {"diagram":{"id":"d1","dir":"right","box":[0,0,360,300]}},
            {"diagram":{"id":"d2","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"d1","label":"A"}},
            {"node":{"id":"b","in":"d1","label":"B"}},
            {"node":{"id":"c","in":"d2","label":"C"}},
            {"node":{"id":"d","in":"d2","label":"D"}},
            {"edge":{"id":"e1","in":"d1","from":"a","to":"b","label":"x"}},
            {"edge":{"id":"e2","in":"d2","from":"c","to":"d","label":"y"}},
            {"anim":{"target":"d2","track":"scale","keys":[[0,2]]}},
            {"anim":{"target":"d2","track":"pos","keys":[[0,[-240,-36]]]}},
            {"anim":{"target":"b","track":"pos","keys":[[0,[120,0]]]}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[120,0]]]}}
        ]}),
    );
    assert_eq!(
        chip(&picture.after, scene.handle(&id("e2")).expect("second")),
        Rect {
            origin: Vec2::new(217.0, 29.0),
            size: Vec2::new(16.0, 14.0),
        }
    );
    let first = chip(&picture.after, scene.handle(&id("e1")).expect("first"));
    assert_eq!(
        first,
        Rect {
            origin: Vec2::new(232.0, 29.0),
            size: Vec2::new(16.0, 14.0),
        }
    );
    let second_chip = chip(&picture.after, scene.handle(&id("e2")).expect("second"));
    assert_eq!(
        (first.origin.x - (second_chip.max().x * 2.0 - 240.0)).to_bits(),
        6.0_f32.to_bits()
    );
}

#[test]
fn chips_avoid_title_ink_and_all_four_frame_borders() {
    let mut scene = new_scene("title_clearance");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"title","put":[
            {"diagram":{"id":"diagram"}},
            {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA\nend\n"}},
            {"diagram":{"id":"d2","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"d2","label":"A"}},
            {"node":{"id":"b","in":"d2","label":"B"}},
            {"edge":{"id":"edge","in":"d2","from":"a","to":"b","label":"x"}},
            {"anim":{"target":"a","track":"pos","keys":[[0,[-100,0]]]}},
            {"anim":{"target":"b","track":"pos","keys":[[0,[100,0]]]}},
            {"anim":{"target":"d2","track":"pos","keys":[[0,[0,9]]]}}
        ]}),
    );
    assert_eq!(
        picture.after.bounds(&id("g")).expect("frame"),
        Some(Rect {
            origin: Vec2::new(132.0, 32.0),
            size: Vec2::new(96.0, 82.0),
        })
    );
    assert_eq!(
        chip(&picture.after, scene.handle(&id("edge")).expect("edge")),
        Rect {
            origin: Vec2::new(108.25, 29.0),
            size: Vec2::new(16.0, 14.0),
        }
    );
}

#[test]
fn framed_group_participates_in_exact_floor_fit_boundaries() {
    for (width, scale, x, right) in [
        (160.0, 1.0_f32, 0.0_f32, 128.0_f32),
        (144.0, 0.875, 9.0, 114.0),
        (112.0, 0.8, 22.4, 99.200_005),
        (80.0, 0.8, 35.2, 105.600_006),
    ] {
        let mut scene = new_scene("frame_floor");
        let before = LayoutState::new(&scene);
        let picture = prepared_on(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"frame","put":[
                {"diagram":{"id":"diagram"}},
                {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA\nend\n"}}
            ]}),
            SurfaceGeometry::Inline { width },
        );
        let items = picture.after.items();
        let root = items
            .iter()
            .find(|item| item.handle == scene.handle(&id("diagram")).expect("root"))
            .expect("root item");
        assert_eq!(root.transform.0[0].to_bits(), scale.to_bits());
        assert_eq!(root.transform.0[4].to_bits(), x.to_bits());
        assert_eq!(
            picture
                .after
                .bounds(&id("g"))
                .expect("frame")
                .expect("bounds")
                .max()
                .x
                .to_bits(),
            right.to_bits()
        );
    }
}

#[test]
fn fresh_and_cached_chips_agree_after_a_lower_id_add_and_label_update() {
    let graph = json!({"id":"first","put":[
        {"diagram":{"id":"diagram","box":[0,0,600,600]}},
        {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA --> B\nC --> D\nend\n"}},
        {"edge":{"id":"z","in":"g","from":"A","to":"B","label":"a wide label in the outer column"}}
    ]});
    let added = json!({"edge":{"id":"early","in":"g","from":"A","to":"B","label":"yes"}});
    let mut scene = new_scene("cached");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(&mut scene, &mut budget, &before, graph.clone());
    let next = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"add","put":[added]}),
    );
    for (cached, label) in [
        (next.after.clone(), "a wide label in the outer column"),
        (prepared(&mut scene, &mut budget, &next.after,
            json!({"id":"update","set":[{"target":"z","label":"a changed label in outer column"}]})).after,
            "a changed label in outer column"),
    ] {
        let mut fresh_scene = new_scene("fresh");
        let fresh_before = LayoutState::new(&fresh_scene);
        let mut value = graph.clone();
        value["put"][2]["edge"]["label"] = json!(label);
        value["put"].as_array_mut().expect("put").push(added.clone());
        let fresh = prepared(&mut fresh_scene, &mut GlobalBudget::default(), &fresh_before, value);
        for name in ["early", "z", "g"] {
            let a = cached.items();
            let b = fresh.after.items();
            let a = a.iter().find(|item| item.handle == scene.handle(&id(name)).expect("cached handle")).expect("cached");
            let b = b.iter().find(|item| item.handle == fresh_scene.handle(&id(name)).expect("fresh handle")).expect("fresh");
            assert_eq!(a.bounds, b.bounds, "{name} bounds");
            assert_eq!(a.primitives, b.primitives, "{name} primitives");
            assert_eq!(cached.bounds(&id(name)).expect("cached world"), fresh.after.bounds(&id(name)).expect("fresh world"));
        }
    }
}

#[test]
fn three_parallel_links_alternate_lanes_in_both_flow_directions() {
    for (direction, centers) in [
        ("right", [(180.0, 4.0), (180.0, 68.0), (180.0, -28.0)]),
        ("down", [(128.0, 92.0), (232.0, 92.0), (76.0, 92.0)]),
    ] {
        let mut scene = new_scene("lanes");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"pair","put":[
                {"diagram":{"id":"diagram","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"diagram","label":"A"}},
                {"node":{"id":"b","in":"diagram","label":"B"}},
                {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"x"}},
                {"edge":{"id":"e2","in":"diagram","from":"b","to":"a","label":"y"}},
                {"edge":{"id":"e3","in":"diagram","from":"a","to":"b","label":"z"}}
            ]}),
        );
        for (name, (x, y)) in ["e1", "e2", "e3"].into_iter().zip(centers) {
            let items = picture.after.items();
            let item = items
                .iter()
                .find(|item| item.handle == scene.handle(&id(name)).expect("edge"))
                .expect("item");
            let Prim::Line { points, .. } = &item.primitives[0] else {
                panic!("curve")
            };
            assert_eq!(points[8], Vec2::new(x, y));
        }
    }
}

#[test]
fn a_detached_bowed_edge_keeps_its_curve_when_its_label_changes() {
    let mut scene = new_scene("detached_bow");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"pair","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"x"}},
            {"edge":{"id":"e2","in":"diagram","from":"b","to":"a","label":"y"}}
        ]}),
    );
    let handle = scene.handle(&id("e2")).expect("kept edge");
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(vec![handle]).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let cut = first.after.cut(&scene).expect("detached layout");
    let next = prepared(
        &mut scene,
        &mut budget,
        &cut,
        json!({"id":"label","set":[{"target":"e2","label":"z"}]}),
    );
    let old = cut.items();
    let new = next.after.items();
    assert_eq!(old[0].primitives[0], new[0].primitives[0]);
    assert_eq!(old[0].primitives[1], new[0].primitives[1]);
    assert_eq!(chip(&cut, handle), chip(&next.after, handle));
    assert_eq!(scene.object_count(), 1);
}

#[test]
fn a_labelled_self_loop_runs_outside_its_node() {
    for (direction, label, width, middle_x) in [
        ("right", "retry", 48.0, 256.0),
        ("down", "retry", 48.0, 256.0),
        ("right", "a very long wrapped self loop label", 188.0, 326.0),
    ] {
        let mut scene = new_scene("self_loop");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"loop","put":[
                {"diagram":{"id":"diagram","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"diagram","label":"A"}},
                {"edge":{"id":"loop","in":"diagram","from":"a","to":"a","label":label}}
            ]}),
        );
        let handle = scene.handle(&id("loop")).expect("loop");
        assert_eq!(
            chip(&picture.after, handle),
            Rect {
                origin: Vec2::new(232.0, 76.0),
                size: Vec2::new(width, 14.0),
            }
        );
        let items = picture.after.items();
        let item = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("edge");
        let Prim::Line { points, .. } = &item.primitives[0] else {
            panic!("curve")
        };
        assert_eq!(points[0], Vec2::new(220.0, 36.0));
        assert_eq!(points[8], Vec2::new(middle_x, 83.0));
        assert_eq!(points[16], Vec2::new(180.0, 56.0));
    }
}

#[test]
fn two_labelled_self_loops_have_wire_ordered_lanes() {
    self_loop_lanes(2);
}

#[test]
fn three_labelled_self_loops_have_wire_ordered_lanes() {
    self_loop_lanes(3);
}

fn self_loop_lanes(count: usize) {
    let mut scene = new_scene("loop_lanes");
    let before = LayoutState::new(&scene);
    let mut put = vec![
        json!({"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}}),
        json!({"node":{"id":"a","in":"diagram","label":"A"}}),
    ];
    for name in ["e3", "e2", "e1"].into_iter().skip(3 - count) {
        put.push(json!({"edge":{"id":name,"in":"diagram","from":"a","to":"a","label":"retrying"}}));
    }
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"lanes","put":put}),
    );
    let mut chips = Vec::new();
    let node = Rect {
        origin: Vec2::new(140.0, 16.0),
        size: Vec2::new(80.0, 40.0),
    };
    for (name, y) in [("e1", 76.0), ("e2", 102.0), ("e3", 128.0)]
        .into_iter()
        .take(count)
    {
        let rect = chip(&picture.after, scene.handle(&id(name)).expect("loop"));
        assert_eq!(
            rect,
            Rect {
                origin: Vec2::new(232.0, y),
                size: Vec2::new(72.0, 14.0)
            },
            "{name} in the {count}-loop scene"
        );
        assert!(!rect.intersects(node), "{name} covers its node");
        assert!(
            chips.iter().all(|other| !rect.intersects(*other)),
            "{name} covers an earlier chip"
        );
        chips.push(rect);
    }
}

#[test]
fn a_cut_self_loop_keeps_its_primitives_when_its_node_survives() {
    let mut scene = new_scene("cut_loop");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"loop","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[40,50,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"edge":{"id":"loop","in":"diagram","from":"a","to":"a","label":"retrying"}},
            {"anim":{"target":"diagram","track":"pos","keys":[[0,[40,50]]]}}
        ]}),
    );
    let node = scene.handle(&id("a")).expect("node");
    let handle = scene.handle(&id("loop")).expect("loop");
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(vec![node, handle]).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let cut = first.after.cut(&scene).expect("cut layout");
    let next = prepared(&mut scene, &mut budget, &cut, json!({"id":"noop"}));
    let old = cut.items();
    let new = next.after.items();
    let old = old
        .iter()
        .find(|item| item.handle == handle)
        .expect("old loop");
    let new = new
        .iter()
        .find(|item| item.handle == handle)
        .expect("new loop");
    assert_eq!(old.primitives, new.primitives);
    assert_eq!(old.transform, new.transform);
}

#[test]
fn tall_self_loop_lanes_saturate_before_the_coordinate_limit() {
    use kumochi_scene::display::{MeasureError, TextMeasure, TextStyle};
    struct Tall;
    impl TextMeasure for Tall {
        fn measure(
            &mut self,
            text: &str,
            _: TextStyle,
            _: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok(match text {
                "A" => [8.0, 10.0],
                _ => [64.0, 3000.0],
            })
        }
    }
    for (count, scale, expected) in [
        (3, 1, 6115.0),
        (4, 1, 8521.0),
        (8, 1, 8521.0),
        (8, 3, 2605.3333),
    ] {
        let mut scene = new_scene("tall_loops");
        let before = LayoutState::new(&scene);
        let mut put = vec![
            json!({"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}}),
            json!({"node":{"id":"a","in":"diagram","label":"A"}}),
        ];
        for index in 0..count {
            put.push(json!({"edge":{"id":format!("e{index}"),"in":"diagram","from":"a","to":"a","label":"retrying"}}));
        }
        put.push(json!({"anim":{"target":"diagram","track":"scale","keys":[[0,scale]]}}));
        let picture = scene_support::prepared_with(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"tall","put":put}),
            SurfaceGeometry::default(),
            &mut Tall,
        );
        let handle = scene
            .handle(&id(&format!("e{}", count - 1)))
            .expect("last loop");
        let items = picture.after.items();
        let item = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("loop item");
        let Prim::Line { points, .. } = &item.primitives[0] else {
            panic!("loop curve");
        };
        assert_eq!(points[8], Vec2::new(268.0, expected), "{count} tall loops");
    }
}

#[test]
fn removing_the_last_label_restores_the_fresh_unlabelled_curve() {
    let mut scene = new_scene("remove_label");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let value = json!({"id":"pair","put":[
        {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
        {"node":{"id":"a","in":"diagram","label":"A"}},
        {"node":{"id":"b","in":"diagram","label":"B"}},
        {"edge":{"id":"keep","in":"diagram","from":"a","to":"b"}},
        {"edge":{"id":"remove","in":"diagram","from":"b","to":"a","label":"retry"}}
    ]});
    let first = prepared(&mut scene, &mut budget, &before, value.clone());
    let next = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"remove","remove":["remove"]}),
    );
    let mut fresh_scene = new_scene("fresh_remove");
    let fresh_before = LayoutState::new(&fresh_scene);
    let mut fresh_value = value;
    fresh_value["put"].as_array_mut().expect("put").pop();
    let fresh = prepared(
        &mut fresh_scene,
        &mut GlobalBudget::default(),
        &fresh_before,
        fresh_value,
    );
    let a = next.after.items();
    let b = fresh.after.items();
    let a = a
        .iter()
        .find(|item| item.handle == scene.handle(&id("keep")).expect("cached"))
        .expect("cached item");
    let b = b
        .iter()
        .find(|item| item.handle == fresh_scene.handle(&id("keep")).expect("fresh"))
        .expect("fresh item");
    assert_eq!(a.bounds, b.bounds);
    assert_eq!(a.primitives, b.primitives);
    let Prim::Line { points, .. } = &a.primitives[0] else {
        panic!("curve")
    };
    assert_eq!(points[8], Vec2::new(180.0, 36.0));
}

#[test]
fn each_frame_side_repels_a_chip_independently() {
    for (side, direction, position, node_offset, expected) in [
        ("top", "right", [0, -10], [100, 0], Vec2::new(108.25, 29.0)),
        (
            "bottom",
            "right",
            [0, 84],
            [100, 0],
            Vec2::new(108.25, 29.0),
        ),
        ("left", "down", [-54, 0], [0, 100], Vec2::new(172.0, 119.0)),
        ("right", "down", [54, 0], [0, 100], Vec2::new(172.0, 119.0)),
    ] {
        let mut scene = new_scene("frame_side");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"side","put":[
                {"diagram":{"id":"diagram"}},
                {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[G]\nA\nend\n"}},
                {"diagram":{"id":"other","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"other","label":"A"}},
                {"node":{"id":"b","in":"other","label":"B"}},
                {"edge":{"id":"edge","in":"other","from":"a","to":"b","label":"x"}},
                {"anim":{"target":"a","track":"pos","keys":[[0,[-node_offset[0],-node_offset[1]]]]}},
                {"anim":{"target":"b","track":"pos","keys":[[0,node_offset]]}},
                {"anim":{"target":"other","track":"pos","keys":[[0,position]]}}
            ]}),
        );
        assert_eq!(
            chip(&picture.after, scene.handle(&id("edge")).expect("edge")),
            Rect {
                origin: expected,
                size: Vec2::new(16.0, 14.0),
            },
            "{side}"
        );
    }
}
