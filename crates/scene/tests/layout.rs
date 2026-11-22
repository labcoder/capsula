//! Incremental placement, preparation isolation and ordered-cut recovery.
use kumochi_scene::{
    Admission, Begin, Budget, ErrorCode, Frame, GlobalBudget, Handle, Record, SceneState,
    SceneView, Step, StopCut, WireId,
    display::{Item, MeasureError, PaintWork, Prim, TextMeasure, TextStyle},
    geom::{Rect, Vec2},
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover,
        recover_all,
    },
    limits::MAX_GLOBAL_FILL_VERTICES,
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
fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.001, "{actual} vs {expected}");
}

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("valid world bounds")
        .expect("prepared bounds")
}

fn item(layout: &LayoutState, name: &str) -> Item {
    let handle = layout.handle(&id(name)).expect("prepared handle");
    layout
        .items()
        .into_iter()
        .find(|item| item.handle == handle)
        .expect("prepared item")
}

fn assert_removed(step: &PreparedStep, before: &LayoutState, names: &[&str]) {
    let mut expected = names
        .iter()
        .map(|name| before.handle(&id(name)).expect("old handle"))
        .collect::<Vec<Handle>>();
    let mut actual = step.removed.clone();
    expected.sort_unstable();
    actual.sort_unstable();
    assert_eq!(
        actual, expected,
        "exact removal handles, independent of order"
    );
}

fn frame(layout: &LayoutState) -> Frame {
    let mut frame = Frame::new();
    SceneView::new(layout, 0)
        .frame(u64::MAX, &mut frame)
        .expect("resting frame");
    frame
}

fn open_stroke(item: &Item) -> &[Vec2] {
    item.primitives
        .iter()
        .find_map(|p| match p {
            Prim::Line {
                points,
                closed: false,
                color,
                width,
                ..
            } if color.opacity > 0.0 && *width > 0.0 => Some(points.as_ref()),
            _ => None,
        })
        .expect("open stroke")
}

fn text<'a>(item: &'a Item, value: &str) -> &'a Prim {
    item.primitives.iter().find(|p| matches!(p, Prim::Text { text, color, .. } if text.as_str() == value && color.opacity > 0.0)).expect("painted text")
}

fn filled_rects(item: &Item) -> Vec<Rect> {
    item.primitives
        .iter()
        .filter_map(|p| match p {
            Prim::Rect {
                rect,
                fill: Some(paint),
                ..
            } if paint.opacity > 0.0 => Some(*rect),
            _ => None,
        })
        .collect()
}

fn contour_segments(item: &Item) -> usize {
    item.primitives
        .iter()
        .map(|p| match p {
            Prim::Line { points, closed, .. } => {
                points.len().saturating_sub(1) + usize::from(*closed)
            }
            Prim::Fill { points, .. }
                if !item.primitives.iter().any(
                    |p| matches!(p, Prim::Line { points: stroke, .. } if stroke == points),
                ) =>
            {
                points.len()
            }
            _ => 0,
        })
        .sum()
}

fn ink(primitive: &Prim) -> Rect {
    use kumochi_scene::record::Align;
    let Prim::Text {
        origin,
        text,
        style,
        max_width,
        align,
        ..
    } = primitive
    else {
        panic!("text primitive");
    };
    let [w, h] = Measure::default()
        .measure(text.as_str(), *style, *max_width)
        .expect("ink bounds");
    let spare = max_width.unwrap_or(w) - w;
    let x = match align {
        Align::Left => 0.0,
        Align::Center => spare * 0.5,
        Align::Right => spare,
    };
    Rect {
        origin: *origin + Vec2::new(x, 0.0),
        size: Vec2::new(w, h),
    }
}

fn chart_lines(chart: &Item, rgb: kumochi_scene::record::Rgb) -> Vec<[Vec2; 2]> {
    chart
        .primitives
        .iter()
        .filter_map(|p| match p {
            Prim::Line {
                points,
                color,
                width,
                closed: false,
                ..
            } if color.rgb == rgb && color.opacity > 0.0 && *width > 0.0 => {
                let a = *points.first().expect("line start");
                let b = *points.last().expect("line end");
                assert!(a.x < b.x, "the grid spans the plot");
                assert!(
                    points.iter().all(|p| (p.y - a.y).abs() < 0.001),
                    "the grid line is horizontal"
                );
                Some([a, b])
            }
            _ => None,
        })
        .collect()
}

fn zero_axis(chart: &Item) -> [Vec2; 2] {
    chart_lines(chart, Theme::default().accent)
        .into_iter()
        .next()
        .expect("painted zero baseline")
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
    let second_items = second.after.items();
    let third = picture(&scene, removal, &second.after, &mut measure).expect("removal picture");
    assert_eq!(
        second.after.items(),
        second_items,
        "the earlier output still has both nodes and the edge"
    );
    assert_eq!(third.after.handle(&id("b")), None);
    assert_eq!(third.after.handle(&id("e")), None);
    assert_removed(&third, &second.after, &["e", "b"]);
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
    assert_eq!(current.primitives, original.primitives);
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
    assert_eq!(Some(next.glides[0].target), next.after.handle(&id("d")));
    let node = bounds(&next.after, "overflow");
    let callout = item(&next.after, "callout");
    let leader = open_stroke(&callout);
    assert_eq!(
        leader.last(),
        Some(&Vec2::new(node.center().x, node.max().y))
    );
    let start = leader.first().expect("callout end");
    assert!(start.y > node.max().y);
    near(start.x, 320.0);
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
            {"highlight":{"id":"h","target":{"ref":"d"},"pad":9}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default())
        .expect("same-step diagram highlight");
    let node = bounds(&first.after, "a");
    assert_eq!(bounds(&first.after, "d"), node);
    let pad = Vec2::new(9.0, 9.0);
    let highlight = bounds(&first.after, "h");
    assert_eq!(highlight.origin, node.origin - pad);
    assert_eq!(highlight.max(), node.max() + pad);
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
    let [a, b, c] = ["a", "b", "c"].map(|name| bounds(&next.after, name));
    assert!(a.max().y < b.origin.y);
    assert!(b.max().y < c.origin.y);
    near(b.center().x, c.center().x);
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
    let mut glides = next
        .glides
        .iter()
        .map(|glide| glide.target)
        .collect::<Vec<_>>();
    let mut expected = ["d", "e"].map(|name| next.after.handle(&id(name)).expect("group"));
    glides.sort_unstable();
    expected.sort_unstable();
    assert_eq!(glides, expected);
    let survivor = bounds(&next.after, "overflow");
    cut(&mut scene, &mut budget, 2, &["overflow"], &next.after);
    let tip = next.after.cut(&scene).expect("glided detachment");
    assert_eq!(tip.items().len(), 1);
    assert_eq!(tip.handle(&id("d")), None);
    assert_eq!(tip.handle(&id("e")), None);
    assert_eq!(
        tip.handle(&id("overflow")),
        next.after.handle(&id("overflow"))
    );
    assert_eq!(bounds(&tip, "overflow"), survivor);
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
    use kumochi_scene::record::Surface;
    for surface in [
        SurfaceGeometry::default(),
        SurfaceGeometry::Overlay {
            default_canvas: [600.0, 400.0],
        },
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
        let [a, b] = ["a", "b"].map(|name| bounds(&result.after, name));
        match surface {
            SurfaceGeometry::Inline { .. } => {
                near(a.center().x, 180.0);
                near(b.center().x, 180.0);
                assert!(b.origin.y > a.max().y);
            }
            SurfaceGeometry::Overlay { .. } => {
                near(a.center().y, b.center().y);
                assert!(b.origin.x > a.max().x);
                near(a.union(b).center().x, 300.0);
            }
        }
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
        json!({"id":"PRIVATE_STEP","put":[{"text":{"id":"PRIVATE_ID","at":[12345,17321],"text":"PRIVATE_TEXT"}}]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("picture");
    let label = item(&first.after, "PRIVATE_ID");
    let prim = text(&label, "PRIVATE_TEXT");
    for value in [
        format!("{first:?}"),
        format!("{:?}", first.display),
        format!("{label:?}"),
        format!("{prim:?}"),
    ] {
        for sentinel in ["PRIVATE_STEP", "PRIVATE_ID", "PRIVATE_TEXT"] {
            assert!(!value.contains(sentinel));
            assert!(!value.contains(&format!("{:?}", sentinel.as_bytes())));
        }
        for coordinate in ["12345", "17321", "17316"] {
            assert!(!value.contains(coordinate));
        }
        for point in [[12_345.0_f32, 17_321.0], [12_345.0, 17_316.0]] {
            assert!(!value.contains(&format!("{point:?}")));
            let bytes: Vec<_> = point.into_iter().flat_map(f32::to_le_bytes).collect();
            assert!(!value.contains(&format!("{bytes:?}")));
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
    let recolor = |path| {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let authored = json!({"id":"first","put":[{"path":{"id":"p","d":path,"fill":"green"}}]});
        let first = committed(&mut scene, &mut budget, authored.clone());
        let first = picture(&scene, first, &before, &mut measure).expect("path");
        assert_eq!(first.step(), &step(authored));
        let next = committed(
            &mut scene,
            &mut budget,
            json!({"id":"recolor","set":[{"target":"p","color":"red"}]}),
        );
        let next =
            picture(&scene, next, &first.after, &mut measure).expect("repaint existing geometry");
        let old = item(&first.after, "p");
        let new = item(&next.after, "p");
        assert_eq!(open_stroke(&old), open_stroke(&new));
        assert!(new.primitives.iter().any(|p| matches!(p, Prim::Line { color, width, .. } if color.rgb.get() == 0x00ff_6b7d && color.opacity > 0.0 && *width > 0.0)));
        assert!(new.primitives.iter().any(|p| matches!(p, Prim::Fill { color, .. } if color.rgb.get() == 0x00ff_6b7d && color.opacity > 0.0)));
        (next.resident_bytes, open_stroke(&new).len())
    };
    let (small, few) = recolor("M0 0 L4 8 L8 0");
    let (large, many) = recolor("M0 0 Q4 8 8 0 ");
    assert!(many > few, "the control has different vertex backing");
    assert_eq!(large, small, "recolor does not charge new vertex backing");
}

#[test]
fn set_color_recolors_a_filled_shape_and_leaves_an_unfilled_one_unfilled() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"first","put":[
            {"shape":{"id":"puff","kind":"circle","at":[50,50],"r":20,"color":"#ffffff","fill":"#ffffff","width":1}},
            {"shape":{"id":"ring","kind":"circle","at":[120,50],"r":20,"color":"#ffffff"}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut measure).expect("shapes");
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"red","set":[{"target":"puff","color":"#ff0000"},{"target":"ring","color":"#ff0000"}]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("recolored");
    for (name, filled) in [("puff", true), ("ring", false)] {
        let shape = item(&next.after, name);
        let mut fills = 0;
        let mut outlines = 0;
        for primitive in &*shape.primitives {
            let (fill, stroke) = match primitive {
                Prim::Rect { fill, border, .. } => (
                    *fill,
                    border.filter(|(_, w)| *w > 0.0).map(|(color, _)| color),
                ),
                Prim::Fill { color, .. } => (Some(*color), None),
                Prim::Line { color, width, .. } if *width > 0.0 => (None, Some(*color)),
                _ => (None, None),
            };
            for (paint, count) in [(fill, &mut fills), (stroke, &mut outlines)] {
                if let Some(paint) = paint.filter(|paint| paint.opacity > 0.0) {
                    assert_eq!(paint.rgb.get(), 0x00ff_0000, "{name}");
                    *count += 1;
                }
            }
        }
        assert_eq!(fills > 0, filled, "{name}");
        assert!(outlines > 0, "{name}");
    }
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
fn node_labels_and_placement_are_deterministic_and_cancellation_is_atomic() {
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
        let layout = picture(&scene, first, &before, &mut measure)
            .expect("deterministic picture")
            .after;
        let items = layout.items();
        [("z", "Z"), ("a", "A")].map(|(name, label)| {
            let id = id(name);
            let handle = layout.handle(&id).expect("prepared node");
            let item = items
                .iter()
                .find(|item| item.handle == handle)
                .expect("node item");
            assert!(
                item.primitives.iter().any(|prim| matches!(prim,
                Prim::Text { text, .. } if text.as_str() == label)),
                "node {name} keeps its authored label"
            );
            layout
                .bounds(&id)
                .expect("valid bounds")
                .expect("node bounds")
        })
    };
    let first = run();
    assert_ne!(
        first[0].origin, first[1].origin,
        "the two nodes have separate placements"
    );
    assert_eq!(
        first,
        run(),
        "repeated preparation preserves each node's position"
    );
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
    let glide = generated.glides[0];
    assert_eq!(Some(glide.target), generated.after.handle(&id("d")));
    let local = item(&generated.after, "c").bounds;
    let authored = 10.0;
    let correction = (local.max().x + authored - 360.0).max(0.0);
    assert!(correction > 0.0, "new node overflows the authored rest");
    assert_vec2_near(glide.to, [authored - correction, 0.0]);
    near(bounds(&generated.after, "c").max().x, 360.0);
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
    let stroke_head_chip = |item: &Item| {
        let mut parts = [None, None, None];
        for primitive in item.primitives.iter() {
            let slot = match primitive {
                Prim::Line { .. } => 0,
                Prim::Fill { .. } => 1,
                Prim::Rect { .. } => 2,
                Prim::Text { .. } => continue,
            };
            parts[slot] = Some(primitive.clone());
        }
        parts.map(|part| part.expect("visible stroke, arrowhead and chip"))
    };
    for bowed in [false, true] {
        let mut scene = SceneState::begin(header()).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut measure = Measure::default();
        let (from, to, label, updated) = if bowed {
            ("b", "a", "y", "z")
        } else {
            ("a", "b", "old", "new")
        };
        let mut put = vec![
            json!({"diagram":{"id":"d","dir":"right","box":[0,0,400,300]}}),
            json!({"node":{"id":"a","in":"d","label":"A"}}),
            json!({"node":{"id":"b","in":"d","label":"B"}}),
            json!({"edge":{"id":"e","in":"d","from":from,"to":to,"label":label}}),
        ];
        if bowed {
            put.push(json!({"edge":{"id":"back","in":"d","from":"a","to":"b","label":"x"}}));
        } else {
            put.push(json!({"anim":{"target":"d","track":"pos","keys":[[0,[20,30]]]}}));
        }
        let first = committed(&mut scene, &mut budget, json!({"id":"first","put":put}));
        let first = picture(&scene, first, &before, &mut measure).expect("edge picture");
        cut(&mut scene, &mut budget, 1, &["e"], &first.after);
        let tip = first.after.cut(&scene).expect("keep detached edge");
        let update = committed(
            &mut scene,
            &mut budget,
            json!({"id":"update","set":[{"target":"e","label":updated}]}),
        );
        let next = picture(&scene, update, &tip, &mut measure).expect("update frozen edge");
        assert_eq!(
            scene.object_count(),
            1,
            "the semantic scene still has only the edge"
        );
        let [old]: [Item; 1] = tip.items().try_into().expect("only the cut edge");
        let [new]: [Item; 1] = next
            .after
            .items()
            .try_into()
            .expect("only the updated edge");
        assert_eq!(next.after.handle(&id("e")), Some(new.handle));
        assert_eq!(
            next.after.bounds(&id("e")).expect("detached world bounds"),
            first.after.bounds(&id("e")).expect("attached world bounds"),
            "the edge keeps its painted bounds when its parents disappear"
        );
        let paint = stroke_head_chip(&new);
        let Prim::Line { points, .. } = &paint[0] else {
            panic!("visible stroke");
        };
        let start = points.first().expect("stroke start");
        let last = points.last().expect("stroke end");
        let has_bow = points
            .iter()
            .any(|p| p.y < start.y.min(last.y) - 0.001 || p.y > start.y.max(last.y) + 0.001);
        assert_eq!(has_bow, bowed, "a bowed stroke stays bowed");
        assert_eq!(
            paint,
            stroke_head_chip(&old),
            "the label-only edit keeps the edge geometry"
        );
        assert!(
            new.primitives
                .iter()
                .any(|prim| matches!(prim, Prim::Text { text, .. } if text.as_str() == updated)),
            "the edge displays the updated label"
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
        .expect("updated detached replay");
        assert_eq!(recovered.items(), [new]);
    }
}

#[test]
fn a_zero_width_dashed_shape_reserves_no_dash_and_draws_no_outline() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    // Its 8,000-long dashed outline needs 513 pieces, one past the global cap.
    let outlined = step(json!({"id":"big","put":[
        {"shape":{"id":"big","kind":"rect","box":[0,0,2000,2000],"fill":"blue","dash":true}}
    ]}));
    assert!(matches!(
        scene.admit(&outlined, &budget),
        Err(error) if error.code == ErrorCode::OverBudget(Budget::DashPieces)
    ));
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"big","put":[
            {"shape":{"id":"big","kind":"rect","box":[0,0,2000,2000],"fill":"blue","width":0,"dash":true}},
            {"shape":{"id":"skybg","kind":"rect","box":[0,0,480,300],"fill":"#BFE8FF","color":"#BFE8FF","width":0}},
            {"shape":{"id":"sun","kind":"circle","at":[400,60],"r":30,"fill":"#FFD700","width":0}}
        ]}),
    );
    assert_eq!(budget.usage().dash_pieces, 0);
    let prepared =
        picture(&scene, first, &before, &mut Measure::default()).expect("no dashed outline");
    assert_eq!(prepared.after.paint_work().dash_pieces, 0);
    let primitives = |name| {
        let handle = prepared.after.handle(&id(name)).expect("shape handle");
        prepared
            .display
            .items
            .iter()
            .find(|item| item.handle == handle)
            .expect("prepared shape")
            .primitives
            .as_ref()
    };
    for (name, rgb) in [
        ("big", 0x007a_a2ff),
        ("skybg", 0x00bf_e8ff),
        ("sun", 0x00ff_d700),
    ] {
        let shape = primitives(name);
        assert!(
            shape.iter().any(|prim| match prim {
                Prim::Rect {
                    fill: Some(color), ..
                }
                | Prim::Fill { color, .. } => color.opacity > 0.0 && color.rgb.get() == rgb,
                _ => false,
            }),
            "{name} keeps its fill"
        );
        assert!(
            !shape.iter().any(|prim| match prim {
                Prim::Rect {
                    border: Some((color, width)),
                    ..
                }
                | Prim::Line { color, width, .. } => color.opacity > 0.0 && *width > 0.0,
                _ => false,
            }),
            "{name} has no outline"
        );
    }
}

#[test]
fn a_zero_radius_rect_prepares_as_a_plain_square_cornered_rect() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"rects","put":[
            {"shape":{"id":"square","kind":"rect","box":[20,20,100,60],"radius":0}},
            {"shape":{"id":"plain","kind":"rect","box":[20,20,100,60]}}
        ]}),
    );
    let prepared = picture(&scene, first, &before, &mut Measure::default()).expect("zero radius");
    let outline = |name| {
        let item = item(&prepared.after, name);
        item.primitives
            .iter()
            .find_map(|p| match p {
                Prim::Rect {
                    rect,
                    radius,
                    border: Some((paint, width)),
                    ..
                } if *radius == 0.0 && paint.opacity > 0.0 && *width > 0.0 => Some(*rect),
                _ => None,
            })
            .expect("visible square-cornered outline")
    };
    assert_eq!(outline("square"), outline("plain"));
    assert_eq!(
        outline("square"),
        Rect {
            origin: Vec2::new(20.0, 20.0),
            size: Vec2::new(100.0, 60.0)
        }
    );
}

#[test]
fn procedural_outputs_fit_the_admission_reservations() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut measure = Measure::default();
    let put = json!([
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
    ]);
    let operations = put.as_array().expect("operations");
    let first = committed(
        &mut scene,
        &mut budget,
        json!({"id":"primitives","put":put}),
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
    let mut control = SceneState::begin(header()).expect("reservation control");
    let mut control_budget = GlobalBudget::default();
    for op in operations {
        let name = op
            .as_object()
            .and_then(|tag| tag.values().next())
            .and_then(|fields| fields["id"].as_str())
            .expect("fixture id");
        let previous = control.usage().segments;
        committed(
            &mut control,
            &mut control_budget,
            json!({"id":name,"put":[op]}),
        );
        let allowed = control.usage().segments - previous;
        let item = item(&first.after, name);
        let work = contour_segments(&item);
        assert!(
            work <= allowed,
            "{name}: {work} segments exceed reservation {allowed}"
        );
        assert_procedural_paint(&item, name);
    }
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"no_geometry","put":[]}),
    );
    let next = picture(&scene, next, &first.after, &mut measure).expect("unchanged geometry");
    assert!(
        next.display.items.is_empty(),
        "unchanged geometry is not emitted again"
    );
}

fn assert_procedural_paint(item: &Item, name: &str) {
    let painted = &*item.primitives;
    let strokes: Vec<_> = painted
        .iter()
        .filter_map(|p| match p {
            Prim::Line {
                points,
                closed,
                width,
                color,
                ..
            } if *width > 0.0 && color.opacity > 0.0 => Some((points.as_ref(), *closed)),
            _ => None,
        })
        .collect();
    let fills: Vec<_> = painted
        .iter()
        .filter_map(|p| match p {
            Prim::Fill { points, color } if color.opacity > 0.0 => Some(points.as_ref()),
            _ => None,
        })
        .collect();
    match name {
        "d" => assert!(painted.is_empty(), "structural diagram does not paint"),
        "ring" | "box" | "spotlight" | "marker" => {
            let fill =
                |p: &Prim| matches!(p, Prim::Rect { fill: Some(paint), .. } if paint.opacity > 0.0);
            let outline = |p: &Prim| matches!(p, Prim::Rect { border: Some((paint, width)), .. } if paint.opacity > 0.0 && *width > 0.0);
            assert_eq!(painted.iter().any(fill), name != "ring", "{name} fill");
            assert_eq!(
                painted.iter().any(outline),
                name != "marker",
                "{name} outline"
            );
        }
        "rect" | "circle" | "polygon" | "diamond" | "cylinder" | "note" => {
            assert!(
                strokes.iter().any(|(p, closed)| *closed && p.len() >= 3),
                "{name} contour"
            );
        }
        _ => assert!(
            strokes.iter().any(|(p, closed)| !closed && p.len() >= 2),
            "{name} stroke"
        ),
    }
    if matches!(
        name,
        "polygon" | "arrow" | "path" | "diamond" | "cylinder" | "note" | "edge"
    ) {
        assert!(!fills.is_empty(), "{name} paints its fill or arrowhead");
    }
    match name {
        "arrow" | "edge" => {
            let line = open_stroke(item);
            let ends = [
                line.last().expect("destination"),
                line.first().expect("source"),
            ];
            for tip in ends.into_iter().take(if name == "arrow" { 2 } else { 1 }) {
                assert!(
                    fills
                        .iter()
                        .any(|head| head.len() >= 3 && head.contains(tip)),
                    "{name} has an arrowhead at its port"
                );
            }
        }
        "note" => assert!(
            strokes.iter().any(|(p, closed)| {
                !closed
                    && p.len() >= 3
                    && p.first()
                        .is_some_and(|a| (a.y - item.bounds.origin.y).abs() < 0.001)
                    && p.last()
                        .is_some_and(|b| (b.x - item.bounds.max().x).abs() < 0.001)
            }),
            "note's visible fold joins its top and right edges"
        ),
        "cylinder" => {
            assert!(
                strokes.iter().any(|(p, closed)| !closed
                    && p.len() >= 3
                    && p.iter().any(|p| (p.y - item.bounds.max().y).abs() < 0.05)),
                "the rounded cylinder base reaches its lower bound"
            );
            for x in [item.bounds.origin.x, item.bounds.max().x] {
                assert!(
                    strokes.iter().any(|(p, closed)| !closed
                        && p.iter().all(|point| (point.x - x).abs() < 0.001)
                        && p.first().zip(p.last()).is_some_and(|(a, b)| a.y < b.y)),
                    "each vertical cylinder side is drawn"
                );
            }
        }
        _ => {}
    }
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
    let line = item(&next.after, "l");
    let stroke = open_stroke(&line);
    let parent = next.after.handle(&id("d"));
    assert_eq!(line.parent, parent);
    let frame = frame(&next.after);
    let transform = frame
        .items()
        .iter()
        .find(|i| i.handle == line.handle)
        .expect("painted line")
        .transform;
    assert_eq!(
        transform.point(*stroke.first().expect("source")),
        bounds(&next.after, "a").center()
    );
    assert_eq!(stroke.last(), Some(&Vec2::new(250.0, 100.0)));
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
    let original = bounds(&first.after, "a");
    assert!(original.within_canvas());
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"next","put":[{"node":{"id":"overflow","in":"d","label":"D"}}]}),
    );
    assert!(
        matches!(picture(&scene,next,&first.after,&mut measure),Err(PrepareError::Layout(error)) if error.code==kumochi_scene::ErrorCode::OutOfRange)
    );
    assert_eq!(bounds(&first.after, "a"), original);
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
    assert_eq!(
        table.after.items().len(),
        1,
        "future rows do not paint in the empty table"
    );
    let empty = item(&table.after, "t");
    assert!(filled_rects(&empty).contains(&empty.bounds));
    text(&empty, "Name");
    text(&empty, "Value");
    let rule = open_stroke(&empty);
    assert_eq!(
        rule.first(),
        Some(&Vec2::new(empty.bounds.origin.x, empty.bounds.max().y))
    );
    assert_eq!(rule.last(), Some(&empty.bounds.max()));
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
    let both = bounds(&rows.after, "t");
    let surviving = bounds(&removed.after, "t");
    let row = bounds(&removed.after, "a");
    assert!(surviving.size.y < both.size.y);
    near(surviving.origin.y, empty.bounds.origin.y);
    assert!((surviving.max().y - row.max().y).abs() < 0.001);
    assert!((surviving.size.y - (empty.bounds.size.y + row.size.y)).abs() < 0.001);
    assert_eq!(rows.after.items(), old_items);
    for (name, expected) in [("a", ["A", "1,000"]), ("b", ["B", "true"])] {
        let row = item(&rows.after, name);
        for value in expected {
            text(&row, value);
        }
    }
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
        json!({"id":"cells","set":[{"target":"r","cells":[2,"P"],"color":"red"}]}),
    );
    let next = picture(&scene, next, &tip, &mut measure).expect("detached cells");
    assert_eq!(
        next.after.bounds(&id("r")).expect("updated bounds"),
        Some(expected),
        "a detached update keeps the absorbed ancestor transform"
    );
    let columns = |row: &Item, values: [&str; 2]| {
        values.map(|value| {
            let Prim::Text {
                origin,
                max_width,
                align,
                ..
            } = text(row, value)
            else {
                unreachable!()
            };
            (*origin, *max_width, *align)
        })
    };
    let initial = columns(&item(&first.after, "r"), ["1", ""]);
    assert!(initial[0].0.x < initial[1].0.x);
    assert_eq!(columns(&item(&tip, "r"), ["1", ""]), initial);
    assert_eq!(columns(&item(&next.after, "r"), ["2", "P"]), initial);
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
    let accepted = prepare_case(8000).expect("measured envelope fits");
    assert!(
        accepted
            .after
            .bounds(&id("child"))
            .expect("valid bounds")
            .is_some(),
        "the child is placed when its measured envelope fits"
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
    let error = scene.admit(&align, &budget).expect_err("extra alignment");
    assert_eq!(
        (error.code, error.path.as_str()),
        (ErrorCode::OutOfRange, "put[0].table.align")
    );
    let cells = step(
        json!({"id":"cells","put":[{"table":{"id":"t","cols":["One"]}},{"row":{"id":"r","in":"t","cells":[1,2]}}]}),
    );
    let error = scene.admit(&cells, &budget).expect_err("extra row cell");
    assert_eq!(
        (error.code, error.path.as_str()),
        (ErrorCode::OutOfRange, "put")
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
        let row = item(&layout, "r");
        let Prim::Text { style, color, .. } = text(&row, "1") else {
            unreachable!()
        };
        assert_eq!(
            (style.bold, *color),
            (emphasis, red),
            "the cell follows the row style"
        );
        let fills = row
            .primitives
            .iter()
            .filter_map(|p| match p {
                Prim::Rect {
                    rect,
                    fill: Some(paint),
                    ..
                } if paint.opacity > 0.0 => Some((*rect, *paint)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            !fills.is_empty(),
            emphasis,
            "only emphasized rows paint a band"
        );
        for (band, paint) in fills {
            assert!(
                band.size.x > 0.0 && band.size.y > 0.0,
                "the band has visible area"
            );
            assert_eq!(paint.rgb, red.rgb, "the band follows the authored color");
            assert!(paint.opacity < 1.0, "the band is translucent");
        }
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
            {"row":{"id":"r","in":"t","cells":[1,2,3,4,5,6,7,8]}}
        ]}),
    );
    let first = picture(&scene, first, &before, &mut Measure::default()).expect("eight columns");
    let row = item(&first.after, "r");
    let mut cells = row
        .primitives
        .iter()
        .filter_map(|p| match p {
            Prim::Text {
                origin,
                max_width: Some(width),
                align,
                text,
                color,
                ..
            } if color.opacity > 0.0 => {
                assert_eq!(*align, kumochi_scene::record::Align::Center);
                assert!(*width > 0.0);
                Some((origin.x, origin.x + width, text.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    cells.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(
        cells.iter().map(|(_, _, text)| *text).collect::<Vec<_>>(),
        ["1", "2", "3", "4", "5", "6", "7", "8"]
    );
    assert!(cells.windows(2).all(|pair| pair[0].1 <= pair[1].0));
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
    let ends = |layout: &LayoutState, name| {
        let handle = layout.handle(&id(name)).expect("prepared series");
        let items = layout.items();
        let series = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("series item");
        let points = series
            .primitives
            .iter()
            .find_map(|primitive| match primitive {
                Prim::Line { points, .. } => Some(points),
                _ => None,
            })
            .expect("line series");
        [
            *points.first().expect("first sample"),
            *points.last().expect("last sample"),
        ]
    };
    let [old_zero, old_one] = ends(&first.after, "a");
    assert!(
        old_one.y < old_zero.y,
        "positive values rise in the original plot"
    );
    let [zero, one] = ends(&second.after, "a");
    let [ten, twenty] = ends(&second.after, "b");
    assert!(
        twenty.y < zero.y,
        "positive values rise in the expanded plot"
    );
    assert_vec2_near(ten, [zero.x, zero.y.midpoint(twenty.y)]);
    assert_vec2_near(one, [twenty.x, zero.y + (twenty.y - zero.y) / 20.0]);
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
    let points = |layout: &LayoutState| {
        let frame = frame(layout);
        let s = item(layout, "s");
        let transform = frame
            .items()
            .iter()
            .find(|entry| entry.handle == s.handle)
            .expect("painted series")
            .transform;
        open_stroke(&s)
            .iter()
            .map(|p| transform.point(*p))
            .collect::<Vec<_>>()
    };
    let old = points(&first.after);
    let start = *old.first().expect("first sample");
    let end = *old.last().expect("last sample");
    assert!(start.x < end.x && start.y > end.y);
    let expected_bounds = bounds(&first.after, "s");
    cut(&mut scene, &mut budget, 1, &["s"], &first.after);
    let detached = first.after.cut(&scene).expect("detached series");
    assert_eq!(bounds(&detached, "s"), expected_bounds);
    assert_eq!(points(&detached), old);
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"data","set":[
            {"target":"s","data":[[0,0],[1,2.5],[2,1000]]}
        ]}),
    );
    let next =
        picture(&scene, next, &detached, &mut Measure::default()).expect("frozen data update");
    assert_eq!(bounds(&next.after, "s"), expected_bounds);
    let world = points(&next.after);
    assert_eq!(world.first(), Some(&start));
    assert_eq!(world.last(), Some(&end));
    let middle = start.x.midpoint(end.x);
    assert!(world.iter().any(|p| (p.x - middle).abs() < 0.001
        && (p.y - (start.y + (end.y - start.y) * 0.25)).abs() < 0.001));
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
    let chart = item(&result.after, "c");
    let series = item(&result.after, "s");
    let plot_bottom = open_stroke(&series)
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max);
    let labels: Vec<_> = chart
        .primitives
        .iter()
        .filter_map(|p| match p {
            Prim::Text {
                origin,
                text,
                max_width,
                color,
                ..
            } if origin.y > plot_bottom => {
                assert!(max_width.is_some_and(|width| width.is_finite() && width > 0.0));
                assert!(color.opacity > 0.0);
                let label = ink(p);
                assert_eq!(chart.bounds.union(label), chart.bounds);
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["0"]);
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
        json!({"id":"color","set":[{"target":"c","color":"#ff6b7d"}]}),
    );
    let next = picture(&scene, next, &first.after, &mut Measure::default()).expect("colored chart");
    let items = next.after.items();
    let item = |name| {
        let handle = next.after.handle(&id(name)).expect("prepared handle");
        items
            .iter()
            .find(|item| item.handle == handle)
            .expect("prepared item")
    };
    let zero = item("s")
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Line { points, .. } => points.first().map(|point| point.y),
            _ => None,
        })
        .expect("zero sample");
    assert!(
        item("c")
            .primitives
            .iter()
            .any(|primitive| match primitive {
                Prim::Line { points, color, .. } => {
                    points.len() >= 2
                        && points.iter().all(|point| (point.y - zero).abs() < 0.001)
                        && color.rgb.get() == 0x00ff_6b7d
                        && color.opacity > 0.0
                }
                _ => false,
            }),
        "the accepted chart color reaches its visible zero baseline"
    );
}

#[test]
fn chart_axes_area_and_all_pie_series_stay_within_their_reservations() {
    for (kind, data) in [("area", json!([0, 1, 2])), ("pie", json!([1, 1]))] {
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
        let mut work = 0;
        let mut control = SceneState::begin(header()).expect("reservation control");
        let mut control_budget = GlobalBudget::default();
        for (index, name) in ["c", "a", "b"].into_iter().take(put.len()).enumerate() {
            let item = item(&result.after, name);
            let segments = contour_segments(&item);
            let fills = item
                .primitives
                .iter()
                .filter_map(|p| match p {
                    Prim::Fill { points, color } if points.len() >= 3 && color.opacity > 0.0 => {
                        Some((points.as_ref(), color.rgb.get()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if kind == "area" {
                if name == "c" {
                    zero_axis(&item);
                    assert!(!chart_lines(&item, Theme::default().line).is_empty());
                } else {
                    let top = open_stroke(&item);
                    let start = *top.first().expect("start of area stroke");
                    let end = *top.last().expect("end of area stroke");
                    assert!(start.x < end.x && end.y < start.y, "nonempty rising area");
                    for point in [start, end, Vec2::new(end.x, start.y)] {
                        assert!(
                            fills.iter().any(|(polygon, _)| polygon.contains(&point)),
                            "visible area extends from its rising top to the zero baseline"
                        );
                    }
                }
            } else if name != "c" {
                let colors: std::collections::BTreeSet<_> =
                    fills.iter().map(|(_, rgb)| *rgb).collect();
                assert_eq!(colors.len(), 2, "{name} paints its two nonzero slices");
            }
            let previous = control.usage().segments;
            committed(
                &mut control,
                &mut control_budget,
                json!({"id":name,"put":[put[index]]}),
            );
            let allowed = control.usage().segments - previous;
            assert!(segments <= allowed, "{kind}/{name}: {segments} > {allowed}");
            work += segments;
        }
        assert!(work <= scene.usage().segments);
    }
    let pie = |last_series_slices| {
        let mut put = vec![json!({"chart":{"id":"c","kind":"pie","box":[0,0,800,300]}})];
        put.extend((0..8).map(|series| {
            let slices = if series == 7 { last_series_slices } else { 20 };
            json!({"series":{"id":format!("s{series}"),"in":"c","data":vec![1;slices]}})
        }));
        json!({"id":"pie","put":put})
    };
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let overflow = scene
        .admit(&step(pie(24)), &budget)
        .expect_err("164 slices");
    assert_eq!(overflow.code, ErrorCode::OverBudget(Budget::PathSegments));
    let before = LayoutState::new(&scene);
    let commit = committed(&mut scene, &mut budget, pie(23));
    let result = picture(&scene, commit, &before, &mut Measure::default()).expect("163 slices");
    assert_eq!(budget.usage().segments, 19_886);
    for series in 0..8 {
        assert!(
            result
                .after
                .bounds(&id(&format!("s{series}")))
                .expect("valid bounds")
                .is_some(),
            "series {series} prepares under the pie cap"
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
    let items = result.after.items();
    let theme = Theme::default();
    for (name, slots, diagonals) in [
        ("a", [0, 1], [[1.0, -1.0], [1.0, 1.0]]),
        ("b", [2, 3], [[-1.0, 1.0], [-1.0, -1.0]]),
    ] {
        let handle = result.after.handle(&id(name)).expect("series handle");
        let item = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("prepared series");
        let fills: Vec<_> = item
            .primitives
            .iter()
            .filter_map(|primitive| match primitive {
                Prim::Fill { points, color } if color.opacity > 0.0 => {
                    Some((points.as_ref(), color.rgb.get()))
                }
                _ => None,
            })
            .collect();
        let center = item.bounds.center();
        let inset = item.bounds.size.x.min(item.bounds.size.y) * 0.125;
        for (slot, [dx, dy]) in slots.into_iter().zip(diagonals) {
            let probe = center + Vec2::new(dx, dy) * inset;
            let colors: std::collections::BTreeSet<_> = fills
                .iter()
                .filter_map(|&(points, color)| {
                    let crossings = points
                        .iter()
                        .zip(points.iter().cycle().skip(1))
                        .filter(|&(a, b)| {
                            (a.y > probe.y) != (b.y > probe.y)
                                && probe.x < a.x + (probe.y - a.y) * (b.x - a.x) / (b.y - a.y)
                        })
                        .count();
                    (!crossings.is_multiple_of(2)).then_some(color)
                })
                .collect();
            assert_eq!(
                colors,
                std::collections::BTreeSet::from([theme.series(slot).get()]),
                "series {name} has its palette slot {slot} at {probe:?}"
            );
        }
    }
}

#[test]
fn titled_bar_header_places_the_legend_axes_and_category_labels() {
    let result = chart_picture(&titled_bar_fixture());
    let chart = item(&result.after, "c");
    assert!(filled_rects(&chart).contains(&chart.bounds));
    let theme = Theme::default();
    let grid = chart_lines(&chart, theme.line);
    let zero = zero_axis(&chart);
    let top = grid
        .iter()
        .map(|line| line[0].y)
        .fold(f32::INFINITY, f32::min);
    let bottom = grid
        .iter()
        .map(|line| line[0].y)
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(top < zero[0].y && zero[0].y < bottom);
    let title = ink(text(&chart, "T"));
    let mut last_swatch = None;
    for (slot, label) in ["A", "B"].into_iter().enumerate() {
        let legend = ink(text(&chart, label));
        let swatch = chart
            .primitives
            .iter()
            .find_map(|p| match p {
                Prim::Rect {
                    rect,
                    fill: Some(paint),
                    ..
                } if paint.rgb == theme.series(slot) && paint.opacity > 0.0 => Some(*rect),
                _ => None,
            })
            .expect("series-colored legend swatch");
        assert!(title.max().y < legend.origin.y && legend.max().y < top);
        assert!(
            swatch.max().x < legend.origin.x
                && swatch.origin.y < legend.max().y
                && swatch.max().y > legend.origin.y
        );
        if let Some(previous) = last_swatch {
            assert!(previous < swatch.origin.x);
        }
        last_swatch = Some(legend.max().x);
    }
    let plot_width = zero[1].x - zero[0].x;
    for (label, slot) in [("Q", 0.25), ("R", 0.75)] {
        let category = ink(text(&chart, label));
        assert!(category.origin.y > bottom);
        assert!((category.center().x - (zero[0].x + plot_width * slot)).abs() < 0.001);
    }
    let y_axis = ink(text(&chart, "Y"));
    assert!(last_swatch.is_some());
    assert!(y_axis.origin.y < top);
    let x_axis = ink(text(&chart, "X"));
    assert!(x_axis.origin.y > ink(text(&chart, "Q")).max().y);
    assert!((x_axis.center().x - zero[0].x.midpoint(zero[1].x)).abs() < 0.001);
    for (label, fraction) in [
        ("10", 0.0),
        ("5", 0.25),
        ("0", 0.5),
        ("-5", 0.75),
        ("-10", 1.0),
    ] {
        let tick = ink(text(&chart, label));
        let y = top + (bottom - top) * fraction;
        assert!(tick.max().x < zero[0].x && tick.origin.y <= y && tick.max().y >= y);
        assert!(grid.iter().any(|line| (line[0].y - y).abs() < 0.001
            && (line[0].x - zero[0].x).abs() < 0.001
            && (line[1].x - zero[1].x).abs() < 0.001));
    }
    assert!((zero[0].y - top.midpoint(bottom)).abs() < 0.001);
}

#[test]
fn grouped_and_stacked_bars_use_series_slots_and_signed_baselines() {
    for stacked in [false, true] {
        let mut put = titled_bar_fixture();
        put[0]["chart"]["stacked"] = json!(stacked);
        let result = chart_picture(&put);
        let chart = item(&result.after, "c");
        let [start, end] = zero_axis(&chart);
        let [a, b] = ["a", "b"].map(|name| {
            let mut rects = filled_rects(&item(&result.after, name));
            rects.sort_by(|a, b| a.center().x.total_cmp(&b.center().x));
            let rects: [Rect; 2] = rects.try_into().expect("one bar per category");
            rects
        });
        let grid = chart_lines(&chart, Theme::default().line);
        let bottom = grid
            .iter()
            .map(|line| line[0].y)
            .fold(f32::NEG_INFINITY, f32::max);
        for (index, (a, b)) in a.into_iter().zip(b).enumerate() {
            let positive = index == 0;
            let baseline = |bar: Rect| if positive { bar.max().y } else { bar.origin.y };
            let end_y = |bar: Rect| if positive { bar.origin.y } else { bar.max().y };
            let slot = if positive { 0.25 } else { 0.75 };
            assert!((baseline(a) - start.y).abs() < 0.001);
            assert!((a.union(b).center().x - (start.x + (end.x - start.x) * slot)).abs() < 0.75);
            assert!((a.size.x - b.size.x).abs() < 0.001);
            assert!((a.size.y - (bottom - start.y) * 0.2).abs() < 0.001);
            assert!((a.size.y * 1.5 - b.size.y).abs() < 0.001);
            if stacked {
                assert_eq!((a.origin.x, a.size.x), (b.origin.x, b.size.x));
                assert!((baseline(b) - end_y(a)).abs() < 0.001);
            } else {
                assert!(a.max().x < b.origin.x);
                assert!((baseline(b) - start.y).abs() < 0.001);
            }
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
    let chart = item(&result.after, "c");
    let grid = chart_lines(&chart, Theme::default().line);
    let [start, end] = zero_axis(&chart);
    let top = grid
        .iter()
        .map(|line| line[0].y)
        .fold(f32::INFINITY, f32::min);
    let bottom = grid
        .iter()
        .map(|line| line[0].y)
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(top < start.y && start.y < bottom);
    let series = item(&result.after, "s");
    let mut curve = Vec::new();
    for p in &*series.primitives {
        if let Prim::Line {
            points,
            closed,
            color,
            width,
            ..
        } = p
        {
            assert!(!closed && color.opacity > 0.0 && *width > 0.0);
            if let Some(last) = curve.last() {
                assert_eq!(Some(last), points.first(), "split strokes meet");
            }
            curve.extend(points.iter().copied());
        }
    }
    assert_eq!(curve.first(), Some(&start));
    assert_eq!(curve.last(), Some(&end));
    assert!(curve.windows(2).all(|p| p[0].x <= p[1].x));
    for (fraction, y) in [(0.25, top), (0.5, start.y), (0.75, bottom)] {
        let x = start.x + (end.x - start.x) * fraction;
        assert!(
            curve
                .iter()
                .any(|p| (p.x - x).abs() < 0.001 && (p.y - y).abs() < 0.001)
        );
    }
}

#[test]
fn pair_x_scatter_uses_uneven_numeric_spacing_and_log_y_midpoints() {
    let result = chart_picture(&json!([
        {"chart":{"id":"c","kind":"scatter","box":[0,0,400,300],"y":{"min":1,"max":100,"scale":"log"}}},
        {"series":{"id":"s","in":"c","data":[[10,1],[15,10],[30,100]]}}
    ]));
    let series = item(&result.after, "s");
    let mut markers = filled_rects(&series)
        .into_iter()
        .map(Rect::center)
        .collect::<Vec<_>>();
    markers.sort_by(|a, b| a.x.total_cmp(&b.x));
    let [first, middle, last]: [Vec2; 3] = markers.try_into().expect("three plotted samples");
    assert!(first.x < last.x && first.y > last.y);
    assert_vec2_near(
        middle,
        [
            first.x + (last.x - first.x) * 0.25,
            first.y.midpoint(last.y),
        ],
    );
    let chart = item(&result.after, "c");
    let grid = chart_lines(&chart, Theme::default().line);
    assert!(grid.iter().any(|[left, _]| *left == first));
    assert!(grid.iter().any(|[_, right]| *right == last));
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
    let next = committed(
        &mut scene,
        &mut budget,
        json!({"id":"remove","remove":["a","c","b"]}),
    );
    let next =
        picture(&scene, next, &first.after, &mut Measure::default()).expect("whole chart removal");
    assert_removed(&next, &first.after, &["a", "c", "b"]);
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
    let chart = item(&result.after, "c");
    let series = item(&result.after, "s");
    assert_eq!(series.parent, Some(chart.handle));
    assert!(
        series.primitives.is_empty(),
        "zero values produce neither slices nor labels"
    );
    assert_eq!(
        bounds(&result.after, "c"),
        Rect {
            origin: Vec2::default(),
            size: Vec2::new(400.0, 300.0)
        }
    );
    assert!(filled_rects(&chart).contains(&chart.bounds));
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
    use kumochi_scene::record::Align;
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
    let table = item(&result.after, "t");
    near(table.bounds.size.x, 80.0);
    assert!(filled_rects(&table).contains(&table.bounds));
    let [title, header] = ["Title wraps here", "Long header"].map(|value| text(&table, value));
    let [title_ink, header_ink] = [ink(title), ink(header)];
    let font_size = |text: &Prim| match text {
        Prim::Text { style, .. } => style.size,
        _ => unreachable!(),
    };
    assert!(font_size(title) > font_size(header));
    assert!(matches!(
        title,
        Prim::Text {
            style: TextStyle { bold: true, .. },
            ..
        }
    ));
    assert!(matches!(
        header,
        Prim::Text {
            align: Align::Center,
            style: TextStyle { bold: true, .. },
            ..
        }
    ));
    near(header_ink.center().x, table.bounds.center().x);
    for ink in [title_ink, header_ink] {
        assert!(ink.size.y > 10.0, "ink wraps onto multiple lines");
        assert_eq!(table.bounds.union(ink), table.bounds);
    }
    assert!(title_ink.max().y < header_ink.origin.y);
    let rule = open_stroke(&table);
    assert_eq!(
        rule.first(),
        Some(&Vec2::new(table.bounds.origin.x, table.bounds.max().y))
    );
    assert_eq!(rule.last(), Some(&table.bounds.max()));
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
    let row = item(&result.after, "r");
    let cell = text(&row, &value);
    let Prim::Text {
        origin,
        max_width: Some(width),
        ..
    } = cell
    else {
        panic!("wrapped cell")
    };
    assert!(*width > 0.0 && *width < row.bounds.size.x);
    assert!(origin.x + width <= row.bounds.max().x);
    let cell_ink = ink(cell);
    assert!(cell_ink.size.y > 10.0);
    assert!(cell_ink.origin.x >= row.bounds.origin.x && cell_ink.origin.y >= row.bounds.origin.y);
    assert!(cell_ink.max().x <= row.bounds.max().x && cell_ink.max().y <= row.bounds.max().y);
    let rule = open_stroke(&row);
    assert_eq!(rule.last(), Some(&row.bounds.max()));
}

#[test]
fn row_order_cell_sets_and_whole_table_removal_keep_their_boundaries() {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut layout = LayoutState::new(&scene);
    for value in [
        json!({"id":"first","put":[{"table":{"id":"t","cols":["V"]}},{"row":{"id":"z","in":"t","cells":["old"]}}]}),
        json!({"id":"second","put":[{"row":{"id":"a","in":"t","cells":["new"]}}]}),
    ] {
        let commit = committed(&mut scene, &mut budget, value);
        layout = picture(&scene, commit, &layout, &mut Measure::default())
            .expect("row step")
            .after;
    }
    let narrow = bounds(&layout, "t");
    let changed = committed(
        &mut scene,
        &mut budget,
        json!({"id":"update","set":[{"target":"z","cells":["longer"]}]}),
    );
    layout = picture(&scene, changed, &layout, &mut Measure::default())
        .expect("updated row")
        .after;
    assert!(bounds(&layout, "z").max().y <= bounds(&layout, "a").origin.y);
    assert!(bounds(&layout, "t").size.x > narrow.size.x);
    text(&item(&layout, "z"), "longer");
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
    assert_eq!(layout.handle(&id("z")), None);
    assert!(bounds(&layout, "a").max().y <= bounds(&layout, "m").origin.y);
    let commit = committed(
        &mut scene,
        &mut budget,
        json!({"id":"clear","remove":["m","a","t"]}),
    );
    let removed =
        picture(&scene, commit, &layout, &mut Measure::default()).expect("remove table and rows");
    assert_removed(&removed, &layout, &["m", "a", "t"]);
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
    assert_eq!(results.len(), sources.len());
    for (index, expected) in [
        (0, &[("a", "A")][..]),
        (2, &[("c", "C"), ("d", "D")]),
        (3, &[("e", "E")]),
        (4, &[("f", "F")]),
    ] {
        let (scene, layout) = results[index].as_ref().expect("recovered scene");
        assert_eq!(scene.object_count(), expected.len());
        assert_eq!(layout.items().len(), expected.len());
        for &(name, label) in expected {
            assert_eq!(scene.handle(&id(name)), layout.handle(&id(name)));
            text(&item(layout, name), label);
        }
    }
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
    for (name, label, anchor) in [("x", "X", [100.0, 100.0]), ("y", "Y", [200.0, 100.0])] {
        let object = item(&prepared.after, name);
        let label = ink(text(&object, label));
        assert_eq!(label, bounds(&prepared.after, name));
        assert_vec2_near(label.center(), anchor);
    }
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
        Err(PrepareError::OverPaintWork)
    ));
}

#[test]
fn generated_fill_points_refuse_before_crossing_count_above_the_global_cap() {
    for curves in [255, 256] {
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
        if curves == 255 {
            let prepared = prepared.expect("2,041 generated points fit the counting bound");
            let Prim::Fill { points, .. } = &prepared.display.items[0].primitives[0] else {
                panic!("prepared fill");
            };
            assert_eq!(points.len(), 2_041);
        } else {
            assert!(matches!(prepared, Err(PrepareError::OverPaintWork)));
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
    let chart = json!({"chart":{"id":"c","kind":"scatter","box":[0,0,400,300]}});
    let bare = chart_picture(&json!([chart])).after.paint_work();
    let marked = chart_picture(&json!([chart, {"series":{"id":"s","in":"c","data":[1]}}]))
        .after
        .paint_work();
    let one_marker_charge = marked.fill_vertices - bare.fill_vertices;
    assert!(one_marker_charge > 0);
    assert_eq!(three.fill_vertices, two.fill_vertices + one_marker_charge);
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
    let frame = frame(&next.after);
    for index in 0..8 {
        let name = format!("r{index}");
        let old = item(&first.after, &name);
        let new = item(&next.after, &name);
        assert_eq!(new.bounds, old.bounds);
        assert_eq!(new.primitives, old.primitives);
        let painted = frame
            .items()
            .iter()
            .find(|item| item.handle == new.handle)
            .expect("painted rect");
        assert_eq!(
            painted.paint_scale.to_bits(),
            (4.0_f32 + 1.0 / 16_384.0).to_bits()
        );
    }
    // Radius 8 needs 4 pieces per corner at 1 and 7 at 4: 4 · 5 + 1 and 4 · 8 + 1 points, filled
    // and bordered. At 64 it needs 26, so eight rects of 109 points pass the outline cap.
    let work = |points: usize| PaintWork {
        dash_pieces: 0,
        fill_vertices: 8 * points,
        largest_fill: points,
        outline_points: 8 * points,
        largest_outline: points,
    };
    assert_eq!(first.after.paint_work(), work(21));
    assert_eq!(next.after.paint_work(), work(33));
    let over = committed(&mut scene, &mut budget, scaled(64));
    assert!(matches!(
        capped(over, &next.after, &mut measure),
        Err(PrepareError::OverPaintWork)
    ));
    assert_eq!(
        first.after.paint_work(),
        work(21),
        "the earlier prefix keeps its own work"
    );
    assert_eq!(next.after.paint_work(), work(33));
}

/// A convex polygon of `count` points on a circle: no crossings, so it charges its points. Whole
/// coordinates keep a scene of fills at the global cap within one record.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "rounded coordinates within 100 to 1,900"
)]
fn convex(id: &str, count: u32) -> Value {
    let points: Vec<[i32; 2]> = (0..count)
        .map(|index| {
            let angle = std::f64::consts::TAU * f64::from(index) / f64::from(count);
            [angle.cos(), angle.sin()].map(|axis| (1_000.0 + 900.0 * axis).round() as i32)
        })
        .collect();
    json!({"shape":{"id":id,"kind":"polygon","points":points,"fill":"blue"}})
}

/// Convex polygons with `base` points in all, at most 512 each (one shape's limit), then one more
/// polygon of `last` points.
fn fills(base: usize, last: u32) -> Vec<Value> {
    let mut put: Vec<Value> = (0..base.div_ceil(512))
        .map(|index| {
            let count = (base - index * 512).min(512);
            convex(
                &format!("f{index}"),
                u32::try_from(count).expect("at most 512"),
            )
        })
        .collect();
    put.push(convex("last", last));
    put
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
        (
            fills(MAX_GLOBAL_FILL_VERTICES - 512, 512),
            Some(MAX_GLOBAL_FILL_VERTICES),
        ),
        (
            fills(MAX_GLOBAL_FILL_VERTICES - 212, 212),
            Some(MAX_GLOBAL_FILL_VERTICES),
        ),
        (fills(MAX_GLOBAL_FILL_VERTICES - 212, 213), None),
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
            None => assert!(matches!(prepared, Err(PrepareError::OverPaintWork))),
        }
    }
    let (scene, commit) = fill_scene(&fills(MAX_GLOBAL_FILL_VERTICES - 212, 213));
    let exact = picture(
        &scene,
        commit,
        &LayoutState::new(&scene),
        &mut Measure::default(),
    )
    .expect("unbounded preparation stays exact");
    assert_eq!(
        exact.after.paint_work().fill_vertices,
        MAX_GLOBAL_FILL_VERTICES + 1
    );
}

#[test]
fn bounded_recovery_refuses_a_scene_over_the_cap() {
    let (scene, _) = fill_scene(&fills(MAX_GLOBAL_FILL_VERTICES - 212, 213));
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
                    error: PrepareError::OverPaintWork,
                })
            ));
        } else {
            let (_, layout) = result.as_ref().expect("unbounded recovery stays exact");
            assert_eq!(
                layout.paint_work().fill_vertices,
                MAX_GLOBAL_FILL_VERTICES + 1
            );
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
            json!({"id":"first","put":fills(MAX_GLOBAL_FILL_VERTICES - 512, 300)}),
        );
        let first = picture(&scene, first, &before, &mut measure).expect("first fills");
        assert_eq!(
            first.after.paint_work().fill_vertices,
            MAX_GLOBAL_FILL_VERTICES - 212
        );
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
            let next = result.expect("the reused points plus 212 new points fit");
            next.validate(&scene, &first.after)
                .expect("current picture");
            assert_eq!(
                next.after.paint_work().fill_vertices,
                MAX_GLOBAL_FILL_VERTICES
            );
            let old = item(&first.after, "f0");
            let kept = item(&next.after, "f0");
            let fill = |item: &Item| {
                item.primitives
                    .iter()
                    .find_map(|p| match p {
                        Prim::Fill { points, color } if color.opacity > 0.0 => {
                            Some((Arc::clone(points), *color))
                        }
                        _ => None,
                    })
                    .expect("visible fill")
            };
            assert_eq!(fill(&kept), fill(&old));
            assert_eq!(kept.bounds, old.bounds);
        } else {
            assert!(matches!(result, Err(PrepareError::OverPaintWork)));
        }
        assert_eq!(
            first.after.paint_work().fill_vertices,
            MAX_GLOBAL_FILL_VERTICES - 212
        );
    }
}
