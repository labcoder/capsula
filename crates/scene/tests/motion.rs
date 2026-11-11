//! Activation, sampling, the host loop and cuts over prepared steps.
#[path = "support/allocation.rs"]
mod allocation;
#[path = "support/motion.rs"]
mod fixtures;
#[path = "support/scene.rs"]
mod scene_support;

use fixtures::{Fixture, four_views, gliding_scene, mixed_track};
use scene_support::{Measure, id, new_scene, prepared};

use kumochi_scene::{
    Admission, Frame, FrameItem, GlobalBudget, Handle, Record, SceneState, SceneView, StopCut,
    Text,
    display::Paint,
    geom::Affine,
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover,
    },
    motion::{at_word_offset_ms, synthetic_duration_ms},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn fresh() -> (SceneState, GlobalBudget, SceneView) {
    let scene = new_scene("motion");
    let view = SceneView::new(&LayoutState::new(&scene), scene.activated());
    (scene, GlobalBudget::default(), view)
}

fn commit(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    value: Value,
) -> kumochi_scene::Committed {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let Record::Step(step) =
        parse_record(&serde_json::to_vec(&envelope).expect("fixture JSON")).expect("typed fixture")
    else {
        panic!("step");
    };
    let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted fixture") else {
        panic!("new step");
    };
    scene.commit(candidate, budget).expect("fresh candidate")
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
/// 256 rectangles, `r0` to `r255`.
fn rects() -> Vec<Value> {
    (0..256).map(|n| rect(&format!("r{n}"))).collect()
}
fn polygons(count: usize) -> Vec<Value> {
    (0..count)
        .map(|n| {
            json!({"shape":{"id":format!("r{n}"),"kind":"polygon",
            "points":[[0,0],[8,0],[8,8],[0,8]]}})
        })
        .collect()
}
/// One opacity track for each of [`rects`].
fn tracks() -> Vec<Value> {
    (0..256)
        .map(
            |n| json!({"anim":{"target":format!("r{n}"),"track":"opacity","keys":[[0,0],[100,1]]}}),
        )
        .collect()
}
/// Runs `run` on this thread and returns its value with the requested bytes it still holds.
fn holding<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let (value, _, [allocated, freed]) = allocation::usage(run);
    let held = allocated
        .checked_sub(freed)
        .expect("the run frees only what it allocated");
    (value, held)
}
/// Prepares B4's 64 retained steps over 256 rectangles: a quiet prefix, one opacity track per
/// rectangle, then quiet prefixes, each with the requested bytes its preparation retained.
fn track_heavy_history(
    name: &str,
    budget: &mut GlobalBudget,
) -> (SceneState, Vec<(PreparedStep, usize)>) {
    retained_history(name, budget, &rects())
}

fn retained_history(
    name: &str,
    budget: &mut GlobalBudget,
    shapes: &[Value],
) -> (SceneState, Vec<(PreparedStep, usize)>) {
    let mut scene = new_scene(name);
    let empty = LayoutState::new(&scene);
    let theme = Arc::new(Theme::default());
    let mut steps: Vec<_> = shapes
        .chunks(256)
        .enumerate()
        .map(|(index, shapes)| json!({"id":format!("shapes{index}"),"put":shapes}))
        .collect();
    steps.extend([
        json!({"id":"plain","put":[]}),
        json!({"id":"tracks","put":tracks()}),
    ]);
    steps.extend((steps.len()..64).map(|n| json!({"id":format!("quiet{n}"),"put":[]})));
    let mut history: Vec<(PreparedStep, usize)> = Vec::with_capacity(steps.len());
    for value in steps {
        let ticket = commit(&mut scene, budget, value).preparation;
        let before = history.last().map_or(&empty, |(step, _)| &step.after);
        let (step, held) = holding(|| {
            let input = PrepareInput::new(
                ticket,
                before.clone(),
                SurfaceGeometry::default(),
                Arc::clone(&theme),
            )
            .expect("consecutive input");
            prepare(input, &mut Measure, &AtomicBool::new(false)).expect("legal history step")
        });
        step.validate(&scene, before).expect("current result");
        history.push((step, held));
    }
    (scene, history)
}

#[test]
fn a_frame_accounts_for_its_reserved_item_world_and_pose_buffers() {
    let (frame, [allocations, _], [allocated, _]) = allocation::usage(Frame::new);
    assert_eq!(allocations, 3);
    assert_eq!(frame.resident_bytes(), allocated);
    let ((), _, [_, released]) = allocation::usage(|| drop(frame));
    assert_eq!(released, allocated);
}

/// Requested bytes of one `Arc<()>`: the version header a cloned layout shares, not copies.
fn shared_version_header() -> usize {
    let (version, [allocations, _], [allocated, _]) = allocation::usage(|| Arc::new(()));
    assert_eq!(allocations, 1, "one independent Arc header allocation");
    drop(version);
    allocated
}

#[test]
fn a_resting_view_covers_its_independently_measured_storage() {
    let shared_version = shared_version_header();
    let mut scene = new_scene("view_storage");
    let mut budget = GlobalBudget::default();
    let empty = LayoutState::new(&scene);
    let filled = prepared(
        &mut scene,
        &mut budget,
        &empty,
        json!({
            "id":"one", "put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}}]
        }),
    );
    for layout in [&empty, &filled.after] {
        let (view, _, [allocated, temporary]) = allocation::usage(|| SceneView::new(layout, 0));
        let retained = allocated - temporary;
        assert_eq!(
            view.resident_bytes(),
            retained + shared_version,
            "the index copy is charged the version allocation it shares with its layout"
        );
        let ((), _, [_, released]) = allocation::usage(|| drop(view));
        assert_eq!(
            released, retained,
            "the original layouts still own shared geometry"
        );
    }
}

#[test]
fn an_active_view_releases_its_whole_charge_but_the_shared_version() {
    let shared_version = shared_version_header();
    let two = json!([[0, 0], [100, 1]]);
    let five = json!([[0, 0], [10, 0.5], [25, 1], [50, 0], [100, 1]]);
    for (keys, at_25) in [(two, 0.25_f32), (five, 1.0)] {
        let (mut scene, mut budget, mut view) = fresh();
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":"fade","put":[
                {"shape":{"id":"s","kind":"rect","box":[0,0,10,10],"fill":"blue"}},
                {"anim":{"target":"s","track":"opacity","ease":"linear","keys":keys}}
            ]}),
            0,
        );
        assert_eq!(
            opacity(&view, 25, "s"),
            at_25.to_bits(),
            "the track is live"
        );
        // The clone keeps the shared geometry and version alive, so the drop frees only what the
        // view owns: its index copy, nodes, paint order, tracks, keys and generation.
        let layout = view.layout().clone();
        let charge = view.resident_bytes();
        let ((), _, [_, released]) = allocation::usage(|| drop(view));
        assert_eq!(charge, released + shared_version);
        drop(layout);
    }
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
fn the_first_appearance_is_the_earliest_object_the_step_put_with_something_to_paint() {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"first","put":[rect("s")]}),
        0,
    );
    assert_eq!(view.first_appearance(), Some(0));
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
        1000,
    );
    assert_eq!(
        view.first_appearance(),
        Some(3018),
        "the child appears with its word-timed, delayed parent; the carried rectangle doesn't count"
    );
    assert_eq!(item(&view, 3017, "child"), None);
    assert_eq!(opacity(&view, 3018, "child"), 0.0_f32.to_bits());
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"moves","put":[{"anim":{"target":"s","track":"x","keys":[[0,0],[100,10]]}}]}),
        4000,
    );
    assert_eq!(view.first_appearance(), None, "a track puts no object");

    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"group","put":[
            {"diagram":{"id":"e","delay":100}},
            {"shape":{"id":"inner","in":"e","kind":"rect","box":[0,0,8,8],"delay":300}}
        ]}),
        0,
    );
    assert_eq!(
        view.first_appearance(),
        Some(300),
        "the diagram appears first but paints nothing"
    );
    assert!(sample(&view, 299).items().is_empty());
    assert!(item(&view, 300, "inner").is_some());
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
fn step_and_numeric_debug_do_not_expose_drawing_arguments() {
    let value = json!({"id":"sentinels","start":{"after":"earlier","ms":54321},"put":[
        {"table":{"id":"table","cols":["Value","Flag"]}},
        {"row":{"id":"row","in":"table","cells":[123_456.5,true]}},
        {"chart":{"id":"chart","kind":"line","box":[0,300,400,300],"y":{"min":123_456.5}}},
        {"series":{"id":"series","in":"chart","data":[123_456.5]}},
        {"chart":{"id":"scatter","kind":"scatter","box":[400,300,400,300]}},
        {"series":{"id":"points","in":"scatter","data":[[1234.5,123_456.5]]}},
        {"line":{"id":"line","from":[1234.5,0],"to":[1234.5,10],"color":"#abcdef","delay":34567,"dur":45678,"at_word":23456}},
        {"anim":{"target":"line","track":"x","keys":[[0,1234.5]],"repeat":56789}},
        {"anim":{"target":"line","track":"color","keys":[[0,"#abcdef"]]}}
    ]});
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let kumochi_scene::Op::Chart(chart) = &step.put[2] else {
        panic!("chart fixture");
    };
    assert_eq!(
        chart.y.min.expect("axis numeric sentinel").get().to_bits(),
        123_456.5_f64.to_bits()
    );
    let kumochi_scene::Op::Line(line) = &step.put[6] else {
        panic!("line fixture");
    };
    let kumochi_scene::record::Anchor::At(from) = &line.from else {
        panic!("coordinate sentinel");
    };
    assert_eq!(from[0].get().to_bits(), 1234.5_f32.to_bits());
    assert_eq!(line.delay.expect("timing sentinel").get(), 34567);
    for output in [
        format!("{step:?}"),
        format!("{:?}", chart.y),
        format!("{:?}", line.from),
        format!("{:?}", line.color),
        format!("{:?}", line.delay),
    ] {
        for sentinel in [
            "123456.5", "1234.5", "#abcdef", "abcdef", "11259375", "34567", "45678", "23456",
            "56789", "54321",
        ] {
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
        None,
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

/// A rectangle with a live x track and a color track held until 2,000 ms, activated at 0.
fn tracked() -> (SceneState, GlobalBudget, SceneView) {
    let (mut scene, mut budget, mut view) = fresh();
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"tracks","put":[rect("s"),
            {"anim":{"target":"s","track":"x","ease":"linear","keys":[[0,0],[1000,100]]}},
            {"anim":{"target":"s","track":"color","ease":"linear","delay":2000,
                "keys":[[0,"red"],[500,"blue"]]}}
        ]}),
        0,
    );
    (scene, budget, view)
}

#[test]
fn a_staged_activation_debug_keeps_content_private() {
    let marker = "staged-private-content";
    let (mut scene, mut budget, view) = fresh();
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"private","say":marker,"put":[
            {"text":{"id":"label","at":[12,34],"text":marker}}
        ]}),
    );
    let stage = view.prepare_activation(&next, 0, 800).expect("stage");
    let output = format!("{stage:?}");
    assert!(!output.contains(marker));
    assert!(!output.contains(&format!("{:?}", marker.as_bytes())));
    assert_eq!(
        output,
        "PreparedActivation { view: SceneView { activated: 1, objects: 1, tracks: 0, .. }, superseded: 0 }"
    );
}

fn pose(view: &SceneView, now: u64) -> (Affine, Option<Paint>) {
    let drawn = item(view, now, "s").expect("emitted");
    (drawn.transform, drawn.color)
}

#[test]
fn refused_and_discarded_stages_keep_live_and_delayed_tracks() {
    let (mut scene, mut budget, view) = tracked();
    let times = [500, 1999, 2250, 2500];
    let before = times.map(|now| pose(&view, now));
    assert_eq!(before[0].0, Affine([1.0, 0.0, 0.0, 1.0, 50.0, 0.0]));
    assert_ne!(before[1].1, before[2].1, "the hold ends at 2,000 ms");
    assert_ne!(before[2].1, before[3].1, "and the track moves on");
    let (mut other, mut other_budget, other_view) = fresh();
    let foreign = prepared(
        &mut other,
        &mut other_budget,
        other_view.layout(),
        json!({"id":"elsewhere","put":[rect("o")]}),
    );
    assert!(
        matches!(
            view.prepare_activation(&foreign, 600, 800),
            Err(PrepareError::Stale)
        ),
        "a step prepared on another layout is stale"
    );
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"unrelated","put":[rect("z")]}),
    );
    drop(view.prepare_activation(&next, 600, 800).expect("stage"));
    assert_eq!(times.map(|now| pose(&view, now)), before);
    assert_eq!((view.activated(), view.next_wake(600)), (1, Some(1000)));
}

#[test]
fn an_installed_stage_carries_the_live_and_delayed_tracks_it_does_not_replace() {
    let (mut scene, mut budget, mut view) = tracked();
    let times = [1999, 2250, 2500];
    let delayed = times.map(|now| pose(&view, now).1);
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"unrelated","put":[rect("z")]}),
    );
    let staged = view.prepare_activation(&next, 250, 800).expect("stage");
    drop(next);
    view.install(staged, &mut Frame::new()).expect("fresh");
    assert_eq!(view.activated(), 2);
    assert_eq!(
        transform(&view, 500, "s"),
        Affine([1.0, 0.0, 0.0, 1.0, 50.0, 0.0]),
        "the running x track keeps its own clock"
    );
    assert_eq!(
        times.map(|now| pose(&view, now).1),
        delayed,
        "the delayed color carries through its hold"
    );
    assert_eq!(view.next_wake(250), Some(1000));
}

#[test]
fn a_stage_installs_only_into_the_view_generation_it_was_staged_from() {
    let (mut scene, mut budget, mut view) = tracked();
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"unrelated","put":[rect("z")]}),
    );
    let (mut other, mut other_budget, mut other_view) = fresh();
    activate(
        &mut other,
        &mut other_budget,
        &mut other_view,
        json!({"id":"elsewhere","put":[rect("o")]}),
        0,
    );
    let mut other_frame = sample(&other_view, 0);
    let foreign = view.prepare_activation(&next, 250, 800).expect("stage");
    let refused = other_view.install(foreign, &mut other_frame);
    assert!(matches!(refused, Err(PrepareError::Stale)));
    assert_eq!(other_view.activated(), 1);
    assert_eq!(
        other_frame.items().len(),
        1,
        "the other view's frame is not cleared"
    );

    let current = view.prepare_activation(&next, 250, 800).expect("stage");
    let late = view.prepare_activation(&next, 300, 800).expect("stage");
    view.install(current, &mut Frame::new()).expect("fresh");
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"later","put":[rect("w")]}),
        400,
    );
    let mut frame = sample(&view, 500);
    let before = (frame.items().len(), pose(&view, 500));
    let refused = view.install(late, &mut frame);
    assert!(matches!(refused, Err(PrepareError::Stale)));
    assert_eq!(
        view.activated(),
        3,
        "a stage from before the advance cannot rewind it"
    );
    assert_eq!((frame.items().len(), pose(&view, 500)), before);
    assert_eq!(before.0, 3, "s, z and w stay in the bound frame");
    view.frame(500, &mut frame).expect("the frame stays bound");
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
                    std::hint::black_box(
                        frame
                            .paint_items()
                            .map(|(_, _, primitives)| primitives.len())
                            .sum::<usize>(),
                    );
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
fn quiet_prefixes_after_tracks_charge_the_motion_maps_they_copy() {
    let mut budget = GlobalBudget::default();
    let (scene, history) = track_heavy_history("tracks", &mut budget);
    assert_eq!(
        (history.len(), scene.usage().objects, scene.usage().tracks),
        (64, 256, 256),
        "the history fills the step cap with one track per rectangle"
    );
    let (plain, plain_bytes) = &history[1];
    let (tip, tip_bytes) = &history[63];
    assert!(
        tip_bytes + plain.resident_bytes <= plain_bytes + tip.resident_bytes,
        "61 steps after its tracks the tip retains {tip_bytes} bytes and charges {}; the prefix \
         before them retains {plain_bytes} and charges {}",
        tip.resident_bytes,
        plain.resident_bytes
    );
}

#[test]
fn track_heavy_prefixes_hold_no_more_than_their_charges() {
    let mut budget = GlobalBudget::default();
    let (_, history) = track_heavy_history("bounds", &mut budget);
    for (index, (step, held)) in history.iter().enumerate() {
        assert!(
            *held <= step.resident_bytes,
            "step {index} holds {held} bytes but charges {}",
            step.resident_bytes
        );
    }
    let ((plain, plain_held), (tracked, tracked_held)) = (&history[1], &history[2]);
    assert_eq!(
        tracked_held - plain_held,
        tracked.resident_bytes - plain.resident_bytes,
        "over the same index, the tracks step charges exactly the 256 motion maps it allocates"
    );
}

#[test]
fn full_object_caps_with_sixty_four_prefixes_fit_the_residency_budget() {
    let rectangles: Vec<_> = (0..600).map(|n| rect(&format!("r{n}"))).collect();
    for (name, scenes, shapes, objects, segments) in [
        ("single", 1, rectangles, 600, 17_400),
        ("global", 4, polygons(375), 1500, 6000),
    ] {
        let mut budget = GlobalBudget::default();
        let histories: Vec<_> = (0..scenes)
            .map(|index| retained_history(&format!("{name}{index}"), &mut budget, &shapes))
            .collect();
        assert_eq!(
            (
                budget.usage().scenes,
                budget.usage().objects,
                budget.usage().segments,
                budget.usage().tracks
            ),
            (scenes, objects, segments, scenes * 256)
        );
        let (held, charged) = histories
            .iter()
            .flat_map(|(_, history)| history)
            .fold((0, 0), |(held, charged), (step, bytes)| {
                (held + bytes, charged + step.resident_bytes)
            });
        assert!(
            held <= charged,
            "{name}: {held} held bytes exceed {charged} charged"
        );
        assert!(
            charged <= 64 * 1024 * 1024,
            "{name}: {charged} requested bytes exceed the 64 MiB budget"
        );
        println!(
            "workload={name} prefixes={} held_bytes={held} charged_bytes={charged}",
            scenes * 64
        );
    }
}

#[test]
fn removing_compiled_objects_releases_the_empty_private_fact_index() {
    let mut budget = GlobalBudget::default();
    let mut scene = new_scene("facts");
    let empty = LayoutState::new(&scene);
    let compiled = prepared(
        &mut scene,
        &mut budget,
        &empty,
        json!({"id":"compiled","put":[{"diagram":{"id":"d"}},
            {"mermaid":{"in":"d","src":"flowchart RL\nA --> B\n"}}]}),
    );
    let ticket = commit(
        &mut scene,
        &mut budget,
        json!({"id":"remove","remove":["d","--1.0","A","B","--1.1"]}),
    )
    .preparation;
    let theme = Arc::new(Theme::default());
    let (removed, held) = holding(|| {
        let input = PrepareInput::new(
            ticket,
            compiled.after.clone(),
            SurfaceGeometry::default(),
            theme,
        )
        .expect("next input");
        prepare(input, &mut Measure, &AtomicBool::new(false)).expect("removed diagram")
    });
    assert_eq!(removed.after.revealed_at_rest().len(), 0);
    assert!(
        held <= removed.resident_bytes,
        "empty compiled layout holds {held} bytes but charges {}",
        removed.resident_bytes
    );
}

#[test]
fn removed_recovered_and_cut_prefixes_hold_no_more_than_their_charges() {
    let theme = Arc::new(Theme::default());
    let mut budget = GlobalBudget::default();
    let mut scene = new_scene("removals");
    let empty = LayoutState::new(&scene);
    let shapes = prepared(
        &mut scene,
        &mut budget,
        &empty,
        json!({"id":"shapes","put":rects()}),
    );
    let removals: Vec<_> = (0..200).map(|n| format!("r{n}")).collect();
    let ticket = commit(
        &mut scene,
        &mut budget,
        json!({"id":"remove","remove":removals}),
    )
    .preparation;
    let (removal, held) = holding(|| {
        let input = PrepareInput::new(
            ticket,
            shapes.after.clone(),
            SurfaceGeometry::default(),
            Arc::clone(&theme),
        )
        .expect("consecutive input");
        prepare(input, &mut Measure, &AtomicBool::new(false)).expect("removal step")
    });
    assert!(
        held <= removal.resident_bytes,
        "removing 200 of 256 rectangles holds {held} bytes but charges {}",
        removal.resident_bytes
    );
    let (recovered, held) = holding(|| {
        let (_, layout) = recover(
            scene.header().clone(),
            scene.recovery_log(),
            SurfaceGeometry::default(),
            &theme,
            &mut Measure,
            &AtomicBool::new(false),
            None,
        )
        .expect("recovery");
        layout
    });
    assert_eq!(
        recovered.revealed_at_rest(),
        removal.after.revealed_at_rest(),
        "recovery keeps the 56 survivors"
    );
    assert!(
        held <= recovered.resident_bytes(),
        "the recovered prefix holds {held} bytes but charges {}",
        recovered.resident_bytes()
    );
    let (kept, held) = holding(|| {
        let mut budget = GlobalBudget::default();
        let mut scene = new_scene("cut");
        let empty = LayoutState::new(&scene);
        let shapes = prepared(
            &mut scene,
            &mut budget,
            &empty,
            json!({"id":"shapes","put":rects()}),
        );
        let tracked = prepared(
            &mut scene,
            &mut budget,
            &shapes.after,
            json!({"id":"tracks","put":tracks()}),
        );
        let revealed = tracked
            .after
            .revealed_at_rest()
            .into_iter()
            .step_by(2)
            .collect();
        scene
            .stop(
                &StopCut {
                    activated: 2,
                    revealed: Bounded::new(revealed).expect("bounded cut"),
                },
                &mut budget,
            )
            .expect("valid cut");
        tracked.after.cut(&scene).expect("kept prefix")
    });
    assert_eq!(
        (kept.revealed_at_rest().len(), kept.revealed_at_rest()),
        (128, SceneView::new(&kept, 2).stop(0)),
        "every other rectangle survives, all of them revealed at rest"
    );
    assert!(
        held <= kept.resident_bytes(),
        "the kept prefix, as sole owner, holds {held} bytes but charges {}",
        kept.resident_bytes()
    );
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
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
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
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

#[test]
#[ignore = "requested-byte report for B4's 64 retained prefixes; run alone with --nocapture"]
fn measure_track_heavy_prefix_residency() {
    let mut budget = GlobalBudget::default();
    let shapes = polygons(256);
    let histories: Vec<_> = (0..4)
        .map(|index| retained_history(&format!("tracks{index}"), &mut budget, &shapes).1)
        .collect();
    assert_eq!(
        (
            budget.usage().scenes,
            budget.usage().objects,
            budget.usage().tracks
        ),
        (4, 1024, 1024),
        "four histories share the legal global caps"
    );
    let sum = |steps: std::ops::Range<usize>| {
        histories
            .iter()
            .flat_map(|history| &history[steps.clone()])
            .fold([0, 0], |[retained, charged], (step, bytes)| {
                [retained + bytes, charged + step.resident_bytes]
            })
    };
    // "shapes" allocates the geometry and sources later prefixes share; each later step holds its
    // own index copy.
    for (phase, steps) in [
        ("shapes", 0..1),
        ("plain", 1..2),
        ("tracks", 2..3),
        ("quiet", 3..64),
        ("total", 0..64),
    ] {
        let [retained, charged] = sum(steps.clone());
        println!(
            "phase={phase} steps={} retained_bytes={retained} charged_bytes={charged}",
            4 * steps.len()
        );
    }
}
