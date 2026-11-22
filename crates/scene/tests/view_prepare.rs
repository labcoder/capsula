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
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};

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
fn reshading_shares_sources_and_meshes_and_text_sets_prepare_no_view() {
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({"color":"#224466"})), cube("u", &json!({"pos":[4,0,0]}))]}),
    );
    let quiet = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","label":"lid"},{"target":"u","info":"opens"}]}),
    );
    assert!(quiet.views.is_empty(), "label and info sets emit no view");
    let shaded = next(
        &mut scene,
        &mut budget,
        &quiet.after,
        json!({"id":"c","set":[{"target":"s","color":"#ff0000"}]}),
    );
    assert_eq!(shaded.views.len(), 1);
    let (old, new) = (geometry(&first.after), geometry(&shaded.after));
    let color = |geometry: &ViewGeometry, name| {
        geometry
            .solids
            .iter()
            .find(|solid| solid.id == id(name))
            .expect("prepared solid")
            .rgb
    };
    assert_eq!(color(new, "s"), [0xff, 0, 0]);
    assert_eq!(color(old, "s"), [0x22, 0x44, 0x66]);
    assert_eq!(color(new, "u"), color(old, "u"));
    let reused: usize = (0..2)
        .map(|index| mesh_bytes(built(old, index)) + source_bytes(&old.solids[index].source))
        .sum::<usize>()
        + source_bytes(&old.source);
    assert_eq!(new.marginal_bytes(Some(old)), new.full_bytes() - reused);
    let grown = next(
        &mut scene,
        &mut budget,
        &shaded.after,
        json!({"id":"d","put":[cube("w", &json!({"pos":[8,0,0],"color":"#00ff00"}))]}),
    );
    let rebuilt = geometry(&grown.after);
    assert_eq!(color(rebuilt, "s"), [0xff, 0, 0]);
    assert_eq!(color(rebuilt, "w"), [0, 0xff, 0]);
    assert!(
        rebuilt.marginal_bytes(Some(new)) <= rebuilt.full_bytes() - mesh_bytes(built(new, 0)),
        "the successor does not charge a second copy of the unchanged mesh"
    );
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
    assert_eq!(
        paints(&second)
            .iter()
            .map(|paint| paint.rgb.get())
            .collect::<Vec<_>>(),
        [0x0000_00ff]
    );
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
        None,
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
#[test]
fn every_face_group_lies_on_its_anchor_plane() {
    for (path, r2, segments, triangles, straight) in [
        (
            json!([[0, 0, 0], [0, 1, 0], [1, 2, 0], [1, 3, 1]]),
            0.1,
            8,
            60,
            false,
        ),
        (json!([[0, 0, 0], [0, 0, 2], [2, 0, 2]]), 0.25, 4, 20, false),
        (json!([[0, 0, 0], [0, 0, 1], [0, 0, 3]]), 0.25, 4, 20, true),
    ] {
        let (mut scene, mut budget) = scene();
        let prepared = opening(
            &mut scene,
            &mut budget,
            json!({"id":"a","put":[view(), tube(&path, r2, segments, &json!([0, 0, 0]))]}),
        );
        let mesh = built(geometry(&prepared.after), 0);
        assert_eq!(mesh.triangles.len(), triangles, "{path}");
        let mut seen = vec![false; triangles];
        let at = |i: u32| mesh.vertices[usize::try_from(i).expect("index")];
        let z_span = |triangle: [u32; 3]| {
            triangle
                .map(|i| at(i)[2])
                .into_iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), z| {
                    (low.min(z), high.max(z))
                })
        };
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
            let group = &mesh.triangles[start..end];
            for (index, triangle) in (start..end).zip(group) {
                assert!(!seen[index], "{path}: overlapping face groups");
                seen[index] = true;
                for &i in triangle {
                    let d = offset(at(i));
                    let off = (d[0] * normal[0] + d[1] * normal[1] + d[2] * normal[2]) / length;
                    assert!(
                        off.abs() <= 1e-5,
                        "{path}: a group vertex is {off} off its plane"
                    );
                }
                if straight {
                    let (low, high) = z_span(*triangle);
                    if high - low > 0.5 {
                        let span = (low.to_bits(), high.to_bits());
                        assert!(
                            group
                                .iter()
                                .filter(|&&other| {
                                    let (low, high) = z_span(other);
                                    (low.to_bits(), high.to_bits()) == span
                                })
                                .count()
                                >= 2,
                            "both triangles across each straight side band share an anchor"
                        );
                    }
                }
            }
        }
        assert!(
            seen.into_iter().all(|covered| covered),
            "{path}: a triangle has no face group"
        );
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
    let mut removed = only.removed;
    removed.sort_unstable();
    assert_eq!(
        removed,
        [
            Handle::new(1, 0).expect("handle"),
            Handle::new(1, 1).expect("handle")
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
    let debug = |id, label, info| {
        let (mut scene, mut budget) = scene();
        let prepared = opening(
            &mut scene,
            &mut budget,
            json!({"id":"a","put":[view(), cube(id, &json!({"label":label,"info":info}))]}),
        );
        [
            format!("{:?}", prepared.views[0]),
            format!("{:?}", geometry(&prepared.after).solids[0].mesh),
        ]
    };
    let observed = debug("sentinel", "SENTINEL", "SENTINEL");
    assert!(
        observed == debug("altmodel", "NEWLABEL", "NEWTITLE"),
        "solid text affected prepared Debug"
    );
    for debug in observed {
        for marker in ["sentinel", "SENTINEL"] {
            let bytes = format!("{:?}", marker.as_bytes());
            assert!(
                !debug.contains(marker),
                "prepared Debug contains solid text"
            );
            assert!(
                !debug.contains(bytes.trim_matches(['[', ']'])),
                "prepared Debug contains text bytes"
            );
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
    // Triangle counts are determined from each shape's topology.
    let cases = [
        (child("box", json!({"size":[1,2,3]})), 12),
        (child("sphere", json!({"r":1,"segments":12})), 320),
        (child("cylinder", json!({"r":1,"h":2,"segments":12})), 44),
        (child("cone", json!({"r":1,"h":2,"segments":12})), 22),
        (child("torus", json!({"r":2,"r2":0.5,"segments":12})), 288),
        (
            child("lathe", json!({"points":[[0,-1],[1,0],[0,1]],"segments":8})),
            16,
        ),
        (
            child(
                "extrude",
                json!({"points":[[0,0],[1,0],[1,1],[0,1]],"depth":1}),
            ),
            12,
        ),
        (
            child(
                "tube",
                json!({"path":[[0,0,0],[0,0,1]],"r2":0.25,"segments":4}),
            ),
            12,
        ),
        (
            alone(
                json!({"shape":"extrude","points":[[-6000,-5999],[1.001,2.001],[0,1]],"depth":1}),
            ),
            8,
        ),
        (
            alone(
                json!({"shape":"extrude","points":[[-8500,1],[9000,0],[9000.001,1],[9000,2],[8999,0.5],[-9500,2],[-9500,0]],"depth":1}),
            ),
            24,
        ),
    ];
    let canonical = |mut triangles: Vec<[[u32; 3]; 3]>| {
        for vertices in &mut triangles {
            let [a, b, c] = *vertices;
            *vertices = [*vertices, [b, c, a], [c, a, b]]
                .into_iter()
                .min()
                .expect("three rotations");
        }
        triangles.sort_unstable();
        triangles
    };
    let mut scratch = MeshScratch::new();
    let mut check = |solid: Value, count| {
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
        assert_eq!(mesh.triangles.len(), count, "{solid}");
        for (i, vertex) in mesh.vertices.iter().enumerate() {
            assert!(
                !mesh.vertices[..i].contains(vertex),
                "{solid}: duplicate local vertex {vertex:?}"
            );
        }
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
            canonical(narrowed(&prepared_solid.rest, mesh)),
            canonical(bits(&world)),
            "{solid}"
        );
        (prepared, index)
    };
    for (solid, count) in cases {
        check(solid, count);
    }
    let (scaled, index) = check(
        cube("s", &json!({"attach":"p","pos":[1,0,0],"scale":1.5})),
        12,
    );
    assert_eq!(scaled.views.len(), 1);
    let geo = geometry(&scaled.after);
    let child = &geo.solids[index];
    let parent = &geo.solids[usize::from(child.parent.expect("attached child"))];
    assert_eq!(parent.id, id("p"));
    let corner = built(geo, index)
        .vertices
        .iter()
        .map(|&vertex| child.rest.apply(vertex))
        .fold([f64::NEG_INFINITY; 3], |max, p| {
            std::array::from_fn(|axis| max[axis].max(p[axis]))
        });
    assert_eq!(
        corner.map(f64::to_bits),
        [15.0_f64, 3.0, 3.0].map(f64::to_bits)
    );
    for (path, segments, count, pos, axis) in [
        (
            json!([[9000, 0, 0], [9000, 1, 0]]),
            4_u8,
            12,
            json!([-9000, 0, 0]),
            [0, 2],
        ),
        (
            json!([[6000, 6000, 0], [6000, 6000, 1]]),
            24,
            92,
            json!([-6000, -6000, 0]),
            [0, 1],
        ),
    ] {
        let (prepared, index) = check(tube(&path, 0.001, segments, &pos), count);
        let geo = geometry(&prepared.after);
        let mesh = built(geo, index);
        for &i in mesh.triangles.iter().flatten() {
            let p = geo.solids[index]
                .rest
                .apply(mesh.vertices[usize::try_from(i).expect("index")]);
            let distance = p[axis[0]].hypot(p[axis[1]]);
            assert!(
                (distance - 0.001).abs() <= 1e-7,
                "{path}: radius {distance}"
            );
        }
    }
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
    let outcomes: BTreeMap<_, _> = geometry
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
            (prepared.id.as_str(), (built, mesh))
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
    assert_eq!(geometry.solids.len(), expected.len());
    assert_eq!(outcomes.len(), expected.len());
    for (name, want) in expected {
        assert_eq!(
            outcomes[name],
            (want, want),
            "{name}: world build and prepared mesh"
        );
    }
    assert!(!geometry.failed);
}

fn rgbs(layout: &LayoutState) -> BTreeMap<&str, [u8; 3]> {
    geometry(layout)
        .solids
        .iter()
        .map(|solid| (solid.id.as_str(), solid.rgb))
        .collect()
}

#[test]
fn a_colour_survives_retire_stop_recovery_removal_and_rebuild() {
    let (grey, dark) = ([0x88, 0x88, 0x88], [0x44, 0x44, 0x44]);
    let (red, blue, green) = ([0xff, 0, 0], [0, 0, 0xff], [0, 0xff, 0]);
    let u = |color: &str| cube("u", &json!({"pos":[4,0,0],"color":color}));
    let s = |color: &str| cube("s", &json!({"color":color}));
    let [one_cube, two_cubes] = [
        vec![view(), s("#ff0000")],
        vec![view(), s("#ff0000"), u("#444444")],
    ]
    .map(|put| {
        let (mut scene, mut budget) = scene();
        opening(&mut scene, &mut budget, json!({"id":"a","put":put}))
            .after
            .resident_bytes()
    });
    assert!(two_cubes > one_cube);
    let (mut scene, mut budget) = scene();
    let first = opening(
        &mut scene,
        &mut budget,
        json!({"id":"a","put":[view(), cube("s", &json!({"color":"#888888"})), u("#444444")]}),
    );
    assert_eq!(
        rgbs(&first.after),
        BTreeMap::from([("s", grey), ("u", dark)])
    );
    assert_eq!(first.after.resident_bytes(), two_cubes);
    let shaded = next(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"b","set":[{"target":"s","color":"#ff0000"}]}),
    );
    scene.retire(1, &mut budget).expect("retire the recolour");
    drop(first);
    let reds = BTreeMap::from([("s", red), ("u", dark)]);
    assert_eq!(rgbs(&shaded.after), reds);
    assert_eq!(shaded.after.resident_bytes(), two_cubes);

    let blued = next(
        &mut scene,
        &mut budget,
        &shaded.after,
        json!({"id":"c","set":[{"target":"s","color":"#0000ff"}]}),
    );
    assert_eq!(
        rgbs(&blued.after),
        BTreeMap::from([("s", blue), ("u", dark)])
    );
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
        None,
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
    assert_eq!(rgbs(&removed.after), BTreeMap::from([("s", red)]));
    assert_eq!(removed.after.resident_bytes(), one_cube);

    let rebuilt = next(
        &mut scene,
        &mut budget,
        &removed.after,
        json!({"id":"e","set":[{"target":"s","color":"#00ff00"}],"put":[cube("w", &json!({"pos":[8,0,0],"color":"#444444"}))]}),
    );
    assert_eq!(
        rgbs(&rebuilt.after),
        BTreeMap::from([("s", green), ("w", dark)])
    );
    assert_eq!(rebuilt.after.resident_bytes(), two_cubes);
}
