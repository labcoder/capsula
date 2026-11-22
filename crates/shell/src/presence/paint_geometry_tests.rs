use std::sync::Arc;

use gpui_kit::{Path, Pixels};
use kumochi_scene::{
    display::{Paint, Prim},
    geom::{Affine, Rect, Vec2},
    limits::MAX_COMPOSED_SCALE,
    record::Rgb,
};

use super::super::{
    paint_paths,
    paint_phase::{self, Phase},
};
use super::{Scratch, paths};

fn ink() -> Paint {
    Paint::from(Rgb::try_from(0x33_66_99).expect("RGB"))
}

type Segment = ([f32; 2], [f32; 2]);

fn geometry(path: &Path<Pixels>) -> (Vec<Segment>, [f32; 4]) {
    let vertices = path
        .vertices
        .iter()
        .map(|v| {
            (
                [v.xy_position.x.into(), v.xy_position.y.into()],
                [v.st_position.x, v.st_position.y],
            )
        })
        .collect();
    (
        vertices,
        [
            path.bounds.origin.x.into(),
            path.bounds.origin.y.into(),
            path.bounds.size.width.into(),
            path.bounds.size.height.into(),
        ],
    )
}

fn parity(scratch: &mut Scratch, prim: &Prim, transform: Affine, draw: f32, flow: f32) {
    let old = paint_paths::paths(prim, transform, draw, flow, MAX_COMPOSED_SCALE).map(|paths| {
        paths
            .into_iter()
            .map(|(path, paint)| (geometry(&path), paint))
            .collect::<Vec<_>>()
    });
    let new = paths(scratch, prim, transform, draw, flow, MAX_COMPOSED_SCALE).map(|paths| {
        paths
            .into_iter()
            .flatten()
            .map(|(path, paint)| (geometry(&path), paint))
            .collect::<Vec<_>>()
    });
    assert_eq!(new, old, "draw={draw} flow={flow} transform={transform:?}");
    let ((), events) = crate::allocation::requests(8_192, || {
        let _app = paint_phase::enter(Phase::App);
        drop(paths(
            scratch,
            prim,
            transform,
            draw,
            flow,
            MAX_COMPOSED_SCALE,
        ));
    });
    paint_phase::tests::assert_paths(&events, 1);
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the scene line's closed and dashed flags, as each case sets them"
)]
fn line(count: usize, closed: bool, dashed: bool) -> Prim {
    let points = (0..count)
        .map(|n| {
            let n = u16::try_from(n).expect("bounded test points");
            Vec2::new(f32::from(n), f32::from(n % 2))
        })
        .collect();
    Prim::Line {
        points,
        closed,
        dashed,
        color: ink(),
        width: 2.0,
    }
}

#[test]
fn streamed_strokes_match_the_reference_across_runs_chunks_and_guards() {
    let mut scratch = Scratch::new();
    let mut cases = vec![
        line(0, false, false),
        line(1, true, false),
        line(5_460, false, false),
        line(5_461, false, false),
    ];
    for closed in [false, true] {
        for dashed in [false, true] {
            cases.push(line(12, closed, dashed));
        }
    }
    cases.push(Prim::Line {
        points: Arc::from([Vec2::new(0.0, 0.0), Vec2::new(32_768.0, 0.0)]),
        closed: false,
        dashed: true,
        color: ink(),
        width: 2.0,
    });
    cases.push(Prim::Line {
        points: Arc::from([
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(20.0, 0.0),
            Vec2::new(20.0, 20.0),
        ]),
        closed: true,
        dashed: true,
        color: ink(),
        width: 2.0,
    });
    for prim in cases {
        for draw in [0.0, 0.001, 0.5, 0.999, 1.0] {
            for flow in [0.0, 0.37, -1.5, f32::NAN] {
                parity(
                    &mut scratch,
                    &prim,
                    Affine::pose(1.3, 0.31, Vec2::new(-7.5, 0.25)),
                    draw,
                    flow,
                );
            }
        }
    }
}

#[test]
fn lazy_rect_contours_match_the_reference_at_degenerate_and_reveal_boundaries() {
    let mut scratch = Scratch::new();
    for (origin, size) in [
        (Vec2::new(-20_000.0, -20_000.0), Vec2::new(2.0, 2.0)),
        (Vec2::default(), Vec2::new(40.0, 24.0)),
        (Vec2::default(), Vec2::new(0.0, 0.0)),
        (Vec2::default(), Vec2::new(40_000.0, 2.0)),
    ] {
        for radius in [0.0, 0.001, 1.0, 6.0, 20_000.0] {
            for width in [-1.0, 0.0, 0.001, 0.5, 1.0, 2.0, 12.0, 20_000.0] {
                let prim = Prim::Rect {
                    rect: Rect { origin, size },
                    radius,
                    fill: Some(ink()),
                    border: Some((ink(), width)),
                };
                for draw in [
                    0.0,
                    0.25_f32.next_down(),
                    0.25,
                    0.25_f32.next_up(),
                    0.5,
                    0.999,
                    1.0,
                ] {
                    parity(
                        &mut scratch,
                        &prim,
                        Affine::pose(1.0, 0.3, Vec2::new(0.125, -0.25)),
                        draw,
                        0.0,
                    );
                }
            }
        }
    }
}

#[test]
fn an_empty_stroke_has_no_handoff_or_other_allocation() {
    let prim = line(0, false, false);
    let mut scratch = Scratch::new();
    let (built, counts) = crate::allocation::activity(|| {
        paths(
            &mut scratch,
            &prim,
            Affine::default(),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE,
        )
    });
    assert_eq!(
        built.expect("empty stroke").into_iter().flatten().count(),
        0
    );
    assert_eq!(counts, [0, 0]);
}

#[test]
fn exact_edge_and_chord_reveal_boundaries_match_on_both_sides() {
    let mut scratch = Scratch::new();
    for (size, radius, boundary, expected) in [
        (
            Vec2::new(12.0, 4.0),
            0.0,
            0.125_f32,
            [111.9375, 100.0, 0.0625, 4.0],
        ),
        (
            Vec2::new(0.25, 0.25),
            0.125,
            0.25,
            [100.125, 100.0, 0.125, 0.125],
        ),
    ] {
        let prim = Prim::Rect {
            rect: Rect {
                origin: Vec2::new(100.0, 100.0),
                size,
            },
            radius,
            fill: None,
            border: Some((ink(), 0.0625)),
        };
        for draw in [boundary.next_down(), boundary, boundary.next_up()] {
            let ((), events) = crate::allocation::requests(128, || {
                let _app = paint_phase::enter(Phase::App);
                let [None, Some((path, paint))] = paths(
                    &mut scratch,
                    &prim,
                    Affine::default(),
                    draw,
                    0.0,
                    MAX_COMPOSED_SCALE,
                )
                .expect("the boundary reveals a sector") else {
                    panic!("one sector, with no background");
                };
                assert_eq!(paint, ink());
                let actual = [
                    f32::from(path.bounds.origin.x),
                    f32::from(path.bounds.origin.y),
                    f32::from(path.bounds.size.width),
                    f32::from(path.bounds.size.height),
                ];
                for (got, want) in actual.into_iter().zip(expected) {
                    assert!(
                        (got - want).abs() <= 1e-5,
                        "bounds at draw {draw}: {actual:?}"
                    );
                }
                let (triangles, tail) = path.vertices.as_chunks::<3>();
                assert!(tail.is_empty());
                let area: f32 = triangles
                    .iter()
                    .map(|triangle| {
                        let [a, b, c] = triangle.each_ref().map(|vertex| {
                            Vec2::new(
                                f32::from(vertex.xy_position.x) - 100.0,
                                f32::from(vertex.xy_position.y) - 100.0,
                            )
                        });
                        for at in [a, b, c] {
                            let inside = if radius > 0.0 {
                                (-1e-5..=0.12501).contains(&at.y)
                                    && (0.06249..=0.12501).contains(&(at.x - at.y))
                            } else {
                                let inset = 12.0 - at.x;
                                (-1e-5..=0.06251).contains(&inset)
                                    && (inset - 1e-5..=4.0 - inset + 1e-5).contains(&at.y)
                            };
                            assert!(inside, "no triangle reaches the next sector at draw {draw}");
                        }
                        ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() * 0.5
                    })
                    .sum();
                let expected_area = if radius > 0.0 {
                    0.005_859_375
                } else {
                    0.246_093_75
                };
                assert!(
                    (area - expected_area).abs() <= 1e-5,
                    "the entire revealed sector is painted"
                );
            });
            assert_eq!(paint_phase::tests::assert_paths(&events, 1), 1);
        }
    }
}

#[test]
fn largest_charged_rects_and_mirrored_strokes_preserve_parity_and_refusal() {
    let mut scratch = Scratch::new();
    for (height, width) in [
        (40_000.0, 2.0),
        (40_000.0, 20_000.0),
        (20_000.0, 10_000.0),
        (40_000.0, 0.003),
        (40_000.0, 0.001),
    ] {
        let prim = Prim::Rect {
            rect: Rect {
                origin: Vec2::new(-20_000.0, -20_000.0),
                size: Vec2::new(40_000.0, height),
            },
            radius: 20_000.0,
            fill: None,
            border: Some((ink(), width)),
        };
        for draw in [1.0, 0.999, 0.5] {
            parity(
                &mut scratch,
                &prim,
                Affine::pose(64.0, 0.3, Vec2::default()),
                draw,
                0.0,
            );
        }
    }
    let rect = Prim::Rect {
        rect: Rect {
            origin: Vec2::default(),
            size: Vec2::new(40_000.0, 40_000.0),
        },
        radius: 20_000.0,
        fill: None,
        border: Some((ink(), 2.0)),
    };
    assert_eq!(
        paths(
            &mut scratch,
            &rect,
            Affine::pose(65.0, 0.3, Vec2::default()),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE,
        )
        .map(|paths| paths.into_iter().flatten().count()),
        Err(paint_paths::BuildError::OverCharge)
    );
    for transform in [
        Affine([-1.0, 0.0, 0.0, 1.0, 300.0, 0.0]),
        Affine::pose(0.0001, 0.17, Vec2::new(20_000.0, -20_000.0)),
    ] {
        parity(&mut scratch, &line(12, true, true), transform, 0.7, 0.37);
    }
}

#[test]
fn one_straight_stroke_requests_only_one_exact_handoff() {
    let prim = Prim::Line {
        points: Arc::from([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)]),
        closed: false,
        dashed: false,
        color: ink(),
        width: 2.0,
    };
    let mut scratch = Scratch::new();
    drop(
        paths(
            &mut scratch,
            &prim,
            Affine::default(),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE,
        )
        .expect("warm"),
    );
    let ((), requests) = crate::allocation::requests(100, || {
        let _app = paint_phase::enter(Phase::App);
        let built = paths(
            &mut scratch,
            &prim,
            Affine::default(),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE,
        )
        .expect("a straight stroke");
        assert_eq!(built[0].as_ref().expect("one handoff").0.vertices.len(), 6);
        drop(built);
    });
    let allocations: Vec<_> = requests
        .iter()
        .filter(|r| r.action == 0 && r.tag == 1)
        .collect();
    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0].size, 192);
    let frees: Vec<_> = requests
        .iter()
        .filter(|r| r.action == 1 && r.pointer == allocations[0].pointer)
        .collect();
    assert_eq!(frees.len(), 1);
    assert_eq!((frees[0].size, frees[0].align), (192, allocations[0].align));
}

#[test]
fn each_transient_fill_releases_its_own_allocations_before_returning() {
    use std::collections::BTreeMap;
    let prim = Prim::Rect {
        rect: Rect {
            origin: Vec2::default(),
            size: Vec2::new(40.0, 24.0),
        },
        radius: 6.0,
        fill: Some(ink()),
        border: Some((ink(), 4.0)),
    };
    let mut scratch = Scratch::new();
    let ((), events) = crate::allocation::requests(10_000, || {
        let _app = paint_phase::enter(Phase::App);
        drop(
            paths(
                &mut scratch,
                &prim,
                Affine::pose(1.0, 0.3, Vec2::default()),
                0.5,
                0.0,
                MAX_COMPOSED_SCALE,
            )
            .expect("background and sector"),
        );
    });
    let mut live = BTreeMap::new();
    let mut builds = 0;
    for event in &events {
        match (event.action, event.tag) {
            (2, 2) => {
                assert!(live.is_empty(), "no earlier build may retain storage");
                builds += 1;
            }
            (0, 2) => {
                assert_eq!(live.insert(event.pointer, (event.size, event.align)), None);
            }
            (1, 2) => {
                assert_eq!(live.remove(&event.pointer), Some((event.size, event.align)));
            }
            (3, 2) => assert!(
                live.is_empty(),
                "all fill-owned lifetimes end inside this build"
            ),
            _ => {}
        }
    }
    assert_eq!(builds, 2);
    let handoffs: Vec<_> = events.iter().filter(|e| e.action == 4).collect();
    assert_eq!(handoffs.len(), 2);
    let app: Vec<_> = events
        .iter()
        .filter(|e| e.action == 0 && e.tag == 1)
        .collect();
    assert_eq!(app.len(), 2);
    for handoff in handoffs {
        assert_eq!(
            app.iter()
                .filter(|e| (e.pointer, e.size, e.align)
                    == (handoff.pointer, handoff.size, handoff.align))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.action == 1
                    && e.pointer == handoff.pointer
                    && e.size == handoff.size
                    && e.align == handoff.align)
                .count(),
            1
        );
    }
}
