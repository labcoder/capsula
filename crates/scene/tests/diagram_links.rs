//! Exact geometry for the diagram look review's visible-link and clearance regressions.
use kumochi_scene::{
    GlobalBudget, StopCut,
    display::{Prim, TextMeasure},
    geom::{Rect, Vec2},
    layout::{LayoutState, SurfaceGeometry},
    motion::{Frame, FrameItem, SceneView},
    record::Rgb,
    text::Bounded,
    theme::Theme,
};
use serde_json::json;

#[path = "support/rectangle_outline.rs"]
mod outline;
#[path = "support/connected_stroke.rs"]
mod painted;
#[path = "support/scene.rs"]
mod scene_support;
#[path = "support/visible_fill.rs"]
mod visible;
use scene_support::{id, new_scene, prepared, prepared_on};

fn frame(layout: &LayoutState) -> Frame {
    let mut frame = Frame::new();
    SceneView::new(layout, 0)
        .frame(u64::MAX, &mut frame)
        .expect("resting frame");
    frame
}

fn item<'a>(layout: &LayoutState, frame: &'a Frame, name: &str) -> &'a FrameItem {
    let handle = layout.handle(&id(name)).expect("prepared handle");
    frame
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .expect("painted item")
}

fn chip(item: &FrameItem) -> Rect {
    assert!(item.opacity > 0.0, "the label chip is visible");
    item.primitives
        .iter()
        .filter_map(visible::visible_fill)
        .filter(|(rgb, _)| *rgb == Theme::default().surface)
        .map(|(_, bounds)| item.transform.bounds(bounds))
        .reduce(Rect::union)
        .expect("chip")
}

fn ink(item: &FrameItem, label: &str) -> Rect {
    let (origin, size) = item
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Text {
                origin,
                text,
                style,
                max_width,
                align,
                color,
                ..
            } if text.as_str() == label && item.opacity * color.opacity > 0.0 => {
                let [w, h] = scene_support::Measure
                    .measure(label, *style, *max_width)
                    .expect("ink");
                let spare = max_width.unwrap_or(w) - w;
                let x = match align {
                    kumochi_scene::record::Align::Left => 0.0,
                    kumochi_scene::record::Align::Center => spare * 0.5,
                    kumochi_scene::record::Align::Right => spare,
                };
                Some((*origin + Vec2::new(x, 0.0), Vec2::new(w, h)))
            }
            _ => None,
        })
        .expect("painted label");
    item.transform.bounds(Rect { origin, size })
}

fn route(item: &FrameItem) -> Vec<Vec2> {
    assert!(
        item.opacity > 0.0 && item.draw >= 1.0,
        "the full edge is visible"
    );
    painted::connected_stroke(&item.primitives)
        .expect("the painted edge connects its ports")
        .into_iter()
        .map(|point| item.transform.point(point))
        .collect()
}

fn ports(route: &[Vec2]) -> [Vec2; 2] {
    [
        *route.first().expect("start port"),
        *route.last().expect("end port"),
    ]
}

fn assert_directed_ports(edge: &FrameItem, route: &[Vec2], expected: [Vec2; 2]) {
    let actual = ports(route);
    assert!(
        actual == expected || actual == [expected[1], expected[0]],
        "route ports {actual:?} do not meet {expected:?}"
    );
    let has_tip = |tip| {
        edge.primitives.iter().any(|primitive| {
            let Prim::Fill { points, color } = primitive else {
                return false;
            };
            if color.opacity <= 0.0 || color.rgb == Theme::default().surface {
                return false;
            }
            let points: Vec<_> = points.iter().map(|p| edge.transform.point(*p)).collect();
            points.contains(&tip)
                && points.windows(2).any(|pair| {
                    let a = pair[0] - tip;
                    let b = pair[1] - tip;
                    (a.x * b.y - a.y * b.x).abs() > 0.001
                })
        })
    };
    assert!(has_tip(expected[1]), "the arrowhead is at the destination");
    assert!(!has_tip(expected[0]), "there is no source arrowhead");
}

fn crossing(route: &[Vec2], axis: fn(Vec2) -> f32, at: f32) -> Vec2 {
    route
        .windows(2)
        .rev()
        .find_map(|pair| {
            let [a, b] = [pair[0], pair[1]];
            let (start, end) = (axis(a), axis(b));
            let fraction = (at - start) / (end - start);
            (0.0..=1.0)
                .contains(&fraction)
                .then(|| a + (b - a) * fraction)
        })
        .expect("route crosses the requested coordinate")
}

fn distance_to_route(point: Vec2, route: &[Vec2]) -> f32 {
    route
        .windows(2)
        .map(|pair| {
            let delta = pair[1] - pair[0];
            let offset = point - pair[0];
            let length_sq = delta.x * delta.x + delta.y * delta.y;
            let fraction = if length_sq > 0.0 {
                ((offset.x * delta.x + offset.y * delta.y) / length_sq).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let residual = offset - delta * fraction;
            residual.x.hypot(residual.y)
        })
        .fold(f32::INFINITY, f32::min)
}

fn assert_route_eq(a: &[Vec2], b: &[Vec2]) {
    let [start, end] = ports(a);
    assert!(
        ports(b) == [start, end] || ports(b) == [end, start],
        "routes preserve both outline ports"
    );
    for (samples, curve) in [(a, b), (b, a)] {
        assert!(
            samples
                .iter()
                .all(|point| distance_to_route(*point, curve) < 1.0),
            "both routes cover the same visible curve"
        );
    }
}

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.75, "{actual} vs {expected}");
}

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("valid world bounds")
        .expect("prepared bounds")
}

fn border_strips(item: &FrameItem, world_bounds: Rect) -> [Rect; 4] {
    assert!(item.opacity > 0.0, "the group outline is visible");
    let rect = item
        .transform
        .inverse()
        .expect("group transform")
        .bounds(world_bounds);
    let ([width, bottom, left, right], _) =
        outline::rectangle_outline(&item.primitives, rect, Theme::default().line);
    assert!(
        width > 0.0
            && [bottom, left, right]
                .into_iter()
                .all(|w| (w - width).abs() < 0.001),
        "the visible group outline has the same width on all four sides"
    );
    let r = width * 0.5;
    let first = rect.origin - Vec2::new(r, r);
    let last = rect.max() - Vec2::new(r, r);
    let horizontal = Vec2::new(rect.size.x + width, width);
    let vertical = Vec2::new(width, rect.size.y + width);
    [
        Rect {
            origin: first,
            size: horizontal,
        },
        Rect {
            origin: Vec2::new(first.x, last.y),
            size: horizontal,
        },
        Rect {
            origin: first,
            size: vertical,
        },
        Rect {
            origin: Vec2::new(last.x, first.y),
            size: vertical,
        },
    ]
    .map(|strip| {
        let world = item.transform.bounds(strip);
        Rect {
            origin: world.origin - Vec2::new(4.0, 4.0),
            size: world.size + Vec2::new(8.0, 8.0),
        }
    })
}

#[test]
fn two_way_labels_have_distinct_opposite_arcs() {
    let mut scene = new_scene("two_way");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"pair","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"yes"}},
            {"edge":{"id":"e2","in":"diagram","from":"b","to":"a","label":"retry"}}
        ]}),
    );
    let frame = frame(&picture.after);
    let a = bounds(&picture.after, "a");
    let b = bounds(&picture.after, "b");
    for (name, label, offset, from, to) in [("e1", "yes", -32.0, a, b), ("e2", "retry", 32.0, b, a)]
    {
        let edge = item(&picture.after, &frame, name);
        let curve = route(edge);
        let start = from.edge_toward(to.center());
        let end = to.edge_toward(from.center());
        assert_directed_ports(edge, &curve, [start, end]);
        let middle = crossing(&curve, |point| point.x, start.x.midpoint(end.x));
        near(middle.y, start.y + offset);
        let text = ink(edge, label);
        assert_eq!(
            chip(edge),
            Rect::centered(middle, text.size + Vec2::new(8.0, 4.0))
        );
    }
}

#[test]
fn neighbouring_chips_keep_an_exact_six_pixel_world_gap() {
    let mut scene = new_scene("chip_gap");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"parents","put":[
            {"diagram":{"id":"d1","dir":"right","box":[0,0,360,300]}},
            {"diagram":{"id":"d2","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"d1","label":"A"}},
            {"node":{"id":"b","in":"d1","label":"B"}},
            {"node":{"id":"c","in":"d2","label":"C"}},
            {"node":{"id":"d","in":"d2","label":"D"}},
            {"edge":{"id":"e1","in":"d1","from":"a","to":"b","label":"x"}},
            {"edge":{"id":"e2","in":"d2","from":"c","to":"d","label":"y"}},
            {"anim":{"target":"d2","track":"scale","keys":[[0,2]]}},
            {"anim":{"target":"d2","track":"pos","keys":[[0,[-240,-36]]]}},
            {"anim":{"target":"b","track":"pos","keys":[[0,[120,0]]]}},
            {"anim":{"target":"d","track":"pos","keys":[[0,[120,0]]]}}
        ]}),
    );
    let frame = frame(&picture.after);
    let first = item(&picture.after, &frame, "e1");
    let second = item(&picture.after, &frame, "e2");
    assert_eq!(ink(first, "x").center(), chip(first).center());
    assert_eq!(ink(second, "y").center(), chip(second).center());
    let (first, second) = (chip(first), chip(second));
    assert!(first.origin.y < second.max().y && second.origin.y < first.max().y);
    assert_eq!(
        (first.origin.x - second.max().x).to_bits(),
        6.0_f32.to_bits()
    );
}

#[test]
fn chips_avoid_title_ink_and_all_four_frame_borders() {
    let mut scene = new_scene("title_clearance");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"title","put":[
            {"diagram":{"id":"diagram"}},
            {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA\nend\n"}},
            {"diagram":{"id":"d2","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"d2","label":"A"}},
            {"node":{"id":"b","in":"d2","label":"B"}},
            {"edge":{"id":"edge","in":"d2","from":"a","to":"b","label":"x"}},
            {"anim":{"target":"a","track":"pos","keys":[[0,[-100,0]]]}},
            {"anim":{"target":"b","track":"pos","keys":[[0,[100,0]]]}},
            {"anim":{"target":"d2","track":"pos","keys":[[0,[0,9]]]}}
        ]}),
    );
    let frame = frame(&picture.after);
    let group = item(&picture.after, &frame, "g");
    let edge = item(&picture.after, &frame, "edge");
    let title = ink(group, "Group");
    let strips = border_strips(group, bounds(&picture.after, "g"));
    let actual = chip(edge);
    let curve = route(edge);
    let [start, end] = ports(&curve);
    let unnudged = Rect::centered(
        crossing(&curve, |p| p.x, start.x.midpoint(end.x)),
        actual.size,
    );
    assert!(unnudged.intersects(title));
    assert!(strips.iter().all(|side| !unnudged.intersects(*side)));
    assert!(distance_to_route(actual.center(), &curve) < 0.01);
    assert_eq!(ink(edge, "x").center(), actual.center());
    assert!(!actual.intersects(title));
    assert!(strips.iter().all(|side| !actual.intersects(*side)));
}

#[test]
fn framed_group_participates_in_exact_floor_fit_boundaries() {
    for (width, scale, left, right, overflows) in [
        (160.0, 1.0, 32.0, 128.0, false),
        (144.0, 0.875, 30.0, 114.0, false),
        (112.0, 0.8, 22.4, 99.2, false),
        (80.0, 0.8, 28.8, 105.6, true),
    ] {
        let mut scene = new_scene("frame_floor");
        let before = LayoutState::new(&scene);
        let picture = prepared_on(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"frame","put":[
                {"diagram":{"id":"diagram"}},
                {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA\nend\n"}}
            ]}),
            SurfaceGeometry::Inline { width },
        );
        let frame = bounds(&picture.after, "g");
        near(frame.size.x, 96.0 * scale);
        near(frame.origin.x, left);
        near(frame.max().x, right);
        assert_eq!(frame.max().x > width, overflows);
    }
}

#[test]
fn fresh_and_cached_chips_agree_after_a_lower_id_add_and_label_update() {
    let graph = json!({"id":"first","put":[
        {"diagram":{"id":"diagram","box":[0,0,600,600]}},
        {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA --> B\nC --> D\nend\n"}},
        {"edge":{"id":"z","in":"g","from":"A","to":"B","label":"a wide label in the outer column"}}
    ]});
    let added = json!({"edge":{"id":"early","in":"g","from":"A","to":"B","label":"yes"}});
    let mut scene = new_scene("cached");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(&mut scene, &mut budget, &before, graph.clone());
    let next = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"add","put":[added]}),
    );
    for (cached, label) in [
        (next.after.clone(), "a wide label in the outer column"),
        (prepared(&mut scene, &mut budget, &next.after,
            json!({"id":"update","set":[{"target":"z","label":"a changed label in outer column"}]})).after,
            "a changed label in outer column"),
    ] {
        let mut fresh_scene = new_scene("fresh");
        let fresh_before = LayoutState::new(&fresh_scene);
        let mut value = graph.clone();
        value["put"][2]["edge"]["label"] = json!(label);
        value["put"].as_array_mut().expect("put").push(added.clone());
        let fresh = prepared(&mut fresh_scene, &mut GlobalBudget::default(), &fresh_before, value);
        let fresh_frame = frame(&fresh.after);
        let cached_frame = frame(&cached);
        for (name, label) in [("early", "yes"), ("z", label)] {
            let a = item(&cached, &cached_frame, name);
            let b = item(&fresh.after, &fresh_frame, name);
            assert_eq!(chip(a), chip(b), "{name} chip");
            assert_eq!(ink(a, label), ink(b, label), "{name} ink");
            assert_route_eq(&route(a), &route(b));
        }
        let a = item(&cached, &cached_frame, "g");
        let b = item(&fresh.after, &fresh_frame, "g");
        assert_eq!(
            border_strips(a, bounds(&cached, "g")),
            border_strips(b, bounds(&fresh.after, "g"))
        );
        assert_eq!(ink(a, "Group"), ink(b, "Group"));
    }
}

#[test]
fn three_parallel_links_alternate_lanes_in_both_flow_directions() {
    let x: fn(Vec2) -> f32 = |p| p.x;
    let y: fn(Vec2) -> f32 = |p| p.y;
    for (direction, along, across, first_lane) in [("right", x, y, 32.0), ("down", y, x, 52.0)] {
        let mut scene = new_scene("lanes");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"pair","put":[
                {"diagram":{"id":"diagram","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"diagram","label":"A"}},
                {"node":{"id":"b","in":"diagram","label":"B"}},
                {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"x"}},
                {"edge":{"id":"e2","in":"diagram","from":"b","to":"a","label":"y"}},
                {"edge":{"id":"e3","in":"diagram","from":"a","to":"b","label":"z"}}
            ]}),
        );
        let frame = frame(&picture.after);
        let [one, two, three] =
            ["e1", "e2", "e3"].map(|name| route(item(&picture.after, &frame, name)));
        let [start, end] = ports(&one);
        let midpoint = along(start).midpoint(along(end));
        for (curve, lane) in [(one, -1.0), (two, 1.0), (three, -2.0)] {
            near(
                across(crossing(&curve, along, midpoint)) - across(start),
                lane * first_lane,
            );
        }
    }
}

#[test]
fn a_labelled_self_loop_runs_outside_its_node() {
    for (direction, label) in [
        ("right", "retry"),
        ("down", "retry"),
        ("right", "a very long wrapped self loop label"),
    ] {
        let mut scene = new_scene("self_loop");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"loop","put":[
                {"diagram":{"id":"diagram","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"diagram","label":"A"}},
                {"edge":{"id":"loop","in":"diagram","from":"a","to":"a","label":label}}
            ]}),
        );
        let frame = frame(&picture.after);
        let edge = item(&picture.after, &frame, "loop");
        let node = bounds(&picture.after, "a");
        let curve = route(edge);
        assert_directed_ports(
            edge,
            &curve,
            [
                Vec2::new(node.max().x, node.center().y),
                Vec2::new(node.center().x, node.max().y),
            ],
        );
        let label_ink = ink(edge, label);
        let actual = chip(edge);
        assert_eq!(
            actual,
            Rect::centered(label_ink.center(), label_ink.size + Vec2::new(8.0, 4.0))
        );
        near(actual.origin.x - node.max().x, 12.0);
        let halfway = crossing(&curve, |p| p.x, actual.center().x);
        near(halfway.y, node.max().y + 27.0);
        near(actual.center().y, halfway.y);
    }
}

#[test]
fn labelled_self_loops_have_wire_ordered_lanes() {
    self_loop_lanes(2);
    self_loop_lanes(3);
}

fn self_loop_lanes(count: usize) {
    let mut scene = new_scene("loop_lanes");
    let before = LayoutState::new(&scene);
    let mut put = vec![
        json!({"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}}),
        json!({"node":{"id":"a","in":"diagram","label":"A"}}),
    ];
    for name in ["e3", "e2", "e1"].into_iter().skip(3 - count) {
        put.push(json!({"edge":{"id":name,"in":"diagram","from":"a","to":"a","label":"retrying"}}));
    }
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"lanes","put":put}),
    );
    let frame = frame(&picture.after);
    let node = bounds(&picture.after, "a");
    let mut chips = Vec::new();
    let mut previous_y = None;
    for name in ["e1", "e2", "e3"].into_iter().take(count) {
        let edge = item(&picture.after, &frame, name);
        let rect = chip(edge);
        let midpoint = crossing(&route(edge), |p| p.x, node.max().x + 48.0);
        assert_eq!(
            rect.center(),
            midpoint,
            "{name} chip rests at its curve midpoint"
        );
        if let Some(y) = previous_y {
            near(midpoint.y - y, 26.0);
        } else {
            near(midpoint.y - node.max().y, 27.0);
        }
        previous_y = Some(midpoint.y);
        assert_eq!(
            ink(edge, "retrying").center(),
            midpoint,
            "{name} label follows its lane"
        );
        assert!(
            rect.origin.x >= node.max().x + 12.0,
            "{name} clears its own node"
        );
        assert!(
            chips.iter().all(|other| !rect.intersects(*other)),
            "{name} avoids earlier chips"
        );
        chips.push(rect);
    }
}

#[test]
fn a_cut_self_loop_keeps_its_primitives_when_its_node_survives() {
    let mut scene = new_scene("cut_loop");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"loop","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[40,50,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"edge":{"id":"loop","in":"diagram","from":"a","to":"a","label":"retrying"}},
            {"anim":{"target":"diagram","track":"pos","keys":[[0,[40,50]]]}}
        ]}),
    );
    let node = scene.handle(&id("a")).expect("node");
    let handle = scene.handle(&id("loop")).expect("loop");
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(vec![node, handle]).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let cut = first.after.cut(&scene).expect("cut layout");
    let next = prepared(&mut scene, &mut budget, &cut, json!({"id":"noop"}));
    let rest = frame(&first.after);
    let old = item(&first.after, &rest, "loop");
    let curve = route(old);
    let node = bounds(&first.after, "a");
    assert_directed_ports(
        old,
        &curve,
        [
            Vec2::new(node.max().x, node.center().y),
            Vec2::new(node.center().x, node.max().y),
        ],
    );
    for layout in [&cut, &next.after] {
        assert_eq!(bounds(layout, "a"), node);
        let sampled = frame(layout);
        let edge = item(layout, &sampled, "loop");
        assert_route_eq(&curve, &route(edge));
        assert_eq!(chip(edge), chip(old));
        assert_eq!(ink(edge, "retrying"), ink(old, "retrying"));
    }
}

#[test]
fn tall_self_loop_lanes_saturate_before_the_coordinate_limit() {
    use kumochi_scene::display::{MeasureError, TextMeasure, TextStyle};
    struct Tall;
    impl TextMeasure for Tall {
        fn measure(
            &mut self,
            text: &str,
            _: TextStyle,
            _: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok(match text {
                "A" => [8.0, 10.0],
                _ => [64.0, 3000.0],
            })
        }
    }
    for (count, expected) in [(3, 6115.0), (4, 8521.0), (8, 8521.0)] {
        let mut scene = new_scene("tall_loops");
        let before = LayoutState::new(&scene);
        let mut put = vec![
            json!({"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}}),
            json!({"node":{"id":"a","in":"diagram","label":"A"}}),
        ];
        for index in 0..count {
            put.push(json!({"edge":{"id":format!("e{index}"),"in":"diagram","from":"a","to":"a","label":"retrying"}}));
        }
        let picture = scene_support::prepared_with(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"tall","put":put}),
            SurfaceGeometry::default(),
            &mut Tall,
        );
        let frame = frame(&picture.after);
        let edge = item(&picture.after, &frame, &format!("e{}", count - 1));
        let curve = route(edge);
        near(crossing(&curve, |p| p.x, 268.0).y, expected);
        assert!(curve.iter().all(|point| point.within_canvas()));
        assert!(chip(edge).within_canvas());
    }
}

#[test]
fn self_loop_lanes_stop_at_lane_eight() {
    let mut scene = new_scene("capped_lanes");
    let before = LayoutState::new(&scene);
    let mut put = vec![
        json!({"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}}),
        json!({"node":{"id":"a","in":"diagram","label":"A"}}),
    ];
    for index in 0..11 {
        put.push(json!({"edge":{"id":format!("e{index:02}"),"in":"diagram","from":"a","to":"a","label":"retrying"}}));
    }
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"cap","put":put}),
    );
    let sampled = frame(&picture.after);
    let line = |name| route(item(&picture.after, &sampled, name));
    let capped = line("e08");
    let midpoint = |curve: &[Vec2]| crossing(curve, |p| p.x, 268.0);
    near(midpoint(&capped).y, 291.0);
    near(midpoint(&capped).y - midpoint(&line("e07")).y, 26.0);
    for name in ["e09", "e10"] {
        assert_route_eq(&line(name), &capped);
    }
}

#[test]
fn removing_the_last_label_restores_the_fresh_unlabelled_curve() {
    let mut scene = new_scene("remove_label");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let value = json!({"id":"pair","put":[
        {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
        {"node":{"id":"a","in":"diagram","label":"A"}},
        {"node":{"id":"b","in":"diagram","label":"B"}},
        {"edge":{"id":"keep","in":"diagram","from":"a","to":"b"}},
        {"edge":{"id":"remove","in":"diagram","from":"b","to":"a","label":"retry"}}
    ]});
    let first = prepared(&mut scene, &mut budget, &before, value.clone());
    let next = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"remove","remove":["remove"]}),
    );
    let mut fresh_scene = new_scene("fresh_remove");
    let fresh_before = LayoutState::new(&fresh_scene);
    let mut fresh_value = value;
    fresh_value["put"].as_array_mut().expect("put").pop();
    let fresh = prepared(
        &mut fresh_scene,
        &mut GlobalBudget::default(),
        &fresh_before,
        fresh_value,
    );
    let old = route(item(&first.after, &frame(&first.after), "keep"));
    let [start, end] = ports(&old);
    let x = start.x.midpoint(end.x);
    assert!(crossing(&old, |p| p.x, x).y < start.y - 12.0);
    assert_eq!(next.after.handle(&id("remove")), None);
    let sampled = frame(&next.after);
    let edge = item(&next.after, &sampled, "keep");
    let curve = route(edge);
    let a = bounds(&next.after, "a");
    let b = bounds(&next.after, "b");
    assert_directed_ports(
        edge,
        &curve,
        [a.edge_toward(b.center()), b.edge_toward(a.center())],
    );
    assert!(curve.iter().all(|point| (point.y - start.y).abs() < 0.001));
    assert_route_eq(
        &curve,
        &route(item(&fresh.after, &frame(&fresh.after), "keep")),
    );
}

#[test]
fn each_frame_side_repels_a_chip_independently() {
    for (side, direction, position, node_offset) in [
        (0, "right", [0, -10], [100, 0]),
        (1, "right", [0, 84], [100, 0]),
        (2, "down", [-54, 0], [0, 100]),
        (3, "down", [54, 0], [0, 100]),
    ] {
        let mut scene = new_scene("frame_side");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"side","put":[
                {"diagram":{"id":"diagram"}},
                {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[G]\nA\nend\n"}},
                {"diagram":{"id":"other","dir":direction,"box":[0,0,360,300]}},
                {"node":{"id":"a","in":"other","label":"A"}},
                {"node":{"id":"b","in":"other","label":"B"}},
                {"edge":{"id":"edge","in":"other","from":"a","to":"b","label":"x"}},
                {"anim":{"target":"a","track":"pos","keys":[[0,[-node_offset[0],-node_offset[1]]]]}},
                {"anim":{"target":"b","track":"pos","keys":[[0,node_offset]]}},
                {"anim":{"target":"other","track":"pos","keys":[[0,position]]}}
            ]}),
        );
        let frame = frame(&picture.after);
        let edge = item(&picture.after, &frame, "edge");
        let curve = route(edge);
        let [start, end] = ports(&curve);
        let axis: fn(Vec2) -> f32 = if direction == "right" {
            |p| p.x
        } else {
            |p| p.y
        };
        let chip = chip(edge);
        let unnudged = Rect::centered(
            crossing(&curve, axis, axis(start).midpoint(axis(end))),
            chip.size,
        );
        let strips = border_strips(
            item(&picture.after, &frame, "g"),
            bounds(&picture.after, "g"),
        );
        let title = ink(item(&picture.after, &frame, "g"), "G");
        let title = Rect {
            origin: title.origin - Vec2::new(4.0, 4.0),
            size: title.size + Vec2::new(8.0, 8.0),
        };
        assert!(
            !unnudged.intersects(title),
            "the title cannot mask side {side}"
        );
        assert!(
            unnudged.intersects(strips[side]),
            "unmoved chip hits side {side}"
        );
        assert!(
            strips
                .iter()
                .enumerate()
                .all(|(i, strip)| i == side || !unnudged.intersects(*strip))
        );
        assert_eq!(ink(edge, "x").center(), chip.center());
        assert!(distance_to_route(chip.center(), &curve) < 0.01);
        assert!(!chip.intersects(strips[side]), "chip clears side {side}");
    }
}
