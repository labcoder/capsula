//! Activation, sampling, the host loop and cuts over prepared steps.
#[path = "support/allocation.rs"]
mod allocation;

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
    view.activate(step, now, segment).expect("fresh activation");
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
    view.frame(now, &mut frame);
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
        matches!(view.activate(pending, 100, 800), Err(PrepareError::Stale)),
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
                "keys":[[0,0],[100,40]]}}]}),
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
        shape.draw.to_bits(),
        0.5_f32.to_bits(),
        "draw rests at its last key"
    );
    assert_eq!(
        shape.transform,
        Affine::default(),
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
    view.activate(next, 1000, 800).expect("fresh activation");
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
    assert_ne!(middle, start, "the group is moving");
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
    view.activate(next, 1225, 800).expect("replacement glide");
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
fn frames_allocate_nothing_at_the_object_and_track_caps() {
    let (mut scene, mut budget, mut view) = fresh();
    let enters = ["fade", "draw", "pop", "slide", "grow", "fly"];
    for chunk in 0..3 {
        let put: Vec<Value> = (chunk * 200..chunk * 200 + 200)
            .map(|n| {
                json!({"shape":{"id":format!("s{n}"),"kind":"rect","box":[0,0,8,8],
                    "enter":enters[n % enters.len()],"dur":1000}})
            })
            .collect();
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":format!("shapes{chunk}"),"put":put}),
            0,
        );
    }
    for chunk in 0..4 {
        let put: Vec<Value> = (chunk * 64..chunk * 64 + 64)
            .map(|n| {
                json!({"anim":{"target":format!("s{n}"),"track":"x","ease":"spring","yoyo":true,
                    "repeat":-1,"keys":(0..32).map(|k| json!([k * 10, k % 2 * 4])).collect::<Vec<_>>()}})
            })
            .collect();
        activate(
            &mut scene,
            &mut budget,
            &mut view,
            json!({"id":format!("tracks{chunk}"),"put":put}),
            0,
        );
    }
    let mut frame = Frame::new();
    view.frame(100, &mut frame);
    assert_eq!(frame.items().len(), 600, "every object is emitted");
    for now in [100, 500, 1500, 59_999, 60_000] {
        let ((), allocations) = allocation::count(|| view.frame(now, &mut frame));
        assert_eq!(allocations, 0, "frame at {now} allocated");
    }
}

fn capped_view(name: &str, count: usize, budget: &mut GlobalBudget) -> SceneView {
    let enters = ["fade", "draw", "pop", "slide", "grow", "fly"];
    let mut scene = new_scene(name);
    let mut view = SceneView::new(&LayoutState::new(&scene), 0);
    activate(
        &mut scene,
        budget,
        &mut view,
        json!({"id":"groups","put":[
            {"diagram":{"id":"g0","box":[0,0,260,300],"enter":"pop","dur":1000}},
            {"diagram":{"id":"g1","in":"g0","box":[0,0,260,300],"enter":"grow","dur":1000}},
            {"diagram":{"id":"g2","in":"g1","box":[0,0,260,300],"dir":"down","enter":"slide","dur":1000}},
            {"node":{"id":"root","in":"g2","label":"Root","enter":"fly","dur":1000}},
            {"node":{"id":"a","in":"g2","label":"A","enter":"pop","dur":1000}},
            {"node":{"id":"b","in":"g2","label":"B","enter":"grow","dur":1000}},
            {"edge":{"id":"ra","in":"g2","from":"root","to":"a","flow":500,"enter":"draw","dur":1000}},
            {"edge":{"id":"rb","in":"g2","from":"root","to":"b","flow":500,"enter":"draw","dur":1000}}
        ]}),
        0,
    );
    let step = prepared(
        &mut scene,
        budget,
        view.layout(),
        json!({"id":"glide","put":[
            {"node":{"id":"c","in":"g2","label":"C","enter":"fly","dur":1000}},
            {"edge":{"id":"rc","in":"g2","from":"root","to":"c","flow":500,"enter":"draw","dur":1000}}
        ]}),
    );
    assert_eq!(step.glides.len(), 1, "each view includes a generated glide");
    view.activate(step, 0, 800).expect("glide activation");
    let shapes = count - 10;
    for start in (0..shapes).step_by(150) {
        let put: Vec<Value> = (start..(start + 150).min(shapes))
            .map(|n| {
                json!({"shape":{
                    "id":format!("s{n}"),"in":"g2","kind":"polygon","points":[[0,0],[8,0],[8,8],[0,8]],
                    "enter":enters[n % enters.len()],"dur":1000
                }})
            })
            .collect();
        activate(
            &mut scene,
            budget,
            &mut view,
            json!({"id":format!("shapes{start}"),"put":put}),
            0,
        );
    }
    for chunk in 0..4 {
        let put: Vec<Value> = (chunk * 64..chunk * 64 + 64)
            .map(|n| {
                json!({"anim":{
                    "target":format!("s{}",n / 2),"track":if n % 2 == 0 { "x" } else { "y" },
                    "ease":"spring","yoyo":true,"repeat":-1,
                    "keys":(0..32).map(|k| json!([k * 900, k % 2 * 4])).collect::<Vec<_>>()
                }})
            })
            .collect();
        activate(
            &mut scene,
            budget,
            &mut view,
            json!({"id":format!("tracks{chunk}"),"put":put}),
            0,
        );
    }
    assert_eq!(
        (
            scene.usage().objects,
            scene.usage().tracks,
            scene.usage().keys
        ),
        (count, 256, 8192),
        "each view reaches its legal object, track and key counts"
    );
    view
}

fn capped_views() -> Vec<SceneView> {
    let mut budget = GlobalBudget::default();
    let views: Vec<_> = [600, 600, 150, 150]
        .into_iter()
        .enumerate()
        .map(|(index, count)| capped_view(&format!("view{index}"), count, &mut budget))
        .collect();
    assert_eq!(
        (
            budget.usage().scenes,
            budget.usage().objects,
            budget.usage().tracks
        ),
        (4, 1500, 1024),
        "all four scenes share one global budget at the legal caps"
    );
    views
}

#[test]
fn four_views_reach_the_legal_caps_without_frame_allocations() {
    let views = capped_views();
    let mut frames: Vec<_> = (0..4).map(|_| Frame::new()).collect();
    for now in [100, 2000, 59_999, 60_000] {
        let ((), allocations) = allocation::count(|| {
            for (view, frame) in views.iter().zip(&mut frames) {
                view.frame(now, frame);
            }
        });
        assert_eq!(
            allocations, 0,
            "four-view sampling at {now} allocates nothing"
        );
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.items().len())
                .sum::<usize>(),
            1488,
            "all geometry survives at the full caps"
        );
    }
}

#[test]
#[ignore = "exclusive Rez timing: release, measuring.lock, no concurrent cargo/rustc"]
fn frame_four_views_max() {
    use std::{hint::black_box, time::Instant};
    let views = capped_views();
    let mut frames: Vec<_> = (0..4).map(|_| Frame::new()).collect();
    for early in [true, false] {
        for tick in 0..64 {
            let now = if early { 100 } else { tick * 16 };
            for (view, frame) in views.iter().zip(&mut frames) {
                view.frame(now, frame);
            }
        }
        let mut samples = Vec::with_capacity(1024);
        let ((), allocations) = allocation::count(|| {
            for tick in 0..1024 {
                let now = if early { 100 } else { (tick + 64) * 16 };
                let start = Instant::now();
                for (view, frame) in views.iter().zip(&mut frames) {
                    view.frame(black_box(now), black_box(frame));
                }
                samples.push(start.elapsed().as_secs_f64() * 1e6);
            }
        });
        assert_eq!(
            allocations, 0,
            "all four views allocate nothing while sampling"
        );
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        println!(
            "workload={} samples=1024 allocations={allocations} p50_us={} p95_us={} p99_us={} max_us={}",
            if early {
                "early-all-entrances"
            } else {
                "long-window"
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
            1488,
            "only the twelve empty diagram items are omitted"
        );
    }
}
