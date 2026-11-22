use std::f32::consts::TAU;

use gpui_kit::{Path, PathBuilder, Pixels, Point, point, px};
use kumochi_scene::{
    Handle, Text,
    display::{
        BUILD_VERTEX_LIMIT, Contour, FILL_VERTEX_LIMIT, Paint, PaintWork, Prim, TextStyle,
        corner_segments, crossings, fallback_corner_segments,
    },
    geom::{Affine, Rect, Vec2},
    limits::MAX_COMPOSED_SCALE,
    record::{Align, Rgb},
};

use super::super::{
    paint_cache::PaintScene,
    paint_geometry::{self, PrimPaths},
    paint_phase::{self, Phase},
    paint_stroke::Scratch,
};
use super::{BuildError, corners, fill_triangles, paths, scale};

fn ink() -> Paint {
    Paint::from(Rgb::try_from(0x33_66_99).expect("a packed RGB value"))
}

fn pts(points: &[(f32, f32)]) -> Vec<Point<Pixels>> {
    points.iter().map(|&(x, y)| point(px(x), px(y))).collect()
}

fn solid_line(points: &[(f32, f32)], contour: Contour) -> Prim {
    Prim::Line {
        points: points.iter().map(|&(x, y)| Vec2::new(x, y)).collect(),
        closed: matches!(contour, Contour::Closed),
        color: ink(),
        width: 2.0,
        dashed: false,
    }
}

fn dashed_line(points: &[(f32, f32)]) -> Prim {
    dash_contour(points, Contour::Open)
}

fn dash_contour(points: &[(f32, f32)], contour: Contour) -> Prim {
    Prim::Line {
        points: points.iter().map(|&(x, y)| Vec2::new(x, y)).collect(),
        closed: matches!(contour, Contour::Closed),
        color: ink(),
        width: 2.0,
        dashed: true,
    }
}

/// A zigzag of `count` points, so no two consecutive edges are collinear.
fn zigzag(count: u16, y: f32) -> Vec<(f32, f32)> {
    (0..count)
        .map(|i| (f32::from(i), y + f32::from(i % 2)))
        .collect()
}

fn geometry_paths(
    scratch: &mut Scratch,
    prim: &Prim,
    transform: Affine,
    draw: f32,
    flow: f32,
) -> Result<PrimPaths, BuildError> {
    paint_geometry::paths(scratch, prim, transform, draw, flow, MAX_COMPOSED_SCALE)
}

fn built(scratch: &mut Scratch, prim: &Prim, draw: f32, flow: f32) -> Path<Pixels> {
    let [Some((path, _)), None] = geometry_paths(scratch, prim, Affine::default(), draw, flow)
        .expect("bounded geometry builds")
    else {
        panic!("one path in the first slot");
    };
    path
}

/// Hand-written runs stroked in one lyon build, each run its own subpath.
fn reference(runs: &[Vec<Point<Pixels>>], contour: Contour) -> Path<Pixels> {
    let mut path = PathBuilder::stroke(px(2.0));
    for run in runs {
        path.move_to(run[0]);
        run[1..].iter().for_each(|to| path.line_to(*to));
        if matches!(contour, Contour::Closed) {
            path.close();
        }
    }
    path.build()
        .expect("a reference stroke under the u16 ceiling builds")
}

fn assert_same_coverage(actual: &Path<Pixels>, expected: &Path<Pixels>) {
    assert_eq!(actual.bounds, expected.bounds, "the same bounds");
    let origin = world(expected.bounds.origin);
    let width = f32::from(expected.bounds.size.width);
    let height = f32::from(expected.bounds.size.height);
    for (i, j) in (0..64_u8).flat_map(|i| (0..64_u8).map(move |j| (i, j))) {
        let at = origin
            + Vec2::new(
                (f32::from(i) + 0.125) * width / 64.0,
                (f32::from(j) + 0.375) * height / 64.0,
            );
        assert_eq!(
            covers(actual, at),
            covers(expected, at),
            "coverage at {at:?}"
        );
    }
}

fn assert_straight_runs(path: &Path<Pixels>, intervals: &[(f32, f32)]) {
    let left = intervals.first().expect("a dash").0;
    let right = intervals.last().expect("a dash").1;
    assert_eq!(
        (
            path.bounds.origin,
            path.bounds.size.width,
            path.bounds.size.height
        ),
        (point(px(left), px(-1.0)), px(right - left), px(2.0)),
        "path bounds tightly cover the first and last dashes",
    );
    let (triangles, tail) = path.vertices.as_chunks::<3>();
    assert!(
        tail.is_empty(),
        "the handoff contains only complete triangles"
    );
    let mut areas = vec![0.0; intervals.len()];
    for triangle in triangles {
        let [a, b, c] = triangle.each_ref().map(|vertex| world(vertex.xy_position));
        let left = a.x.min(b.x).min(c.x);
        let right = a.x.max(b.x).max(c.x);
        let index = intervals.partition_point(|&(_, end)| end < left);
        let &(start, end) = intervals.get(index).expect("a triangle lies in a dash");
        assert!(
            start <= left && right <= end,
            "a dash cannot bleed into a gap"
        );
        for at in [a, b, c] {
            assert!(
                (-1.0..=1.0).contains(&at.y),
                "the stroke is two pixels wide"
            );
        }
        areas[index] += side(a, b, c).abs() * 0.5;
    }
    for (area, &(start, end)) in areas.iter().zip(intervals) {
        assert!(
            (*area - (end - start) * 2.0).abs() <= 1e-5,
            "the dash at {start}..{end} covers its full strip",
        );
    }
}

#[test]
fn dashes_follow_the_prim_pattern_and_its_flow() {
    let mut scratch = Scratch::new();
    let dashed = dashed_line(&[(0.0, 0.0), (128.0, 0.0)]);
    let at_rest: Vec<_> = (0..13_u8)
        .map(|k| (10.0 * f32::from(k), 10.0 * f32::from(k) + 6.0))
        .collect();
    assert_straight_runs(&built(&mut scratch, &dashed, 1.0, 0.0), &at_rest);

    let mut moved = vec![(0.0, 1.0)];
    moved.extend((1..13_u8).map(|k| (10.0 * f32::from(k) - 5.0, 10.0 * f32::from(k) + 1.0)));
    moved.push((125.0, 128.0));
    for flow in [0.5, -1.5] {
        assert_straight_runs(&built(&mut scratch, &dashed, 1.0, flow), &moved);
    }
    assert_eq!(
        PaintWork::of(std::slice::from_ref(&dashed), MAX_COMPOSED_SCALE).dash_pieces,
        14,
        "the scene charges enough pieces for the half-period shift"
    );

    let mut revealed: Vec<_> = at_rest[..6].to_vec();
    revealed.push((60.0, 64.0));
    assert_straight_runs(&built(&mut scratch, &dashed, 0.5, 0.0), &revealed);

    let closed = dash_contour(
        &[(0.0, 0.0), (0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)],
        Contour::Closed,
    );
    assert_same_coverage(
        &built(&mut scratch, &closed, 1.0, 0.0),
        &reference(
            &[
                pts(&[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0)]),
                pts(&[(2.0, 4.0), (0.0, 4.0), (0.0, 0.0)]),
            ],
            Contour::Open,
        ),
    );
}

/// The second line's first dash ends on a vertex, and its gap is exactly the next segment.
#[test]
fn a_dash_crosses_a_vertex_but_never_a_gap() {
    let mut scratch = Scratch::new();
    let corner = dashed_line(&[(0.0, 0.0), (4.0, 0.0), (4.0, 8.0)]);
    let expected = [
        pts(&[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0)]),
        pts(&[(4.0, 6.0), (4.0, 8.0)]),
    ];
    assert_same_coverage(
        &built(&mut scratch, &corner, 1.0, 0.0),
        &reference(&expected, Contour::Open),
    );

    let gap = dashed_line(&[(0.0, 0.0), (6.0, 0.0), (6.0, 4.0), (6.0, 10.0)]);
    let expected = [
        pts(&[(0.0, 0.0), (6.0, 0.0)]),
        pts(&[(6.0, 4.0), (6.0, 10.0)]),
    ];
    let path = built(&mut scratch, &gap, 1.0, 0.0);
    assert_same_coverage(&path, &reference(&expected, Contour::Open));
    assert!(
        !covers(&path, Vec2::new(6.0, 2.0)),
        "the vertex gap stays clear"
    );

    let straight = dashed_line(&[(0.0, 0.0), (12.0, 0.0)]);
    let [Some((mirrored, _)), None] = geometry_paths(
        &mut scratch,
        &straight,
        Affine([-1.0, 0.0, 0.0, 1.0, 300.0, 0.0]),
        1.0,
        0.0,
    )
    .expect("the reflected stroke builds") else {
        panic!("one reflected stroke");
    };
    assert_straight_runs(&mirrored, &[(288.0, 290.0), (294.0, 300.0)]);
    assert_eq!(
        (
            mirrored.bounds.origin,
            mirrored.bounds.size.width,
            mirrored.bounds.size.height
        ),
        (point(px(288.0), px(-1.0)), px(12.0), px(2.0)),
    );
}

#[test]
fn solid_closed_strokes_close_only_when_whole() {
    let mut scratch = Scratch::new();
    let square = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
    let closed = solid_line(&square, Contour::Closed);
    assert_same_coverage(
        &built(&mut scratch, &closed, 1.0, 0.0),
        &reference(&[pts(&square)], Contour::Closed),
    );
    assert_same_coverage(
        &built(&mut scratch, &closed, 0.5, 0.0),
        &reference(&[pts(&square[..3])], Contour::Open),
    );
    let drawn = geometry_paths(&mut scratch, &closed, Affine::default(), 0.0, 0.0)
        .expect("an unrevealed stroke builds");
    assert!(matches!(drawn, [None, None]), "draw 0 reveals no stroke");
}

/// 3,277 two-point runs cost 39,324, so they take two chunks.
#[test]
fn a_long_dashed_line_merges_its_chunks_into_one_build() {
    assert_eq!(BUILD_VERTEX_LIMIT, 21_845, "the documented build limit");
    let mut scratch = Scratch::new();
    let dashed = dashed_line(&[(0.0, 0.0), (32_768.0, 0.0)]);
    let intervals: Vec<_> = (0..3_277_u16)
        .map(|k| (10.0 * f32::from(k), 10.0 * f32::from(k) + 6.0))
        .collect();
    let near_boundary: Vec<_> = (0..5_456_u16)
        .map(|index| (f32::from(index) / 1024.0, 0.0))
        .chain([(6.0, 0.0), (26.0, 0.0)])
        .collect();
    let mixed = dashed_line(&near_boundary);
    for (prim, strips) in [
        (&dashed, intervals.as_slice()),
        (&mixed, &[(0.0, 6.0), (10.0, 16.0), (20.0, 26.0)][..]),
    ] {
        let ((), events) = crate::allocation::requests(16, || {
            let _app = paint_phase::enter(Phase::App);
            let path = built(&mut scratch, prim, 1.0, 0.0);
            drop(path);
        });
        assert_eq!(paint_phase::tests::assert_paths(&events, 1), 1);
        assert_straight_runs(&built(&mut scratch, prim, 1.0, 0.0), strips);
    }
}

#[test]
fn one_run_past_the_build_limit_is_refused() {
    let mut scratch = Scratch::new();
    for (count, expected) in [(5_460, Ok(1)), (5_461, Err(BuildError::OverCharge))] {
        let prim = solid_line(&zigzag(count, 0.0), Contour::Open);
        let (actual, events) = crate::allocation::requests(16, || {
            let _app = paint_phase::enter(Phase::App);
            geometry_paths(&mut scratch, &prim, Affine::default(), 1.0, 0.0)
                .map(|paths| paths.into_iter().flatten().count())
        });
        assert_eq!(actual, expected, "stroke with {count} points");
        assert_eq!(
            paint_phase::tests::assert_paths(&events, 1),
            expected.unwrap_or(0)
        );
    }
}

fn circle(count: u16, radius: f32) -> Vec<Vec2> {
    (0..count)
        .map(|i| {
            let angle = TAU * f32::from(i) / f32::from(count);
            Vec2::new(angle.cos(), angle.sin()) * radius
        })
        .collect()
}

#[test]
fn fills_are_refused_past_their_point_limit() {
    assert_eq!(FILL_VERTEX_LIMIT, 21_845, "the documented fill limit");
    let triangles = fill_triangles(&circle(21_845, 10_000.0)).expect("the fill limit builds");
    assert!(
        triangles
            .as_chunks::<3>()
            .0
            .iter()
            .any(|triangle| { triangle_covers(*triangle, Vec2::default()) }),
        "a fill at the limit emits triangles that cover the centre"
    );
    let over = circle(21_846, 10_000.0);
    let (result, counts, bytes) = crate::allocation::usage(|| fill_triangles(&over));
    assert_eq!(result.err(), Some(BuildError::OverCharge));
    assert_eq!(
        (counts, bytes),
        ([0, 0], [0, 0]),
        "refused before tessellation"
    );
}

fn lyon_fill(points: impl IntoIterator<Item = Vec2>) -> Result<(), ()> {
    let mut path = PathBuilder::fill();
    let mut points = points.into_iter().map(|v| point(px(v.x), px(v.y)));
    path.move_to(points.next().expect("a polygon"));
    points.for_each(|to| path.line_to(to));
    path.close();
    path.build().map(drop).map_err(drop)
}

fn lyon_stroke(subpaths: &[Vec<(f32, f32)>], contour: Contour) -> Result<(), ()> {
    let mut path = PathBuilder::stroke(px(2.0));
    for subpath in subpaths {
        let points = pts(subpath);
        path.move_to(points[0]);
        points[1..].iter().for_each(|to| path.line_to(*to));
        if matches!(contour, Contour::Closed) {
            path.close();
        }
    }
    path.build().map(drop).map_err(drop)
}

/// The regular star {361/180}: 361 points and 361 · 179 crossings, 64,980 of lyon's 65,536.
fn star(radius: f32) -> Vec<Vec2> {
    (0..361_u16)
        .map(|i| {
            let angle = TAU * f32::from(i * 180 % 361) / 361.0;
            Vec2::new(angle.cos(), angle.sin()) * radius
        })
        .collect()
}

#[test]
fn lyon_fills_stay_within_points_and_crossings_at_the_u16_ceiling() {
    let doubled = circle(32_768, 10_000.0).into_iter().flat_map(|v| [v, v]);
    assert_eq!(lyon_fill(doubled), Ok(()), "65_536 points, each twice");
    let collinear = (0..65_534_u16)
        .map(|i| Vec2::new(f32::from(i) * 0.25, 0.0))
        .chain([Vec2::new(16_383.25, 100.0), Vec2::new(0.0, 100.0)]);
    assert_eq!(lyon_fill(collinear), Ok(()), "65_536 points on one edge");
    for radius in [1_000.0, 1_280_000.0] {
        assert_eq!(
            crossings(&star(radius), usize::MAX),
            64_619,
            "the guard counts every non-adjacent pair of the star"
        );
    }
    let star_fill = Prim::Fill {
        points: star(1_000.0).into(),
        color: ink(),
    };
    assert_eq!(
        PaintWork::of(&[star_fill], MAX_COMPOSED_SCALE).largest_fill,
        21_846,
        "the guard refuses the 361-point star the painter's point check would pass"
    );
    assert_eq!(
        lyon_fill(star(1_000.0)),
        Ok(()),
        "crossings at a small extent"
    );
    assert_eq!(
        lyon_fill(star(1_280_000.0)),
        Ok(()),
        "near-parallel crossings at MAX_COORD * MAX_COMPOSED_SCALE, where the f32 ulp is largest"
    );

    let step = TAU / 70_000.0;
    let convex = std::iter::successors(Some(0.0_f32), |angle| Some(angle + step))
        .take(70_000)
        .map(|angle| Vec2::new(angle.cos(), angle.sin()) * 100_000.0);
    assert_eq!(lyon_fill(convex), Err(()), "70_000 vertices overflow u16");
}

/// 4 · 16,383 + 4 = 65,536, every vertex id lyon's u16 index can name.
#[test]
fn lyon_strokes_stay_within_four_per_point_at_the_u16_ceiling() {
    let reversing: Vec<_> = (0..16_383_u16)
        .map(|i| (100.0 * f32::from(i % 2), 0.01 * f32::from(i)))
        .collect();
    assert_eq!(
        lyon_stroke(std::slice::from_ref(&reversing), Contour::Open),
        Ok(()),
        "near-180 turns"
    );
    assert_eq!(
        lyon_stroke(&[reversing], Contour::Closed),
        Ok(()),
        "closed near-180 turns"
    );
    let straight: Vec<_> = (0..16_383_u16)
        .map(|i| (f32::from(i), 0.001 * f32::from(i % 2)))
        .collect();
    assert_eq!(
        lyon_stroke(&[straight], Contour::Open),
        Ok(()),
        "near-0 turns"
    );
    let repeated: Vec<_> = (0..16_383_u16).map(|i| (f32::from(i / 2), 0.0)).collect();
    assert_eq!(
        lyon_stroke(&[repeated], Contour::Open),
        Ok(()),
        "repeated points"
    );

    let pairs: Vec<_> = (0..16_400_u16)
        .map(|i| vec![(0.0, f32::from(i)), (1.0, f32::from(i))])
        .collect();
    assert_eq!(
        lyon_stroke(&pairs, Contour::Open),
        Err(()),
        "65_600 vertices overflow u16"
    );
}

/// The largest sagitta between consecutive points of one corner around `center`.
fn outer_sagitta(path: &Path<Pixels>, radius: f64) -> f64 {
    let mut edges = std::collections::BTreeMap::new();
    for triangle in path.vertices.as_chunks::<3>().0 {
        for (a, b) in [(0, 1), (1, 2), (2, 0)] {
            let mut edge = [a, b].map(|index| {
                let at = world(triangle[index].xy_position);
                [at.x.to_bits(), at.y.to_bits()]
            });
            edge.sort_unstable();
            *edges.entry(edge).or_insert(0) += 1;
        }
    }
    edges
        .into_iter()
        .filter(|(_, uses)| *uses == 1)
        .map(|([from, to], _)| {
            let [ax, ay] = from.map(|bits| f64::from(f32::from_bits(bits)));
            let [bx, by] = to.map(|bits| f64::from(f32::from_bits(bits)));
            radius - f64::midpoint(ax, bx).hypot(f64::midpoint(ay, by))
        })
        .reduce(f64::max)
        .expect("the fill has boundary edges")
}

#[test]
fn corners_flatten_from_the_world_radius_without_coarsening() {
    assert_eq!(corner_segments(20_000.0), 158, "the shared corner charge");
    assert_eq!(corner_segments(0.0), 0, "a sharp corner charges no chords");
    let filled = |size: f32, radius, border| Prim::Rect {
        rect: Rect {
            origin: Vec2::new(-size / 2.0, -size / 2.0),
            size: Vec2::new(size, size),
        },
        radius,
        fill: Some(ink()),
        border,
    };
    let mut scratch = Scratch::new();
    let [Some((circle, _)), Some((_, _))] = geometry_paths(
        &mut scratch,
        &filled(20_000.0, 10_000.0, Some((ink(), 2.0))),
        Affine::pose(2.0, 0.0, Vec2::default()),
        0.5,
        0.0,
    )
    .expect("a filled round rect builds") else {
        panic!("the large rect paints its fill and partial border");
    };
    let sagitta = outer_sagitta(&circle, 20_000.0);
    assert!(sagitta <= 0.25, "the outer edges lose {sagitta} pixels");
    let coarse = 20_000.0 * (1.0 - (std::f64::consts::FRAC_PI_4 / 157.0).cos());
    assert!(coarse > 0.25, "157 uniform chords miss the tolerance");

    let sharp = built(&mut scratch, &filled(1_000.0, 0.0, None), 1.0, 0.0);
    assert_same_coverage(
        &sharp,
        &polygon(&[
            (-500.0, -500.0),
            (500.0, -500.0),
            (500.0, 500.0),
            (-500.0, 500.0),
        ]),
    );
}

#[test]
fn a_pose_past_the_charged_scale_fails_instead_of_coarsening() {
    let rect = Rect {
        origin: Vec2::new(0.0, 0.0),
        size: Vec2::new(1_000.0, 1_000.0),
    };
    let prim = Prim::Rect {
        rect,
        radius: 400.0,
        fill: Some(ink()),
        border: Some((ink(), 4.0)),
    };
    let mut scratch = Scratch::new();
    let charge = fallback_corner_segments(400.0, 2.0);
    for (zoom, expected_pieces, built) in [
        (2.0, Ok(32), Ok(2)),
        (2.07, Ok(32), Ok(2)),
        (
            2.08,
            Err(BuildError::OverCharge),
            Err(BuildError::OverCharge),
        ),
    ] {
        assert_eq!(
            corner_segments(400.0 * f64::from(zoom)),
            if zoom < 2.08 { 32 } else { 33 },
            "the shared budget crosses the next subdivision at 2.08"
        );
        // Turned and half drawn, so the painter takes the path fallback, not a native quad.
        let turned = Affine::pose(zoom, 0.5, Vec2::new(0.0, 0.0));
        assert_eq!(
            corners(rect, 400.0, scale(turned), charge).map(|(_, segments)| segments),
            expected_pieces,
            "world-scale admission uses the shared charge"
        );
        assert_eq!(
            paint_geometry::paths(&mut scratch, &prim, turned, 0.5, 0.0, 2.0)
                .map(|paths| paths.into_iter().flatten().count()),
            built,
            "a pose needing more pieces than charged isn't coarsened to fit"
        );
    }
}

#[test]
fn text_is_refused_and_an_unusable_phase_paints_nothing() {
    let mut scratch = Scratch::new();
    let text = Prim::Text {
        origin: Vec2::new(0.0, 0.0),
        text: Text::try_from("label".to_owned()).expect("short text"),
        style: TextStyle {
            size: 12.0,
            bold: false,
        },
        color: ink(),
        max_width: None,
        align: Align::Left,
    };
    assert_eq!(
        geometry_paths(&mut scratch, &text, Affine::default(), 1.0, 0.0,).err(),
        Some(BuildError::Text),
        "text is painted outside"
    );
    let dashed = dashed_line(&[(0.0, 0.0), (128.0, 0.0)]);
    assert!(
        matches!(
            geometry_paths(&mut scratch, &dashed, Affine::default(), 1.0, f32::NAN,),
            Ok([None, None])
        ),
        "a NaN phase ends the walk"
    );
    let empty = Prim::Rect {
        rect: Rect {
            origin: Vec2::new(-20_000.0, -20_000.0),
            size: Vec2::new(40_000.0, 40_000.0),
        },
        radius: 20_000.0,
        fill: None,
        border: None,
    };
    let (result, counts, bytes) = crate::allocation::usage(|| {
        geometry_paths(
            &mut scratch,
            &empty,
            Affine::pose(65.0, 0.0, Vec2::default()),
            1.0,
            0.0,
        )
    });
    assert!(matches!(result, Ok([None, None])));
    assert_eq!(
        (counts, bytes),
        ([0, 0], [0, 0]),
        "an unpainted rect does no uncharged tessellation work"
    );
}

fn side(p: Vec2, q: Vec2, r: Vec2) -> f32 {
    (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x)
}

fn triangle_covers([a, b, c]: [Vec2; 3], at: Vec2) -> bool {
    let sides = [side(a, b, at), side(b, c, at), side(c, a, at)];
    side(a, b, c).abs() > 1e-6
        && (sides.iter().all(|s| *s >= -1e-3) || sides.iter().all(|s| *s <= 1e-3))
}

/// Whether a non-degenerate triangle of the path covers `at`, edges included.
fn covers(path: &Path<Pixels>, at: Vec2) -> bool {
    path.vertices.as_chunks::<3>().0.iter().any(|triangle| {
        triangle_covers(
            triangle.each_ref().map(|vertex| world(vertex.xy_position)),
            at,
        )
    })
}

fn world(point: Point<Pixels>) -> Vec2 {
    Vec2::new(f32::from(point.x), f32::from(point.y))
}

fn border(size: f32, radius: f32, width: f32) -> Prim {
    bordered(Vec2::new(size, size), radius, width)
}

fn bordered(size: Vec2, radius: f32, width: f32) -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size,
        },
        radius,
        fill: None,
        border: Some((ink(), width)),
    }
}

fn border_path(prim: &Prim, transform: Affine) -> Path<Pixels> {
    let [None, Some((path, _))] = geometry_paths(&mut Scratch::new(), prim, transform, 1.0, 0.0)
        .expect("a bordered rect builds")
    else {
        panic!("one border, no background");
    };
    path
}

/// A quad's border wider than half its short side fills the whole rect and nothing outside it,
/// so a rotated 1x1 rect with the default border 2 must too, square or round.
#[test]
fn a_border_wider_than_its_rect_covers_exactly_the_rect() {
    let transform = Affine::pose(40.0, 0.5, Vec2::new(100.0, 100.0));
    let local = transform.inverse().expect("a pose inverts");
    let samples: Vec<_> = (0..10_u8)
        .flat_map(|i| (0..10_u8).map(move |j| (i, j)))
        .map(|(i, j)| Vec2::new(0.05 + 0.1 * f32::from(i), 0.05 + 0.1 * f32::from(j)))
        .collect();

    let square = border_path(&border(1.0, 0.0, 2.0), transform);
    for vertex in &square.vertices {
        let at = local.point(world(vertex.xy_position));
        assert!(
            (-1e-3..=1.001).contains(&at.x) && (-1e-3..=1.001).contains(&at.y),
            "every vertex stays inside the square"
        );
    }
    for sample in &samples {
        assert!(
            covers(&square, transform.point(*sample)),
            "every point of the square is border"
        );
    }

    let center = Vec2::new(0.5, 0.5);
    let round = border_path(&border(1.0, 0.5, 2.0), transform);
    for vertex in &round.vertices {
        let at = local.point(world(vertex.xy_position)) - center;
        assert!(
            at.x.hypot(at.y) <= 0.5001,
            "every vertex stays inside the circle"
        );
    }
    let inside = samples.iter().filter(|sample| {
        let off = **sample - center;
        off.x.hypot(off.y) <= 0.45
    });
    for sample in inside {
        assert!(
            covers(&round, transform.point(*sample)),
            "every point of the circle is border"
        );
    }
    assert!(
        covers(&square, transform.point(center)),
        "the square center is painted"
    );
    assert!(
        covers(&round, transform.point(center)),
        "the circle center is painted"
    );
}

/// How far `at` lies outside a rounded rect, negative inside: the distance a quad tests,
/// independent of the painter's flattened outline.
fn outside(at: Vec2, rect: Rect, radius: f32) -> f32 {
    let half = rect.size * 0.5;
    let from = at - (rect.origin + half);
    let corner = Vec2::new(
        from.x.abs() - half.x + radius,
        from.y.abs() - half.y + radius,
    );
    corner.x.max(0.0).hypot(corner.y.max(0.0)) + corner.x.max(corner.y).min(0.0) - radius
}

/// Asserts a fully drawn border covers what its quad's would on a half-unit grid: inside the
/// rounded rect and outside the rect inset by the width at radius `max(radius − width, 0)`.
/// Samples within the 0.25 flattening tolerance of either outline are skipped.
fn assert_quad_border(size: Vec2, radius: f32, width: f32) {
    let rect = Rect {
        origin: Vec2::new(0.0, 0.0),
        size,
    };
    let inset = Vec2::new(width, width);
    let hole = Rect {
        origin: inset,
        size: size - inset * 2.0,
    };
    let path = border_path(&bordered(size, radius, width), Affine::default());
    let mut checked = 0_u32;
    for (i, j) in (0..=88_u8).flat_map(|i| (0..=88_u8).map(move |j| (i, j))) {
        let at = Vec2::new(f32::from(i), f32::from(j)) * 0.5 - Vec2::new(1.0, 1.0);
        let outer = outside(at, rect, radius);
        let inner = outside(at, hole, (radius - width).max(0.0));
        if outer.abs() < 0.26 || inner.abs() < 0.26 {
            continue;
        }
        assert_eq!(
            covers(&path, at),
            outer < 0.0 && inner > 0.0,
            "quad coverage at {at:?}, radius {radius}, width {width}"
        );
        checked += 1;
    }
    assert!(
        checked > 3_000,
        "most of the grid is clear of both outlines"
    );
}

#[test]
fn a_border_covers_what_its_quad_would() {
    for (radius, width) in [
        (0.0, 4.0),
        (0.5, 3.0),
        (3.0, 4.0),
        (4.0, 4.0),
        (10.0, 4.0),
        (12.0, 12.0),
    ] {
        assert_quad_border(Vec2::new(40.0, 24.0), radius, width);
    }
    assert_quad_border(Vec2::new(40.0, 40.0), 1.0, 4.0);
    let narrow = border_path(&border(40.0, 1.0, 4.0), Affine::default());
    for (x, y) in [(0.1, 0.1), (39.9, 0.1), (39.9, 39.9), (0.1, 39.9)] {
        assert!(
            !covers(&narrow, Vec2::new(x, y)),
            "outside the rounded corner"
        );
    }
    for (x, y) in [(0.5, 1.0), (1.0, 0.5), (3.0, 3.0)] {
        assert!(covers(&narrow, Vec2::new(x, y)), "the band at the corner");
    }

    let sharp = border_path(&border(40.0, 0.0, 4.0), Affine::default());
    for vertex in &sharp.vertices {
        let at = world(vertex.xy_position);
        assert!((0.0..=40.0).contains(&at.x) && (0.0..=40.0).contains(&at.y));
    }
    for (x, y) in [(2.0, 20.0), (20.0, 2.0), (38.0, 20.0), (1.0, 1.0)] {
        assert!(covers(&sharp, Vec2::new(x, y)), "the band is border");
    }
    for (x, y) in [(20.0, 20.0), (5.0, 20.0), (20.0, 35.0), (5.0, 5.0)] {
        assert!(!covers(&sharp, Vec2::new(x, y)), "the interior isn't");
    }

    let mut scratch = Scratch::new();
    for (size, width) in [(0.0, 4.0), (40.0, 0.0), (40.0, -1.0)] {
        let rect = Prim::Rect {
            rect: Rect {
                origin: Vec2::default(),
                size: Vec2::new(size, size),
            },
            radius: 0.0,
            fill: Some(ink()),
            border: Some((ink(), width)),
        };
        let [fill, None] = geometry_paths(&mut scratch, &rect, Affine::default(), 1.0, 0.0)
            .expect("degenerate contour is not a tessellation error")
        else {
            panic!("a degenerate contour paints no border");
        };
        if size > 0.0 {
            let (path, _) = fill.expect("a visible background");
            assert!(covers(&path, Vec2::new(20.0, 20.0)));
            assert_eq!(path.bounds.origin, point(px(0.0), px(0.0)));
            assert_eq!(
                (path.bounds.size.width, path.bounds.size.height),
                (px(40.0), px(40.0))
            );
        } else {
            assert!(fill.is_none(), "a zero-size rect paints no fill");
        }
    }
}

fn drawn(prim: &Prim, draw: f32) -> Option<Path<Pixels>> {
    let [None, border] = geometry_paths(&mut Scratch::new(), prim, Affine::default(), draw, 0.0)
        .expect("a bordered rect builds")
    else {
        panic!("a border does not paint a background");
    };
    border.map(|(path, _)| path)
}

fn polygon(points: &[(f32, f32)]) -> Path<Pixels> {
    let points = pts(points);
    let mut path = PathBuilder::fill();
    path.move_to(points[0]);
    points[1..].iter().for_each(|to| path.line_to(*to));
    path.close();
    path.build().expect("a small polygon builds")
}

/// A partial border is one sector, clockwise from the right end of the top edge for that
/// fraction of the outer outline, closed across to the inner outline at the same fraction of
/// the matching edge.
#[test]
fn a_partial_border_fills_one_sector_of_the_band() {
    let square = border(40.0, 0.0, 4.0);
    let sector = |draw| drawn(&square, draw).expect("a revealed sector");
    assert_same_coverage(
        &sector(0.25),
        &polygon(&[(40.0, 0.0), (40.0, 40.0), (36.0, 36.0), (36.0, 4.0)]),
    );
    assert_same_coverage(
        &sector(0.3125),
        &polygon(&[
            (40.0, 0.0),
            (40.0, 40.0),
            (30.0, 40.0),
            (28.0, 36.0),
            (36.0, 36.0),
            (36.0, 4.0),
        ]),
    );
    for draw in [0.0, -1.0] {
        assert!(drawn(&square, draw).is_none(), "nothing is revealed");
    }
}

/// On a straight side the cut runs straight across the band. At draw 0.2 it crosses the right
/// side at one height, where pairing by the inner outline's own length would slant it by about
/// 1.25 and reveal the samples below it.
#[test]
fn a_partial_border_cuts_straight_across_the_band() {
    let path = drawn(&border(40.0, 10.0, 4.0), 0.2).expect("a revealed sector");
    let cut = 22.87;
    for x in [36.5, 37.5, 39.5] {
        assert!(
            covers(&path, Vec2::new(x, cut - 0.25)),
            "revealed above the cut"
        );
        assert!(!covers(&path, Vec2::new(x, cut + 0.25)), "hidden below it");
    }
    assert!(
        covers(&path, Vec2::new(31.0, 2.0)),
        "revealed after the start"
    );
    assert!(!covers(&path, Vec2::new(29.0, 2.0)), "hidden before it");
}

/// A border of half the short side collapses its inner outline to a line or a point: drawn
/// whole it fills the outline, and drawn in part its sector folds onto the collapsed outline.
#[test]
fn a_partial_border_folds_onto_a_collapsed_inner_outline() {
    let wide = bordered(Vec2::new(40.0, 20.0), 0.0, 10.0);
    let whole = drawn(&wide, 1.0).expect("the whole rect");
    for (x, y) in [(20.0, 10.0), (5.0, 3.0), (36.0, 2.0)] {
        assert!(covers(&whole, Vec2::new(x, y)), "the whole rect is border");
    }
    let half = drawn(&wide, 0.5).expect("the right and bottom");
    for (x, y) in [(38.0, 10.0), (37.0, 12.0), (20.0, 18.0)] {
        assert!(covers(&half, Vec2::new(x, y)), "the right and bottom");
    }
    for (x, y) in [(20.0, 2.0), (2.0, 10.0), (5.0, 3.0)] {
        assert!(!covers(&half, Vec2::new(x, y)), "not the top or left");
    }
    let most = drawn(&wide, 0.9).expect("all but the top's right end");
    for (x, y) in [(2.0, 10.0), (20.0, 18.0), (20.0, 3.0), (38.0, 10.0)] {
        assert!(covers(&most, Vec2::new(x, y)), "up to the top's cut");
    }
    for (x, y) in [(36.0, 2.0), (31.0, 8.0)] {
        assert!(!covers(&most, Vec2::new(x, y)), "the top past the cut");
    }

    let pie = drawn(&border(40.0, 20.0, 20.0), 0.25).expect("a quarter disc");
    assert!(covers(&pie, Vec2::new(28.0, 12.0)), "the first quarter");
    for (x, y) in [(12.0, 12.0), (28.0, 28.0)] {
        assert!(!covers(&pie, Vec2::new(x, y)), "not the other quarters");
    }
}

fn distinct(path: &Path<Pixels>) -> usize {
    let mut positions: Vec<_> = path
        .vertices
        .iter()
        .map(|vertex| vertex.xy_position)
        .map(|at| (f32::from(at.x).to_bits(), f32::from(at.y).to_bits()))
        .collect();
    positions.sort_unstable();
    positions.dedup();
    positions.len()
}

/// The largest charged corner, 1,257 chords at radius 20,000 and composed scale 64, gives
/// p = 5,033, a ring of 2p − 2 points and a sector of at most 2p. Widths at and under the f32
/// spacing near 20,000 put the inner outline on or across the outer one, and half the short
/// side collapses it to a point or a line; each builds within one fill's vertex limit.
#[test]
fn ring_borders_build_within_the_fill_limit_at_the_largest_charge() {
    let n = corner_segments(20_000.0 * 64.0);
    assert_eq!(n, 1_257, "the largest charged corner");
    assert_eq!(2 * ((n + 1) * 4 + 1), 10_066, "2p contour points at most");
    let mut scratch = Scratch::new();
    let largest = |height, width| Prim::Rect {
        rect: Rect {
            origin: Vec2::new(-20_000.0, -20_000.0),
            size: Vec2::new(40_000.0, height),
        },
        radius: 20_000.0,
        fill: None,
        border: Some((ink(), width)),
    };
    let charged = Affine::pose(64.0, 0.3, Vec2::new(0.0, 0.0));
    let cases = [
        (40_000.0, 2.0, true),
        (40_000.0, 20_000.0, true),
        (20_000.0, 10_000.0, true),
        (40_000.0, 3e-3, false),
        (40_000.0, 1e-3, false),
    ];
    for (height, width, visible) in cases {
        for draw in [1.0, 0.999, 0.5] {
            let [None, border] =
                geometry_paths(&mut scratch, &largest(height, width), charged, draw, 0.0)
                    .expect("the largest border builds")
            else {
                panic!("at most one border; no background");
            };
            if let Some((path, _)) = border {
                assert!(
                    distinct(&path) <= 21_845,
                    "lyon's vertices stay within one fill's limit"
                );
            } else {
                assert!(!visible, "a visible control border paints");
            }
        }
    }
    let past = Affine::pose(65.0, 0.3, Vec2::new(0.0, 0.0));
    assert_eq!(
        geometry_paths(&mut scratch, &largest(40_000.0, 2.0), past, 1.0, 0.0,).err(),
        Some(BuildError::OverCharge),
        "a scale past the charge isn't coarsened"
    );
}

/// Tessellation sees prim points, so a tiny scale at the canvas edge, where f32 world points
/// collapse into each other, places the same triangles rather than retriangulating them.
#[test]
fn crossing_geometry_is_placed_not_retriangulated() {
    const EDGE: f32 = 20_000.0;
    const ZOOM: f32 = 0.00025;

    let handle = Handle::new(1, 0).expect("a handle");
    let scene = PaintScene::of(handle, &[pentagram()]);
    let fill = |transform| {
        scene
            .fill(handle, 0, transform)
            .expect("the staged fill")
            .expect("the star paints")
    };
    let baseline = fill(Affine::default());
    assert!(
        baseline.vertices.len() >= 15,
        "a star's five points tessellate"
    );
    for (transform, min, width, height) in [
        (
            Affine::pose(1e-4, 0.3, Vec2::new(EDGE, EDGE)),
            px(EDGE),
            px(0.0),
            px(0.001_953_125),
        ),
        (
            Affine([ZOOM, 0.0, 0.0, ZOOM, EDGE, EDGE]),
            px(EDGE.next_down()),
            px(0.003_906_25),
            px(0.003_906_25),
        ),
    ] {
        let placed = fill(transform);
        assert_eq!(
            (
                placed.bounds.origin,
                placed.bounds.size.width,
                placed.bounds.size.height
            ),
            (point(min, min), width, height),
            "the exact bounds at the far canvas edge"
        );
        assert!(
            placed
                .vertices
                .iter()
                .map(|vertex| world(vertex.xy_position))
                .eq(baseline
                    .vertices
                    .iter()
                    .map(|vertex| transform.point(world(vertex.xy_position)))),
            "every cached triangle is placed in order, even when its corners collapse",
        );
    }
}

fn pentagram() -> Prim {
    Prim::Fill {
        points: (0..5_u8)
            .map(|i| {
                let angle = TAU * f32::from(i * 2 % 5) / 5.0;
                Vec2::new(angle.cos(), angle.sin()) * 10.0
            })
            .collect(),
        color: ink(),
    }
}

#[test]
fn a_pentagram_fills_its_centre_nonzero() {
    let handle = Handle::new(1, 0).expect("a handle");
    let path = PaintScene::of(handle, &[pentagram()])
        .fill(handle, 0, Affine::default())
        .expect("the staged fill")
        .expect("the star paints");
    assert!(
        covers(&path, Vec2::new(0.0, 0.0)),
        "the centre pentagon winds twice, so nonzero fills it"
    );
    assert!(covers(&path, Vec2::new(8.0, 0.0)), "a tip winds once");
    assert!(!covers(&path, Vec2::new(9.0, 9.0)), "outside the star");
}

/// The lyon logging child, as libtest names it.
const LYON_LOGGING_CHILD: &str = "presence::paint_paths::tests::lyon_logging_child";

/// Builds today's static fill, the same fill staged and placed by the paint cache, and the rect
/// fallbacks (background, ring, sector) at sentinel coordinates, for a parent that set
/// `LYON_FORCE_LOGGING` and reads everything this prints.
#[test]
#[ignore = "the child process of lyon_logging_is_compiled_out"]
fn lyon_logging_child() {
    let watchdog = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(10));
        std::process::exit(124);
    });
    drop(watchdog);
    const {
        assert!(
            cfg!(debug_assertions),
            "the child runs with the application's debug assertions on"
        );
    }
    let at = |x: f32, y: f32| Vec2::new(4_321.25 + x, 4_321.25 + y);
    let star = Prim::Fill {
        points: (0..5_u8)
            .map(|i| {
                let angle = TAU * f32::from(i * 2 % 5) / 5.0;
                at(angle.cos() * 10.0, angle.sin() * 10.0)
            })
            .collect(),
        color: ink(),
    };
    let rect = Prim::Rect {
        rect: Rect {
            origin: at(0.0, 0.0),
            size: Vec2::new(40.0, 24.0),
        },
        radius: 6.0,
        fill: Some(ink()),
        border: Some((ink(), 4.0)),
    };
    let turned = Affine::pose(1.0, 0.3, Vec2::new(0.0, 0.0));
    let mut scratch = super::super::paint_stroke::Scratch::new();
    let fallback = [1.0, 0.5].map(|draw| {
        super::super::paint_geometry::paths(
            &mut scratch,
            &rect,
            turned,
            draw,
            0.0,
            MAX_COMPOSED_SCALE,
        )
        .map(|paths| paths.into_iter().flatten().count())
    });
    assert_eq!(
        fallback,
        [Ok(2), Ok(2)],
        "transient fallback keeps logging compiled out"
    );
    let built = [
        paths(&star, Affine::default(), 1.0, 0.0, MAX_COMPOSED_SCALE),
        paths(&rect, turned, 1.0, 0.0, MAX_COMPOSED_SCALE),
        paths(&rect, turned, 0.5, 0.0, MAX_COMPOSED_SCALE),
    ]
    .map(|result| result.map(|list| list.len()));
    assert_eq!(
        built,
        [Ok(1), Ok(2), Ok(2)],
        "the star; the background and ring; the background and sector"
    );
    let handle = Handle::new(1, 0).expect("a handle");
    let staged = PaintScene::of(handle, std::slice::from_ref(&star));
    assert!(
        matches!(staged.fill(handle, 0, turned), Ok(Some(_))),
        "the star staged as the paint cache stages a view's fills"
    );
}

/// Whether `line` is libtest's summary for exactly one passed test.
fn one_passed(line: &str) -> bool {
    let Some(rest) =
        line.strip_prefix("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")
    else {
        return false;
    };
    let Some((filtered, time)) = rest.split_once(" filtered out; finished in ") else {
        return false;
    };
    let Some((whole, fraction)) = time.strip_suffix('s').and_then(|time| time.split_once('.'))
    else {
        return false;
    };
    [filtered, whole, fraction]
        .iter()
        .all(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// lyon's debug build prints every point it fills while `LYON_FORCE_LOGGING` is set. The
/// workspace builds `lyon_tessellation` without debug assertions in supported profiles, so a debug
/// child with the variable set writes libtest's own framing and nothing else.
#[test]
fn lyon_logging_is_compiled_out() {
    use std::os::windows::process::CommandExt as _;

    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            LYON_LOGGING_CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LYON_FORCE_LOGGING", "1")
        .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
        .output()
        .expect("the child test process runs");
    assert!(output.status.success(), "the child builds every fill");
    assert!(
        output.stderr.is_empty(),
        "the child wrote {} bytes to stderr",
        output.stderr.len()
    );
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        panic!("the child's stdout isn't UTF-8");
    };
    let name = format!("test {LYON_LOGGING_CHILD} ... ok");
    let expected = ["", "running 1 test", name.as_str(), "", "", "", ""];
    let lines: Vec<&str> = stdout.split('\n').collect();
    let unexpected = lines
        .iter()
        .enumerate()
        .filter(|&(index, line)| {
            if index == 4 {
                !one_passed(line)
            } else {
                expected.get(index) != Some(line)
            }
        })
        .count();
    assert_eq!(
        (unexpected, lines.len()),
        (0, expected.len()),
        "unexpected child stdout lines, and the child's line count"
    );
}
