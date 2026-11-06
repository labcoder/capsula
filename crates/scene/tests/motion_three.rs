//! 3D motion: owners and their holds, settled rests, spin, predicates and allocation-free poses.
#![expect(
    clippy::float_cmp,
    reason = "poses and angles are exact literals of exactly representable values"
)]
#[path = "support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared counting allocator has helpers this file doesn't use"
)]
mod allocation;
#[path = "support/scene.rs"]
mod scene_support;

use kumochi_scene::{
    Frame, GlobalBudget, SceneState, SceneView,
    layout::LayoutState,
    limits::MAX_SOLIDS,
    three::{
        frame::{Solid, frame},
        raster::FrameScratch,
        view::{Mesh, ViewGeometry},
    },
};
use scene_support::{id, new_scene, prepared};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn fresh() -> (SceneState, GlobalBudget, SceneView) {
    let scene = new_scene("three_motion");
    let view = SceneView::new(&LayoutState::new(&scene), scene.activated());
    (scene, GlobalBudget::default(), view)
}
/// Commits, prepares on the view's layout and activates one step at `now` into `frame`.
fn activate(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    view: &mut SceneView,
    frame: &mut Frame,
    value: Value,
    now: u64,
) {
    let step = prepared(scene, budget, view.layout(), value);
    view.activate(step, now, 0, frame)
        .expect("fresh activation");
}
fn box_at(name: &str, pos: [f64; 3]) -> Value {
    json!({"solid":{"in":"v","id":name,"shape":"box","size":[1,1,1],"pos":pos}})
}
fn pos3(target: &str, delay: u32, keys: Value) -> Value {
    json!({"anim":{"target":target,"track":"pos3","ease":"linear","delay":delay,"keys":keys}})
}
fn geometry(view: &SceneView) -> &ViewGeometry {
    view.layout().views().get(&id("v")).expect("prepared view")
}
fn index(view: &SceneView, name: &str) -> usize {
    geometry(view)
        .solids
        .iter()
        .position(|solid| solid.id == id(name))
        .expect("prepared solid")
}
/// Where `point` of solid `name` lands in the world at `now`, from a fresh frame.
fn at(view: &SceneView, now: u64, name: &str, point: [f64; 3]) -> [f64; 3] {
    let mut frame = Frame::new();
    view.frame(now, &mut frame).expect("fresh frame");
    frame.views()[0].poses()[index(view, name)].apply(point)
}
fn x(view: &SceneView, now: u64, name: &str) -> f64 {
    at(view, now, name, [0.0; 3])[0]
}
fn yaw(view: &SceneView, now: u64) -> f64 {
    let mut frame = Frame::new();
    view.frame(now, &mut frame).expect("fresh frame");
    frame.views()[0]
        .camera()
        .expect("built camera")
        .parameters()[0]
}

#[test]
fn a_delayed_pos3_holds_its_first_key_then_runs_to_its_last_beside_any_other_track() {
    let delayed = pos3("a", 1000, json!([[0, [0, 0, 0]], [1000, [10, 0, 0]]]));
    let other = pos3("b", 0, json!([[0, [0, 0, 0]], [2000, [4, 0, 0]]]));
    for put in [
        vec![
            json!({"view3d":{"id":"v"}}),
            box_at("a", [0.0; 3]),
            delayed.clone(),
        ],
        vec![
            json!({"view3d":{"id":"v"}}),
            box_at("a", [0.0; 3]),
            box_at("b", [0.0, 3.0, 0.0]),
            delayed.clone(),
            other.clone(),
        ],
    ] {
        let beside = put.len() == 5;
        let (mut scene, mut budget, mut view) = fresh();
        let mut frame = Frame::new();
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            &mut frame,
            json!({"id":"s","put":put}),
            0,
        );
        for (now, expected) in [
            (0, 0.0),
            (1000, 0.0),
            (1500, 5.0),
            (2000, 10.0),
            (3000, 10.0),
        ] {
            assert_eq!(x(&view, now, "a"), expected, "a at {now}, beside: {beside}");
        }
        assert_eq!(view.next_wake(0), Some(1000), "beside: {beside}");
        assert!(!view.at_rest(0), "a held owner is not rest");
        assert_eq!(
            [view.view_motion(0), view.animating(0)],
            [beside, beside],
            "a hold alone is not motion"
        );
        if beside {
            assert_eq!(x(&view, 1000, "b"), 2.0);
        }
    }
}

#[test]
fn a_delayed_rot3_holds_its_first_key() {
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"s","put":[{"view3d":{"id":"v"}}, box_at("a", [0.0; 3]),
            {"anim":{"target":"a","track":"rot3","ease":"linear","delay":1000,
                "keys":[[0,[0,0,0]],[1000,[0,90,0]]]}}]}),
        0,
    );
    assert_eq!(at(&view, 500, "a", [1.0, 0.0, 0.0]), [1.0, 0.0, 0.0]);
    let [px, py, pz] = at(&view, 1500, "a", [1.0, 0.0, 0.0]);
    let half = std::f64::consts::FRAC_1_SQRT_2;
    assert!(
        (px - half).abs() < 1e-12 && py == 0.0 && (pz + half).abs() < 1e-12,
        "45 degrees about y: {:?}",
        [px, py, pz]
    );
}

#[test]
fn a_finished_pos3_settles_into_rest_through_a_cut_view_and_without_a_rebuild() {
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"a","put":[{"view3d":{"id":"v"}}, box_at("s", [1.0, 0.0, 0.0])]}),
        0,
    );
    let before = Arc::clone(&geometry(&view).solids[0].source);
    let Mesh::Built(mesh) = geometry(&view).solids[0].mesh.clone() else {
        panic!("built mesh");
    };
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"b","put":[pos3("s", 0, json!([[0, [0, 0, 0]], [100, [10, 0, 0]]]))]}),
        0,
    );
    let solid = &geometry(&view).solids[0];
    assert!(Arc::ptr_eq(&solid.source, &before));
    assert!(
        matches!(&solid.mesh, Mesh::Built(after) if Arc::ptr_eq(after, &mesh)),
        "a track-only step shares the mesh"
    );
    assert_eq!(solid.pos3, [10.0, 0.0, 0.0]);
    assert_eq!(
        x(&view, 50, "s"),
        6.0,
        "the owner overrides the settled end"
    );
    let rested = SceneView::new(view.layout(), scene.activated());
    assert_eq!(
        x(&rested, 0, "s"),
        11.0,
        "a cut view rests at the end offset"
    );
    assert!(rested.at_rest(0));

    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"a","put":[{"view3d":{"id":"v"}}, box_at("s", [1.0, 0.0, 0.0]),
            pos3("s", 0, json!([[0, [0, 0, 0]], [100, [10, 0, 0]]]))]}),
        0,
    );
    let rested = SceneView::new(view.layout(), scene.activated());
    assert_eq!(
        x(&rested, 0, "s"),
        11.0,
        "a track put with its solid settles at its end too"
    );
}

#[test]
fn removing_a_preceding_solid_leaves_a_running_track_on_its_own_solid() {
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"a","put":[{"view3d":{"id":"v"}}, box_at("first", [0.0, 5.0, 0.0]),
            box_at("tracked", [0.0; 3]),
            pos3("tracked", 0, json!([[0, [0, 0, 0]], [10000, [10, 0, 0]]]))]}),
        0,
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"b","remove":["first"]}),
        100,
    );
    assert_eq!(index(&view, "tracked"), 0);
    assert_eq!(x(&view, 5000, "tracked"), 5.0);
}

#[test]
fn a_replacement_rot3_takes_over_only_at_its_activation() {
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    let turn = |degrees: u32| {
        json!({"anim":{"target":"s","track":"rot3","ease":"linear",
            "keys":[[0,[0,0,0]],[1000,[0,0,degrees]]]}})
    };
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"a","put":[{"view3d":{"id":"v"}}, box_at("s", [0.0; 3]), turn(90)]}),
        0,
    );
    let staged = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"b","put":[turn(180)]}),
    );
    let up = |view: &SceneView, now| at(view, now, "s", [1.0, 0.0, 0.0])[1];
    assert!(
        (up(&view, 1000) - 1.0).abs() < 1e-12,
        "the first track still owns rot3"
    );
    view.activate(staged, 1000, 0, &mut frame)
        .expect("fresh activation");
    assert!(
        (up(&view, 1500) - 1.0).abs() < 1e-12,
        "90 of 180 degrees about z"
    );
    assert!(up(&view, 2000).abs() < 1e-12, "the replacement's end");
}

#[test]
fn a_spin_turns_at_its_rate_for_fifteen_seconds_and_holds_its_angle() {
    let put = json!({"id":"a","put":[{"view3d":{"id":"v","spin":30}}, box_at("s", [0.0; 3])]});
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        put.clone(),
        1000,
    );
    // The authored camera's default yaw is 30 degrees; 30 degrees a second adds to it.
    for (now, expected) in [
        (500, 30.0),
        (1000, 30.0),
        (1500, 45.0),
        (2250, 67.5),
        (16_000, 120.0),
        (21_000, 120.0),
    ] {
        assert_eq!(yaw(&view, now), expected, "at {now}");
    }
    assert!(view.view_motion(1000) && view.animating(1000) && !view.at_rest(15_999));
    assert!(!view.view_motion(16_000) && view.at_rest(16_000));
    assert_eq!(view.next_wake(1000), Some(16_000));
    let (mut scene, mut budget, mut view) = fresh();
    activate(&mut scene, &mut budget, &mut view, &mut frame, put, 0);
    view.hold_spin(2000);
    view.hold_spin(3000);
    for now in [2000, 9000] {
        assert_eq!(yaw(&view, now), 90.0, "held at {now}");
    }
    assert!(!view.view_motion(2000) && view.at_rest(2000));
    let still = SceneView::new(view.layout(), scene.activated());
    assert_eq!(yaw(&still, 5000), 30.0, "a cut view installs no spin");
}

/// 160 boxes in attach chains of fifteen under the view: depth 16, the cap. Each chain's root
/// moves and its middle box turns, so every pose composes through a running owner.
fn chains(view: &str) -> Value {
    let mut put = vec![json!({"view3d":{"id":view}})];
    for (chain, depth) in (0..11)
        .flat_map(|chain| (0..15).map(move |depth| (chain, depth)))
        .take(MAX_SOLIDS)
    {
        let name = format!("{view}{chain}_{depth}");
        let mut solid = json!({"in":view,"id":name,"shape":"box","size":[0.5,0.5,0.5],
            "pos":[if depth == 0 { 0 } else { 1 }, if depth == 0 { 2 * chain } else { 0 }, 0]});
        if depth > 0 {
            solid["attach"] = json!(format!("{view}{chain}_{}", depth - 1));
        }
        put.push(json!({ "solid": solid }));
        match depth {
            0 => put.push(pos3(&name, 0, json!([[0, [0, 0, 0]], [50000, [5, 0, 0]]]))),
            8 => put.push(json!({"anim":{"target":name,"track":"rot3",
                "keys":[[0,[0,0,0]],[50000,[0,90,0]]]}})),
            _ => {}
        }
    }
    json!({"id":format!("chains_{view}"),"put":put})
}
/// The steady-state sampler: the view's frame, its posed solids and the 3D frame stage.
fn sample(view: &SceneView, now: u64, frame: &mut Frame, out: &mut FrameScratch) -> usize {
    view.frame(now, frame).expect("bound frame");
    let Some(sampled) = frame.views().first() else {
        return 0;
    };
    let geometry = sampled.geometry();
    let mut solids = [Solid::EMPTY; MAX_SOLIDS];
    let posed = geometry.solids_at(sampled.poses(), &mut solids);
    let camera = sampled.camera().expect("built camera");
    let cancel = AtomicBool::new(false);
    frame_stage(posed, &camera, geometry, out, &cancel);
    posed.len()
}
fn frame_stage(
    posed: &[Solid<'_>],
    camera: &kumochi_scene::three::camera::Camera,
    geometry: &ViewGeometry,
    out: &mut FrameScratch,
    cancel: &AtomicBool,
) {
    frame(posed, camera, &geometry.light, [256, 256], out, cancel).expect("frame stage");
}

#[test]
fn sampling_at_cap_allocates_nothing_across_revisions_removal_and_re_add() {
    let (mut scene, mut budget, mut view) = fresh();
    let mut frame = Frame::new();
    let mut out = FrameScratch::with_capacity(2_000);
    let mut counted = |view: &SceneView, frame: &mut Frame, now: u64, label: &str| {
        let (posed, [allocations, frees]) =
            allocation::activity(|| sample(view, now, frame, &mut out));
        assert_eq!([allocations, frees], [0, 0], "{label}");
        posed
    };
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        chains("v"),
        0,
    );
    assert_eq!(
        counted(&view, &mut frame, 100, "the first sample after activation"),
        160
    );
    assert_eq!(counted(&view, &mut frame, 200, "a warmed sample"), 160);
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"shrink","remove":["v10_9"]}),
        300,
    );
    assert_eq!(
        counted(
            &view,
            &mut frame,
            400,
            "the first sample after a geometry revision"
        ),
        159
    );
    let mut remove: Vec<String> = geometry(&view)
        .solids
        .iter()
        .rev()
        .map(|solid| solid.id.as_str().to_owned())
        .collect();
    remove.push("v".to_owned());
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        json!({"id":"drop","remove":remove}),
        500,
    );
    assert_eq!(
        counted(&view, &mut frame, 600, "the first sample without a view"),
        0
    );
    for index in [1, 2] {
        scene
            .retire(index, &mut budget)
            .expect("retire the removals");
    }
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        &mut frame,
        chains("w"),
        700,
    );
    assert_eq!(
        counted(
            &view,
            &mut frame,
            800,
            "the first sample after re-adding the view"
        ),
        160
    );
}
