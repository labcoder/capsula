//! 3D preparation: canonical meshes, sharing, routing, failure, cut, recovery and charges.
use kumochi_scene::{
    Admission, Begin, Committed, GlobalBudget, Record, SceneState, Step, StopCut, WireId,
    display::{MeasureError, Prim, TextMeasure, TextStyle},
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover,
    },
    parse_record,
    text::Bounded,
    theme::Theme,
    three::{
        size::MeshError,
        view::{Mesh, SolidMesh, ViewGeometry},
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

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
    let Ok(Record::Step(step)) = parse_record(&serde_json::to_vec(&envelope).expect("JSON")) else {
        panic!("step fixture");
    };
    step
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
    cancel: bool,
) -> Result<PreparedStep, PrepareError> {
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .map_err(|(error, _)| error)?;
    let result = prepare(input, &mut Measure, &AtomicBool::new(cancel))?;
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
    picture(scene, commit, before, false).expect("prepared step")
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
fn mesh_bytes(mesh: &SolidMesh) -> usize {
    size_of_val(&*mesh.vertices) + size_of_val(&*mesh.triangles) + size_of_val(&*mesh.faces)
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
    let mut corner = [f32::NEG_INFINITY; 3];
    for &v in &*mesh.vertices {
        let p = child.rest.point(v).expect("finite rest");
        corner = [corner[0].max(p.x), corner[1].max(p.y), corner[2].max(p.z)];
    }
    assert_eq!(
        corner,
        [15.0, 3.0, 3.0],
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
    let shared: usize = (0..2).map(|index| mesh_bytes(built(old, index))).sum();
    assert!(new.marginal_bytes(Some(old)) + shared <= new.full_bytes());
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
    let mesh = mesh_bytes(built(reshaded, 0));
    assert_eq!(reshaded.full_bytes(), scratch.full_bytes());
    assert!(reshaded.marginal_bytes(Some(geometry(&first.after))) + mesh <= reshaded.full_bytes());
    assert!(shaded.resident_bytes < fresh.resident_bytes);
    assert!(shaded.after.resident_bytes() >= reshaded.full_bytes());
    assert_eq!(shaded.after.resident_bytes(), fresh.after.resident_bytes());
    assert!(fresh.resident_bytes >= scratch.full_bytes());
    assert!(fresh.scratch_bytes > 0);
    assert_eq!(shaded.scratch_bytes, fresh.scratch_bytes);
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
        picture(&scene, commit, &before, true),
        Err(PrepareError::Cancelled)
    ));
    assert!(before.views().is_empty());
}
