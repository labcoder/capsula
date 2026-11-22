use super::{new_scene, prepared};
use kumochi_scene::motion::synthetic_duration_ms;
use kumochi_scene::{Frame, GlobalBudget, SceneState, SceneView, layout::LayoutState};
use serde_json::{Value, json};

fn capped_view(name: &str, count: usize, budget: &mut GlobalBudget) -> SceneView {
    let enters = ["fade", "draw", "pop", "slide", "grow", "fly"];
    let mut scene = new_scene(name);
    let mut view = SceneView::new(&LayoutState::new(&scene), 0);
    // A zero delay keeps each step's authored timing, so no default stagger spreads it out.
    activate(
        &mut scene,
        budget,
        &mut view,
        json!({"id":"groups","put":[
            {"diagram":{"id":"g0","box":[0,0,260,300],"enter":"pop","dur":1000,"delay":0}},
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
            {"node":{"id":"c","in":"g2","label":"C","enter":"fly","dur":1000,"delay":0}},
            {"edge":{"id":"rc","in":"g2","from":"root","to":"c","flow":500,"enter":"draw","dur":1000}}
        ]}),
    );
    assert_eq!(step.glides.len(), 1, "each view includes a generated glide");
    view.activate(step, 0, 800, &mut Frame::new())
        .expect("glide activation");
    let shapes = count - 10;
    for start in (0..shapes).step_by(150) {
        let put: Vec<Value> = (start..(start + 150).min(shapes))
            .map(|n| {
                json!({"shape":{
                    "id":format!("s{n}"),"in":"g2","kind":"polygon","points":[[0,0],[8,0],[8,8],[0,8]],
                    "enter":enters[n % enters.len()],"dur":1000,"delay":0
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

pub(super) fn mixed_track(index: usize, key_count: usize, word: Option<usize>) -> Value {
    let property = ["scale", "color", "rotate", "pos"][index % 4];
    let keys: Vec<_> = (0..key_count)
        .map(|key| {
            let high = key % 2 == 1;
            let value = match property {
                "scale" => json!(if high { 4 } else { 1 }),
                "color" => json!(if high { "#0000ff" } else { "#ff0000" }),
                "rotate" => json!(if high { 45 } else { 0 }),
                _ => json!(if high { [4, 4] } else { [0, 0] }),
            };
            json!([key * (800 + index % 61), value])
        })
        .collect();
    json!({"anim":{"target":format!("n{}",index / 4),"track":property,
        "ease":"spring","repeat":-1,"yoyo":true,"at_word":word,"keys":keys}})
}

pub(super) fn gliding_scene(
    name: &str,
    count: usize,
    budget: &mut GlobalBudget,
) -> (SceneState, SceneView) {
    let mut scene = new_scene(name);
    let mut view = SceneView::new(&LayoutState::new(&scene), 0);
    let groups = count / 2;
    // A zero delay keeps each step's authored timing, so no default stagger spreads it out.
    for start in (0..groups).step_by(80) {
        let end = (start + 80).min(groups);
        let put: Vec<_> = (start..end)
            .flat_map(|index| {
                let parent = match index {
                    1 => Some("g0"),
                    2 => Some("g1"),
                    _ => None,
                };
                [
                    json!({"diagram":{"id":format!("g{index}"),"in":parent,"box":[0,0,1,1],
                "enter":(["pop","grow","slide","fly"][index % 4]),"dur":1000,"delay":0}}),
                    json!({"node":{"id":format!("n{index}"),"in":format!("g{index}"),"label":"Node",
                "enter":(["fade","draw","pop","slide","grow","fly"][index % 6]),"dur":1000}}),
                ]
            })
            .collect();
        let step = prepared(
            &mut scene,
            budget,
            view.layout(),
            json!({"id":format!("groups{start}"),"put":put}),
        );
        assert_eq!(
            step.glides.len(),
            end - start,
            "every one-node diagram overflows its narrow viewport and glides"
        );
        view.activate(step, 0, 800, &mut Frame::new())
            .expect("glide batch");
    }
    for chunk in 0..8 {
        let put: Vec<_> = (chunk * 32..chunk * 32 + 32)
            .map(|index| mixed_track(index, 32, None))
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
        "the mixed fixture reaches its admitted caps"
    );
    (scene, view)
}

#[derive(Clone, Copy)]
pub(super) enum Fixture {
    Control,
    Glide,
}

pub(super) fn four_views(fixture: Fixture) -> Vec<SceneView> {
    let mut budget = GlobalBudget::default();
    let views: Vec<_> = [600, 600, 150, 150]
        .into_iter()
        .enumerate()
        .map(|(index, count)| {
            let name = format!("view{index}");
            match fixture {
                Fixture::Glide => gliding_scene(&name, count, &mut budget).1,
                Fixture::Control => capped_view(&name, count, &mut budget),
            }
        })
        .collect();
    assert_eq!(
        (
            budget.usage().scenes,
            budget.usage().objects,
            budget.usage().tracks
        ),
        (4, 1500, 1024),
        "all four views share the legal global caps"
    );
    views
}

/// Commits, prepares on the view's layout and activates one step at `now`.
pub(super) fn activate(
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

pub(super) fn sample(view: &SceneView, now: u64) -> Frame {
    let mut frame = Frame::new();
    view.frame(now, &mut frame).expect("fresh frame");
    frame
}
