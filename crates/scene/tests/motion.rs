//! Activation, sampling, the host loop and cuts over prepared steps.
#[path = "support/allocation.rs"]
mod allocation;
#[path = "support/motion.rs"]
mod fixtures;

use fixtures::{Fixture, four_views, gliding_scene, mixed_track};

use kumochi_scene::{
    Admission, Frame, FrameItem, GlobalBudget, Handle, Record, SceneState, SceneView, StopCut,
    Text, WireId,
    display::{MeasureError, TextMeasure, TextStyle},
    geom::Affine,
    layout::{LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare},
    motion::{at_word_offset_ms, synthetic_duration_ms},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

struct Measure;
impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let width =
            f32::from(u16::try_from(text.chars().count()).expect("bounded fixture text")) * 8.0;
        Ok([max_width.map_or(width, |limit| width.min(limit)), 10.0])
    }
}

fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}
fn fresh() -> (SceneState, GlobalBudget, SceneView) {
    let scene = new_scene("motion");
    let view = SceneView::new(&LayoutState::new(&scene), scene.activated());
    (scene, GlobalBudget::default(), view)
}
fn new_scene(name: &str) -> SceneState {
    let Record::Begin(header) = parse_record(
        &serde_json::to_vec(&json!({"begin":{"v":1,"scene":name,"surface":"inline","alt":""}}))
            .expect("header JSON"),
    )
    .expect("header") else {
        panic!("begin");
    };
    SceneState::begin(header).expect("scene")
}
fn prepared(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
) -> PreparedStep {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&envelope).expect("fixture JSON")).expect("typed fixture")
    else {
        panic!("step");
    };
    let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted fixture") else {
        panic!("new step");
    };
    let committed = scene.commit(candidate, budget).expect("fresh candidate");
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("consecutive input");
    let result = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("prepared step");
    result.validate(scene, before).expect("current result");
    result
}
/// Commits, prepares on the view's layout and activates one step at `now`.
fn activate(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    view: &mut SceneView,
    value: Value,
    now: u64,
) {
    let step = prepared(scene, budget, view.layout(), value);
    let segment = synthetic_duration_ms(step.step().say.as_ref());
    view.activate(step, now, segment, &mut Frame::new())
        .expect("fresh activation");
}
/// Host Esc: reveal what `stop` or the caller names, cut, and reinstall at rest.
fn cut(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    view: &SceneView,
    revealed: Vec<Handle>,
) -> SceneView {
    scene
        .stop(
            &StopCut {
                activated: view.activated(),
                revealed: Bounded::new(revealed).expect("bounded cut"),
            },
            budget,
        )
        .expect("valid cut");
    let layout = view.layout().cut(scene).expect("frozen layout");
    SceneView::new(&layout, scene.activated())
}
fn handle(view: &SceneView, name: &str) -> Handle {
    view.layout().handle(&id(name)).expect("placed object")
}
fn sample(view: &SceneView, now: u64) -> Frame {
    let mut frame = Frame::new();
    view.frame(now, &mut frame).expect("fresh frame");
    frame
}
fn item(view: &SceneView, now: u64, name: &str) -> Option<FrameItem> {
    let handle = handle(view, name);
    sample(view, now)
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .cloned()
}
fn opacity(view: &SceneView, now: u64, name: &str) -> u32 {
    item(view, now, name).expect("emitted").opacity.to_bits()
}
fn transform(view: &SceneView, now: u64, name: &str) -> Affine {
    item(view, now, name).expect("emitted").transform
}
fn rect(name: &str) -> Value {
    json!({"shape":{"id":name,"kind":"rect","box":[0,0,10,10]}})
}

#[test]
fn a_child_entrance_waits_for_its_delayed_parent_without_shortening() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({
            "id":"delayed","say":"one two three four five six seven eight nine ten","put":[
                {"diagram":{"id":"d","at_word":5,"delay":200}},
                {"shape":{"id":"child","in":"d","kind":"rect","box":[0,0,8,8],"enter":"fade","dur":400}}
            ]
        }),
        0,
    );
    assert_eq!(item(&view, 2017, "child"), None);
    assert_eq!(
        opacity(&view, 2018, "child"),
        0.0_f32.to_bits(),
        "the child starts when its parent appears"
    );
    assert_eq!(opacity(&view, 2218, "child"), 0.5_f32.to_bits());
    assert_eq!(opacity(&view, 2418, "child"), 1.0_f32.to_bits());
    assert_eq!(view.stop(2417), vec![handle(&view, "d")]);
    assert_eq!(
        view.stop(2418),
        vec![handle(&view, "d"), handle(&view, "child")]
    );
    assert!(!view.animating(0));
    assert_eq!(view.next_wake(0), Some(2018));
}

#[test]
fn frame_never_frees_geometry_replaced_by_an_activation() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[rect("s")]}),
        0,
    );
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("first frame");
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"remove","remove":["s"]}),
    );
    view.activate(next, 100, 800, &mut frame)
        .expect("remove activation");
    let ((), counts) =
        allocation::activity(|| view.frame(101, &mut frame).expect("replacement frame"));
    assert_eq!(
        counts,
        [0, 0],
        "sampling neither allocates nor frees the replaced primitives"
    );
    assert_eq!(frame.items().len(), 0);
}

#[test]
fn a_three_level_entrance_chain_keeps_its_start_when_a_child_is_added_later() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"pending","say":"one two three four five six seven eight nine ten","put":[
            {"diagram":{"id":"d","at_word":5,"delay":200}},
            {"table":{"id":"t","in":"d","cols":["V"],"enter":"fade","dur":400}},
            {"row":{"id":"first","in":"t","cells":[1],"enter":"fade","dur":400}}
        ]}),
        0,
    );
    assert_eq!(item(&view, 2017, "first"), None);
    assert_eq!(opacity(&view, 2018, "first"), 0.0_f32.to_bits());
    assert_eq!(opacity(&view, 2218, "first"), 0.25_f32.to_bits());
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"later","put":[
            {"row":{"id":"later","in":"t","cells":[2],"enter":"fade","dur":400}}
        ]}),
        1000,
    );
    assert_eq!(view.next_wake(1000), Some(2018));
    assert_eq!(item(&view, 2017, "later"), None);
    assert_eq!(opacity(&view, 2018, "later"), 0.0_f32.to_bits());
    assert_eq!(opacity(&view, 2218, "first"), 0.25_f32.to_bits());
    assert_eq!(opacity(&view, 2218, "later"), 0.25_f32.to_bits());
    assert_eq!(view.stop(2417), vec![handle(&view, "d")]);
    assert_eq!(
        view.stop(2418),
        vec![
            handle(&view, "d"),
            handle(&view, "t"),
            handle(&view, "first"),
            handle(&view, "later"),
        ]
    );
}

#[test]
fn frame_debug_summarizes_counts_instead_of_drawing_properties() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"text","put":[
            {"text":{"id":"label","at":[12,34],"text":"drawing marker"}}
        ]}),
        0,
    );
    let frame = sample(&view, 0);
    assert_eq!(format!("{frame:?}"), "Frame { items: 1, .. }");
    assert_eq!(
        format!("{:?}", frame.items()[0]),
        "FrameItem { primitives: 1, .. }"
    );
}

#[test]
fn step_and_frame_debug_do_not_expose_numeric_or_colour_sentinels() {
    let value = json!({"id":"sentinels","put":[
        {"table":{"id":"table","cols":["Value","Flag"]}},
        {"row":{"id":"row","in":"table","cells":[123_456.5,true]}},
        {"chart":{"id":"chart","kind":"line","box":[0,300,400,300]}},
        {"series":{"id":"series","in":"chart","data":[123_456.5]}},
        {"chart":{"id":"scatter","kind":"scatter","box":[400,300,400,300]}},
        {"series":{"id":"points","in":"scatter","data":[[1234.5,123_456.5]]}},
        {"line":{"id":"line","from":[1234.5,0],"to":[1234.5,10],"color":"#abcdef"}},
        {"anim":{"target":"line","track":"x","keys":[[0,1234.5]]}},
        {"anim":{"target":"line","track":"color","keys":[[0,"#abcdef"]]}}
    ]});
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let (mut scene, mut budget, mut view) = fresh();
    activate(&mut scene, &mut budget, &mut view, value, 0);
    let frame = sample(&view, 0);
    let line = frame
        .items()
        .iter()
        .find(|item| item.handle == handle(&view, "line"))
        .expect("line carrying the sentinels");
    assert_eq!(line.transform, Affine([1.0, 0.0, 0.0, 1.0, 1234.5, 0.0]));
    assert_eq!(
        line.color.expect("literal colour owner").rgb.get(),
        0x00ab_cdef
    );
    for output in [format!("{step:?}"), format!("{frame:?}")] {
        for sentinel in ["123456.5", "1234.5", "#abcdef", "abcdef", "11259375"] {
            assert!(
                !output.contains(sentinel),
                "Debug must not expose drawing values"
            );
        }
    }
}

#[test]
fn an_infinite_track_samples_the_last_tick_then_rests_at_the_deadline() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"repeat","put":[rect("s"),
            {"anim":{"target":"s","track":"x","ease":"linear","repeat":-1,
                "keys":[[0,0],[100,100]]}}
        ]}),
        0,
    );
    assert_eq!(
        transform(&view, 59_999, "s"),
        Affine([1.0, 0.0, 0.0, 1.0, 99.0, 0.0])
    );
    assert!(view.animating(59_999));
    assert_eq!(
        transform(&view, 60_000, "s"),
        Affine([1.0, 0.0, 0.0, 1.0, 100.0, 0.0])
    );
    assert!(!view.animating(60_000));
    assert_eq!(view.next_wake(60_000), None);
}

#[test]
fn a_delayed_track_holds_its_first_key_then_animates_to_its_last() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"still","put":[rect("a")]}),
        1000,
    );
    assert!(!view.animating(1000), "a step without motion is at rest");
    assert_eq!(view.next_wake(1000), None, "and has nothing to wake for");
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"moving","put":[{"anim":{"target":"a","track":"opacity","ease":"linear",
            "delay":500,"keys":[[0,0],[300,1]]}}]}),
        1000,
    );
    assert!(!view.animating(1000), "a delay is a hold");
    assert_eq!(
        view.next_wake(1000),
        Some(1500),
        "the host wakes at the origin"
    );
    assert_eq!(
        opacity(&view, 1200, "a"),
        0.0_f32.to_bits(),
        "the hold shows the first key"
    );
    assert!(
        view.animating(1500) && view.animating(1799),
        "the track runs"
    );
    assert_eq!(
        opacity(&view, 1650, "a"),
        0.5_f32.to_bits(),
        "linear midpoint"
    );
    assert!(!view.animating(1800), "the last frame is at rest");
    assert_eq!(view.next_wake(1800), None, "nothing is left to wake for");
    assert_eq!(
        opacity(&view, 1800, "a"),
        1.0_f32.to_bits(),
        "it ends on the last key"
    );
}

#[test]
fn explicit_replacement_waits_for_activation_and_carries_through_unrelated_steps() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[rect("s"),
            {"anim":{"target":"s","track":"opacity","ease":"linear","keys":[[0,0],[1000,1]]}}
        ]}),
        0,
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"unrelated","put":[rect("other")]}),
        500,
    );
    assert_eq!(opacity(&view, 500, "s"), 0.5_f32.to_bits());
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"replacement","put":[
            {"anim":{"target":"s","track":"opacity","ease":"linear","delay":200,"keys":[[0,0],[100,1]]}}
        ]}),
    );
    assert_eq!(
        opacity(&view, 750, "s"),
        0.75_f32.to_bits(),
        "preparing a future zero key does not replace the activated owner"
    );
    view.activate(next, 600, 800, &mut Frame::new())
        .expect("replacement activation");
    assert_eq!(opacity(&view, 600, "s"), 0.0_f32.to_bits());
    assert_eq!(opacity(&view, 799, "s"), 0.0_f32.to_bits());
    assert_eq!(opacity(&view, 850, "s"), 0.5_f32.to_bits());
    assert_eq!(opacity(&view, 900, "s"), 1.0_f32.to_bits());
}

#[test]
fn delayed_track_chain_rests_at_the_last_activation_deadline() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"shape","put":[rect("s")]}),
        0,
    );
    for (index, word) in [0, 100, 199].into_iter().enumerate() {
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":format!("track{index}"),
                "say":vec!["a";200].join(" "),"put":[{"anim":{
                    "target":"s","track":"opacity","ease":"linear","at_word":word,"delay":500,"keys":[[0,0],[1000,1]]
                }}]
            }),
            u64::try_from(index).expect("index") * 1000,
        );
    }
    assert_eq!(opacity(&view, 61_999, "s"), 0.0_f32.to_bits());
    assert_eq!(view.next_wake(61_999), Some(62_000));
    assert_eq!(opacity(&view, 62_000, "s"), 1.0_f32.to_bits());
    assert!(!view.animating(62_000));
    assert_eq!(view.next_wake(62_000), None);
}

#[test]
fn a_stale_frame_keeps_its_geometry_until_cleared_outside_sampling() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[rect("s")]}),
        0,
    );
    let mut stale = sample(&view, 0);
    let original = stale.items()[0].handle;
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"remove","remove":["s"]}),
        100,
    );
    let (result, counts) = allocation::activity(|| view.frame(100, &mut stale));
    assert!(matches!(result, Err(PrepareError::Stale)));
    assert_eq!(counts, [0, 0]);
    assert_eq!(stale.items().len(), 1);
    assert_eq!(stale.items()[0].handle, original);
    let ((), counts) = allocation::activity(|| stale.clear());
    assert_eq!(
        counts,
        [0, 2],
        "geometry and the old generation release outside sampling"
    );
    view.frame(100, &mut stale)
        .expect("cleared frame binds again");
    assert!(stale.items().is_empty());
}

#[test]
fn a_reconstructed_view_requires_an_outside_paint_frame_clear() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"shape","put":[rect("s")]}),
        0,
    );
    let mut frame = sample(&view, 0);
    let restored = SceneView::new(view.layout(), view.activated());
    let (result, counts) = allocation::activity(|| restored.frame(0, &mut frame));
    assert!(matches!(result, Err(PrepareError::Stale)));
    assert_eq!(counts, [0, 0]);
    assert_eq!(frame.items().len(), 1);
    frame.clear();
    restored
        .frame(0, &mut frame)
        .expect("cleared frame binds to the reconstructed view");
    assert_eq!(frame.items().len(), 1);
}

#[test]
fn scene_debug_summarizes_counts_instead_of_poses_and_keys() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"shape","put":[rect("s"),
            {"anim":{"target":"s","track":"x","keys":[[0,123.5],[100,456.5]]}}
        ]}),
        0,
    );
    assert_eq!(
        format!("{view:?}"),
        "SceneView { activated: 1, objects: 1, tracks: 1, .. }"
    );
}

#[test]
fn empty_zero_duration_and_clock_edges_are_safe() {
    let (mut scene, mut budget, mut view) = fresh();
    assert!(sample(&view, 0).items().is_empty());
    assert!(!view.animating(0));
    assert_eq!(view.next_wake(0), None);
    assert_eq!(view.stop(0), []);
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"instant","put":[
            {"shape":{"id":"s","kind":"rect","box":[0,0,8,8],"enter":"fade","dur":0}}
        ]}),
        1000,
    );
    assert_eq!(
        item(&view, 999, "s"),
        None,
        "a clock before activation hides a new object"
    );
    assert_eq!(opacity(&view, 1000, "s"), 1.0_f32.to_bits());
    assert!(!view.animating(1000));
    assert_eq!(view.next_wake(1000), None);
    assert_eq!(view.stop(999), []);
    assert_eq!(view.stop(1000), vec![handle(&view, "s")]);
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"late","put":[
            {"shape":{"id":"late","kind":"rect","box":[0,0,8,8],"enter":"fade","delay":100,"dur":400}}
        ]}),
        u64::MAX - 10,
    );
    assert_eq!(item(&view, u64::MAX - 1, "late"), None);
    assert_eq!(view.next_wake(u64::MAX - 1), Some(u64::MAX));
    assert_eq!(opacity(&view, u64::MAX, "late"), 1.0_f32.to_bits());
    assert!(!view.animating(u64::MAX));
    assert_eq!(view.next_wake(u64::MAX), None);
    assert_eq!(
        view.stop(u64::MAX),
        vec![handle(&view, "s"), handle(&view, "late")]
    );
}

#[test]
fn a_clock_before_an_existing_objects_track_holds_the_first_key() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[rect("s")]}),
        0,
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"track","put":[
            {"anim":{"target":"s","track":"opacity","keys":[[0,0.25],[100,1]]}}
        ]}),
        1000,
    );
    assert_eq!(opacity(&view, 999, "s"), 0.25_f32.to_bits());
    assert!(!view.animating(999));
    assert_eq!(view.next_wake(999), Some(1000));
}

#[test]
fn esc_during_an_explicit_opacity_fade_uses_the_documented_final_rest() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"fade","put":[rect("s"),
            {"anim":{"target":"s","track":"opacity","ease":"linear","keys":[[0,0],[1000,1]]}}
        ]}),
        0,
    );
    assert_eq!(opacity(&view, 500, "s"), 0.5_f32.to_bits());
    let revealed = view.stop(500);
    assert_eq!(revealed, vec![handle(&view, "s")]);
    let after = cut(&mut scene, &mut budget, &view, revealed);
    assert_eq!(opacity(&after, 500, "s"), 1.0_f32.to_bits());
}

#[test]
fn explicit_position_replaces_a_running_glide_and_suppresses_the_next_one() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,1,1]}},
            {"node":{"id":"a","in":"d","label":"A"}}
        ]}),
        0,
    );
    assert!(view.animating(100), "the narrow diagram is still gliding");
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"override","put":[
            {"node":{"id":"b","in":"d","label":"B"}},
            {"anim":{"target":"d","track":"pos","ease":"linear","keys":[[0,[10,20]],[100,[30,40]]]}}
        ]}),
    );
    assert_eq!(
        next.glides.len(),
        0,
        "the explicit owner suppresses this step's generated glide"
    );
    view.activate(next, 100, 800, &mut Frame::new())
        .expect("position activation");
    assert_eq!(
        transform(&view, 100, "a"),
        Affine([1.0, 0.0, 0.0, 1.0, 10.0, 20.0])
    );
    assert_eq!(
        transform(&view, 150, "a"),
        Affine([1.0, 0.0, 0.0, 1.0, 20.0, 30.0])
    );
    assert_eq!(
        transform(&view, 200, "a"),
        Affine([1.0, 0.0, 0.0, 1.0, 30.0, 40.0])
    );
}

#[test]
fn a_one_millisecond_flow_stays_bounded_and_ends_at_the_deadline() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"flow","put":[
            {"diagram":{"id":"d"}}, {"node":{"id":"a","in":"d","label":"A"}},
            {"node":{"id":"b","in":"d","label":"B"}},
            {"edge":{"id":"edge","in":"d","from":"a","to":"b","flow":1}}
        ]}),
        0,
    );
    assert_eq!(
        item(&view, 59_999, "edge").expect("edge").flow.to_bits(),
        0.0_f32.to_bits()
    );
    assert!(view.animating(59_999));
    assert!(!view.animating(60_000));
    assert_eq!(view.next_wake(60_000), None);
    assert_eq!(
        item(&view, 60_000, "edge").expect("edge").flow.to_bits(),
        0.0_f32.to_bits()
    );
}

#[test]
fn an_opacity_spring_clamps_to_exactly_one_at_its_overshoot() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"spring","put":[rect("a"),{"anim":{"target":"a","track":"opacity",
            "ease":"spring","keys":[[0,0],[1000,1]]}}]}),
        0,
    );
    assert_eq!(
        opacity(&view, 165, "a"),
        1.0_f32.to_bits(),
        "t1 overshoots past 1"
    );
}

#[test]
fn synthetic_segments_use_integer_word_rates_and_offsets() {
    let words = |count: usize| Text::try_from(vec!["a"; count].join(" ")).expect("bounded say");
    assert_eq!(synthetic_duration_ms(None), 800, "no narration");
    assert_eq!(
        synthetic_duration_ms(Some(&words(0))),
        800,
        "empty narration"
    );
    assert_eq!(synthetic_duration_ms(Some(&words(11))), 4000, "11 words");
    assert_eq!(
        synthetic_duration_ms(Some(&words(2))),
        800,
        "the floor includes two words"
    );
    assert_eq!(
        synthetic_duration_ms(Some(&words(3))),
        1091,
        "three words exceed the floor"
    );
    assert_eq!(
        synthetic_duration_ms(Some(&words(200))),
        72_728,
        "200 words round up"
    );
    assert_eq!(at_word_offset_ms("", 3, 1000), 0, "no words start at once");
    assert_eq!(at_word_offset_ms("a b c d", 2, 1000), 500, "proportional");
    assert_eq!(
        at_word_offset_ms("a b c d", 9, 1000),
        1000,
        "clamped to the segment"
    );
    assert_eq!(
        at_word_offset_ms("a b", 1, u64::MAX),
        u64::MAX / 2,
        "u128 product"
    );
    assert_eq!(
        at_word_offset_ms("a b", 2, u64::MAX),
        u64::MAX,
        "no overflow at the end"
    );
}

#[test]
fn an_entrance_scheduled_past_the_deadline_appears_at_rest_there() {
    let (mut scene, mut budget, mut view) = fresh();
    let say = ["a"; 200].join(" ");
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"long","say":say,"put":[{"shape":{"id":"late","kind":"rect",
            "box":[0,0,10,10],"enter":"fade","at_word":199}}]}),
        0,
    );
    assert_eq!(
        item(&view, 59_999, "late"),
        None,
        "not emitted before its origin"
    );
    assert!(!view.animating(59_999), "a pending entrance is a hold");
    assert_eq!(
        view.next_wake(59_999),
        Some(60_000),
        "its origin clips to D"
    );
    assert_eq!(
        opacity(&view, 60_000, "late"),
        1.0_f32.to_bits(),
        "at rest at D"
    );
    assert!(!view.animating(60_000), "motion ends at D");
    assert_eq!(view.next_wake(60_000), None, "nothing runs past D");
    assert_eq!(
        view.stop(60_000),
        vec![handle(&view, "late")],
        "Esc at D reveals it"
    );
}

#[test]
fn stop_reveals_only_objects_whose_own_and_ancestor_entrances_finished() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"mixed","put":[
            {"diagram":{"id":"d"}},
            {"table":{"id":"t","in":"d","cols":["V"],"enter":"fade","dur":1000}},
            {"row":{"id":"r","in":"t","cells":[1],"enter":"fade","dur":200}},
            {"shape":{"id":"x","kind":"rect","box":[300,0,320,20],"enter":"fade","dur":500}},
            {"shape":{"id":"y","kind":"rect","box":[340,0,360,20]}}
        ]}),
        0,
    );
    let handles = |names: &[&str]| {
        names
            .iter()
            .map(|name| handle(&view, name))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        view.stop(200),
        handles(&["d", "y"]),
        "a finished row under an entering table stays hidden"
    );
    assert_eq!(view.stop(499), handles(&["d", "y"]), "x is still entering");
    assert_eq!(
        view.stop(500),
        handles(&["d", "x", "y"]),
        "x finished exactly"
    );
    assert_eq!(
        view.stop(1000),
        handles(&["d", "t", "r", "x", "y"]),
        "the parent finished"
    );
}

#[test]
fn detached_rows_keep_the_cut_ancestors_factor_under_their_own_track() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","opacity":0.5}},
            {"table":{"id":"t","in":"d","cols":["V"]}},
            {"row":{"id":"r","in":"t","cells":[1],"opacity":0}},
            {"anim":{"target":"r","track":"opacity","ease":"linear","keys":[[0,0],[100,1]]}}
        ]}),
        0,
    );
    assert_eq!(
        opacity(&view, 50, "r"),
        0.25_f32.to_bits(),
        "own 0.5 under d's 0.5"
    );
    assert_eq!(
        opacity(&view, 100, "r"),
        0.5_f32.to_bits(),
        "the track overrides base 0"
    );
    let revealed = vec![handle(&view, "t"), handle(&view, "r")];
    view = cut(&mut scene, &mut budget, &view, revealed);
    assert_eq!(
        opacity(&view, 200, "r"),
        0.5_f32.to_bits(),
        "the cut keeps d's 0.5 as a factor"
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"cells","set":[{"target":"r","cells":[2]}]}),
        300,
    );
    assert_eq!(
        opacity(&view, 300, "r"),
        0.5_f32.to_bits(),
        "an own update keeps both"
    );
    for (index, own) in [0.0_f32, 1.0].into_iter().enumerate() {
        let now = 400 + u64::try_from(index).expect("fixture index") * 200;
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":format!("opacity{index}"),"put":[{"anim":{
                "target":"r","track":"opacity","keys":[[0,own]]
            }}]}),
            now,
        );
        assert_eq!(
            opacity(&view, now, "r"),
            (own * 0.5).to_bits(),
            "the explicit owner overrides without losing the inherited factor"
        );
        let revealed = view.stop(now);
        view = cut(&mut scene, &mut budget, &view, revealed);
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":format!("cells{index}"),"set":[{"target":"r","cells":[index + 3]}]}),
            now + 100,
        );
        assert_eq!(
            opacity(&view, now + 100, "r"),
            (own * 0.5).to_bits(),
            "another cut and rebuild preserve the factor"
        );
    }
}

#[test]
fn explicit_opacity_can_raise_a_zero_base_without_an_ancestor() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"zero_base","put":[
            {"shape":{"id":"s","kind":"rect","box":[0,0,8,8],"opacity":0}},
            {"anim":{"target":"s","track":"opacity","ease":"linear","keys":[[0,0],[100,1]]}}
        ]}),
        0,
    );
    assert_eq!(opacity(&view, 50, "s"), 0.5_f32.to_bits());
    assert_eq!(opacity(&view, 100, "s"), 1.0_f32.to_bits());
}

#[test]
fn a_cut_installs_the_activated_revision_and_rejects_older_bases() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"one","put":[rect("a")]}),
        0,
    );
    let pending = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"two","put":[rect("b")]}),
    );
    let revealed = view.stop(100);
    view = cut(&mut scene, &mut budget, &view, revealed);
    assert_eq!(
        (view.activated(), view.layout().revision()),
        (1, 2),
        "the layout counts the dropped commit, the view does not"
    );
    assert!(
        matches!(
            view.activate(pending, 100, 800, &mut Frame::new()),
            Err(PrepareError::Stale)
        ),
        "a step prepared before the cut is stale"
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"three","put":[rect("c")]}),
        200,
    );
    assert_eq!(
        view.activated(),
        3,
        "the next step activates on the cut layout"
    );
}

#[test]
fn a_later_set_color_replaces_a_running_color_track_at_activation() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"paint","put":[rect("s"),
            {"anim":{"target":"s","track":"color","keys":[[0,"red"],[100,"blue"]]}}]}),
        0,
    );
    assert!(
        item(&view, 50, "s").expect("shape").color.is_some(),
        "the track paints first"
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"recolor","set":[{"target":"s","color":"green"}]}),
        50,
    );
    for now in [50, 200] {
        assert_eq!(
            item(&view, now, "s").expect("shape").color,
            None,
            "the newest write wins at {now}"
        );
    }
}

#[test]
fn finished_tracks_frame_the_same_live_and_after_a_cut() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"tracks","put":[{"path":{"id":"s","d":"M0 0 L8 0 L8 8 L0 8 Z"}},
            {"anim":{"target":"s","track":"color","keys":[[0,"red"],[100,"blue"]]}},
            {"anim":{"target":"s","track":"draw","keys":[[0,0],[100,0.5]]}},
            {"anim":{"target":"s","track":"x","ease":"spring","yoyo":true,"repeat":1,
                "keys":[[0,5],[100,40]]}}]}),
        0,
    );
    let live = sample(&view, 300);
    let revealed = view.stop(300);
    let recovered = cut(&mut scene, &mut budget, &view, revealed);
    assert_eq!(
        live.items(),
        sample(&recovered, 300).items(),
        "rest equals the finished tracks"
    );
    let (state, layout) = kumochi_scene::layout::recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
    )
    .expect("replay the original steps and cut");
    let replayed = SceneView::new(&layout, state.activated());
    assert_eq!(
        live.items(),
        sample(&replayed, 300).items(),
        "logged recovery replays the same final color, draw and yoyo values"
    );
    let shape = &live.items()[0];
    assert_eq!(
        shape.color.map(|paint| paint.rgb.get()),
        Some(0x007a_a2ff),
        "the finished color is blue, not a missing owner on both paths"
    );
    assert_eq!(
        shape.draw.to_bits(),
        0.5_f32.to_bits(),
        "draw rests at its last key"
    );
    assert_eq!(
        shape.transform,
        Affine([1.0, 0.0, 0.0, 1.0, 5.0, 0.0]),
        "a two-play yoyo rests on its first key"
    );
}

#[test]
fn a_glide_starts_where_the_group_was_and_survives_unrelated_steps() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[
            {"diagram":{"id":"d","box":[0,0,260,300],"dir":"down"}},
            {"node":{"id":"root","in":"d","label":"Root"}},
            {"node":{"id":"a","in":"d","label":"A"}},{"node":{"id":"b","in":"d","label":"B"}},
            {"edge":{"id":"ra","in":"d","from":"root","to":"a"}},{"edge":{"id":"rb","in":"d","from":"root","to":"b"}}
        ]}),
        0,
    );
    let start = transform(&view, 0, "a");
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"next","put":[{"node":{"id":"c","in":"d","label":"C"}},
            {"edge":{"id":"rc","in":"d","from":"root","to":"c"}}]}),
    );
    assert_eq!(next.glides.len(), 1, "the overflow shifts the group");
    let rest = SceneView::new(&next.after, 2);
    view.activate(next, 1000, 800, &mut Frame::new())
        .expect("fresh activation");
    assert_eq!(
        transform(&view, 1000, "a"),
        start,
        "the glide starts at the live position"
    );
    assert_eq!(
        transform(&view, 1450, "a"),
        transform(&rest, 1450, "a"),
        "and ends at rest"
    );
    let middle = transform(&view, 1225, "a");
    let end = transform(&rest, 1450, "a");
    assert_eq!(
        middle,
        Affine(std::array::from_fn(|i| start.0[i].midpoint(end.0[i]))),
        "InOut at half time is the exact midpoint"
    );
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"other","put":[rect("z")]}),
        1225,
    );
    assert_eq!(
        transform(&view, 1225, "a"),
        middle,
        "an unrelated step keeps the running glide"
    );
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"again","put":[{"node":{"id":"extra","in":"d","label":"Extra"}},
            {"edge":{"id":"re","in":"d","from":"root","to":"extra"}}]}),
    );
    assert_eq!(next.glides.len(), 1, "the next sibling overflows again");
    let rest = SceneView::new(&next.after, 4);
    view.activate(next, 1225, 800, &mut Frame::new())
        .expect("replacement glide");
    assert_eq!(
        transform(&view, 1225, "a"),
        middle,
        "a replacement glide starts at the sampled midpoint"
    );
    assert_eq!(
        transform(&view, 1675, "a"),
        transform(&rest, 1675, "a"),
        "the replacement reaches its new target"
    );
}

#[test]
fn four_views_reach_the_legal_caps_without_frame_allocations() {
    for fixture in [Fixture::Control, Fixture::Glide] {
        let mixed = matches!(fixture, Fixture::Glide);
        let views = four_views(fixture);
        let mut frames: Vec<_> = (0..4).map(|_| Frame::new()).collect();
        for now in [100, 2000, 59_999, 60_000] {
            let ((), counts) = allocation::activity(|| {
                for (view, frame) in views.iter().zip(&mut frames) {
                    view.frame(now, frame).expect("caps frame");
                }
            });
            assert_eq!(
                counts,
                [0, 0],
                "four-view sampling at {now} neither allocates nor frees"
            );
            assert_eq!(
                frames
                    .iter()
                    .map(|frame| frame.items().len())
                    .sum::<usize>(),
                if mixed { 750 } else { 1488 },
                "all geometry survives at the full caps"
            );
        }
    }
}

#[test]
#[ignore = "exclusive Rez timing: release, measuring.lock, no concurrent cargo/rustc"]
fn frame_four_views_max() {
    use std::{hint::black_box, time::Instant};
    for fixture in [Fixture::Control, Fixture::Glide] {
        let mixed = matches!(fixture, Fixture::Glide);
        let views = four_views(fixture);
        let mut frames: Vec<_> = (0..4).map(|_| Frame::new()).collect();
        for early in [true, false] {
            for tick in 0..64 {
                let now = if early { 100 } else { tick * 16 };
                for (view, frame) in views.iter().zip(&mut frames) {
                    view.frame(now, frame).expect("warmup frame");
                }
            }
            let mut samples = Vec::with_capacity(1024);
            let ((), [allocations, frees]) = allocation::activity(|| {
                for tick in 0..1024 {
                    let now = if early { 100 } else { (tick + 64) * 16 };
                    let start = Instant::now();
                    for (view, frame) in views.iter().zip(&mut frames) {
                        view.frame(black_box(now), black_box(frame))
                            .expect("timed frame");
                        if mixed {
                            black_box(view.animating(now));
                        }
                    }
                    samples.push(start.elapsed().as_secs_f64() * 1e6);
                }
            });
            assert_eq!(
                allocations, 0,
                "all four views allocate nothing while sampling"
            );
            assert_eq!(frees, 0, "all four views free nothing while sampling");
            let mut sorted = samples.clone();
            sorted.sort_by(f64::total_cmp);
            println!(
                "workload={} samples=1024 allocations={allocations} frees={frees} p50_us={} p95_us={} p99_us={} max_us={}",
                match (mixed, early) {
                    (false, true) => "early-all-entrances",
                    (false, false) => "long-window",
                    (true, true) => "glide-property-mix-early",
                    (true, false) => "glide-property-mix-long",
                },
                sorted[511],
                sorted[972],
                sorted[1013],
                sorted[1023]
            );
            for (index, sample) in samples.into_iter().enumerate() {
                println!("sample {index} {sample:.3}");
            }
            assert_eq!(
                frames
                    .iter()
                    .map(|frame| frame.items().len())
                    .sum::<usize>(),
                if mixed { 750 } else { 1488 },
                "empty diagram items are omitted"
            );
        }
    }
}

#[test]
#[ignore = "exclusive Rez timing: release, measuring.lock, no concurrent cargo/rustc"]
fn activation_at_caps() {
    use std::{hint::black_box, time::Instant};
    let mut samples = Vec::with_capacity(64);
    let mut allocations = Vec::with_capacity(64);
    for iteration in 0..69 {
        let mut budget = GlobalBudget::default();
        let (mut scene, mut view) = gliding_scene("activation", 600, &mut budget);
        let put: Vec<_> = (0..256)
            .map(|index| mixed_track(index, 2, Some(index % 200)))
            .collect();
        let step = prepared(
            &mut scene,
            &mut budget,
            view.layout(),
            json!({"id":"activate","say":vec!["a";200].join(" "),"put":put}),
        );
        let mut frame = sample(&view, 100);
        let start = Instant::now();
        let ((), count) = allocation::count(|| {
            view.activate(black_box(step), 100, 72_728, &mut frame)
                .expect("caps activation");
        });
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        if iteration >= 5 {
            samples.push(elapsed);
            allocations.push(count);
        }
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "workload=activation-600-objects-256-replacement-tracks-200-words samples=64 p50_us={} p95_us={} p99_us={} max_us={}",
        sorted[31], sorted[60], sorted[63], sorted[63]
    );
    for (index, (sample, allocations)) in samples.into_iter().zip(allocations).enumerate() {
        println!("sample {index} {sample:.3} allocations={allocations}");
    }
}
