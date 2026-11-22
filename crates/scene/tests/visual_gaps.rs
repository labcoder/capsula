//! Literal geometry for alignment, initial fitting and incremental diagram views.
use kumochi_scene::{
    GlobalBudget, StopCut,
    display::{Item, MeasureError, Prim, TextMeasure, TextStyle},
    geom::{Affine, Rect, Vec2},
    layout::{LayoutState, SurfaceGeometry},
    record::Align,
    text::Bounded,
};
use serde_json::json;

#[path = "support/scene.rs"]
mod scene_support;
use scene_support::{id, new_scene, prepared, prepared_on};

#[test]
fn a_titled_group_frames_its_title_band_and_member_with_eight_pixel_padding() {
    let mut scene = new_scene("group_frame");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"frame","put":[
            {"diagram":{"id":"diagram"}},
            {"mermaid":{"in":"diagram","src":"flowchart TB\nsubgraph g[Group]\nA\nend\n"}}
        ]}),
    );
    let world = &picture.after;
    let group = item(world, "g");
    let (frame, radius, paint, width) = group
        .primitives
        .iter()
        .find_map(|p| match p {
            Prim::Rect {
                rect,
                radius,
                fill: None,
                border: Some((paint, width)),
            } => Some((*rect, *radius, *paint, *width)),
            _ => None,
        })
        .expect("unfilled group outline");
    assert!(radius > 0.0);
    assert_eq!(
        (paint, width),
        (kumochi_scene::theme::Theme::default().line.into(), 1.0)
    );
    let title = ink(&group, "Group", &mut scene_support::Measure);
    let frame_world = bounds(world, "g");
    let title_world = title.origin + frame_world.origin - frame.origin;
    let member = bounds(world, "A");
    near(title_world.y - frame_world.origin.y, 8.0);
    near(title_world.x + title.size.x * 0.5, frame_world.center().x);
    near(member.origin.x - frame_world.origin.x, 8.0);
    near(frame_world.max().x - member.max().x, 8.0);
    near(frame_world.max().y - member.max().y, 8.0);
    near(member.origin.y - (title_world.y + title.size.y), 16.0);
    assert_eq!(group.bounds, frame);
}

fn labelled_pair(labels: [&str; 2]) -> serde_json::Value {
    json!({"id":"labels","put":[
        {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
        {"node":{"id":"a","in":"diagram","label":"A"}},
        {"node":{"id":"b","in":"diagram","label":"B"}},
        {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":labels[0]}},
        {"edge":{"id":"e2","in":"diagram","from":"a","to":"b","label":labels[1]}}
    ]})
}

fn item(layout: &LayoutState, name: &str) -> Item {
    let handle = layout.handle(&id(name)).expect("prepared handle");
    layout
        .items()
        .into_iter()
        .find(|item| item.handle == handle)
        .expect("prepared item")
}

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("valid world bounds")
        .expect("prepared bounds")
}

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.001, "{actual} vs {expected}");
}

fn chip(item: &Item) -> Rect {
    item.primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Rect {
                rect,
                radius,
                fill: Some(fill),
                border: None,
            } if *radius > 0.0
                && *fill == kumochi_scene::theme::Theme::default().surface.into() =>
            {
                Some(*rect)
            }
            _ => None,
        })
        .expect("label chip")
}

fn ink(item: &Item, label: &str, measure: &mut impl TextMeasure) -> Rect {
    item.primitives
        .iter()
        .find_map(|p| match p {
            Prim::Text {
                origin,
                text,
                style,
                max_width,
                align,
                color,
            } if text.as_str() == label && color.opacity > 0.0 => {
                let [w, h] = measure
                    .measure(label, *style, *max_width)
                    .expect("measured ink");
                let spare = max_width.unwrap_or(w) - w;
                let x = match align {
                    Align::Left => 0.0,
                    Align::Center => spare * 0.5,
                    Align::Right => spare,
                };
                Some(Rect {
                    origin: *origin + Vec2::new(x, 0.0),
                    size: Vec2::new(w, h),
                })
            }
            _ => None,
        })
        .expect("painted label")
}

fn stroke(item: &Item) -> &[Vec2] {
    item.primitives
        .iter()
        .find_map(|p| match p {
            Prim::Line {
                points,
                closed: false,
                width,
                color,
                ..
            } if *width > 0.0 && color.opacity > 0.0 => Some(points.as_ref()),
            _ => None,
        })
        .expect("painted route")
}

fn crossing(points: &[Vec2], axis: fn(Vec2) -> f32, coordinate: f32) -> Vec2 {
    points
        .windows(2)
        .rev()
        .find_map(|pair| {
            let [a, b] = [pair[0], pair[1]];
            let t = (coordinate - axis(a)) / (axis(b) - axis(a));
            (0.0..=1.0).contains(&t).then(|| a + (b - a) * t)
        })
        .expect("route crosses the coordinate")
}

fn crossing_y(points: &[Vec2], x: f32) -> Vec2 {
    crossing(points, |p| p.x, x)
}

fn midpoint(points: &[Vec2]) -> Vec2 {
    crossing_y(
        points,
        points
            .first()
            .expect("start")
            .x
            .midpoint(points.last().expect("end").x),
    )
}

fn on_route(point: Vec2, route: &[Vec2]) -> bool {
    let actual = crossing_y(route, point.x);
    (actual.y - point.y).abs() < 0.01
}

#[test]
fn an_edge_label_chip_has_four_by_two_pixel_padding() {
    let mut scene = new_scene("label_chip");
    let before = LayoutState::new(&scene);
    let mut value = labelled_pair(["abc", "x"]);
    value["put"].as_array_mut().expect("put").pop();
    let picture = prepared(&mut scene, &mut GlobalBudget::default(), &before, value);
    let edge = item(&picture.after, "e1");
    let text = ink(&edge, "abc", &mut scene_support::Measure);
    let chip = chip(&edge);
    assert_eq!(text.origin - chip.origin, Vec2::new(4.0, 2.0));
    assert_eq!(chip.max() - text.max(), Vec2::new(4.0, 2.0));
    assert!(on_route(chip.center(), stroke(&edge)));
    assert_eq!(chip.center(), midpoint(stroke(&edge)));
}

#[test]
fn colliding_labels_nudge_along_the_curve_and_recenter_after_removal() {
    let mut scene = new_scene("label_nudge");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut value = labelled_pair(["x", "y"]);
    value["put"][4]["edge"]["in"] = json!("other");
    value["put"][4]["edge"]["from"] = json!("c");
    value["put"][4]["edge"]["to"] = json!("d");
    value["put"].as_array_mut().expect("put").extend([
        json!({"diagram":{"id":"other","dir":"right","box":[0,0,360,300]}}),
        json!({"node":{"id":"c","in":"other","label":"C"}}),
        json!({"node":{"id":"d","in":"other","label":"D"}}),
    ]);
    let picture = prepared(&mut scene, &mut budget, &before, value);
    let first = item(&picture.after, "e1");
    let second = item(&picture.after, "e2");
    let (still, nudged) = (chip(&first), chip(&second));
    assert_eq!(still.center(), midpoint(stroke(&first)));
    assert!(still.origin.x - nudged.max().x >= 4.0 || nudged.origin.x - still.max().x >= 4.0);
    near(nudged.center().y, still.center().y);
    assert!(on_route(nudged.center(), stroke(&second)));
    assert_eq!(
        ink(&second, "y", &mut scene_support::Measure).center(),
        nudged.center()
    );
    let updated = prepared(
        &mut scene,
        &mut budget,
        &picture.after,
        json!({"id":"color","set":[{"target":"e2","color":"red"}]}),
    );
    assert_eq!(chip(&item(&updated.after, "e2")), nudged);
    let removed = prepared(
        &mut scene,
        &mut budget,
        &updated.after,
        json!({"id":"remove","remove":["e1"]}),
    );
    let edge = item(&removed.after, "e2");
    assert_eq!(chip(&edge).center(), midpoint(stroke(&edge)));
}

#[test]
fn labels_with_no_free_interior_sample_stay_present_at_the_midpoint() {
    let mut scene = new_scene("full_edge");
    let before = LayoutState::new(&scene);
    let mut value = labelled_pair(["abcdefghijklmnopqrstuvwx", "abcdefghijklmnopqrstuvwx"]);
    value["put"][2]["node"]["label"] = json!("abcdefghijklmnopqrstuvwx");
    for index in [3, 4] {
        value["put"][index]["edge"]["to"] = json!("a");
    }
    value["put"][4]["edge"]["in"] = json!("overlap");
    value["put"][4]["edge"]["from"] = json!("c");
    value["put"][4]["edge"]["to"] = json!("c");
    value["put"].as_array_mut().expect("put").extend([
        json!({"anim":{"target":"b","track":"pos","keys":[[0,[0,-30]]]}}),
        json!({"diagram":{"id":"overlap","dir":"right","box":[0,0,360,300]}}),
        json!({"node":{"id":"c","in":"overlap","label":"A"}}),
    ]);
    let picture = prepared(&mut scene, &mut GlobalBudget::default(), &before, value);
    let chips = [("e1", "a"), ("e2", "c")].map(|(name, node)| {
        let edge = item(&picture.after, name);
        let chip = chip(&edge);
        assert_eq!(
            ink(
                &edge,
                "abcdefghijklmnopqrstuvwx",
                &mut scene_support::Measure
            )
            .center(),
            chip.center()
        );
        let curve = stroke(&edge);
        let node = bounds(&picture.after, node);
        let x = node.max().x + 12.0 + chip.size.x * 0.5;
        assert_eq!(chip.center(), crossing_y(curve, x));
        near(chip.center().y, node.max().y + 27.0);
        chip
    });
    assert!(chips[0].intersects(chips[1]));
}

#[test]
fn edge_label_chips_paint_after_every_edge_stroke() {
    use kumochi_scene::motion::{Frame, SceneView};
    let mut scene = new_scene("chip_layers");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        labelled_pair(["x", "y"]),
    );
    let mut view = SceneView::new(&before, scene.activated());
    let mut frame = Frame::new();
    view.activate(picture, 0, 800, &mut frame)
        .expect("activate");
    view.frame(u64::MAX, &mut frame).expect("settled frame");
    let handles = ["e1", "e2"].map(|name| scene.handle(&id(name)).expect("edge"));
    let paint: Vec<_> = frame
        .paint_items()
        .filter(|(item, _, _)| handles.contains(&item.handle) && item.opacity > 0.0)
        .flat_map(|(item, _, prims)| {
            prims.iter().filter_map(move |p| {
                let visible = match p {
                    Prim::Rect {
                        fill: Some(paint), ..
                    } => paint.opacity > 0.0,
                    Prim::Line { width, color, .. } => *width > 0.0 && color.opacity > 0.0,
                    Prim::Fill { color, .. } | Prim::Text { color, .. } => color.opacity > 0.0,
                    Prim::Rect { fill: None, .. } => false,
                };
                visible.then_some((item.handle, p))
            })
        })
        .collect();
    let first_label = paint
        .iter()
        .position(|(_, p)| matches!(p, Prim::Rect { .. } | Prim::Text { .. }))
        .expect("first chip or label");
    for (handle, label) in handles.into_iter().zip(["x", "y"]) {
        for is_stroke in [true, false] {
            let role = |(owner, p): &(kumochi_scene::Handle, &Prim)| {
                *owner == handle
                    && if is_stroke {
                        matches!(p, Prim::Line { .. })
                    } else {
                        matches!(p, Prim::Fill { .. })
                    }
            };
            assert!(paint[..first_label].iter().any(role));
            assert!(!paint[first_label..].iter().any(role));
        }
        assert!(paint[first_label..].iter().any(|(owner, p)| *owner == handle && matches!(p, Prim::Rect { fill: Some(_), .. })));
        assert!(
            paint[first_label..]
                .iter()
                .any(|(owner, p)| *owner == handle
                    && matches!(p, Prim::Text { text, .. } if text.as_str() == label))
        );
    }
}

#[test]
fn a_chip_follows_the_bow_over_a_skipped_node() {
    let mut scene = new_scene("chip_node");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"skip","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,600,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"node":{"id":"c","in":"diagram","label":"C"}},
            {"edge":{"id":"ab","in":"diagram","from":"a","to":"b"}},
            {"edge":{"id":"bc","in":"diagram","from":"b","to":"c"}},
            {"edge":{"id":"skip","in":"diagram","from":"a","to":"c","label":"x"}}
        ]}),
    );
    let edge = item(&picture.after, "skip");
    let node = bounds(&picture.after, "b");
    let curve = stroke(&edge);
    let middle = midpoint(curve);
    let chip = chip(&edge);
    assert_eq!(middle, chip.center());
    assert_eq!(
        ink(&edge, "x", &mut scene_support::Measure).center(),
        middle
    );
    assert!(chip.max().y < node.origin.y);
    for x in [node.origin.x, node.center().x, node.max().x] {
        assert!(crossing_y(curve, x).y < node.origin.y);
    }
}

#[test]
fn labelled_skipped_ranks_leave_room_for_separate_chips() {
    let mut scene = new_scene("skip_labels");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"skip","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,600,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"node":{"id":"c","in":"diagram","label":"C"}},
            {"edge":{"id":"e1","in":"diagram","from":"a","to":"b","label":"x"}},
            {"edge":{"id":"e2","in":"diagram","from":"b","to":"c","label":"x"}},
            {"edge":{"id":"e3","in":"diagram","from":"a","to":"c","label":"shared"}}
        ]}),
    );
    let [a, b, c] = ["a", "b", "c"].map(|name| bounds(&picture.after, name));
    assert!(b.origin.x - a.max().x >= 72.0);
    assert!(c.origin.x - b.max().x >= 72.0);
    let mut chips = Vec::new();
    for (name, label) in [("e1", "x"), ("e2", "x"), ("e3", "shared")] {
        let edge = item(&picture.after, name);
        let chip = chip(&edge);
        assert_eq!(
            ink(&edge, label, &mut scene_support::Measure).center(),
            chip.center()
        );
        assert!(!chip.intersects(b));
        assert!(chips.iter().all(|other| !chip.intersects(*other)));
        chips.push(chip);
    }
}

fn rightward_chain() -> serde_json::Value {
    json!({"id":"chain","put":[
        {"diagram":{"id":"diagram","dir":"right"}},
        {"node":{"id":"a","in":"diagram","label":"abcdefghij"}},
        {"node":{"id":"b","in":"diagram","label":"B"}},
        {"node":{"id":"c","in":"diagram","label":"C"}},
        {"node":{"id":"last","in":"diagram","label":"D"}},
        {"edge":{"id":"ab","in":"diagram","from":"a","to":"b"}},
        {"edge":{"id":"bc","in":"diagram","from":"b","to":"c"}},
        {"edge":{"id":"cd","in":"diagram","from":"c","to":"last"}}
    ]})
}

#[test]
fn initial_fit_has_exact_unscaled_floor_and_clipped_boundaries() {
    for (width, scale, x, y) in [
        (592.0, 1.0, 0.0, 0.0),
        (591.0, 0.998_214_3, 0.527_679_44, 0.028_571_129),
        (522.0, 0.875, 32.625, 2.0),
        (480.0, 0.8, 48.0, 3.199_999_8),
        (479.0, 0.8, 48.4, 3.199_999_8),
        (300.0, 0.8, 120.0, 3.199_999_8),
    ] {
        let mut scene = new_scene("fit_boundary");
        let before = LayoutState::new(&scene);
        let picture = prepared_on(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            rightward_chain(),
            SurfaceGeometry::Inline { width },
        );
        let handle = scene.handle(&id("diagram")).expect("diagram");
        let items = picture.after.items();
        let diagram = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("diagram item");
        assert_eq!(diagram.transform, Affine([scale, 0.0, 0.0, scale, x, y]));
        assert_eq!(picture.glides.len(), 0);
        assert_eq!(scene.object_count(), 8);
    }
}

#[test]
fn a_long_horizontal_edge_label_widens_the_second_rank_origin() {
    let mut scene = new_scene("label_gap");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"ranks","put":[
            {"diagram":{"id":"diagram","dir":"right","box":[0,0,360,300]}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"edge":{"id":"edge","in":"diagram","from":"a","to":"b","label":"abcdefgh"}}
        ]}),
    );
    let [a, b] = ["a", "b"].map(|name| bounds(&picture.after, name));
    let label = ink(
        &item(&picture.after, "edge"),
        "abcdefgh",
        &mut scene_support::Measure,
    );
    near(b.origin.x - a.max().x, (label.size.x + 24.0).max(72.0));
    near(b.center().y, a.center().y);
}

#[test]
fn a_long_diamond_label_enlarges_both_square_dimensions() {
    let mut scene = new_scene("diamond_size");
    let before = LayoutState::new(&scene);
    let mut fixture = json!({"id":"diamond","put":[
        {"diagram":{"id":"diagram","box":[0,0,360,300]}},
        {"node":{"id":"diamond","in":"diagram","label":"abcdefghij","shape":"diamond"}}
    ]});
    let long = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        fixture.clone(),
    );
    let mut scene = new_scene("short_diamond");
    let before = LayoutState::new(&scene);
    fixture["put"][1]["node"]["label"] = json!("A");
    let short = prepared(&mut scene, &mut GlobalBudget::default(), &before, fixture);
    let long_node = item(&long.after, "diamond");
    let short_node = item(&short.after, "diamond");
    assert!(long_node.bounds.size.x > short_node.bounds.size.x);
    assert!(long_node.bounds.size.y > short_node.bounds.size.y);
    near(long_node.bounds.size.x, long_node.bounds.size.y);
    let label = ink(&long_node, "abcdefghij", &mut scene_support::Measure);
    let center = long_node.bounds.center();
    for x in [label.origin.x - 12.0, label.max().x + 12.0] {
        for y in [label.origin.y - 10.0, label.max().y + 10.0] {
            assert!((x - center.x).abs() + (y - center.y).abs() <= long_node.bounds.size.x * 0.5);
        }
    }
}

#[test]
fn a_fitted_diagram_keeps_its_fit_and_old_local_nodes_when_it_grows() {
    let mut scene = new_scene("fit_growth");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let surface = SurfaceGeometry::Inline { width: 522.0 };
    let first = prepared_on(&mut scene, &mut budget, &before, rightward_chain(), surface);
    let a = item(&first.after, "a");
    near(bounds(&first.after, "a").origin.x, 16.0);
    near(bounds(&first.after, "last").max().x, 522.0 - 16.0);
    let fit = item(&first.after, "diagram").transform;
    assert_eq!(
        fit.point(Vec2::new(1.0, 0.0)) - fit.point(Vec2::default()),
        Vec2::new(0.875, 0.0)
    );
    let next = prepared_on(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"more","put":[
            {"node":{"id":"e","in":"diagram","label":"E"}},
            {"edge":{"id":"de","in":"diagram","from":"last","to":"e"}}
        ]}),
        surface,
    );
    assert_eq!(item(&next.after, "diagram").transform, fit);
    assert_eq!(item(&next.after, "a").bounds, a.bounds);
}

#[test]
fn an_empty_kept_diagram_restarts_centered_batch_placement() {
    let mut scene = new_scene("cut_reset");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"diagram"}},
            {"node":{"id":"a","in":"diagram","label":"A"}}
        ]}),
    );
    let root = scene.handle(&id("diagram")).expect("diagram");
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(vec![root]).expect("cut"),
            },
            &mut budget,
        )
        .expect("stop");
    let cut = first.after.cut(&scene).expect("kept empty diagram");
    assert_eq!(scene.handle(&id("a")), None);
    let next = prepared(
        &mut scene,
        &mut budget,
        &cut,
        json!({"id":"again","put":[{"node":{"id":"b","in":"diagram","label":"B"}}]}),
    );
    assert_eq!(scene.handle(&id("diagram")), Some(root));
    assert_eq!(
        next.after.bounds(&id("b")).expect("new batch"),
        Some(Rect {
            origin: Vec2::new(140.0, 16.0),
            size: Vec2::new(80.0, 40.0)
        })
    );
}

#[test]
fn updating_every_label_does_not_start_another_initial_fit() {
    let mut scene = new_scene("fit_update");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let surface = SurfaceGeometry::Inline { width: 522.0 };
    let first = prepared_on(&mut scene, &mut budget, &before, rightward_chain(), surface);
    let next = prepared_on(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"labels","set":[
            {"target":"a","label":"A"},
            {"target":"b","label":"B"},
            {"target":"c","label":"C"},
            {"target":"last","label":"D"}
        ]}),
        surface,
    );
    assert_eq!(
        item(&next.after, "diagram").transform,
        item(&first.after, "diagram").transform
    );
    let a = item(&next.after, "a");
    assert_eq!(
        ink(&a, "A", &mut scene_support::Measure).center(),
        a.bounds.center()
    );
    assert!(next.glides.is_empty());
}

#[test]
fn a_new_slot_does_not_fill_a_hole_outside_the_already_glided_view() {
    let mut scene = new_scene("glided_slot");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"diagram","box":[0,0,360,300],"dir":"down"}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"node":{"id":"c","in":"diagram","label":"C"}}
        ]}),
    );
    let overflow = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"overflow","put":[{"node":{"id":"d","in":"diagram","label":"D"}}]}),
    );
    let root = overflow.after.handle(&id("diagram")).expect("group");
    let targets = |step: &kumochi_scene::layout::PreparedStep| {
        step.glides.iter().map(|g| g.target).collect::<Vec<_>>()
    };
    assert_eq!(targets(&overflow), [root]);
    let hole = bounds(&overflow.after, "a");
    assert!(hole.origin.x < 0.0);
    let old_slot = item(&overflow.after, "a").bounds;
    let next = prepared(
        &mut scene,
        &mut budget,
        &overflow.after,
        json!({"id":"next","remove":["a"],"put":[{"node":{"id":"e","in":"diagram","label":"E"}}]}),
    );
    assert_eq!(targets(&next), [root]);
    assert_ne!(item(&next.after, "e").bounds, old_slot);
    let e = bounds(&next.after, "e");
    assert!(e.origin.x >= 0.0 && e.max().x <= 360.0);
    assert!(e.origin.y >= 0.0 && e.max().y <= 300.0);
}

#[test]
fn a_small_model_scale_does_not_expand_a_new_slot_beyond_the_canvas() {
    for parent in [None, Some("outer")] {
        let mut scene = new_scene("scaled_slot");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let mut value = json!({"id":"first","put":[
            {"diagram":{"id":"diagram","in":parent,"dir":"right"}},
            {"node":{"id":"a","in":"diagram","label":"A"}},
            {"node":{"id":"b","in":"diagram","label":"B"}},
            {"edge":{"id":"ab","in":"diagram","from":"a","to":"b"}},
            {"anim":{"target":parent.unwrap_or("diagram"),"track":"scale","keys":[[0,0.005]]}}
        ]});
        if let Some(parent) = parent {
            value["put"]
                .as_array_mut()
                .expect("operations")
                .push(json!({"diagram":{"id":parent}}));
        }
        let first = prepared(&mut scene, &mut budget, &before, value);
        let root = scene.handle(&id("diagram")).expect("diagram");
        let retained = scene.handle(&id("b")).expect("retained node");
        scene
            .stop(
                &StopCut {
                    activated: 1,
                    revealed: Bounded::new(vec![root, retained]).expect("cut"),
                },
                &mut budget,
            )
            .expect("stop");
        let cut = first.after.cut(&scene).expect("retained higher rank");
        let next = prepared(
            &mut scene,
            &mut budget,
            &cut,
            json!({"id":"again","put":[
                {"node":{"id":"c","in":"diagram","label":"C"}}
            ]}),
        );
        let handle = scene.handle(&id("c")).expect("new node");
        let items = next.after.items();
        assert_eq!(
            items
                .iter()
                .find(|item| item.handle == handle)
                .expect("new item")
                .bounds,
            Rect {
                origin: Vec2::new(140.0, 16.0),
                size: Vec2::new(80.0, 40.0)
            }
        );
        assert_eq!(next.glides.len(), 0);
        assert_eq!(scene.object_count(), 3);
    }
}

#[test]
fn an_empty_detached_diagram_keeps_its_absorbed_parent_transform() {
    let cut_and_append = |position| {
        let mut scene = new_scene("detached_fit");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = prepared(
            &mut scene,
            &mut budget,
            &before,
            json!({"id":"first","put":[
                {"diagram":{"id":"root","box":[0,0,360,300]}},
                {"diagram":{"id":"child","in":"root"}},
                {"anim":{"target":"root","track":"pos","keys":[[0,position]]}}
            ]}),
        );
        let anchor = bounds(&first.after, "child").origin;
        let child = scene.handle(&id("child")).expect("child diagram");
        scene
            .stop(
                &StopCut {
                    activated: 1,
                    revealed: Bounded::new(vec![child]).expect("cut"),
                },
                &mut budget,
            )
            .expect("stop");
        let cut = first.after.cut(&scene).expect("detached empty diagram");
        assert_eq!(bounds(&cut, "child").origin, anchor);
        let next = prepared(
            &mut scene,
            &mut budget,
            &cut,
            json!({"id":"again","put":[
                {"node":{"id":"node","in":"child","label":"A"}}
            ]}),
        );
        assert_eq!(scene.object_count(), 2);
        bounds(&next.after, "node")
    };
    let moved = cut_and_append([40, 20]);
    let control = cut_and_append([0, 0]);
    assert_eq!(moved.origin - control.origin, Vec2::new(40.0, 20.0));
    assert_eq!(moved.size, control.size);
}

#[test]
fn wrapped_text_aligns_its_ink_at_the_anchor_without_shifting_its_bounds() {
    struct Wrapped;
    impl TextMeasure for Wrapped {
        fn measure(
            &mut self,
            text: &str,
            _: TextStyle,
            width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            if text != "aaaa bbbb" {
                return Err(MeasureError);
            }
            Ok(if width.is_some_and(|w| w < 72.0) {
                [32.0, 20.0]
            } else {
                [72.0, 10.0]
            })
        }
    }

    for (name, align, width, box_x, ink_x) in [
        ("left", Align::Left, Some(80_u16), 100.0, 100.0),
        ("center", Align::Center, Some(80), 60.0, 88.0),
        ("right", Align::Right, Some(80), 20.0, 76.0),
        ("left", Align::Left, None, 100.0, 100.0),
        ("center", Align::Center, None, 88.0, 88.0),
        ("right", Align::Right, None, 76.0, 76.0),
    ] {
        let mut scene = new_scene("text_alignment");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"text","put":[{"text":{
                "id":"label","at":[100,100],"text":"abc","align":name,"max_width":width
            }}]}),
        );
        assert_eq!(
            picture.after.bounds(&id("label")).expect("bounds"),
            Some(Rect {
                origin: Vec2::new(ink_x, 95.0),
                size: Vec2::new(24.0, 10.0)
            })
        );
        let items = picture.after.items();
        assert_eq!(items.len(), 1);
        let [
            Prim::Text {
                origin,
                max_width,
                align: actual,
                ..
            },
        ] = &*items[0].primitives
        else {
            panic!("one text primitive");
        };
        assert_eq!(*origin, Vec2::new(box_x, 95.0));
        assert_eq!(*max_width, width.map(f32::from));
        assert_eq!(*actual, align);
    }

    let mut scene = new_scene("wrapped_alignment");
    let before = LayoutState::new(&scene);
    let picture = scene_support::prepared_with(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"wrapped","put":[{"text":{
            "id":"label","at":[100,100],"text":"aaaa bbbb","align":"right","max_width":60
        }}]}),
        SurfaceGeometry::default(),
        &mut Wrapped,
    );
    let expected = Rect {
        origin: Vec2::new(68.0, 90.0),
        size: Vec2::new(32.0, 20.0),
    };
    assert_eq!(bounds(&picture.after, "label"), expected);
    assert_eq!(
        ink(&item(&picture.after, "label"), "aaaa bbbb", &mut Wrapped),
        expected
    );
}

#[test]
fn a_centered_shape_label_uses_the_shape_width_as_its_alignment_box() {
    let mut scene = new_scene("shape_alignment");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"shape","put":[{"shape":{
            "id":"box","kind":"rect","at":[100,100],"size":[80,40],"label":"abc"
        }}]}),
    );
    assert_eq!(
        picture.after.bounds(&id("box")).expect("bounds"),
        Some(Rect {
            origin: Vec2::new(60.0, 80.0),
            size: Vec2::new(80.0, 40.0)
        })
    );
    let items = picture.after.items();
    let texts: Vec<_> = items[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Text {
                origin,
                max_width,
                align,
                ..
            } => Some((*origin, *max_width, *align)),
            _ => None,
        })
        .collect();
    assert_eq!(texts, [(Vec2::new(60.0, 95.0), Some(80.0), Align::Center)]);
}

#[test]
fn diagonal_outline_ports_and_coincident_centers_survive_edge_updates() {
    for (shape, pose, start, middle, end) in [
        (
            "diamond",
            [80, -52],
            [200.0, 76.0],
            [210.0, 96.0],
            [240.0, 116.0],
        ),
        (
            "circle",
            [60, -52],
            [204.0, 88.0],
            [209.25, 102.0],
            [225.0, 116.0],
        ),
        (
            "diamond",
            [0, -132],
            [180.0, 56.0],
            [180.0, 56.0],
            [180.0, 56.0],
        ),
        (
            "circle",
            [0, -132],
            [180.0, 56.0],
            [180.0, 56.0],
            [180.0, 56.0],
        ),
    ] {
        let mut scene = new_scene("outline_ports");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = prepared(
            &mut scene,
            &mut budget,
            &before,
            json!({"id":"first","put":[
                {"diagram":{"id":"diagram","dir":"down","box":[0,0,360,300]}},
                {"node":{"id":"a","in":"diagram","label":"A","shape":shape}},
                {"node":{"id":"b","in":"diagram","label":"B"}},
                {"edge":{"id":"edge","in":"diagram","from":"a","to":"b"}},
                {"anim":{"target":"b","track":"pos","keys":[[0,pose]]}}
            ]}),
        );
        let changed = prepared(
            &mut scene,
            &mut budget,
            &first.after,
            json!({"id":"color","set":[{"target":"edge","color":"red"}]}),
        );
        for layout in [&first.after, &changed.after] {
            let edge = item(layout, "edge");
            let points = stroke(&edge);
            assert_eq!(points.first(), Some(&Vec2::new(start[0], start[1])));
            assert_eq!(points.last(), Some(&Vec2::new(end[0], end[1])));
            if pose == [0, -132] {
                assert!(
                    points
                        .iter()
                        .all(|point| *point == Vec2::new(start[0], start[1]))
                );
            } else {
                let actual = crossing(points, |p| p.y, start[1].midpoint(end[1]));
                assert!((actual.x - middle[0]).abs() < 0.5 && (actual.y - middle[1]).abs() < 0.5);
            }
        }
    }
}
