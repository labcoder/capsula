//! 3D preparation: canonical meshes, sharing, routing, failure, cut, recovery and charges.
use kumochi_scene::{
    Admission, Begin, Committed, GlobalBudget, Handle, Op, Record, SceneState, Step, StopCut,
    WireId,
    display::{MeasureError, Paint, Prim, TextMeasure, TextStyle},
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover,
    },
    parse_record,
    text::Bounded,
    theme::Theme,
    three::{
        Transform, Triangle,
        mesh::{MeshScratch, build},
        size::{MeshError, triangles},
        view::{Mesh, SolidMesh, ViewGeometry},
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[path = "support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared counting allocator has helpers this file doesn't use"
)]
mod allocation;

struct Measure;
impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        _max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let width = f32::from(u16::try_from(text.chars().count()).expect("bounded text")) * 8.0;
        Ok([width, 10.0])
    }
}
fn header() -> Begin {
    let Ok(Record::Begin(header)) =
        parse_record(br#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#)
    else {
        panic!("begin fixture");
    };
    header
}
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}
fn step(value: Value) -> Step {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    match parse_record(&serde_json::to_vec(&envelope).expect("JSON")) {
        Ok(Record::Step(step)) => step,
        other => panic!("step fixture: {other:?}"),
    }
}
fn scene() -> (SceneState, GlobalBudget) {
    (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    )
}
fn committed(scene: &mut SceneState, budget: &mut GlobalBudget, value: Value) -> Committed {
    let Admission::New(candidate) = scene.admit(&step(value), budget).expect("admitted fixture")
    else {
        panic!("new step");
    };
    scene.commit(candidate, budget).expect("fresh candidate")
}
fn picture(
    scene: &SceneState,
    committed: Committed,
    before: &LayoutState,
    cancel: &AtomicBool,
) -> Result<PreparedStep, PrepareError> {
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .map_err(|(error, _)| error)?;
    let result = prepare(input, &mut Measure, cancel)?;
    result.validate(scene, before)?;
    Ok(result)
}
fn next(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
) -> PreparedStep {
    let commit = committed(scene, budget, value);
    picture(scene, commit, before, &AtomicBool::new(false)).expect("prepared step")
}
fn opening(scene: &mut SceneState, budget: &mut GlobalBudget, value: Value) -> PreparedStep {
    let before = LayoutState::new(scene);
    next(scene, budget, &before, value)
}
fn view() -> Value {
    json!({"view3d":{"id":"v"}})
}
fn cube(name: &str, extra: &Value) -> Value {
    let mut solid = json!({"in":"v","id":name,"shape":"box","size":[2,2,2]});
    solid
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    json!({ "solid": solid })
}
/// Admitted, but its corners narrow together at the composed rest transform.
fn collapsing(name: &str) -> Value {
    cube(
        name,
        &json!({"size":[1,1,1],"pos":[19998,0,0],"scale":0.001}),
    )
}
fn text() -> Value {
    json!({"text":{"id":"t","text":"T","at":[0,0]}})
}
fn geometry(layout: &LayoutState) -> &ViewGeometry {
    layout.views().get(&id("v")).expect("prepared view")
}
fn built(geometry: &ViewGeometry, index: usize) -> &Arc<SolidMesh> {
    match &geometry.solids[index].mesh {
        Mesh::Built(mesh) => mesh,
        Mesh::Failed(error) => panic!("solid {index} failed: {error}"),
    }
}
/// Bytes the allocator hands out to build `make`'s value: an oracle independent of the charges.
fn requested<T>(make: impl FnOnce() -> T) -> usize {
    let (_, _, [allocated, _]) = allocation::usage(make);
    allocated
}
/// A mesh's allocations: its `Arc` and three arrays.
fn mesh_bytes(mesh: &SolidMesh) -> usize {
    requested(|| {
        Arc::new(SolidMesh {
            vertices: mesh.vertices.clone(),
            triangles: mesh.triangles.clone(),
            faces: mesh.faces.clone(),
        })
    })
}
/// A shared source's allocations: its `Arc` and the record's own heap.
fn source_bytes(source: &Arc<Op>) -> usize {
    requested(|| Arc::new((**source).clone()))
}
fn paints(prepared: &PreparedStep) -> Vec<Paint> {
    prepared
        .display
        .items
        .iter()
        .flat_map(|item| item.primitives.iter())
        .filter_map(|prim| match prim {
            Prim::Text { color, .. } => Some(*color),
            _ => None,
        })
        .collect()
}
fn texts(prepared: &PreparedStep) -> Vec<&str> {
    prepared
        .display
        .items
        .iter()
        .flat_map(|item| item.primitives.iter())
        .filter_map(|prim| match prim {
            Prim::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn scale_reaches_world_vertices_once_through_the_composed_rest() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[
            view(),
            cube("p", &json!({"pos":[10,0,0],"scale":2})),
            cube("c", &json!({"attach":"p","pos":[1,0,0],"scale":1.5})),
        ]}),
    );
    assert_eq!(first.views.len(), 1);
    let geometry = geometry(&first.after);
    let child = &geometry.solids[1];
    assert_eq!(child.parent, Some(0));
    let mesh = built(geometry, 1);
    assert_eq!(mesh.triangles.len(), 12);
    let mut corner = [f64::NEG_INFINITY; 3];
    for &v in &*mesh.vertices {
        let p = child.rest.apply(v);
        corner = [
            corner[0].max(p[0]),
            corner[1].max(p[1]),
            corner[2].max(p[2]),
        ];
    }
    assert_eq!(
        corner.map(f64::to_bits),
        [15.0_f64, 3.0, 3.0].map(f64::to_bits),
        "a baked scale gives (16.5, 4.5, 4.5)"
    );
}

#[test]
fn reshading_shares_sources_and_meshes_and_text_sets_prepare_no_view() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({})), cube("u", &json!({"pos":[4,0,0]}))]}),
    );
    let quiet = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","label":"lid"},{"target":"u","info":"opens"}]}),
    );
    assert!(quiet.views.is_empty(), "label and info sets emit no view");
    assert!(Arc::ptr_eq(
        &first.after.views()[&id("v")],
        &quiet.after.views()[&id("v")]
    ));
    let shaded = next(
        &mut scene,
        &mut budget,
        &quiet.after,
        json!({"id":"c","set":[{"target":"s","color":"#ff0000"}]}),
    );
    assert_eq!(shaded.views.len(), 1);
    let (old, new) = (geometry(&first.after), geometry(&shaded.after));
    assert_eq!(new.solids[0].rgb, [0xff, 0, 0]);
    assert_ne!(old.solids[0].rgb, [0xff, 0, 0]);
    assert_eq!(new.solids[1].rgb, old.solids[1].rgb);
    for index in 0..2 {
        assert!(Arc::ptr_eq(
            &old.solids[index].source,
            &new.solids[index].source
        ));
        assert!(Arc::ptr_eq(built(old, index), built(new, index)));
    }
    let reused: usize = (0..2)
        .map(|index| mesh_bytes(built(old, index)) + source_bytes(&old.solids[index].source))
        .sum::<usize>()
        + source_bytes(&old.source);
    assert_eq!(new.marginal_bytes(Some(old)), new.full_bytes() - reused);
}

#[test]
fn a_reshaded_view_carries_the_full_mesh_charge_of_a_fresh_one() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({}))]}),
    );
    let shaded = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","color":"#ff0000"}]}),
    );
    let (mut fresh_scene, mut fresh_budget) = self::scene();
    let fresh = opening(
        &mut fresh_scene,
        &mut fresh_budget,
        json!({"id":"a","put":[view(), cube("s", &json!({"color":"#ff0000"}))]}),
    );
    let (reshaded, scratch) = (geometry(&shaded.after), geometry(&fresh.after));
    let reused = mesh_bytes(built(reshaded, 0))
        + source_bytes(&reshaded.solids[0].source)
        + source_bytes(&reshaded.source);
    assert_eq!(reshaded.full_bytes(), scratch.full_bytes());
    assert_eq!(
        reshaded.marginal_bytes(Some(geometry(&first.after))),
        reshaded.full_bytes() - reused
    );
    let weak = Arc::downgrade(built(reshaded, 0));
    let full = reshaded.full_bytes();
    drop(first);
    assert_eq!(geometry(&shaded.after).full_bytes(), full);
    assert!(
        weak.upgrade().is_some(),
        "the successor still owns the mesh"
    );
    assert!(shaded.resident_bytes < fresh.resident_bytes);
    assert!(shaded.after.resident_bytes() >= reshaded.full_bytes());
    assert_eq!(shaded.after.resident_bytes(), fresh.after.resident_bytes());
    assert!(fresh.resident_bytes >= scratch.full_bytes());
    assert!(fresh.scratch_bytes > 0);
    assert_eq!(shaded.scratch_bytes, 0, "a reshade builds nothing");
}

#[test]
fn one_step_routes_its_3d_and_2d_sets_to_their_own_phases() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({})), text()]}),
    );
    let second = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","color":"#00ff00"},{"target":"t","color":"#0000ff"}]}),
    );
    assert_eq!(geometry(&second.after).solids[0].rgb, [0, 0xff, 0]);
    assert_eq!(texts(&second), ["T"]);
    let (mut blue_scene, mut blue_budget) = self::scene();
    let blue = opening(
        &mut blue_scene,
        &mut blue_budget,
        json!({"id":"a","put":[{"text":{"id":"t","text":"T","at":[0,0],"color":"#0000ff"}}]}),
    );
    assert_eq!(paints(&second), paints(&blue), "the 2D colour set applied");
    assert_ne!(paints(&second), paints(&first));
    let third = next(
        &mut scene,
        &mut budget,
        &second.after,
        json!({"id":"c","remove":["s"]}),
    );
    assert_eq!(third.removed, [geometry(&first.after).solids[0].handle]);
    assert!(geometry(&third.after).solids.is_empty());
}

#[test]
fn a_failed_solid_empties_its_view_until_removed_and_recovery_matches() {
    let (mut scene, mut budget) = scene();
    let good = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({}))]}),
    );
    assert!(!geometry(&good.after).failed);
    let bad = next(
        &mut scene,
        &mut budget,
        &good.after,
        json!({"id":"b","put":[collapsing("x"), text()]}),
    );
    assert_eq!(bad.views.len(), 1, "an explicit replacement");
    assert!(bad.views[0].geometry.failed);
    assert_eq!(texts(&bad), ["T"]);
    assert!(matches!(
        geometry(&bad.after).solids[1].mesh,
        Mesh::Failed(MeshError::Degenerate("pos"))
    ));
    let shaded = next(
        &mut scene,
        &mut budget,
        &bad.after,
        json!({"id":"c","set":[{"target":"s","color":"#ff0000"}]}),
    );
    assert!(
        geometry(&shaded.after).failed,
        "the failure stays while its source does"
    );
    let healed = next(
        &mut scene,
        &mut budget,
        &shaded.after,
        json!({"id":"d","remove":["x"]}),
    );
    let view = geometry(&healed.after);
    assert!(!view.failed);
    assert_eq!(view.solids.len(), 1);
    assert!(Arc::ptr_eq(built(view, 0), built(geometry(&good.after), 0)));
    assert_eq!(view.solids[0].rgb, [0xff, 0, 0]);

    let (_, replayed) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
    )
    .expect("recovery");
    let again = geometry(&replayed);
    assert!(!again.failed);
    assert_eq!(again.solids.len(), 1);
    assert_eq!(again.solids[0].rgb, [0xff, 0, 0]);
    assert_eq!(built(again, 0).vertices, built(view, 0).vertices);
    assert_eq!(replayed.resident_bytes(), healed.after.resident_bytes());
}

#[test]
fn a_cut_drops_unrevealed_solids_and_recomputes_the_failure() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({})), collapsing("x")]}),
    );
    let activated = geometry(&first.after);
    assert!(activated.failed);
    let revealed = vec![activated.handle, activated.solids[0].handle];
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(revealed).expect("bounded cut"),
            },
            &mut budget,
        )
        .expect("valid cut");
    let tip = first.after.cut(&scene).expect("cut");
    let kept = geometry(&tip);
    assert_eq!(kept.solids.len(), 1);
    assert_eq!(kept.solids[0].id, id("s"));
    assert!(!kept.failed);
    assert!(kept.camera.is_some());
    assert!(Arc::ptr_eq(built(kept, 0), built(activated, 0)));
}

#[test]
fn a_cancelled_preparation_publishes_nothing() {
    let (mut scene, mut budget) = scene();
    let before = LayoutState::new(&scene);
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({}))]}),
    );
    assert!(matches!(
        picture(&scene, commit, &before, &AtomicBool::new(true)),
        Err(PrepareError::Cancelled)
    ));
    assert!(before.views().is_empty());
}

fn tube(path: &Value, r2: f64, segments: u8, pos: &Value) -> Value {
    json!({"solid":{"in":"v","id":"s","shape":"tube","path":path,"r2":r2,"segments":segments,"pos":pos}})
}
/// The single solid's stored vertices at its rest pose, in f64 as the frame stage reads them.
fn rest_vertices(geometry: &ViewGeometry) -> Vec<[f64; 3]> {
    let rest = geometry.solids[0].rest;
    built(geometry, 0)
        .vertices
        .iter()
        .map(|&v| rest.apply(v))
        .collect()
}

#[test]
fn offset_thin_tubes_keep_their_rest_cross_section() {
    // Each tube's axis lands on a world axis; every stored vertex must sit at r2 from it.
    // Lengths admit three decimals, so the first runs CAPS-191's tube at r2 0.001, not 0.0001.
    for (path, r2, segments, pos, axis) in [
        (
            json!([[9000, 0, 0], [9000, 1, 0]]),
            0.001,
            4,
            json!([-9000, 0, 0]),
            [0, 2],
        ),
        (
            json!([[6000, 6000, 0], [6000, 6000, 1]]),
            0.001,
            24,
            json!([-6000, -6000, 0]),
            [0, 1],
        ),
    ] {
        let (mut scene, mut budget) = scene();
        let prepared = opening(
            &mut scene,
            &mut budget,
            json!({"id":"a","put":[view(), tube(&path, r2, segments, &pos)]}),
        );
        let geometry = geometry(&prepared.after);
        assert!(!geometry.failed, "{path}");
        let vertices = rest_vertices(geometry);
        assert_eq!(vertices.len(), 2 * usize::from(segments), "{path}");
        for p in vertices {
            let distance = p[axis[0]].hypot(p[axis[1]]);
            assert!(
                (distance - r2).abs() <= r2 * 1e-4,
                "{path}: a vertex sits {distance} from the axis"
            );
        }
    }
}

#[test]
fn every_face_group_lies_on_its_anchor_plane() {
    for (path, r2, segments, triangles, faces) in [
        (
            json!([[0, 0, 0], [0, 1, 0], [1, 2, 0], [1, 3, 1]]),
            0.1,
            8,
            60,
            60,
        ),
        (json!([[0, 0, 0], [0, 0, 2], [2, 0, 2]]), 0.25, 4, 20, 20),
        (json!([[0, 0, 0], [0, 0, 1], [0, 0, 3]]), 0.25, 4, 20, 12),
    ] {
        let (mut scene, mut budget) = scene();
        let prepared = opening(
            &mut scene,
            &mut budget,
            json!({"id":"a","put":[view(), tube(&path, r2, segments, &json!([0, 0, 0]))]}),
        );
        let mesh = built(geometry(&prepared.after), 0);
        assert_eq!(
            (mesh.triangles.len(), mesh.faces.len()),
            (triangles, faces),
            "{path}: a straight run keeps its bands grouped, a bend splits them"
        );
        let at = |i: u32| mesh.vertices[usize::try_from(i).expect("index")];
        for face in &*mesh.faces {
            let [base, second, third] = face.anchors.map(at);
            let offset = |p: [f64; 3]| [p[0] - base[0], p[1] - base[1], p[2] - base[2]];
            let [e1, e2] = [second, third].map(offset);
            let normal = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let length = normal.iter().map(|v| v * v).sum::<f64>().sqrt();
            let start = usize::try_from(face.start).expect("start");
            let end = start + usize::try_from(face.len).expect("len");
            for triangle in &mesh.triangles[start..end] {
                for &i in triangle {
                    let d = offset(at(i));
                    let off = (d[0] * normal[0] + d[1] * normal[1] + d[2] * normal[2]) / length;
                    assert!(
                        off.abs() <= 1e-5,
                        "{path}: a group vertex is {off} off its plane"
                    );
                }
            }
        }
    }
}

#[test]
fn a_3d_object_put_and_removed_in_one_step_prepares() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({}))]}),
    );
    let second = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","put":[cube("x", &json!({})), text()],"remove":["x"]}),
    );
    assert_eq!(texts(&second), ["T"]);
    assert_eq!(second.removed, [Handle::new(2, 0).expect("handle")]);
    let kept = geometry(&second.after);
    assert_eq!(kept.solids.len(), 1);
    assert_eq!(kept.solids[0].id, id("s"));
    assert!(Arc::ptr_eq(
        built(kept, 0),
        built(geometry(&first.after), 0)
    ));

    // A whole view and its solid, put and removed together.
    let (mut scene, mut budget) = self::scene();
    let only = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({})), text()],"remove":["s","v"]}),
    );
    assert_eq!(texts(&only), ["T"]);
    assert_eq!(
        only.removed,
        [
            Handle::new(1, 1).expect("handle"),
            Handle::new(1, 0).expect("handle")
        ]
    );
    assert!(only.views.is_empty());
    assert!(only.after.views().is_empty());
}

#[test]
fn removing_a_meshes_last_owner_frees_exactly_its_allocations() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({})), cube("x", &json!({"pos":[4,0,0]}))]}),
    );
    let old = geometry(&first.after);
    let mesh = built(old, 1);
    let freed = mesh_bytes(mesh)
        + source_bytes(&old.solids[1].source)
        + size_of_val(&old.solids[1])
        + "x".len();
    let weak = Arc::downgrade(mesh);
    let full = old.full_bytes();
    let second = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","remove":["x"]}),
    );
    assert_eq!(geometry(&second.after).full_bytes(), full - freed);
    assert!(weak.upgrade().is_some(), "the predecessor still owns it");
    drop(first);
    assert!(weak.upgrade().is_none(), "no owner outlives the removal");
}

#[test]
fn removing_the_last_view_retains_no_map_allocation() {
    // The same 2D content with and without a view that came and went.
    let layouts = [true, false].map(|with_view| {
        let (mut scene, mut budget) = scene();
        let mut put = vec![text()];
        if with_view {
            put.extend([view(), cube("s", &json!({}))]);
        }
        let first = opening(&mut scene, &mut budget, json!({"id":"a","put":put}));
        let mut second = json!({"id":"b","set":[{"target":"t","color":"#ff0000"}]});
        if with_view {
            second["remove"] = json!(["s", "v"]);
        }
        next(&mut scene, &mut budget, &first.after, second).after
    });
    let [emptied, plain] = layouts;
    assert_eq!(emptied.resident_bytes(), plain.resident_bytes());
    let ((), _, [_, freed_emptied]) = allocation::usage(|| drop(emptied));
    let ((), _, [_, freed_plain]) = allocation::usage(|| drop(plain));
    assert_eq!(freed_emptied, freed_plain);
}

#[test]
fn prepared_carriers_keep_solid_text_out_of_debug() {
    let (mut scene, mut budget) = scene();
    let prepared = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("sentinel", &json!({"label":"SENTINEL","info":"SENTINEL"}))]}),
    );
    for debug in [
        format!("{:?}", prepared.views[0]),
        format!("{:?}", geometry(&prepared.after).solids[0].mesh),
    ] {
        for marker in ["sentinel", "SENTINEL"] {
            let bytes = format!("{:?}", marker.as_bytes());
            assert!(!debug.contains(marker), "{debug}");
            assert!(!debug.contains(bytes.trim_matches(['[', ']'])), "{debug}");
        }
    }
}

/// Each stored vertex placed by `rest` and narrowed as the world build narrows, as f32 bits.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "narrows exactly as the world build does"
)]
fn narrowed(rest: &Transform, mesh: &SolidMesh) -> Vec<[[u32; 3]; 3]> {
    mesh.triangles
        .iter()
        .map(|triangle| {
            triangle.map(|i| {
                let p = mesh.vertices[usize::try_from(i).expect("index")];
                rest.apply(p).map(|c| {
                    let v = c as f32;
                    if v == 0.0 { 0 } else { v.to_bits() }
                })
            })
        })
        .collect()
}
fn bits(world: &[Triangle]) -> Vec<[[u32; 3]; 3]> {
    world
        .iter()
        .map(|triangle| {
            triangle
                .v
                .map(|p| [p.x, p.y, p.z].map(|v| if v == 0.0 { 0 } else { v.to_bits() }))
        })
        .collect()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture table beside its single check"
)]
fn prepared_rest_vertices_narrow_to_the_world_build_bit_for_bit() {
    let child = |shape: &str, fields: Value| {
        let mut solid = json!({"in":"v","id":"s","shape":shape,"attach":"p","pos":[1,0,0],"rot":[30,0,0],"scale":1.5});
        solid
            .as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        json!({ "solid": solid })
    };
    let alone = |fields: Value| {
        let mut solid = json!({"in":"v","id":"s"});
        solid
            .as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        json!({ "solid": solid })
    };
    // (solid, triangles, distinct vertices), counted from each shape's topology.
    let cases = [
        (child("box", json!({"size":[1,2,3]})), 12, 8),
        (child("sphere", json!({"r":1,"segments":12})), 320, 162),
        (
            child("cylinder", json!({"r":1,"h":2,"segments":12})),
            44,
            24,
        ),
        (child("cone", json!({"r":1,"h":2,"segments":12})), 22, 13),
        (
            child("torus", json!({"r":2,"r2":0.5,"segments":12})),
            288,
            144,
        ),
        (
            child("lathe", json!({"points":[[0,-1],[1,0],[0,1]],"segments":8})),
            16,
            10,
        ),
        (
            child(
                "extrude",
                json!({"points":[[0,0],[1,0],[1,1],[0,1]],"depth":1}),
            ),
            12,
            8,
        ),
        (
            child(
                "tube",
                json!({"path":[[0,0,0],[0,0,1]],"r2":0.25,"segments":4}),
            ),
            12,
            8,
        ),
        (
            alone(
                json!({"shape":"tube","path":[[9000,0,0],[9000,1,0]],"r2":0.001,"segments":4,"pos":[-9000,0,0]}),
            ),
            12,
            8,
        ),
        (
            alone(
                json!({"shape":"tube","path":[[6000,6000,0],[6000,6000,1]],"r2":0.001,"segments":24,"pos":[-6000,-6000,0]}),
            ),
            92,
            48,
        ),
        (
            alone(
                json!({"shape":"extrude","points":[[-6000,-5999],[1.001,2.001],[0,1]],"depth":1}),
            ),
            8,
            6,
        ),
        (
            alone(
                json!({"shape":"extrude","points":[[-8500,1],[9000,0],[9000.001,1],[9000,2],[8999,0.5],[-9500,2],[-9500,0]],"depth":1}),
            ),
            24,
            14,
        ),
    ];
    let mut scratch = MeshScratch::new();
    for (solid, count, distinct) in cases {
        let (mut scene, mut budget) = scene();
        let prepared = opening(
            &mut scene,
            &mut budget,
            json!({"id":"a","put":[view(), cube("p", &json!({"pos":[10,0,0],"scale":2})), solid]}),
        );
        let geometry = geometry(&prepared.after);
        assert!(!geometry.failed, "{solid}");
        let index = geometry
            .solids
            .iter()
            .position(|solid| solid.id == id("s"))
            .expect("the solid");
        let prepared_solid = &geometry.solids[index];
        let mesh = built(geometry, index);
        assert_eq!(
            (mesh.triangles.len(), mesh.vertices.len()),
            (count, distinct),
            "{solid}"
        );
        let Op::Solid(record) = &*prepared_solid.source else {
            panic!("a solid record");
        };
        let mut world = Vec::with_capacity(
            usize::try_from(triangles(record).expect("valid count")).expect("bounded"),
        );
        let written = build(
            record,
            &prepared_solid.rest,
            0,
            &mut world,
            &AtomicBool::new(false),
            &mut scratch,
        )
        .expect("the world build accepts it")
        .written;
        assert_eq!(usize::try_from(written).expect("bounded"), count);
        assert_eq!(
            narrowed(&prepared_solid.rest, mesh),
            bits(&world),
            "{solid}"
        );
    }
}

#[test]
fn a_set_colour_survives_a_later_rebuild_of_its_view() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({}))]}),
    );
    let red = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","color":"#ff0000"}]}),
    );
    let grown = next(
        &mut scene,
        &mut budget,
        &red.after,
        json!({"id":"c","put":[cube("u", &json!({"pos":[4,0,0],"color":"#00ff00"}))]}),
    );
    let view = geometry(&grown.after);
    assert_eq!(
        view.solids
            .iter()
            .map(|solid| solid.rgb)
            .collect::<Vec<_>>(),
        [[0xff, 0, 0], [0, 0xff, 0]]
    );
    assert!(Arc::ptr_eq(
        built(view, 0),
        built(geometry(&first.after), 0)
    ));
}

#[test]
fn each_prepared_mesh_matches_its_world_build_at_the_f32_boundary() {
    // CAPS-194's chain: the extrusion's caps are collinear in unrounded f64 but not once narrowed,
    // as the world build checks them, so an f64 veto failed the whole view.
    let solid = |id: &str, fields: Value| {
        let mut solid = json!({"in":"v","id":id});
        solid
            .as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        json!({ "solid": solid })
    };
    let boxed = |id: &str, attach: Option<&str>, pos: Value, scale: f64| {
        let mut fields = json!({"shape":"box","size":[1,1,1],"pos":pos,"scale":scale});
        if let Some(parent) = attach {
            fields["attach"] = json!(parent);
        }
        solid(id, fields)
    };
    let (mut scene, mut budget) = scene();
    let prepared = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[
            view(),
            boxed("p0", None, json!([10, 10.000_000_953_674_316, 0]), 0.5),
            boxed("p1", Some("p0"), json!([0.000_000_953_674_316_406_25, 0.000_000_953_674_316_406_25, 0]), 0.001),
            boxed("p2", Some("p1"), json!([0, 0, 0]), 0.001),
            boxed("p3", Some("p2"), json!([0, 0, 0]), 0.001),
            boxed("p4", Some("p3"), json!([0, 0, 0]), 0.001),
            solid("s", json!({"shape":"extrude","attach":"p4","scale":0.001,"points":[[-4,-4],[0,0.001],[4,4]],"depth":1})),
        ]}),
    );
    let geometry = geometry(&prepared.after);
    let mut scratch = MeshScratch::new();
    let outcomes: Vec<Outcome> = geometry
        .solids
        .iter()
        .map(|prepared| {
            let Op::Solid(record) = &*prepared.source else {
                panic!("a solid record");
            };
            let mut world = Vec::with_capacity(
                usize::try_from(triangles(record).expect("valid count")).expect("bounded"),
            );
            let built = build(
                record,
                &prepared.rest,
                0,
                &mut world,
                &AtomicBool::new(false),
                &mut scratch,
            )
            .map(|built| usize::try_from(built.written).expect("bounded"));
            let mesh = match &prepared.mesh {
                Mesh::Built(mesh) => Ok(mesh.triangles.len()),
                Mesh::Failed(error) => Err(*error),
            };
            (prepared.id.as_str().to_owned(), built, mesh)
        })
        .collect();
    let expected = [
        ("p0", Ok(12)),
        ("p1", Ok(12)),
        ("p2", Ok(12)),
        ("p3", Ok(12)),
        ("p4", Ok(12)),
        ("s", Ok(8)),
    ];
    for ((id, built, mesh), (want_id, want)) in outcomes.iter().zip(expected) {
        assert_eq!(id, want_id);
        assert_eq!(*built, want, "{id}: world build");
        assert_eq!(*mesh, want, "{id}: prepared mesh");
    }
    assert_eq!(outcomes.len(), 6);
    assert!(!geometry.failed);
}

type Outcome = (String, Result<usize, MeshError>, Result<usize, MeshError>);

fn rgbs(layout: &LayoutState) -> Vec<(String, [u8; 3])> {
    geometry(layout)
        .solids
        .iter()
        .map(|solid| (solid.id.as_str().to_owned(), solid.rgb))
        .collect()
}

fn named(pairs: &[(&str, [u8; 3])]) -> Vec<(String, [u8; 3])> {
    pairs
        .iter()
        .map(|(id, rgb)| ((*id).to_owned(), *rgb))
        .collect()
}

#[test]
fn a_colour_survives_retire_stop_recovery_removal_and_rebuild() {
    let (grey, dark) = ([0x88, 0x88, 0x88], [0x44, 0x44, 0x44]);
    let (red, blue, green) = ([0xff, 0, 0], [0, 0, 0xff], [0, 0xff, 0]);
    let u = |color: &str| cube("u", &json!({"pos":[4,0,0],"color":color}));
    // Measured at c1ec74c: a scene holding only the view charges 738 bytes, and each cube adds
    // 1,083: its mesh (520) and source (298) by the allocator oracle, then its object, slot and id.
    // 738 + 1,083: the view and one cube.
    let one_cube = 1_821;
    // 738 + 2 * 1,083: the view and two cubes; colour is inline, so a recolour keeps it.
    let two_cubes = 2_904;
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({"color":"#888888"})), u("#444444")]}),
    );
    assert_eq!(rgbs(&first.after), named(&[("s", grey), ("u", dark)]));
    assert_eq!(first.after.resident_bytes(), two_cubes);
    let shaded = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","color":"#ff0000"}]}),
    );
    scene.retire(1, &mut budget).expect("retire the recolour");
    drop(first);
    let reds = named(&[("s", red), ("u", dark)]);
    assert_eq!(rgbs(&shaded.after), reds);
    assert_eq!(shaded.after.resident_bytes(), two_cubes);

    let blued = next(
        &mut scene,
        &mut budget,
        &shaded.after,
        json!({"id":"c","set":[{"target":"s","color":"#0000ff"}]}),
    );
    assert_eq!(rgbs(&blued.after), named(&[("s", blue), ("u", dark)]));
    assert_eq!(blued.after.resident_bytes(), two_cubes);
    let held = geometry(&shaded.after);
    let revealed = vec![held.handle, held.solids[0].handle, held.solids[1].handle];
    scene
        .stop(
            &StopCut {
                activated: 2,
                revealed: Bounded::new(revealed).expect("bounded cut"),
            },
            &mut budget,
        )
        .expect("valid cut");
    drop(blued);
    let tip = shaded.after.cut(&scene).expect("cut");
    assert_eq!(rgbs(&tip), reds, "the stopped blue never activated");
    assert_eq!(tip.resident_bytes(), two_cubes);

    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
    )
    .expect("recovery");
    assert_eq!(rgbs(&recovered), reds);
    assert_eq!(recovered.resident_bytes(), two_cubes);

    let removed = next(
        &mut scene,
        &mut budget,
        &tip,
        json!({"id":"d","remove":["u"]}),
    );
    assert_eq!(rgbs(&removed.after), named(&[("s", red)]));
    assert_eq!(removed.after.resident_bytes(), one_cube);

    let rebuilt = next(
        &mut scene,
        &mut budget,
        &removed.after,
        json!({"id":"e","set":[{"target":"s","color":"#00ff00"}],"put":[cube("w", &json!({"pos":[8,0,0],"color":"#444444"}))]}),
    );
    assert_eq!(rgbs(&rebuilt.after), named(&[("s", green), ("w", dark)]));
    assert_eq!(rebuilt.after.resident_bytes(), two_cubes);
}
