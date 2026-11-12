//! Incremental placement, preparation isolation and ordered-cut recovery.
use kumochi_scene::{
    Admission, Begin, Budget, ErrorCode, Frame, GlobalBudget, Handle, Record, SceneState,
    SceneView, Step, StopCut, WireId,
    display::{Item, MeasureError, PaintWork, Prim, TextMeasure, TextStyle},
    geom::Vec2,
    layout::{
        Glide, LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare,
        recover, recover_all,
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
) -> kumochi_scene::Committed {
    let Admission::New(candidate) = scene.admit(&step(value), budget).expect("admitted fixture")
    else {
        panic!("new step");
    };
    scene.commit(candidate, budget).expect("fresh candidate")
}
fn picture(
    scene: &SceneState,
    committed: kumochi_scene::Committed,
    before: &LayoutState,
    measure: &mut Measure,
) -> Result<PreparedStep, PrepareError> {
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .map_err(|(error, _)| error)?;
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
fn assert_vec2_near(actual: Vec2, expected: [f32; 2]) {
    assert!(
        (actual.x - expected[0]).abs() <= 0.001 && (actual.y - expected[1]).abs() <= 0.001,
        "expected {expected:?}, got {actual:?}"
    );
}
fn chart_picture(put: &Value) -> PreparedStep {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let commit = committed(&mut scene, &mut budget, json!({"id":"chart","put":put}));
    picture(&scene, commit, &before, &mut Measure::default()).expect("chart picture")
}
fn titled_bar_fixture() -> Value {
    json!([
        {"chart":{"id":"c","kind":"bar","box":[0,0,400,300],"title":"T","categories":["Q","R"],
            "x":{"min":-0.5,"max":1.5,"label":"X"},"y":{"min":-10,"max":10,"label":"Y"}}},
        {"series":{"id":"a","in":"c","name":"A","data":[2,-2]}},
        {"series":{"id":"b","in":"c","name":"B","data":[3,-3]}}
    ])
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
    let second = committed(
        &mut scene,
        &mut budget,
        json!({"id":"second","put":[{"node":{"id":"b","in":"d","label":"B"}},{"edge":{"id":"e","in":"d","from":"a","to":"b"}}]}),
    );
    let removal = committed(
        &mut scene,
        &mut budget,
        json!({"id":"third","remove":["e","b"]}),
    );
    let expected_removed = [second.handles[1].1, second.handles[0].1];
    assert!(
        scene.handle(&id("b")).is_none(),
        "semantic tip already removed b"
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first picture");
    assert_eq!(first.after.items().len(), 2);
    assert!(first.after.handle(&id("b")).is_none());
    let unchanged = first.after.items();
    let bounds = first.after.bounds(&id("a")).expect("safe").expect("a");
    let second = picture(&scene, second, &first.after, &mut measure).expect("second picture");
    assert_eq!(second.after.bounds(&id("a")).expect("safe"), Some(bounds));
    assert!(!bounds.intersects(second.after.bounds(&id("b")).expect("safe").expect("b")));
    let third = picture(&scene, removal, &second.after, &mut measure).expect("removal picture");
    assert!(
        second.after.handle(&id("b")).is_some(),
        "earlier output still has b"
    );
    assert_eq!(third.after.handle(&id("b")), None);
    assert_eq!(third.removed, expected_removed);
    assert_eq!(first.after.items(), unchanged);
    assert!(first.after.handle(&id("b")).is_none());
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
fn review_empty_boxless_diagram_can_be_cut_after_a_resting_offset() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d"}},{"anim":{"target":"d","track":"y","keys":[[0,40]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("empty diagram");
    cut(&mut scene, &mut budget, 1, &["d"], &first.after);
    let kept = first
        .after
        .cut(&scene)
        .expect("published geometry must remain safe during cut");
    assert_eq!(kept.items().len(), 1);
    assert_eq!(
        kept.bounds(&id("d")).expect("bounds"),
        Some(kumochi_scene::geom::Rect {
            origin: kumochi_scene::geom::Vec2::new(0.0, 40.0),
            size: kumochi_scene::geom::Vec2::default()
        })
    );
}

#[test]
fn review_same_step_callout_resolves_the_nodes_post_glide_position() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,360,300],"dir":"down"}},
            {"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},{"node":{"id":"c","in":"d","label":"C"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first nodes");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[
            {"node":{"id":"overflow","in":"d","label":"D"}},
            {"callout":{"id":"callout","target":{"ref":"overflow"},"at":[320,200],"text":"X"}}
        ]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("node and callout");
    assert_eq!(next.glides.len(), 1);
    assert_eq!(
        next.after
            .bounds(&id("overflow"))
            .expect("bounds")
            .expect("node")
            .center(),
        kumochi_scene::geom::Vec2::new(320.0, 36.0)
    );
    let callout = next
        .display
        .items
        .iter()
        .find(|item| Some(item.handle) == next.after.handle(&id("callout")))
        .expect("callout");
    let Prim::Line { points, .. } = &callout.primitives[0] else {
        panic!("leader");
    };
    assert_eq!(points[1], kumochi_scene::geom::Vec2::new(320.0, 56.0));
}

#[test]
fn review_same_step_highlight_uses_the_diagrams_measured_children() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d"}},{"node":{"id":"a","in":"d","label":"A"}},
            {"highlight":{"id":"h","target":{"ref":"d"}}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default())
        .expect("same-step diagram highlight");
    assert_eq!(
        first
            .after
            .bounds(&id("h"))
            .expect("bounds")
            .expect("highlight")
            .size,
        kumochi_scene::geom::Vec2::new(92.0, 52.0)
    );
}

#[test]
fn review_reverse_put_order_places_new_predecessors_before_successors() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,360,300],"dir":"down"}},{"node":{"id":"a","in":"d","label":"A"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("first node");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[
            {"node":{"id":"c","in":"d","label":"C"}},{"node":{"id":"b","in":"d","label":"B"}},
            {"edge":{"id":"ab","in":"d","from":"a","to":"b"}},{"edge":{"id":"bc","in":"d","from":"b","to":"c"}}
        ]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("reverse order nodes");
    let b = next.after.bounds(&id("b")).expect("bounds").expect("b");
    let c = next.after.bounds(&id("c")).expect("bounds").expect("c");
    assert!(!b.intersects(c));
    assert_eq!(b.origin, kumochi_scene::geom::Vec2::new(16.0, 128.0));
    assert_eq!(c.origin, kumochi_scene::geom::Vec2::new(16.0, 240.0));
}

#[test]
fn glided_detachment_prunes_both_groups_and_parentless_updates_emit_no_glide() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let mut put = vec![
        json!({"diagram":{"id":"d","box":[0,0,360,300],"dir":"down"}}),
        json!({"diagram":{"id":"e","box":[0,0,360,300],"dir":"down"}}),
    ];
    for group in ["d", "e"] {
        for name in ["a", "b", "c"] {
            put.push(json!({"node":{"id":format!("{group}{name}"),"in":group,"label":"X"}}));
        }
    }
    let first = committed(&mut scene, &mut budget, json!({"id":"first","put":put}));
    let first = picture(&scene, first, &before, &mut measure).expect("two initial groups");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"node":{"id":"overflow","in":"d","label":"X"}},{"node":{"id":"other","in":"e","label":"X"}}]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("two overflows");
    assert_eq!(
        next.glides
            .iter()
            .map(|glide| glide.target)
            .collect::<Vec<_>>(),
        ["d", "e"].map(|name| next.after.handle(&id(name)).expect("group"))
    );
    cut(&mut scene, &mut budget, 2, &["overflow"], &next.after);
    let tip = next.after.cut(&scene).expect("glided detachment");
    assert_eq!(tip.items().len(), 1);
    assert_eq!(
        tip.items()[0].handle,
        next.after.handle(&id("overflow")).expect("survivor")
    );
    assert_eq!(
        tip.bounds(&id("overflow"))
            .expect("bounds")
            .expect("node")
            .center(),
        kumochi_scene::geom::Vec2::new(320.0, 36.0)
    );
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"label","set":[{"target":"overflow","label":"Q"}]}),
    );
    let next = picture(&scene, next, &tip, &mut measure).expect("parentless node");
    assert!(next.glides.is_empty());
    let (_, replayed) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
    )
    .expect("replay");
    assert_eq!(replayed.items(), next.after.items());
}

#[test]
fn host_surface_defaults_choose_the_expected_diagram_flow_and_width() {
    use kumochi_scene::{geom::Vec2, record::Surface};
    for (surface, a, b) in [
        (
            SurfaceGeometry::default(),
            Vec2::new(180.0, 36.0),
            Vec2::new(180.0, 148.0),
        ),
        (
            SurfaceGeometry::Overlay {
                default_canvas: [600.0, 400.0],
            },
            Vec2::new(224.0, 36.0),
            Vec2::new(376.0, 36.0),
        ),
    ] {
        let mut config = header();
        config.surface = match surface {
            SurfaceGeometry::Inline { .. } => Surface::Inline,
            SurfaceGeometry::Overlay { .. } => Surface::Overlay,
        };
        let mut scene = SceneState::begin(config).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = committed(
            &mut scene,
            &mut budget,
            json!({"id":"first","put":[{"diagram":{"id":"d"}},{"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},{"edge":{"id":"edge","in":"d","from":"a","to":"b"}}]}),
        );
        let input = PrepareInput::new(
            first.preparation,
            before,
            surface,
            Arc::new(Theme::default()),
        )
        .expect("input");
        let result =
            prepare(input, &mut Measure::default(), &AtomicBool::new(false)).expect("default flow");
        assert_eq!(
            result
                .after
                .bounds(&id("a"))
                .expect("bounds")
                .expect("a")
                .center(),
            a
        );
        assert_eq!(
            result
                .after
                .bounds(&id("b"))
                .expect("bounds")
                .expect("b")
                .center(),
            b
        );
        assert!(result.glides.is_empty());
    }
}

#[test]
fn cancellation_during_host_measurement_publishes_no_layout() {
    struct Cancelling<'a>(&'a AtomicBool);
    impl TextMeasure for Cancelling<'_> {
        fn measure(
            &mut self,
            _text: &str,
            _style: TextStyle,
            _width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok([8.0, 10.0])
        }
    }
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"cancel","put":[{"text":{"id":"t","at":[100,100],"text":"X"}}]}),
    );
    let input = PrepareInput::new(
        first.preparation,
        before,
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("input");
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        prepare(input, &mut Cancelling(&cancel), &cancel),
        Err(PrepareError::Cancelled)
    ));
}

#[test]
fn prepared_debug_summarizes_geometry_without_drawing_content() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"PRIVATE_STEP","put":[{"text":{"id":"PRIVATE_ID","at":[12345,12345],"text":"PRIVATE_TEXT"}}]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("picture");
    for value in [
        format!("{first:?}"),
        format!("{:?}", first.display.items),
        format!("{:?}", first.display.items[0].primitives),
    ] {
        for sentinel in ["PRIVATE_STEP", "PRIVATE_ID", "PRIVATE_TEXT", "12345"] {
            assert!(!value.contains(sentinel));
        }
    }
}

#[test]
fn shared_recovery_steps_keep_the_existing_encoding() {
    let step = step(json!({"id":"first","put":[]}));
    let event = kumochi_scene::SceneEvent::Step {
        revision: 1,
        step: Arc::new(step.clone()),
    };
    let mut expected = vec![0, 1];
    expected.extend(postcard::to_allocvec(&step).expect("step bytes"));
    let encoded = postcard::to_allocvec(&event).expect("event bytes");
    assert_eq!(encoded, expected);
    assert_eq!(
        postcard::from_bytes::<kumochi_scene::SceneEvent>(&encoded).expect("decode"),
        event
    );
    assert_eq!(
        serde_json::to_value(&event).expect("JSON"),
        json!({"step":{"revision":1,"step":step}})
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
    assert!(std::ptr::eq(
        first.step(),
        scene.step(0).expect("stored input")
    ));
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
        None,
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
    assert_eq!(live.items().len(), 1);
    assert_eq!(
        live.items()[0].handle,
        first.after.handle(&id("a")).expect("a")
    );
    assert_eq!(live.items()[0].parent, None);
    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
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
        Err((PrepareError::Stale, _))
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
        None,
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
        matches!(error,PrepareError::Layout(error) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
    // A shrinking track charges its rect at the envelope's 4, not the resting 1: 7 pieces per
    // corner, so 4 · 8 + 1 points, live and recovered.
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let shrinking = committed(
        &mut scene,
        &mut budget,
        json!({"id":"shrinking","put":[
            {"shape":{"id":"r","kind":"rect","box":[0,0,16,16],"radius":8,"fill":"blue"}},
            {"anim":{"target":"r","track":"scale","keys":[[0,4],[100,1]]}}
        ]}),
    );
    let live = picture(&scene, shrinking, &before, &mut Measure::default()).expect("shrinking");
    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        Some(PaintWork::LIMITS),
    )
    .expect("recovered under the cap");
    for work in [live.after.paint_work(), recovered.paint_work()] {
        assert_eq!((work.fill_vertices, work.outline_points), (33, 33));
    }
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
fn initial_fit_still_enforces_minimum_normal_composed_scale() {
    for (scale, expected) in [
        (f32::MIN_POSITIVE * 2.0, Ok(8)),
        (f32::MIN_POSITIVE, Err(ErrorCode::TransformOverflow)),
    ] {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let ticket = committed(
            &mut scene,
            &mut budget,
            json!({"id":"scaled","put":[
                {"diagram":{"id":"d","dir":"right"}},
                {"node":{"id":"a","in":"d","label":"abcdefghij","enter":"none"}},
                {"node":{"id":"b","in":"d","label":"B","enter":"none"}},
                {"node":{"id":"c","in":"d","label":"C","enter":"none"}},
                {"node":{"id":"last","in":"d","label":"D","enter":"none"}},
                {"edge":{"id":"ab","in":"d","from":"a","to":"b"}},
                {"edge":{"id":"bc","in":"d","from":"b","to":"c"}},
                {"edge":{"id":"cd","in":"d","from":"c","to":"last"}},
                {"anim":{"target":"d","track":"scale","keys":[[0,scale]]}}
            ]}),
        );
        let result = picture(&scene, ticket, &before, &mut Measure::default())
            .map(|picture| picture.after.items().len())
            .map_err(|error| match error {
                PrepareError::Layout(error) => error.code,
                other => panic!("unexpected preparation failure: {other:?}"),
            });
        assert_eq!(result, expected);
    }
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
        kumochi_scene::geom::Vec2::new(-25.0, 0.0)
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
        None,
    )
    .expect("updated detached replay");
    assert_eq!(recovered.items(), next.after.items());
}

#[test]
fn a_zero_width_shape_keeps_its_fill_and_draws_no_outline() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"sky","put":[
            {"shape":{"id":"skybg","kind":"rect","box":[0,0,480,300],"fill":"#BFE8FF","color":"#BFE8FF","width":0}},
            {"shape":{"id":"sun","kind":"circle","at":[400,60],"r":30,"fill":"#FFD700","width":0}}
        ]}),
    );
    let prepared =
        picture(&scene, first, &before, &mut Measure::default()).expect("zero-width shapes");
    let primitives = |name: &'static str| {
        let handle = prepared.after.handle(&id(name));
        let item = prepared
            .display
            .items
            .iter()
            .find(|item| Some(item.handle) == handle)
            .expect("prepared item");
        Arc::clone(&item.primitives)
    };
    assert!(matches!(
        &*primitives("skybg"),
        [Prim::Rect {
            fill: Some(_),
            border: None,
            ..
        }]
    ));
    assert!(matches!(&*primitives("sun"), [Prim::Fill { .. }]));
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
            {"edge":{"id":"edge","in":"d","from":"diamond","to":"cylinder"}},
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
            default_canvas: [800.0, 600.0],
        },
        Arc::new(Theme::default()),
    )
    .expect("input");
    let first = prepare(input, &mut measure, &AtomicBool::new(false)).expect("procedural geometry");
    assert_eq!(scene.usage().segments, 261);
    for (name, expected) in [
        ("rect", 28),
        ("circle", 48),
        ("polygon", 3),
        ("arrow", 22),
        ("path", 8),
        ("diamond", 4),
        ("cylinder", 32),
        ("note", 7),
        ("edge", 19),
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
    let empty = committed(&mut scene, &mut budget, json!({"id":"another_empty"}));
    let empty =
        picture(&scene, empty, &next.after, &mut measure).expect("same index without new payload");
    assert_eq!(next.resident_bytes, empty.resident_bytes);
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
    assert_eq!(points[0], kumochi_scene::geom::Vec2::new(150.0, 36.0));
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
        matches!(prepare(input,&mut Invalid,&AtomicBool::new(false)),Err(PrepareError::Layout(error)) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
    assert!(
        before.items().is_empty(),
        "failed measurement changes no previous output"
    );
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
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
    println!("prepare_600_text_updates_raw_us={samples:?}");
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
        None,
    )
    .expect("empty recovery");
    assert_eq!(scene.usage(), kumochi_scene::budget::Usage::default());
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
        matches!(picture(&scene,next,&first.after,&mut measure),Err(PrepareError::Layout(error)) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
    assert_eq!(
        first
            .after
            .bounds(&id("a"))
            .expect("old bounds")
            .expect("old node")
            .origin,
        kumochi_scene::geom::Vec2::new(-19970.0, 16.0)
    );
}

#[test]
fn alignment_defaults_and_explicit_override() {
    use kumochi_scene::record::Align;
    let run = |align: Value| {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = committed(
            &mut scene,
            &mut budget,
            json!({"id":"first","put":[
                {"table":{"id":"t","box":[0,0,180,300],"cols":["Name","Value","Override"],"align":align}},
                {"row":{"id":"r","in":"t","cells":["Long text that should wrap",12,3]}}
            ]}),
        );
        picture(&scene, first, &before, &mut Measure::default())
            .expect("fitted table")
            .after
    };
    let explicit = run(json!(["center"]));
    let row = explicit
        .items()
        .into_iter()
        .find(|item| Some(item.handle) == explicit.handle(&id("r")))
        .expect("row");
    let aligns: Vec<_> = row
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text { align, .. } => Some(*align),
            _ => None,
        })
        .collect();
    assert_eq!(aligns, [Align::Center, Align::Right, Align::Right]);
    assert_eq!(row.bounds.size, kumochi_scene::geom::Vec2::new(180.0, 84.0));
    let inferred = run(json!([]));
    let row = inferred
        .items()
        .into_iter()
        .find(|item| Some(item.handle) == inferred.handle(&id("r")))
        .expect("row");
    let aligns: Vec<_> = row
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text { align, .. } => Some(*align),
            _ => None,
        })
        .collect();
    assert_eq!(aligns, [Align::Left, Align::Right, Align::Right]);
}

#[test]
fn tables_prepare_empty_then_keep_future_removals_out_of_the_previous_picture() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let table = committed(
        &mut scene,
        &mut budget,
        json!({"id":"table","put":[{"table":{"id":"t","cols":["Name","Value"]}}]}),
    );
    let table = picture(&scene, table, &before, &mut measure).expect("empty table");
    assert_eq!(table.after.items().len(), 1);
    let empty = table.after.items().pop().expect("table item");
    assert_eq!(
        empty.bounds.size,
        kumochi_scene::geom::Vec2::new(112.0, 31.55),
        "empty content keeps the header height"
    );
    assert!(
        matches!(
            &*empty.primitives,
            [
                Prim::Rect { .. },
                Prim::Text { .. },
                Prim::Text { .. },
                Prim::Line { .. }
            ]
        ),
        "an empty table has its background, headers and rule"
    );
    let rows = committed(
        &mut scene,
        &mut budget,
        json!({"id":"rows","put":[
            {"row":{"id":"a","in":"t","cells":["A",1000]}},{"row":{"id":"b","in":"t","cells":["B",true],"emphasis":true}}
        ]}),
    );
    let removal = committed(
        &mut scene,
        &mut budget,
        json!({"id":"remove","remove":["b"]}),
    );
    let rows =
        picture(&scene, rows, &table.after, &mut measure).expect("rows before future removal");
    let old_items = rows.after.items();
    assert!(rows.after.handle(&id("b")).is_some());
    let removed = picture(&scene, removal, &rows.after, &mut measure).expect("activation removal");
    assert_eq!(
        removed.removed,
        [rows.after.handle(&id("b")).expect("old row")]
    );
    assert!(removed.after.handle(&id("b")).is_none());
    assert_eq!(
        removed
            .after
            .bounds(&id("t"))
            .expect("table bounds")
            .expect("table")
            .size,
        kumochi_scene::geom::Vec2::new(112.0, 65.05),
        "a removal-only step rebuilds the surviving table height"
    );
    assert_eq!(rows.after.items(), old_items);
    let values: Vec<_> = rows
        .after
        .items()
        .into_iter()
        .flat_map(|item| item.primitives.to_vec())
        .filter_map(|primitive| match primitive {
            Prim::Text { text, .. } => Some(String::from(text)),
            _ => None,
        })
        .collect();
    assert_eq!(
        values,
        ["Name", "Value", "A", "1,000", "B", "true"],
        "header and row text follow handle order"
    );
}

#[test]
fn row_cell_updates_and_detached_replay_keep_the_prepared_column_geometry() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"table":{"id":"t","box":[100,100,240,200],"cols":["Value","Padded"]}},
            {"row":{"id":"r","in":"t","cells":[1]}},
            {"anim":{"target":"t","track":"pos","keys":[[0,[20,30]]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("table");
    let expected = first.after.bounds(&id("r")).expect("bounds").expect("row");
    cut(&mut scene, &mut budget, 1, &["r"], &first.after);
    let tip = first.after.cut(&scene).expect("detached row");
    assert_eq!(tip.bounds(&id("r")).expect("bounds"), Some(expected));
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"cells","set":[{"target":"r","cells":[2],"color":"red"}]}),
    );
    let next = picture(&scene, next, &tip, &mut measure).expect("detached cells");
    assert_eq!(
        next.after.bounds(&id("r")).expect("updated bounds"),
        Some(expected),
        "a detached update keeps the absorbed ancestor transform"
    );
    let texts: Vec<_> = next.after.items()[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .map(str::to_owned)
        .collect();
    assert_eq!(texts, ["2", ""]);
    let (_, replayed) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
    )
    .expect("detached replay");
    assert_eq!(replayed.items(), next.after.items());
}

#[test]
fn row_only_rebuild_preserves_the_detached_tables_absorbed_opacity() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","opacity":0.5}},
            {"table":{"id":"t","in":"d","cols":["Value"]}},
            {"row":{"id":"r","in":"t","cells":[1]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("table");
    let table = first.after.handle(&id("t")).expect("table handle");
    cut(&mut scene, &mut budget, 1, &["t", "r"], &first.after);
    let detached = first.after.cut(&scene).expect("detached table");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"row","set":[{"target":"r","cells":[2]}]}),
    );
    let next = picture(&scene, next, &detached, &mut Measure::default()).expect("row update");
    let mut view = kumochi_scene::motion::SceneView::new(&detached, scene.activated());
    let mut frame = kumochi_scene::motion::Frame::new();
    view.activate(next, 0, 800, &mut frame)
        .expect("row activation");
    view.frame(0, &mut frame).expect("row frame");
    let table = frame
        .items()
        .iter()
        .find(|item| item.handle == table)
        .expect("table");
    assert_eq!(
        table.opacity.to_bits(),
        0.5_f32.to_bits(),
        "editing a child must not reset an untouched detached table's inherited opacity"
    );
}

#[test]
fn measured_entrance_pivots_bound_child_motion_outside_the_resting_union() {
    let prepare_case = |offset| {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let step = committed(
            &mut scene,
            &mut budget,
            json!({"id":"pivot","put":[
                {"diagram":{"id":"parent","enter":"pop"}},
                {"shape":{"id":"child","in":"parent","kind":"rect","box":[0,0,10000,10000]}},
            {"anim":{"target":"child","track":"x","keys":[[0,offset],[100,0]]}}
            ]}),
        );
        picture(&scene, step, &before, &mut Measure::default())
    };
    // The parent pivot adds 676.6: offset 8000 bounds 19894.5, but 8100 bounds 20001.3.
    assert!(
        prepare_case(8000).is_ok(),
        "the measured parent pivot and child envelope fit"
    );
    assert!(
        matches!(
            prepare_case(8100),
            Err(PrepareError::Layout(error)) if error.code == kumochi_scene::ErrorCode::OutOfRange
        ),
        "only the measured parent pivot pushes the child envelope beyond the canvas"
    );
}

#[test]
fn chart_title_admission_reserves_the_origin_but_preparation_checks_its_height() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let step = committed(
        &mut scene,
        &mut budget,
        json!({"id":"title","put":[
            {"chart":{"id":"chart","box":[0,19985,300,15],"title":"Title"}}
        ]}),
    );
    assert!(
        matches!(picture(&scene, step, &before, &mut Measure::default()),
        Err(PrepareError::Layout(error)) if error.code == kumochi_scene::ErrorCode::OutOfRange),
        "the title starts at 19997 but its measured height extends beyond 20000"
    );
}

#[test]
fn table_alignment_and_row_cell_caps_are_admission_errors_not_layout_truncation() {
    let scene = SceneState::begin(header()).expect("scene");
    let budget = GlobalBudget::default();
    let align = step(
        json!({"id":"align","put":[{"table":{"id":"t","cols":["One"],"align":["left","right"]}}]}),
    );
    assert!(
        matches!(scene.admit(&align,&budget),Err(error) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
    let cells = step(
        json!({"id":"cells","put":[{"table":{"id":"t","cols":["One"]}},{"row":{"id":"r","in":"t","cells":[1,2]}}]}),
    );
    assert!(
        matches!(scene.admit(&cells,&budget),Err(error) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
}

fn row_style_updates(cut_revision: Option<u32>) {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"table":{"id":"t","cols":["Value"]}},
            {"row":{"id":"r","in":"t","cells":[1],"emphasis":true,"color":"blue"}}
        ]}),
    );
    let mut layout = picture(&scene, first, &before, &mut Measure::default())
        .expect("table")
        .after;
    if let Some(revision) = cut_revision {
        cut(&mut scene, &mut budget, revision, &["r"], &layout);
        layout = layout.cut(&scene).expect("detached row");
    }
    let red = kumochi_scene::display::Paint::from(
        kumochi_scene::record::Rgb::try_from(0x00ff_6b7d).expect("RGB"),
    );
    for (index, emphasis) in [true, false, true].into_iter().enumerate() {
        let next = committed(
            &mut scene,
            &mut budget,
            json!({"id":format!("style{index}"),"set":[{"target":"r","emphasis":emphasis,"color":"red"}]}),
        );
        layout = picture(&scene, next, &layout, &mut Measure::default())
            .expect("style update")
            .after;
        let row = layout
            .items()
            .into_iter()
            .find(|item| Some(item.handle) == layout.handle(&id("r")))
            .expect("row");
        let bands: Vec<_> = row
            .primitives
            .iter()
            .filter_map(|primitive| {
                if let Prim::Rect { fill, .. } = primitive {
                    Some(*fill)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            bands,
            if emphasis {
                vec![Some(red.faded(0.1))]
            } else {
                vec![]
            },
            "background follows the updated row emphasis and color"
        );
        let styles: Vec<_> = row
            .primitives
            .iter()
            .filter_map(|primitive| {
                if let Prim::Text { style, color, .. } = primitive {
                    Some((style.bold, *color))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            styles,
            [(emphasis, red)],
            "text follows the updated row style"
        );
    }
}

#[test]
fn attached_row_style_updates_apply_emphasis_and_color() {
    row_style_updates(None);
}

#[test]
fn detached_row_style_updates_apply_emphasis_and_color() {
    row_style_updates(Some(1));
}

#[test]
fn table_lists_accept_eight_cells_and_alignments_and_reject_nine() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let cols = vec!["Column"; 8];
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"table":{"id":"t","cols":cols,"align":vec!["center";8]}},
            {"row":{"id":"r","in":"t","cells":vec![1;8]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("eight columns");
    let row = first
        .after
        .items()
        .into_iter()
        .find(|item| Some(item.handle) == first.after.handle(&id("r")))
        .expect("row");
    assert_eq!(
        row.primitives
            .iter()
            .filter(|primitive| matches!(
                primitive,
                Prim::Text {
                    align: kumochi_scene::record::Align::Center,
                    ..
                }
            ))
            .count(),
        8
    );
    for record in [
        json!({"step":{"id":"align","put":[{"table":{"id":"bad","cols":vec!["Column";8],"align":vec!["center";9]}}]}}),
        json!({"step":{"id":"cells","put":[{"row":{"id":"bad","in":"t","cells":vec![1;9]}}]}}),
    ] {
        assert!(
            matches!(parse_record(&serde_json::to_vec(&record).expect("fixture")),Err(error) if error.code==kumochi_scene::ErrorCode::TooLong)
        );
    }
}

#[test]
fn chart_series_removal_uses_the_prepared_prefix_and_keeps_older_pictures() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"chart":{"id":"c","kind":"line","box":[0,0,400,300]}},
            {"series":{"id":"a","in":"c","data":[0,1]}}
        ]}),
    );
    let second = committed(
        &mut scene,
        &mut budget,
        json!({"id":"second","put":[
            {"series":{"id":"b","in":"c","data":[10,20]}}
        ]}),
    );
    let removed_handle = second.handles[0].1;
    let third = committed(
        &mut scene,
        &mut budget,
        json!({"id":"third","remove":["b"]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("first chart");
    let unchanged = first.after.items();
    assert!(
        first.after.handle(&id("b")).is_none(),
        "future series never enters the earlier picture"
    );
    let second =
        picture(&scene, second, &first.after, &mut Measure::default()).expect("second chart");
    assert!(
        second.after.handle(&id("b")).is_some(),
        "a future removal does not affect this picture"
    );
    let a = first.after.handle(&id("a")).expect("original series");
    let second_items = second.after.items();
    assert_ne!(
        second_items
            .iter()
            .find(|item| item.handle == a)
            .expect("rescaled series")
            .primitives,
        unchanged
            .iter()
            .find(|item| item.handle == a)
            .expect("original picture")
            .primitives,
        "the added series changes the existing series scale before its removal"
    );
    let third =
        picture(&scene, third, &second.after, &mut Measure::default()).expect("removed series");
    assert_eq!(
        third.removed,
        [removed_handle],
        "removal is published at its own activation"
    );
    assert_eq!(
        first.after.items(),
        unchanged,
        "later reflows never mutate an earlier picture"
    );
    assert_eq!(
        third.after.items(),
        unchanged,
        "after removal the original series again determines the ranges"
    );
}

#[test]
fn detached_series_updates_keep_the_frozen_scale_and_absorbed_transform() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"chart":{"id":"c","kind":"line","box":[100,100,300,200],"x":{"min":0,"max":2},"y":{"min":0,"max":10}}},
            {"series":{"id":"s","in":"c","data":[[0,0],[1,5],[2,10]]}},
            {"anim":{"target":"c","track":"pos","keys":[[0,[20,30]]]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("chart");
    cut(&mut scene, &mut budget, 1, &["s"], &first.after);
    let detached = first.after.cut(&scene).expect("detached series");
    let expected = detached.items().pop().expect("survivor");
    let expected_bounds = detached.bounds(&id("s")).expect("bounds");
    let original_points = expected
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Line { points, .. } => Some(points.as_ref()),
            _ => None,
        })
        .expect("original line");
    assert_eq!(original_points.len(), 3);
    assert_vec2_near(original_points[1], [260.0, 192.2]);
    assert_eq!(
        expected.transform,
        kumochi_scene::geom::Affine([1.0, 0.0, 0.0, 1.0, 20.0, 30.0]),
        "the cut absorbs the chart translation"
    );
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"data","set":[
            {"target":"s","data":[[0,0],[1,2.5],[2,1000]]}
        ]}),
    );
    let next =
        picture(&scene, next, &detached, &mut Measure::default()).expect("frozen data update");
    assert_eq!(
        next.after.bounds(&id("s")).expect("updated bounds"),
        expected_bounds,
        "the update keeps the pre-update world placement and scale"
    );
    let updated = next.after.items().pop().expect("updated survivor");
    assert_eq!(
        updated.transform, expected.transform,
        "the update retains the absorbed translation"
    );
    let points = updated
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Line { points, .. } => Some(points.as_ref()),
            _ => None,
        })
        .expect("updated line");
    assert_eq!(points.len(), 3);
    assert_vec2_near(points[0], [138.0, 268.4]);
    assert_vec2_near(points[1], [260.0, 230.3]);
    assert_vec2_near(points[2], [382.0, 116.0]);
    let (_, replayed) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
    )
    .expect("recovery");
    assert_eq!(
        replayed.items(),
        next.after.items(),
        "recovery reconstructs the frozen basis before replaying the data update"
    );
}

#[test]
fn narrow_chart_labels_do_not_invert_their_rounded_clamp_bounds() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"narrow","put":[
            {"chart":{"id":"c","kind":"line","box":[0,0,64,300]}},
            {"series":{"id":"s","in":"c","data":[0,1,2]}}
        ]}),
    );
    let result = picture(&scene, commit, &before, &mut Measure::default()).expect("narrow chart");
    assert_eq!(result.display.items.len(), 2);
    let x_labels: Vec<_> = result.display.items[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text {
                origin,
                text,
                max_width,
                ..
            } if origin.y > 270.0 => Some((*origin, text.as_str(), *max_width)),
            _ => None,
        })
        .collect();
    assert_eq!(
        x_labels,
        [(kumochi_scene::geom::Vec2::new(25.6, 275.4), "0", Some(8.0))]
    );
}

#[test]
fn chart_color_sets_repaint_the_zero_baseline() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"chart":{"id":"c","kind":"line","box":[0,0,400,300]}},
            {"series":{"id":"s","in":"c","data":[0,1]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("chart");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"color","set":[{"target":"c","color":"red"}]}),
    );
    let next = picture(&scene, next, &first.after, &mut Measure::default()).expect("colored chart");
    let items = next.after.items();
    let Prim::Line { color, .. } = items[0].primitives.last().expect("baseline") else {
        panic!("baseline line");
    };
    let red = serde_json::from_value(json!("red")).expect("color");
    let expected = kumochi_scene::display::Paint::from(Theme::default().resolve(&red)).faded(0.4);
    assert_eq!(
        *color, expected,
        "the accepted chart color reaches its visible baseline"
    );
    assert_ne!(
        items[0].primitives,
        first.after.items()[0].primitives,
        "a chart color update changes the output"
    );
}

#[test]
fn chart_axes_area_and_all_pie_series_stay_within_their_reservations() {
    for (kind, data, expected_series_segments, expected_charge) in [
        ("area", json!([0, 1, 2]), 7, 71),
        ("pie", json!([1, 1]), 244, 488),
    ] {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut put = vec![
            json!({"chart":{"id":"c","kind":kind,"box":[0,0,400,300]}}),
            json!({"series":{"id":"a","in":"c","data":data}}),
        ];
        if kind == "pie" {
            put.push(json!({"series":{"id":"b","in":"c","data":[1,1]}}));
        }
        let commit = committed(&mut scene, &mut budget, json!({"id":"first","put":put}));
        let result =
            picture(&scene, commit, &before, &mut Measure::default()).expect("chart primitives");
        for item in result.after.items() {
            let segments: usize = item
                .primitives
                .iter()
                .map(|primitive| match primitive {
                    Prim::Line { points, closed, .. } => {
                        points.len().saturating_sub(1) + usize::from(*closed)
                    }
                    Prim::Fill { points, .. } => points.len(),
                    _ => 0,
                })
                .sum();
            let expected = if Some(item.handle) == result.after.handle(&id("c")) {
                if kind == "pie" { 0 } else { 6 }
            } else {
                expected_series_segments
            };
            assert_eq!(segments, expected, "exact generated topology for {kind}");
        }
        assert_eq!(
            scene.usage().segments,
            expected_charge,
            "admission reserves both area primitives and every pie series"
        );
    }
}

#[test]
fn pie_series_continue_the_slice_palette() {
    let result = chart_picture(&json!([
        {"chart":{"id":"c","kind":"pie","box":[0,0,400,300]}},
        {"series":{"id":"a","in":"c","data":[1,1]}},
        {"series":{"id":"b","in":"c","data":[1,1]}}
    ]));
    let pie_colors: Vec<_> = result
        .after
        .items()
        .iter()
        .flat_map(|item| {
            item.primitives
                .iter()
                .filter_map(|primitive| match primitive {
                    Prim::Fill { color, .. } => Some(*color),
                    _ => None,
                })
        })
        .collect();
    assert_eq!(
        pie_colors,
        (0..4)
            .map(|slot| Theme::default().series(slot).into())
            .collect::<Vec<kumochi_scene::display::Paint>>(),
        "adjacent series continue the slice palette rather than repeating their shared boundary color"
    );
}

#[test]
fn titled_bar_header_places_the_legend_axes_and_category_labels() {
    let result = chart_picture(&titled_bar_fixture());
    let items = result.after.items();
    let handle = result.after.handle(&id("c")).expect("chart handle");
    let chart = items
        .iter()
        .find(|item| item.handle == handle)
        .expect("chart");
    let labels: Vec<_> = chart
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text { origin, text, .. } => Some((text.as_str(), *origin)),
            _ => None,
        })
        .collect();
    let expected_labels = [
        ("T", [12.0, 12.0]),
        ("A", [26.0, 30.0]),
        ("B", [62.0, 30.0]),
        ("Y", [12.0, 53.6]),
        ("-10", [6.0, 244.6]),
        ("-5", [6.0, 199.9]),
        ("0", [6.0, 155.2]),
        ("5", [6.0, 110.5]),
        ("10", [6.0, 65.8]),
        ("Q", [126.0, 259.4]),
        ("R", [294.0, 259.4]),
        ("X", [46.0, 282.0]),
    ];
    assert_eq!(labels.len(), expected_labels.len());
    for ((label, origin), (expected_label, expected_origin)) in
        labels.into_iter().zip(expected_labels)
    {
        assert_eq!(label, expected_label);
        assert_vec2_near(origin, expected_origin);
    }
    let rectangles: Vec<_> = chart
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Rect { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(rectangles.len(), 3);
    assert_vec2_near(rectangles[0].origin, [0.0, 0.0]);
    assert_vec2_near(rectangles[0].size, [400.0, 300.0]);
    for (rect, origin) in rectangles[1..].iter().zip([[12.0, 33.0], [48.0, 33.0]]) {
        assert_vec2_near(rect.origin, origin);
        assert_vec2_near(rect.size, [9.0, 9.0]);
    }
    let lines: Vec<_> = chart
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Line { points, .. } => Some(points),
            _ => None,
        })
        .collect();
    assert_eq!(lines.len(), 6);
    for (points, y) in lines
        .into_iter()
        .zip([252.4, 207.7, 163.0, 118.3, 73.6, 163.0])
    {
        assert_eq!(points.len(), 2);
        assert_vec2_near(points[0], [46.0, y]);
        assert_vec2_near(points[1], [382.0, y]);
    }
}

#[test]
fn grouped_and_stacked_bars_use_series_slots_and_signed_baselines() {
    for (stacked, expected) in [
        (
            false,
            [
                [66.16, 145.12, 62.84, 17.88],
                [234.16, 163.0, 62.84, 17.88],
                [130.0, 136.18, 62.84, 26.82],
                [298.0, 163.0, 62.84, 26.82],
            ],
        ),
        (
            true,
            [
                [66.16, 145.12, 126.68, 17.88],
                [234.16, 163.0, 126.68, 17.88],
                [66.16, 118.3, 126.68, 26.82],
                [234.16, 180.88, 126.68, 26.82],
            ],
        ),
    ] {
        let mut put = titled_bar_fixture();
        put[0]["chart"]["stacked"] = json!(stacked);
        let result = chart_picture(&put);
        let chart = result.after.handle(&id("c")).expect("chart handle");
        let rectangles: Vec<_> = result
            .after
            .items()
            .iter()
            .filter(|item| item.parent == Some(chart))
            .flat_map(|item| {
                item.primitives
                    .iter()
                    .filter_map(|primitive| match primitive {
                        Prim::Rect { rect, .. } => Some(*rect),
                        _ => None,
                    })
            })
            .collect();
        assert_eq!(rectangles.len(), 4);
        for (rect, [x, y, width, height]) in rectangles.into_iter().zip(expected) {
            assert_vec2_near(rect.origin, [x, y]);
            assert_vec2_near(rect.size, [width, height]);
        }
    }
}

#[test]
fn sampled_sine_maps_one_period_with_positive_y_upward() {
    use std::f64::consts::{FRAC_PI_2, PI, TAU};

    let result = chart_picture(&json!([
        {"chart":{"id":"c","kind":"line","box":[0,0,400,300],
            "x":{"min":0,"max":TAU},"y":{"min":-1,"max":1}}},
        {"series":{"id":"s","in":"c","data":[
            [0,0],[FRAC_PI_2,1],[PI,0],[3.0 * FRAC_PI_2,-1],[TAU,0]
        ]}}
    ]));
    let items = result.after.items();
    let handle = result.after.handle(&id("s")).expect("sine handle");
    let series = items
        .iter()
        .find(|item| item.handle == handle)
        .expect("sine series");
    let lines: Vec<_> = series
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Line { points, closed, .. } => Some((points, closed)),
            _ => None,
        })
        .collect();
    assert_eq!(lines.len(), 1);
    let (points, closed) = lines[0];
    assert!(!closed, "a sine period is an open curve");
    let expected = [
        [54.0, 142.2],
        [136.0, 16.0],
        [218.0, 142.2],
        [300.0, 268.4],
        [382.0, 142.2],
    ];
    assert_eq!(points.len(), expected.len());
    for (actual, expected) in points.iter().zip(expected) {
        assert_vec2_near(*actual, expected);
    }
}

#[test]
fn pair_x_scatter_uses_uneven_numeric_spacing_and_log_y_midpoints() {
    let result = chart_picture(&json!([
        {"chart":{"id":"c","kind":"scatter","box":[0,0,400,300],"y":{"min":1,"max":100,"scale":"log"}}},
        {"series":{"id":"s","in":"c","data":[[10,1],[15,10],[30,100]]}}
    ]));
    let items = result.after.items();
    let handle = result.after.handle(&id("s")).expect("series handle");
    let series = items
        .iter()
        .find(|item| item.handle == handle)
        .expect("series");
    let rectangles: Vec<_> = series
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Rect { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(rectangles.len(), 3);
    for (rect, center) in rectangles
        .into_iter()
        .zip([[46.0, 268.4], [130.0, 142.2], [382.0, 16.0]])
    {
        assert_vec2_near(rect.center(), center);
        assert_vec2_near(rect.size, [8.0, 8.0]);
    }
}

#[test]
fn removing_a_chart_and_all_its_series_publishes_no_rebuilt_items() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"chart":{"id":"c","kind":"line","box":[0,0,400,300]}},
            {"series":{"id":"a","in":"c","data":[0,1]}},
            {"series":{"id":"b","in":"c","data":[1,2]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("first chart");
    let unchanged = first.after.items();
    let expected = ["a", "c", "b"].map(|key| first.after.handle(&id(key)).expect("removed handle"));
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"remove","remove":["a","c","b"]}),
    );
    let next =
        picture(&scene, next, &first.after, &mut Measure::default()).expect("whole chart removal");
    assert_eq!(next.removed, expected);
    assert!(
        next.display.items.is_empty(),
        "no removed parent is rebuilt"
    );
    assert!(
        next.after.items().is_empty(),
        "the chart and both series are gone"
    );
    assert_eq!(
        first.after.items(),
        unchanged,
        "removal leaves the earlier picture intact"
    );
}

#[test]
fn attached_zero_total_pie_keeps_its_series_without_generating_slices() {
    let result = chart_picture(&json!([
        {"chart":{"id":"c","kind":"pie","box":[0,0,400,300]}},
        {"series":{"id":"s","in":"c","data":[0,0]}}
    ]));
    let items = result.after.items();
    assert_eq!(items.len(), 2);
    let chart = result.after.handle(&id("c")).expect("chart handle");
    let handle = result
        .after
        .handle(&id("s"))
        .expect("zero-total series handle");
    let series = items
        .iter()
        .find(|item| item.handle == handle)
        .expect("attached series");
    assert_eq!(series.parent, Some(chart));
    assert!(
        series.primitives.is_empty(),
        "zero values produce neither slices nor labels"
    );
    let chart = items
        .iter()
        .find(|item| item.handle == chart)
        .expect("chart background");
    assert_eq!(chart.primitives.len(), 1);
    assert!(
        matches!(chart.primitives[0], Prim::Rect { .. }),
        "the empty pie still has its frame"
    );
}

#[test]
fn an_admitted_over_tall_table_fails_preparation_with_out_of_range() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut put = vec![json!({"table":{"id":"t","box":[0,0,48,1000],"cols":["V"]}})];
    put.extend(
        (0..40)
            .map(|row| json!({"row":{"id":format!("r{row}"),"in":"t","cells":["x".repeat(400)]}})),
    );
    let commit = committed(&mut scene, &mut budget, json!({"id":"tall","put":put}));
    let result = picture(&scene, commit, &before, &mut Measure::default());
    assert!(
        matches!(result, Err(PrepareError::Layout(error)) if error.code == kumochi_scene::ErrorCode::OutOfRange),
        "the admitted table's wrapped height exceeds the preparation coordinate bound"
    );
}

#[test]
fn narrow_table_measures_its_wrapped_title_and_header() {
    use kumochi_scene::{geom::Vec2, record::Align};
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"title","put":[
            {"table":{"id":"t","box":[10,20,80,300],"title":"Title wraps here","cols":["Long header"],"align":["center"]}}
        ]}),
    );
    let result = picture(&scene, first, &before, &mut Measure::default()).expect("wrapped table");
    let item = result.after.items().pop().expect("table");
    assert_eq!(
        item.bounds.size,
        Vec2::new(80.0, 78.0),
        "title and header contribute their wrapped heights"
    );
    let [
        Prim::Rect { .. },
        Prim::Text {
            origin: title_at,
            text: title,
            style: title_style,
            ..
        },
        Prim::Text {
            origin: header_at,
            text: label,
            style: header_style,
            max_width,
            align,
            ..
        },
        Prim::Line { points, .. },
    ] = &*item.primitives
    else {
        panic!("background, title, header and rule");
    };
    assert_eq!(
        (title.as_str(), label.as_str()),
        ("Title wraps here", "Long header"),
        "the title and column label are preserved"
    );
    assert_eq!(
        (*title_at, *header_at),
        (Vec2::new(20.0, 27.0), Vec2::new(20.0, 71.0)),
        "wrapped blocks are vertically separated"
    );
    assert_eq!(
        (*title_style, *header_style),
        (
            TextStyle {
                size: 15.0,
                bold: true
            },
            TextStyle {
                size: 13.5,
                bold: true
            }
        ),
        "title and header use their distinct styles"
    );
    assert_eq!(
        (*max_width, *align),
        (Some(60.0), Align::Center),
        "the header uses the fitted cell width and explicit alignment"
    );
    assert_eq!(
        &**points,
        &[Vec2::new(10.0, 98.0), Vec2::new(90.0, 98.0)],
        "the rule follows the wrapped header"
    );
}

#[test]
fn a_four_hundred_character_cell_prepares_without_truncation() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let value = "x".repeat(400);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"cap","put":[
            {"table":{"id":"t","box":[0,0,1600,1000],"cols":["Value"]}},
            {"row":{"id":"r","in":"t","cells":[value]}}
        ]}),
    );
    let result = picture(&scene, first, &before, &mut Measure::default()).expect("cell at cap");
    let row = result
        .after
        .items()
        .into_iter()
        .find(|item| Some(item.handle) == result.after.handle(&id("r")))
        .expect("row");
    let Prim::Text {
        text, max_width, ..
    } = &row.primitives[0]
    else {
        panic!("cell text");
    };
    assert_eq!(
        text.as_str(),
        value,
        "all four hundred characters remain in the prepared cell"
    );
    assert_eq!(
        *max_width,
        Some(1580.0),
        "padding leaves the expected wrapped width"
    );
    assert_eq!(
        row.bounds.size,
        kumochi_scene::geom::Vec2::new(1600.0, 44.0),
        "wrapped text determines the row height"
    );
}

#[test]
fn row_order_cell_sets_and_whole_table_removal_keep_their_boundaries() {
    use kumochi_scene::{ErrorCode, geom::Vec2};
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut layout = LayoutState::new(&scene);
    for value in [
        json!({"id":"first","put":[{"table":{"id":"t","cols":["V"]}},{"row":{"id":"z","in":"t","cells":["old"]}}]}),
        json!({"id":"second","put":[{"row":{"id":"a","in":"t","cells":["new"]}}]}),
        json!({"id":"update","set":[{"target":"z","cells":["longer"]}]}),
    ] {
        let commit = committed(&mut scene, &mut budget, value);
        layout = picture(&scene, commit, &layout, &mut Measure::default())
            .expect("row step")
            .after;
    }
    let origins = ["z", "a"].map(|name| {
        layout
            .bounds(&id(name))
            .expect("bounds")
            .expect("row")
            .origin
    });
    assert_eq!(
        origins,
        [Vec2::new(0.0, 31.55), Vec2::new(0.0, 65.05)],
        "updating an older row does not reorder it"
    );
    assert_eq!(
        layout
            .bounds(&id("t"))
            .expect("bounds")
            .expect("table")
            .size,
        Vec2::new(68.0, 98.55),
        "an attached cell update refits the column"
    );
    let excess = step(json!({"id":"excess","set":[{"target":"z","cells":[1,2]}]}));
    assert!(
        matches!(scene.admit(&excess, &budget), Err(error) if error.code == ErrorCode::OutOfRange),
        "a set cannot exceed the owning column count"
    );
    for value in [
        json!({"id":"remove","remove":["z"]}),
        json!({"id":"append","put":[{"row":{"id":"m","in":"t","cells":["last"]}}]}),
    ] {
        let commit = committed(&mut scene, &mut budget, value);
        layout = picture(&scene, commit, &layout, &mut Measure::default())
            .expect("row step")
            .after;
    }
    let origins = ["a", "m"].map(|name| {
        layout
            .bounds(&id(name))
            .expect("bounds")
            .expect("row")
            .origin
    });
    assert_eq!(
        origins,
        [Vec2::new(0.0, 31.55), Vec2::new(0.0, 65.05)],
        "new handles append after surviving rows"
    );
    let handles = ["m", "a", "t"].map(|name| layout.handle(&id(name)).expect("handle"));
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"clear","remove":["m","a","t"]}),
    );
    let removed =
        picture(&scene, commit, &layout, &mut Measure::default()).expect("remove table and rows");
    assert_eq!(
        removed.removed, handles,
        "whole-table removals retain their exact activation handles"
    );
    assert!(
        removed.after.items().is_empty(),
        "no removed parent is rebuilt"
    );
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn measure_prepare_table_at_column_and_row_caps() {
    let mut samples = Vec::new();
    for iteration in 0..69 {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let mut layout = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let table = committed(
            &mut scene,
            &mut budget,
            json!({"id":"table","put":[{"table":{"id":"t","box":[0,0,1600,1000],"cols":["A","B","C","D","E","F","G","H"]}}]}),
        );
        layout = picture(&scene, table, &layout, &mut measure)
            .expect("table")
            .after;
        for row in 0..40 {
            let cells: Vec<_> = (0..8).map(|_| Value::String("x".repeat(400))).collect();
            let commit = committed(
                &mut scene,
                &mut budget,
                json!({"id":format!("step{row}"),"put":[{"row":{"id":format!("r{row}"),"in":"t","cells":cells}}]}),
            );
            layout = picture(&scene, commit, &layout, &mut measure)
                .expect("row at cap")
                .after;
        }
        assert_eq!(scene.usage().cells, 320);
        let commit = committed(
            &mut scene,
            &mut budget,
            json!({"id":"update","set":[{"target":"r0","color":"red"}]}),
        );
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
        .expect("table at caps");
        let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
        result.validate(&scene, &layout).expect("current result");
        assert_eq!(result.display.items.len(), 41);
        if iteration >= 5 {
            samples.push(elapsed);
        }
    }
    println!("prepare_table_cap_raw_us={samples:?}");
    let mean = samples.iter().sum::<f64>() / 64.0;
    let maximum = samples.into_iter().fold(0.0_f64, f64::max);
    println!(
        "prepare_table_8x40_400chars samples=64 warmups=5 mean_us={mean:.3} max_us={maximum:.3}; deterministic measurement; no painter or frame timing"
    );
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn measure_prepare_twelve_tables_at_row_and_column_caps() {
    let mut samples = Vec::new();
    for iteration in 0..69 {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let mut layout = LayoutState::new(&scene);
        let mut measure = Measure::default();
        for table in 0..12 {
            let mut put = vec![
                json!({"table":{"id":format!("t{table}"),"box":[0,table*1500,1000,1000],"cols":vec!["C";8]}}),
            ];
            put.extend((0..40).map(|row| json!({"row":{"id":format!("r{table}_{row}"),"in":format!("t{table}"),"cells":vec!["v";8]}})));
            let commit = committed(
                &mut scene,
                &mut budget,
                json!({"id":format!("table{table}"),"put":put}),
            );
            layout = picture(&scene, commit, &layout, &mut measure)
                .expect("table fixture")
                .after;
        }
        assert_eq!(
            scene.usage().cells,
            3840,
            "the fixture occupies twelve bounded tables"
        );
        let set: Vec<_> = (0..12)
            .map(|table| json!({"target":format!("r{table}_0"),"color":"red"}))
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
        .expect("many-table update");
        let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
        result.validate(&scene, &layout).expect("current result");
        assert_eq!(
            result.display.items.len(),
            492,
            "all twelve table headers and 480 rows are replaced"
        );
        if iteration >= 5 {
            samples.push(elapsed);
        }
    }
    println!("prepare_twelve_tables_raw_us={samples:?}");
    let mean = samples.iter().sum::<f64>() / 64.0;
    let maximum = samples.into_iter().fold(0.0_f64, f64::max);
    println!(
        "prepare_12_tables_40x8_short samples=64 warmups=5 mean_us={mean:.3} max_us={maximum:.3}; deterministic measurement; no painter or frame timing"
    );
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn measure_prepare_charts_at_mark_and_pie_segment_caps() {
    for (kind, charts, marks, segments) in [("scatter", 5, 20_000, 320), ("pie", 1, 163, 19_886)] {
        let mut samples = Vec::new();
        for iteration in 0..69 {
            let mut scene = SceneState::begin(header()).expect("scene");
            let mut budget = GlobalBudget::default();
            let mut layout = LayoutState::new(&scene);
            let mut measure = Measure::default();
            for chart in 0..charts {
                let mut put = vec![
                    json!({"chart":{"id":format!("c{chart}"),"kind":kind,"box":[0,chart*350,800,300]}}),
                ];
                put.extend((0..8).map(|series| {
                    let points = if kind == "scatter" { 500 } else if series == 7 { 23 } else { 20 };
                    json!({"series":{"id":format!("s{chart}_{series}"),"in":format!("c{chart}"),"data":vec![1;points]}})
                }));
                let commit = committed(
                    &mut scene,
                    &mut budget,
                    json!({"id":format!("chart{chart}"),"put":put}),
                );
                layout = picture(&scene, commit, &layout, &mut measure)
                    .expect("capped charts")
                    .after;
            }
            assert_eq!(
                scene.usage().marks,
                marks,
                "scatter fills the scene mark cap; pie is segment-limited"
            );
            assert_eq!(
                scene.usage().segments,
                segments,
                "one additional pie slice would exceed 20,000 segments"
            );
            let set: Vec<_> = (0..charts)
                .flat_map(|chart| {
                    (0..8).map(
                        move |series| json!({"target":format!("s{chart}_{series}"),"color":"red"}),
                    )
                })
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
            .expect("chart update");
            let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
            result.validate(&scene, &layout).expect("current result");
            assert_eq!(
                result.display.items.len(),
                charts * 9,
                "every chart and its eight series are replaced"
            );
            if iteration >= 5 {
                samples.push(elapsed);
            }
        }
        println!("prepare_{kind}_cap_raw_us={samples:?}");
        let mean = samples.iter().sum::<f64>() / 64.0;
        let maximum = samples.into_iter().fold(0.0_f64, f64::max);
        println!(
            "prepare_{kind}_cap samples=64 warmups=5 marks={marks} segments={segments} mean_us={mean:.3} max_us={maximum:.3}; deterministic measurement; no painter or frame timing"
        );
    }
}

fn recorded(put: &[Value]) -> SceneState {
    let mut scene = SceneState::begin(header()).expect("scene");
    committed(
        &mut scene,
        &mut GlobalBudget::default(),
        json!({"id":"only","put":put}),
    );
    scene
}

#[test]
fn batch_recovery_keeps_input_order_and_charges_only_what_it_returns() {
    let label = |id: &str, text: &str| json!({"text":{"id":id,"at":[100,100],"text":text}});
    let scenes = [
        recorded(&[label("a", "A")]),
        recorded(&[label("b", "fail-measure")]),
        recorded(&[label("c", "C"), label("d", "D")]),
        recorded(&[label("e", "E")]),
        recorded(&[label("f", "F")]),
        recorded(&[label("g", "G")]),
    ];
    let header = header();
    let sources: Vec<_> = scenes
        .iter()
        .map(|scene| (&header, scene.recovery_log()))
        .collect();
    let (results, mut global) = recover_all(
        &sources,
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
    );
    let counts: Vec<_> = results
        .iter()
        .map(|result| {
            result
                .as_ref()
                .ok()
                .map(|(scene, layout)| (scene.object_count(), layout.items().len()))
        })
        .collect();
    assert_eq!(
        counts,
        [
            Some((1, 1)),
            None,
            Some((2, 2)),
            Some((1, 1)),
            Some((1, 1)),
            None
        ]
    );
    let Err(failed) = &results[1] else {
        panic!("a failed measurement fails its scene");
    };
    assert_eq!(failed.revision, 1);
    assert!(matches!(failed.error, PrepareError::TextMeasure));
    let Err(over) = &results[5] else {
        panic!("a fifth recovered scene exceeds the visible cap");
    };
    assert_eq!(over.revision, 0);
    assert!(matches!(
        &over.error,
        PrepareError::Layout(error) if error.code == ErrorCode::OverBudget(Budget::VisibleScenes)
    ));
    assert_eq!(
        (global.usage().objects, global.usage().scenes),
        (5, 4),
        "neither the failed nor the rejected scene is charged"
    );
    let (mut first, _) = results
        .into_iter()
        .next()
        .expect("first result")
        .expect("first scene");
    assert_eq!(
        first
            .clear(&mut GlobalBudget::default())
            .expect_err("a foreign budget")
            .code,
        ErrorCode::BudgetContext
    );
    first.clear(&mut global).expect("the batch budget");
    assert_eq!((global.usage().objects, global.usage().scenes), (4, 3));
}

/// The step's own vectors and empty private-fact allocation, which leave with its ticket.
fn own(step: &PreparedStep) -> usize {
    step.display.items.capacity() * size_of::<Item>()
        + step.removed.capacity() * size_of::<Handle>()
        + step.glides.capacity() * size_of::<Glide>()
        + 2 * size_of::<usize>()
        + size_of::<std::collections::BTreeMap<usize, usize>>()
}

#[test]
fn retained_bytes_match_step_charges_and_recovery() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let empty = LayoutState::new(&scene);
    assert_eq!(
        empty.resident_bytes(),
        2 * size_of::<usize>(),
        "an empty prefix holds only its version allocation"
    );
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"shared","put":[
            {"chart":{"id":"c","kind":"bar","box":[0,0,400,300],"categories":["Q","R"],
                "x":{"min":-0.5,"max":1.5},"y":{"min":-10,"max":10}}},
            {"series":{"id":"s","in":"c","data":[2,-2]}},
            {"series":{"id":"t","in":"c","data":[3,-3]}},
            {"path":{"id":"p","d":"M0 0 Q4 8 8 0","fill":"green"}}
        ]}),
    );
    let first = picture(&scene, commit, &empty, &mut measure).expect("shared geometry");
    assert_eq!(
        first.after.resident_bytes() + own(&first),
        first.resident_bytes,
        "a first step charges everything it retains, with the plot and vertices once"
    );
    let commit = committed(&mut scene, &mut budget, json!({"id":"nothing","put":[]}));
    let unchanged = picture(&scene, commit, &first.after, &mut measure).expect("same geometry");
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"more","put":[{"text":{"id":"x","at":[10,10],"text":"X"}}]}),
    );
    let second = picture(&scene, commit, &unchanged.after, &mut measure).expect("added text");
    assert_eq!(
        second.after.resident_bytes() + unchanged.resident_bytes + own(&second),
        unchanged.after.resident_bytes() + second.resident_bytes + own(&unchanged),
        "a step adds its new geometry, source and index to the shared prefix"
    );
    let (_, recovered) = recover(
        header(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure::default(),
        &AtomicBool::new(false),
        None,
    )
    .expect("recovery");
    assert_eq!(
        recovered.resident_bytes(),
        second.after.resident_bytes(),
        "recovery charges what the live layout retains"
    );
}

#[test]
fn a_cut_rejects_a_layout_prepared_past_its_kept_prefix() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let before = LayoutState::new(&scene);
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"x","at":[100,100],"text":"X"}}]}),
    );
    let first = picture(&scene, commit, &before, &mut measure).expect("first picture");
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"edit","set":[{"target":"x","text":"Longer"}]}),
    );
    let edited = picture(&scene, commit, &first.after, &mut measure).expect("edited picture");
    cut(&mut scene, &mut budget, 1, &["x"], &first.after);
    assert!(
        matches!(
            edited.after.cut(&scene),
            Err(PrepareError::Layout(error)) if error.code == ErrorCode::OutOfRange
        ),
        "the dropped edit must not survive the cut"
    );
    let kept = first.after.cut(&scene).expect("activated prefix");
    assert_eq!(
        kept.bounds(&id("x")).expect("kept bounds"),
        first.after.bounds(&id("x")).expect("first bounds")
    );
}

#[test]
fn a_stale_join_hands_the_ticket_back_for_its_predecessor() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let before = LayoutState::new(&scene);
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[{"text":{"id":"x","at":[100,100],"text":"X"}}]}),
    );
    let first = picture(&scene, commit, &before, &mut measure).expect("first picture");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"text":{"id":"y","at":[200,100],"text":"Y"}}]}),
    );
    let Err((PrepareError::Stale, ticket)) = PrepareInput::new(
        next.preparation,
        before,
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    ) else {
        panic!("a skipped predecessor is stale");
    };
    let input = PrepareInput::new(
        ticket,
        first.after.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("the returned ticket joins its predecessor");
    let prepared = prepare(input, &mut measure, &AtomicBool::new(false)).expect("picture");
    prepared
        .validate(&scene, &first.after)
        .expect("current picture");
    assert_eq!(prepared.after.items().len(), 2);
}

/// The regular star polygon {n/k} as path data: n points, each edge crossing 2(k − 1) others.
fn star_path(n: u32, k: u32) -> String {
    use std::fmt::Write as _;
    let mut d = String::new();
    for i in 0..n {
        let angle = std::f64::consts::TAU * f64::from(i * k % n) / f64::from(n);
        let command = if i == 0 { 'M' } else { 'L' };
        write!(
            d,
            "{command}{:.2} {:.2} ",
            1000.0 * angle.cos(),
            1000.0 * angle.sin()
        )
        .expect("a String accepts formatted text");
    }
    d.push('Z');
    d
}

#[test]
fn preparation_counts_paint_work_once_and_refuses_an_unbuildable_fill() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"work","put":[
            {"path":{"id":"dash","d":"M0 0 L100 0","dash":true}},
            {"shape":{"id":"rect","kind":"rect","box":[0,0,100,60],"radius":10,"fill":"blue"}},
            {"path":{"id":"star","at":[1000,1000],"d":star_path(257, 85),"fill":"blue"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("buildable paint work");
    // The dash: floor(100 / 10) + 2. The rect: radius 10 at the inline scale needs 4 pieces per
    // corner, so 4 · 5 + 1 points, filled and bordered. The star: 257 points + 257 · 84
    // crossings, exactly one build.
    assert_eq!(
        first.after.paint_work(),
        kumochi_scene::display::PaintWork {
            dash_pieces: 12,
            fill_vertices: 21 + 21_845,
            largest_fill: 21_845,
            outline_points: 21,
            largest_outline: 21,
        }
    );
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"over","put":[{"path":{"id":"over","at":[1000,1000],"d":star_path(331, 66),"fill":"blue"}}]}),
    );
    // 331 points + 331 · 65 crossings is one vertex past a single build.
    assert!(matches!(
        picture(&scene, next, &first.after, &mut measure),
        Err(PrepareError::OverResidency)
    ));
}

#[test]
fn generated_fill_points_refuse_before_crossing_count_above_the_global_cap() {
    for curves in [63, 64] {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let d = format!("M0 0 {}Z", "q5 20 10 0 ".repeat(curves));
        let commit = committed(
            &mut scene,
            &mut budget,
            json!({"id":"curves","put":[{"path":{"id":"fill","d":d,"fill":"blue"}}]}),
        );
        let prepared = picture(&scene, commit, &before, &mut Measure::default());
        if curves == 63 {
            let prepared = prepared.expect("505 generated points fit the counting bound");
            let Prim::Fill { points, .. } = &prepared.display.items[0].primitives[0] else {
                panic!("prepared fill");
            };
            assert_eq!(points.len(), 505);
        } else {
            assert!(matches!(prepared, Err(PrepareError::OverResidency)));
        }
    }
}

#[test]
fn a_replaced_item_is_recounted_and_an_unchanged_one_keeps_its_count() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"three","put":[
            {"path":{"id":"kept","d":"M0 0 L100 0","dash":true}},
            {"chart":{"id":"c","kind":"scatter","box":[0,0,400,300]}},
            {"series":{"id":"s","in":"c","data":[1,2,3]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("three markers");
    let three = first.after.paint_work();
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"two","set":[{"target":"s","data":[3,1]}]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("two markers");
    let two = next.after.paint_work();
    // A scatter marker is a filled 8-unit rect of radius 4: 3 pieces per corner at the inline
    // scale, so 4 · 4 + 1 points. The untouched dashed path keeps floor(100 / 10) + 2.
    assert_eq!(three.fill_vertices - two.fill_vertices, 17);
    assert_eq!((three.dash_pieces, two.dash_pieces), (12, 12));
    assert_eq!(
        first.after.paint_work(),
        three,
        "the earlier layout keeps its own counts"
    );
}

#[test]
fn a_later_ancestor_track_recharges_shared_rects_and_refuses_growth_past_the_cap() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut measure = Measure::default();
    let before = LayoutState::new(&scene);
    let capped =
        |committed: kumochi_scene::Committed, before: &LayoutState, measure: &mut Measure| {
            let input = PrepareInput::new(
                committed.preparation,
                before.clone(),
                SurfaceGeometry::default(),
                Arc::new(Theme::default()),
            )
            .map_err(|(error, _)| error)
            .expect("the step follows its base")
            .with_paint_cap(Some(PaintWork::LIMITS));
            prepare(input, measure, &AtomicBool::new(false))
        };
    let scaled = |scale: u32| json!({"id":format!("scale{scale}"),"put":[{"anim":{"target":"d","track":"scale","keys":[[0,scale]]}}]});
    let mut put = vec![json!({"diagram":{"id":"d"}})];
    put.extend((0..8).map(|index| {
        json!({"shape":{"id":format!("r{index}"),"in":"d","kind":"rect","box":[index * 20,0,16,16],"radius":8,"fill":"blue"}})
    }));
    let first = committed(&mut scene, &mut budget, json!({"id":"rects","put":put}));
    let first = capped(first, &before, &mut measure).expect("eight rects");
    let next = committed(&mut scene, &mut budget, scaled(4));
    let next = capped(next, &first.after, &mut measure).expect("a scaled ancestor");
    let rect = |layout: &LayoutState| {
        layout
            .items()
            .into_iter()
            .find(|item| !item.primitives.is_empty())
            .expect("a rect")
    };
    assert!(Arc::ptr_eq(
        &rect(&first.after).primitives,
        &rect(&next.after).primitives
    ));
    let mut frame = Frame::new();
    SceneView::new(&next.after, 0)
        .frame(0, &mut frame)
        .expect("a fresh frame");
    let charges: Vec<u32> = frame
        .items()
        .iter()
        .filter(|item| !item.primitives.is_empty())
        .map(|item| item.paint_scale.to_bits())
        .collect();
    // The envelope's 4 at the inline limit of 1, with the 2⁻¹⁶ allowance.
    assert_eq!(charges, vec![(4.0_f32 + 1.0 / 16_384.0).to_bits(); 8]);
    // Radius 8 needs 4 pieces per corner at 1 and 7 at 4: 4 · 5 + 1 and 4 · 8 + 1 points, filled
    // and bordered. At 32 it needs 18, so eight rects of 77 points pass the cap.
    let work = |points: usize| PaintWork {
        dash_pieces: 0,
        fill_vertices: 8 * points,
        largest_fill: points,
        outline_points: 8 * points,
        largest_outline: points,
    };
    assert_eq!(first.after.paint_work(), work(21));
    assert_eq!(next.after.paint_work(), work(33));
    let over = committed(&mut scene, &mut budget, scaled(32));
    assert!(matches!(
        capped(over, &next.after, &mut measure),
        Err(PrepareError::OverResidency)
    ));
    assert_eq!(
        first.after.paint_work(),
        work(21),
        "the earlier prefix keeps its own work"
    );
    assert_eq!(next.after.paint_work(), work(33));
}

/// A convex polygon of `count` points on a circle: no crossings, so it charges its points.
fn convex(id: &str, count: u32) -> Value {
    let points: Vec<[f64; 2]> = (0..count)
        .map(|index| {
            let angle = std::f64::consts::TAU * f64::from(index) / f64::from(count);
            [1_000.0 + 900.0 * angle.cos(), 1_000.0 + 900.0 * angle.sin()]
        })
        .collect();
    json!({"shape":{"id":id,"kind":"polygon","points":points,"fill":"blue"}})
}

fn fill_scene(put: &[Value]) -> (SceneState, kumochi_scene::Committed) {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let commit = committed(&mut scene, &mut budget, json!({"id":"fills","put":put}));
    (scene, commit)
}

#[test]
fn bounded_preparation_refuses_once_the_scene_s_own_work_passes_the_cap() {
    let cap = Some(kumochi_scene::display::PaintWork::LIMITS);
    for (put, bounded) in [
        (vec![convex("a", 512)], Some(512)),
        (vec![convex("a", 300), convex("b", 212)], Some(512)),
        (vec![convex("a", 300), convex("b", 213)], None),
    ] {
        let (scene, commit) = fill_scene(&put);
        let before = LayoutState::new(&scene);
        let input = PrepareInput::new(
            commit.preparation,
            before,
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .map_err(|(error, _)| error)
        .expect("the first step follows the empty layout")
        .with_paint_cap(cap);
        let prepared = prepare(input, &mut Measure::default(), &AtomicBool::new(false));
        match bounded {
            Some(fill) => assert_eq!(
                prepared
                    .expect("within the cap")
                    .after
                    .paint_work()
                    .fill_vertices,
                fill
            ),
            None => assert!(matches!(prepared, Err(PrepareError::OverResidency))),
        }
    }
    let (scene, commit) = fill_scene(&[convex("a", 300), convex("b", 213)]);
    let exact = picture(
        &scene,
        commit,
        &LayoutState::new(&scene),
        &mut Measure::default(),
    )
    .expect("unbounded preparation stays exact");
    assert_eq!(exact.after.paint_work().fill_vertices, 513);
}

#[test]
fn bounded_recovery_refuses_a_scene_over_the_cap() {
    let (scene, _) = fill_scene(&[convex("a", 300), convex("b", 213)]);
    let log = scene.recovery_log();
    for (cap, refused) in [
        (Some(kumochi_scene::display::PaintWork::LIMITS), true),
        (None, false),
    ] {
        let (results, _) = recover_all(
            &[(&header(), log)],
            SurfaceGeometry::default(),
            &Arc::new(Theme::default()),
            &mut Measure::default(),
            &AtomicBool::new(false),
            cap,
        );
        let [result] = results.as_slice() else {
            panic!("one result per scene");
        };
        if refused {
            assert!(matches!(
                result,
                Err(kumochi_scene::layout::RecoveryError {
                    revision: 1,
                    error: PrepareError::OverResidency,
                })
            ));
        } else {
            let (_, layout) = result.as_ref().expect("unbounded recovery stays exact");
            assert_eq!(layout.paint_work().fill_vertices, 513);
        }
    }
}

#[test]
fn bounded_second_step_counts_reused_fill_work_before_new_objects() {
    for (count, accepted) in [(212, true), (213, false)] {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let first = committed(
            &mut scene,
            &mut budget,
            json!({"id":"first","put":[convex("a", 300)]}),
        );
        let first = picture(&scene, first, &before, &mut measure).expect("first fill");
        assert_eq!(first.after.paint_work().fill_vertices, 300);
        let next = committed(
            &mut scene,
            &mut budget,
            json!({"id":"next","put":[convex("b", count)]}),
        );
        let input = PrepareInput::new(
            next.preparation,
            first.after.clone(),
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("next step follows the first layout")
        .with_paint_cap(Some(kumochi_scene::display::PaintWork::LIMITS));
        let result = prepare(input, &mut measure, &AtomicBool::new(false));
        if accepted {
            let next = result.expect("300 reused points plus 212 new points fit");
            next.validate(&scene, &first.after)
                .expect("current picture");
            assert_eq!(next.after.paint_work().fill_vertices, 512);
            let Prim::Fill { points: old, .. } = &first.after.items()[0].primitives[0] else {
                panic!("original fill");
            };
            let Prim::Fill { points: reused, .. } = &next.after.items()[0].primitives[0] else {
                panic!("reused fill");
            };
            assert!(Arc::ptr_eq(old, reused));
        } else {
            assert!(matches!(result, Err(PrepareError::OverResidency)));
        }
        assert_eq!(first.after.paint_work().fill_vertices, 300);
    }
}
