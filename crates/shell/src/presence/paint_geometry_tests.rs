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
#[expect(
    clippy::float_cmp,
    reason = "the fixture puts the boundary exactly on an edge endpoint"
)]
fn exact_edge_and_chord_reveal_boundaries_match_on_both_sides() {
    let mut scratch = Scratch::new();
    for (rect, radius, boundary) in [
        (
            Rect {
                origin: Vec2::new(100.0, 100.0),
                size: Vec2::new(12.0, 4.0),
            },
            0.0,
            0.125_f32,
        ),
        (
            Rect {
                origin: Vec2::new(100.0, 100.0),
                size: Vec2::new(0.25, 0.25),
            },
            0.125,
            0.25,
        ),
    ] {
        let (_, segments) = paint_paths::corners(
            rect,
            radius,
            1.0,
            kumochi_scene::display::fallback_corner_segments(radius, MAX_COMPOSED_SCALE),
        )
        .expect("charged corners");
        let outline = paint_paths::rounded(rect, radius, segments);
        let lengths: Vec<_> = outline
            .iter()
            .zip(outline.iter().cycle().skip(1))
            .map(|(a, b)| (f64::from(b.x) - f64::from(a.x)).hypot(f64::from(b.y) - f64::from(a.y)))
            .collect();
        let first = lengths
            .iter()
            .copied()
            .find(|span| *span > 0.0)
            .expect("one nonzero edge");
        let total: f64 = lengths.iter().sum();
        assert_eq!(
            total * f64::from(boundary),
            first,
            "the middle fixture is exactly on an edge/chord endpoint"
        );
        let prim = Prim::Rect {
            rect,
            radius,
            fill: None,
            border: Some((ink(), 0.0625)),
        };
        for draw in [boundary.next_down(), boundary, boundary.next_up()] {
            parity(&mut scratch, &prim, Affine::default(), draw, 0.0);
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
#[ignore = "bounded parity corpus; select explicitly after execution and timeout admission"]
fn changing_geometry_matches_the_reference_over_the_charged_segments_and_off_grid_draws() {
    let mut scratch = Scratch::new();
    let prim = Prim::Rect {
        rect: Rect {
            origin: Vec2::new(-20_000.0, -20_000.0),
            size: Vec2::new(40_000.0, 40_000.0),
        },
        radius: 20_000.0,
        fill: Some(ink()),
        border: Some((ink(), 2.0)),
    };
    let lower_zoom = |target: u16| {
        let (mut low, mut high) = (0.0_f32, 64.0_f32);
        for _ in 0..32 {
            let middle = f32::midpoint(low, high);
            if kumochi_scene::display::corner_segments(20_000.0 * f64::from(middle))
                < usize::from(target)
            {
                low = middle;
            } else {
                high = middle;
            }
        }
        high
    };
    for target in 1..=1_257_u16 {
        let lower = lower_zoom(target);
        let upper = if target == 1_257 {
            64.0
        } else {
            lower_zoom(target + 1)
        };
        let zoom = f32::midpoint(lower, upper);
        let transform = Affine::pose(zoom, 0.0, Vec2::default());
        assert_eq!(
            kumochi_scene::display::corner_segments(
                20_000.0 * f64::from(paint_paths::scale(transform))
            ),
            usize::from(target)
        );
        parity(&mut scratch, &prim, transform, 0.731, 0.0);
    }
    let small = Prim::Rect {
        rect: Rect {
            origin: Vec2::new(-20_000.0, -20_000.0),
            size: Vec2::new(2.0, 2.0),
        },
        radius: 1.0,
        fill: Some(ink()),
        border: Some((ink(), 0.001)),
    };
    for sample in 1..=997_u16 {
        let draw = f32::from(sample) / 998.0;
        parity(
            &mut scratch,
            &small,
            Affine::pose(1.0, 0.314_159, Vec2::new(0.125, -0.25)),
            draw,
            0.0,
        );
        parity(
            &mut scratch,
            &line(12, true, true),
            Affine::default(),
            draw,
            draw,
        );
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
fn rect_scratch_stays_with_the_painter_and_is_charged_once() {
    use super::{FILL_SCRATCH_BYTES, Rounded, events, tessellated};
    use gpui_kit::FillRule;

    let mut scratch = Scratch::new();
    let rect = Rect {
        origin: Vec2::default(),
        size: Vec2::new(20_000.0, 18_000.0),
    };
    let global = kumochi_scene::limits::MAX_GLOBAL_FILL_VERTICES;
    let fill_segments = u32::try_from((global - 1) / 4 - 1).unwrap();
    let outline = kumochi_scene::limits::MAX_GLOBAL_OUTLINE_POINTS;
    let ring_segments = u32::try_from((outline - 1) / 4 - 1).unwrap();
    let far = Rounded::new(rect, 8_000.0, fill_segments);
    let outer = Rounded::new(rect, 8_000.0, ring_segments);
    let inner = Rounded::new(
        Rect {
            origin: Vec2::new(4.0, 4.0),
            size: rect.size - Vec2::new(8.0, 8.0),
        },
        7_996.0,
        ring_segments,
    );
    let transform = Affine::pose(2.0, 0.25, Vec2::default());
    let ((), _, [allocated, released]) = crate::allocation::usage(|| {
        drop(
            tessellated(
                &mut scratch.fill,
                events(far.iter()),
                FillRule::NonZero,
                transform,
            )
            .expect("a cap-sized fill")
            .expect("has path vertices"),
        );
        drop(
            tessellated(
                &mut scratch.fill,
                events(outer.iter()).chain(events(inner.iter())),
                FillRule::EvenOdd,
                transform,
            )
            .expect("a cap-sized ring")
            .expect("has path vertices"),
        );
    });
    let held = allocated - released;
    assert!(
        held > 0 && held <= FILL_SCRATCH_BYTES,
        "scratch held {held} bytes"
    );
    let ((), _, [allocated, freed]) = crate::allocation::usage(|| drop(scratch.fill));
    assert_eq!(
        (allocated, freed),
        (0, held),
        "scratch releases exactly what it kept"
    );
}

#[test]
fn warmed_rect_fallback_allocates_only_the_path_handoffs_at_placement_two() {
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
    let transform = Affine::pose(2.0, 0.3, Vec2::default());
    let draws = [0.25, 0.5, 0.999, 1.0];
    for draw in draws {
        assert!(
            paths(&mut scratch, &prim, transform, draw, 0.0, 2.0)
                .expect("warm the same fallback")
                .iter()
                .all(Option::is_some)
        );
    }
    let ((), events) = crate::allocation::requests(8_192, || {
        let _app = paint_phase::enter(Phase::App);
        for draw in draws {
            drop(
                paths(&mut scratch, &prim, transform, draw, 0.0, 2.0)
                    .expect("fill and border stay paintable"),
            );
        }
    });
    let scratch_allocations = events
        .iter()
        .filter(|event| event.action == 0 && event.tag == 2)
        .count();
    assert_eq!(
        scratch_allocations, 0,
        "warm fallback tessellation allocates nothing"
    );
    assert_eq!(
        paint_phase::tests::assert_paths(&events, 1),
        2 * draws.len()
    );
}
