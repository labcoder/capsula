//! Incremental placement, preparation isolation and ordered-cut recovery.
use capsula_scene::{
    Admission, Begin, GlobalBudget, Record, SceneState, Step, StopCut, WireId,
    display::{MeasureError, Prim, TextMeasure, TextStyle},
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover,
    },
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Default)]
struct Measure {
    calls: Vec<String>,
}
impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        self.calls.push(text.to_owned());
        if text == "fail-measure" {
            return Err(MeasureError);
        }
        let width =
            f32::from(u16::try_from(text.chars().count()).expect("bounded fixture text")) * 8.0;
        let limit = max_width.unwrap_or(width.max(1.0));
        Ok([width.min(limit), 10.0 * (width / limit).ceil().max(1.0)])
    }
}
fn header() -> Begin {
    let Record::Begin(header) =
        parse_record(br#"{"begin":{"v":1,"scene":"layout","surface":"inline","alt":""}}"#)
            .expect("header")
    else {
        panic!("begin");
    };
    header
}
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}
fn step(value: Value) -> Step {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&envelope).expect("fixture JSON")).expect("typed fixture")
    else {
        panic!("step");
    };
    step
}
fn committed(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    value: Value,
) -> capsula_scene::Committed {
    let Admission::New(candidate) = scene.admit(&step(value), budget).expect("admitted fixture")
    else {
        panic!("new step");
    };
    scene.commit(candidate, budget).expect("fresh candidate")
}
fn picture(
    scene: &SceneState,
    committed: capsula_scene::Committed,
    before: &LayoutState,
    measure: &mut Measure,
) -> Result<PreparedStep, PrepareError> {
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )?;
    let result = prepare(input, measure, &AtomicBool::new(false))?;
    result.validate(scene, before)?;
    Ok(result)
}
fn cut(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    revision: u32,
    ids: &[&str],
    layout: &LayoutState,
) {
    let revealed = ids
        .iter()
        .map(|id_value| layout.handle(&id(id_value)).expect("activated handle"))
        .collect();
    scene
        .stop(
            &StopCut {
                activated: revision,
                revealed: Bounded::new(revealed).expect("bounded cut"),
            },
            budget,
        )
        .expect("valid cut");
}

#[test]
fn future_nodes_and_removals_leave_the_previous_picture_unchanged() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"diagram":{"id":"d","box":[0,0,600,300]}},{"node":{"id":"a","in":"d","label":"A"}}]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first picture");
    let unchanged = first.after.items();
    let bounds = first.after.bounds(&id("a")).expect("safe").expect("a");
    let second = committed(
        &mut scene,
        &mut budget,
        json!({"id":"second","put":[{"node":{"id":"b","in":"d","label":"B"}},{"edge":{"id":"e","in":"d","from":"a","to":"b"}}]}),
    );
    assert_eq!(first.after.items(), unchanged);
    let second = picture(&scene, second, &first.after, &mut measure).expect("second picture");
    assert_eq!(first.after.items(), unchanged);
    assert_eq!(second.after.bounds(&id("a")).expect("safe"), Some(bounds));
    assert!(
        !bounds.intersects(second.after.bounds(&id("b")).expect("safe").expect("b")),
        "new rank does not overlap the old one"
    );
    let removal = committed(
        &mut scene,
        &mut budget,
        json!({"id":"third","remove":["e","b"]}),
    );
    let third = picture(&scene, removal, &second.after, &mut measure).expect("removal picture");
    assert!(
        second.after.handle(&id("b")).is_some(),
        "earlier output still has b"
    );
    assert_eq!(third.after.handle(&id("b")), None);
    assert_eq!(third.removed.len(), 2);
}

#[test]
fn a_full_rank_emits_one_group_glide_and_preserves_node_local_rectangles() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,260,300],"dir":"down"}},
            {"node":{"id":"root","in":"d","label":"Root"}},
            {"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},
            {"edge":{"id":"ra","in":"d","from":"root","to":"a"}},{"edge":{"id":"rb","in":"d","from":"root","to":"b"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first picture");
    assert!(first.glides.is_empty(), "initial batch fits");
    let original = first
        .after
        .items()
        .into_iter()
        .find(|item| Some(item.handle) == first.after.handle(&id("a")))
        .expect("a item");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"node":{"id":"c","in":"d","label":"C"}},{"edge":{"id":"rc","in":"d","from":"root","to":"c"}}]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("overflow picture");
    assert_eq!(next.glides.len(), 1);
    assert_eq!(Some(next.glides[0].target), next.after.handle(&id("d")));
    let current = next
        .after
        .items()
        .into_iter()
        .find(|item| item.handle == original.handle)
        .expect("kept a");
    assert_eq!(current.bounds, original.bounds);
    assert!(
        Arc::ptr_eq(&current.primitives, &original.primitives),
        "glide reuses geometry"
    );
}

#[test]
fn path_paint_changes_reuse_geometry_without_requesting_new_sizing_facts() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"path":{"id":"p","d":"M0 0 Q4 8 8 0","fill":"green"}}]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("path");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"recolor","set":[{"target":"p","color":"red"}]}),
    );
    let next =
        picture(&scene, next, &first.after, &mut measure).expect("repaint existing geometry");
    let Prim::Line { points: old, .. } = &first.display.items[0].primitives[1] else {
        panic!("original stroke");
    };
    let Prim::Line {
        points: new, color, ..
    } = &next.display.items[0].primitives[1]
    else {
        panic!("updated stroke");
    };
    assert!(Arc::ptr_eq(old, new));
    assert_eq!(color.rgb.get(), 0x00ff_6b7d);
    let Prim::Fill { color, .. } = &next.display.items[0].primitives[0] else {
        panic!("unchanged fill");
    };
    assert_eq!(color.rgb.get(), 0x005f_d39a);
}

#[test]
fn commit_tessellation_facts_do_not_read_later_tracks_and_rebuild_changed_paths() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"diagram":{"id":"d"}},{"path":{"id":"p","in":"d","d":"M0 0 Q0 4 4 4"}}]}),
    );
    let second = committed(
        &mut scene,
        &mut budget,
        json!({"id":"second","put":[{"anim":{"target":"d","track":"scale","keys":[[0,4]]}}]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("old ticket");
    let points = |picture: &PreparedStep| {
        picture
            .display
            .items
            .iter()
            .flat_map(|item| item.primitives.iter())
            .filter_map(|primitive| match primitive {
                Prim::Line { points, .. } => Some(points.len()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(points(&first), vec![5]);
    let second =
        picture(&scene, second, &first.after, &mut measure).expect("new path tessellation");
    assert_eq!(points(&second), vec![9]);
    assert_eq!(points(&first), vec![5]);
}

#[test]
fn recovery_skips_failed_dropped_preparations_and_restores_removed_targets() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"x","at":[100,100],"text":"X"}}]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("activated picture");
    scene.retire(0, &mut budget).expect("shown");
    let failed = committed(
        &mut scene,
        &mut budget,
        json!({"id":"failed","remove":["x"],"put":[{"text":{"id":"bad","at":[100,100],"text":"fail-measure"}}]}),
    );
    assert!(matches!(
        picture(&scene, failed, &first.after, &mut measure),
        Err(PrepareError::TextMeasure)
    ));
    cut(&mut scene, &mut budget, 1, &["x"], &first.after);
    let tip = first
        .after
        .cut(&scene)
        .expect("cut retains activated state");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"callout":{"id":"c","target":{"ref":"x"},"text":"ok"}}]}),
    );
    let live = picture(&scene, next, &tip, &mut measure).expect("later call");
    let mut replay_measure = Measure::default();
    let (restored, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut replay_measure,
        &AtomicBool::new(false),
    )
    .expect("failed step is not prepared again");
    assert_eq!(restored.usage(), scene.usage());
    assert_eq!(recovered.items(), live.after.items());
    assert!(
        !replay_measure
            .calls
            .iter()
            .any(|text| text == "fail-measure"),
        "dropped failure is skipped"
    );
}

#[test]
fn detached_fast_nodes_absorb_the_ancestors_resting_transform() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,240,200],"enter":"fade","dur":1000}},
            {"node":{"id":"a","in":"d","label":"A","enter":"fade","dur":10}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[40,20]]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("group picture");
    let expected = first.after.bounds(&id("a")).expect("bounds");
    cut(&mut scene, &mut budget, 1, &["a"], &first.after);
    let live = first.after.cut(&scene).expect("detach node");
    assert_eq!(live.bounds(&id("a")).expect("frozen bounds"), expected);
    assert_eq!(live.items()[0].parent, None);
    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
    )
    .expect("detached replay");
    assert_eq!(recovered.items(), live.items());
    assert_eq!(
        recovered.bounds(&id("a")).expect("recovered bounds"),
        expected
    );
}

#[test]
fn a_cut_invalidates_kept_results_and_their_old_layout_base() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"x","at":[100,100],"text":"X"}},{"text":{"id":"y","at":[200,100],"text":"Y"}}]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first picture");
    cut(&mut scene, &mut budget, 1, &["x"], &first.after);
    assert!(
        scene.step(0).is_some(),
        "step stays kept while one object is dropped"
    );
    assert!(matches!(
        first.validate(&scene, &before),
        Err(PrepareError::Stale)
    ));
    let next = committed(&mut scene, &mut budget, json!({"id":"next","put":[]}));
    assert!(matches!(
        PrepareInput::new(
            next.preparation,
            first.after,
            SurfaceGeometry::default(),
            Arc::new(Theme::default())
        ),
        Err(PrepareError::Stale)
    ));
}

#[test]
fn kept_recovery_failures_report_the_revision_and_layout_checks_full_envelopes() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"x","at":[100,100],"text":"X"}}]}),
    );
    committed(
        &mut scene,
        &mut budget,
        json!({"id":"failed","put":[{"text":{"id":"bad","at":[100,100],"text":"fail-measure"}}]}),
    );
    let error = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
    )
    .expect_err("kept failure must be reported");
    assert_eq!(error.revision, 2);
    assert!(matches!(error.error, PrepareError::TextMeasure));
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let scaled = committed(
        &mut scene,
        &mut budget,
        json!({"id":"scaled","put":[{"text":{"id":"t","at":[0,0],"text":"x".repeat(100)}},{"anim":{"target":"t","track":"scale","keys":[[0,64],[100,1]]}}]}),
    );
    let error = picture(&scene, scaled, &before, &mut Measure::default())
        .expect_err("intermediate scale exceeds measured cap even though resting scale is one");
    assert!(
        matches!(error,PrepareError::Layout(error) if error.code==capsula_scene::ErrorCode::OutOfRange)
    );
}

#[test]
fn node_measurement_order_and_placement_are_deterministic_and_cancellation_is_atomic() {
    let run = || {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let first = committed(
            &mut scene,
            &mut budget,
            json!({"id":"first","put":[{"diagram":{"id":"d"}},{"node":{"id":"z","in":"d","label":"Z"}},{"node":{"id":"a","in":"d","label":"A"}}]}),
        );
        let picture = picture(&scene, first, &before, &mut measure).expect("deterministic picture");
        (picture.after.items(), measure.calls)
    };
    let first = run();
    assert_eq!(first.1, vec!["Z", "A"]);
    assert_eq!(first, run());
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let next = committed(&mut scene, &mut budget, json!({"id":"cancelled","put":[]}));
    let input = PrepareInput::new(
        next.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("input");
    assert!(matches!(
        prepare(input, &mut Measure::default(), &AtomicBool::new(true)),
        Err(PrepareError::Cancelled)
    ));
    assert_eq!(before.revision(), 0);
    assert!(
        before.items().is_empty(),
        "cancelled construction changes no prior state"
    );
}

#[test]
fn explicit_position_precedes_a_glide_and_later_glides_use_its_resting_value() {
    let run = |extra: Vec<Value>| {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let first = committed(
            &mut scene,
            &mut budget,
            json!({"id":"first","put":[
                {"diagram":{"id":"d","box":[0,0,360,300],"dir":"down"}},
                {"node":{"id":"root","in":"d","label":"Root"}},{"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},
                {"edge":{"id":"ra","in":"d","from":"root","to":"a"}},{"edge":{"id":"rb","in":"d","from":"root","to":"b"}},
                {"anim":{"target":"d","track":"pos","keys":[[0,[10,0]]]}}
            ]}),
        );
        let first = picture(&scene, first, &before, &mut measure).expect("first picture");
        let mut ops = vec![
            json!({"node":{"id":"c","in":"d","label":"C"}}),
            json!({"edge":{"id":"rc","in":"d","from":"root","to":"c"}}),
        ];
        ops.extend(extra);
        let next = committed(&mut scene, &mut budget, json!({"id":"next","put":ops}));
        picture(&scene, next, &first.after, &mut measure).expect("next picture")
    };
    let generated = run(vec![]);
    assert_eq!(generated.glides.len(), 1);
    assert_eq!(
        generated.glides[0].to,
        capsula_scene::geom::Vec2::new(-25.0, 0.0)
    );
    let explicit = run(vec![
        json!({"anim":{"target":"d","track":"pos","keys":[[0,[20,0]]]}}),
    ]);
    assert!(
        explicit.glides.is_empty(),
        "same-step explicit pos owns the property"
    );
}

#[test]
fn detached_edges_can_be_updated_without_resurrecting_their_endpoints() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,400,300]}},{"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},
            {"edge":{"id":"e","in":"d","from":"a","to":"b","label":"old"}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[20,30]]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("edge picture");
    cut(&mut scene, &mut budget, 1, &["e"], &first.after);
    let tip = first.after.cut(&scene).expect("keep detached edge");
    let old_lines: Vec<_> = tip.items()[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Line { points, .. } => Some(Arc::clone(points)),
            _ => None,
        })
        .collect();
    let update = committed(
        &mut scene,
        &mut budget,
        json!({"id":"update","set":[{"target":"e","label":"new"}]}),
    );
    let next = picture(&scene, update, &tip, &mut measure).expect("update frozen edge");
    let new_lines: Vec<_> = next.after.items()[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Line { points, .. } => Some(Arc::clone(points)),
            _ => None,
        })
        .collect();
    assert_eq!(new_lines, old_lines);
    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
    )
    .expect("updated detached replay");
    assert_eq!(recovered.items(), next.after.items());
}

#[test]
fn procedural_outputs_fit_the_admission_reservations_and_share_stroked_vertices() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"primitives","put":[
            {"shape":{"id":"rect","kind":"rect","box":[20,20,100,60],"radius":10,"dash":true}},
            {"shape":{"id":"circle","kind":"circle","at":[180,80],"r":20}},
            {"shape":{"id":"polygon","kind":"polygon","points":[[220,20],[260,20],[240,60]],"fill":"blue"}},
            {"arrow":{"id":"arrow","from":[20,150],"to":[140,150],"bend":0.5,"head":"both"}},
            {"path":{"id":"path","at":[300,20],"d":"M0 0 C0 4 4 4 4 0","fill":"green"}},
            {"diagram":{"id":"d","box":[0,200,600,400]}},
            {"node":{"id":"diamond","in":"d","label":"D","shape":"diamond"}},
            {"node":{"id":"cylinder","in":"d","label":"C","shape":"cylinder"}},
            {"node":{"id":"note","in":"d","label":"N","shape":"note"}},
            {"highlight":{"id":"ring","target":{"ref":"rect"},"style":"ring"}},
            {"highlight":{"id":"box","target":{"ref":"rect"},"style":"box"}},
            {"highlight":{"id":"spotlight","target":{"ref":"rect"},"style":"spotlight"}},
            {"highlight":{"id":"marker","target":{"ref":"rect"},"style":"marker"}},
            {"highlight":{"id":"underline","target":{"ref":"rect"},"style":"underline"}},
            {"callout":{"id":"callout","target":{"ref":"circle"},"text":"caption"}}
        ]}),
    );
    let input = PrepareInput::new(
        first.preparation,
        before,
        SurfaceGeometry::Overlay {
            logical_size: [800.0, 600.0],
        },
        Arc::new(Theme::default()),
    )
    .expect("input");
    let first = prepare(input, &mut measure, &AtomicBool::new(false)).expect("procedural geometry");
    assert_eq!(scene.usage().segments, 242);
    for (name, expected) in [
        ("rect", 28),
        ("circle", 48),
        ("polygon", 3),
        ("arrow", 22),
        ("path", 8),
        ("diamond", 4),
        ("cylinder", 32),
        ("note", 7),
        ("underline", 1),
        ("callout", 1),
    ] {
        let item = first
            .display
            .items
            .iter()
            .find(|item| Some(item.handle) == first.after.handle(&id(name)))
            .expect("prepared item");
        let stroked: std::collections::HashSet<_> = item
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                Prim::Line { points, .. } => Some(points.as_ptr()),
                _ => None,
            })
            .collect();
        let segments: usize = item
            .primitives
            .iter()
            .map(|primitive| match primitive {
                Prim::Line { points, closed, .. } => points.len() - 1 + usize::from(*closed),
                Prim::Fill { points, .. } if !stroked.contains(&points.as_ptr()) => points.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            segments, expected,
            "actual centerline/contour segments for {name}"
        );
    }
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"no_geometry","put":[]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("unchanged geometry");
    assert!(
        next.display.items.is_empty(),
        "unchanged shared geometry is not emitted again"
    );
    assert!(
        next.resident_bytes < first.resident_bytes,
        "only the new layout index is charged"
    );
}

#[test]
fn reference_anchors_use_the_referencing_parents_basis() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,300,240]}},{"node":{"id":"a","in":"d","label":"A"}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[20,30]]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("parent picture");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"line":{"id":"l","in":"d","from":{"ref":"a"},"to":[250,100]}}]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("reference picture");
    let line = next
        .display
        .items
        .iter()
        .find(|item| Some(item.handle) == next.after.handle(&id("l")))
        .expect("line");
    let Prim::Line { points, .. } = &line.primitives[0] else {
        panic!("polyline");
    };
    assert_eq!(points[0], capsula_scene::geom::Vec2::new(150.0, 36.0));
}

#[test]
fn invalid_host_measurements_fail_before_publication() {
    struct Invalid;
    impl TextMeasure for Invalid {
        fn measure(
            &mut self,
            _text: &str,
            _style: TextStyle,
            _width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok([f32::NAN, 10.0])
        }
    }
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"t","at":[100,100],"text":"hello"}}]}),
    );
    let input = PrepareInput::new(
        first.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("input");
    assert!(
        matches!(prepare(input,&mut Invalid,&AtomicBool::new(false)),Err(PrepareError::Layout(error)) if error.code==capsula_scene::ErrorCode::OutOfRange)
    );
    assert!(
        before.items().is_empty(),
        "failed measurement changes no previous output"
    );
}

#[test]
#[ignore = "Rez timing session; hold measuring.lock and wait for a quiet machine"]
fn measure_prepare_six_hundred_text_updates() {
    let mut samples = Vec::new();
    for iteration in 0..69 {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let mut layout = LayoutState::new(&scene);
        let mut measure = Measure::default();
        for batch in 0..2 {
            let put: Vec<_> = (batch * 300..(batch + 1) * 300)
                .map(|index| json!({"text":{"id":format!("t{index}"),"at":[100,100],"text":"X"}}))
                .collect();
            let commit = committed(
                &mut scene,
                &mut budget,
                json!({"id":format!("batch{batch}"),"put":put}),
            );
            layout = picture(&scene, commit, &layout, &mut measure)
                .expect("initial picture")
                .after;
        }
        let set: Vec<_> = (0..600)
            .map(|index| json!({"target":format!("t{index}"),"color":"blue"}))
            .collect();
        let commit = committed(&mut scene, &mut budget, json!({"id":"update","set":set}));
        let input = PrepareInput::new(
            commit.preparation,
            layout.clone(),
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("input");
        measure.calls.clear();
        let start = std::time::Instant::now();
        let result = std::hint::black_box(prepare(
            std::hint::black_box(input),
            &mut measure,
            &AtomicBool::new(false),
        ))
        .expect("updated picture");
        let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
        result.validate(&scene, &layout).expect("current result");
        assert_eq!(result.display.items.len(), 600);
        if iteration >= 5 {
            samples.push(elapsed);
        }
    }
    let mean = samples.iter().sum::<f64>() / 64.0;
    let maximum = samples.into_iter().fold(0.0_f64, f64::max);
    println!(
        "prepare_600_text_updates samples=64 warmups=5 mean_us={mean:.3} max_us={maximum:.3}; fixed deterministic text measurement; no GPUI or frame timing"
    );
}

#[test]
fn empty_recovery_does_not_charge_a_scene_before_its_first_commit() {
    let (scene, layout) = recover(
        header(),
        &[],
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
    )
    .expect("empty recovery");
    assert_eq!(scene.usage(), capsula_scene::budget::Usage::default());
    assert_eq!(layout.revision(), 0);
    assert!(layout.items().is_empty());
}

#[test]
fn generated_group_glides_cannot_push_old_nodes_beyond_the_coordinate_cap() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[-20000,0,360,300],"dir":"down"}},
            {"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},{"node":{"id":"c","in":"d","label":"C"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("initial in-range nodes");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"node":{"id":"overflow","in":"d","label":"D"}}]}),
    );
    assert!(
        matches!(picture(&scene,next,&first.after,&mut measure),Err(PrepareError::Layout(error)) if error.code==capsula_scene::ErrorCode::OutOfRange)
    );
    assert_eq!(
        first
            .after
            .bounds(&id("a"))
            .expect("old bounds")
            .expect("old node")
            .origin,
        capsula_scene::geom::Vec2::new(-19970.0, 16.0)
    );
}
