use std::f32::consts::{FRAC_PI_4, TAU};

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

use super::super::paint_cache::PaintScene;
use super::{BuildError, Strokes, corners, paths, rounded, scale};

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
    Prim::Line {
        points: points.iter().map(|&(x, y)| Vec2::new(x, y)).collect(),
        closed: false,
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

fn built(prim: &Prim, draw: f32, flow: f32) -> Path<Pixels> {
    let mut built = paths(prim, Affine::default(), draw, flow, MAX_COMPOSED_SCALE)
        .expect("bounded geometry builds");
    assert_eq!(built.len(), 1, "a line builds one merged path");
    built.remove(0).0
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

fn xy(path: &Path<Pixels>) -> Vec<Point<Pixels>> {
    path.vertices
        .iter()
        .map(|vertex| vertex.xy_position)
        .collect()
}

fn assert_same(actual: &Path<Pixels>, expected: &Path<Pixels>) {
    assert_eq!(xy(actual), xy(expected), "the same triangles in order");
    assert_eq!(actual.bounds, expected.bounds, "the same bounds");
}

fn runs(intervals: &[(f32, f32)]) -> Vec<Vec<Point<Pixels>>> {
    intervals
        .iter()
        .map(|&(from, to)| pts(&[(from, 0.0), (to, 0.0)]))
        .collect()
}

#[test]
fn dashes_follow_the_prim_pattern_and_its_flow() {
    let dashed = dashed_line(&[(0.0, 0.0), (128.0, 0.0)]);
    let at_rest: Vec<_> = (0..13_u8)
        .map(|k| (10.0 * f32::from(k), 10.0 * f32::from(k) + 6.0))
        .collect();
    assert_same(
        &built(&dashed, 1.0, 0.0),
        &reference(&runs(&at_rest), Contour::Open),
    );

    let mut moved = vec![(0.0, 1.0)];
    moved.extend((1..13_u8).map(|k| (10.0 * f32::from(k) - 5.0, 10.0 * f32::from(k) + 1.0)));
    moved.push((125.0, 128.0));
    assert_same(
        &built(&dashed, 1.0, 0.5),
        &reference(&runs(&moved), Contour::Open),
    );
    assert_eq!(
        PaintWork::of(std::slice::from_ref(&dashed), MAX_COMPOSED_SCALE).dash_pieces,
        14,
        "half a period of flow paints exactly the charged pieces"
    );

    let mut revealed: Vec<_> = at_rest[..6].to_vec();
    revealed.push((60.0, 64.0));
    assert_same(
        &built(&dashed, 0.5, 0.0),
        &reference(&runs(&revealed), Contour::Open),
    );
}

/// The second line's first dash ends on a vertex, and its gap is exactly the next segment.
#[test]
fn a_dash_crosses_a_vertex_but_never_a_gap() {
    let corner = dashed_line(&[(0.0, 0.0), (4.0, 0.0), (4.0, 8.0)]);
    let expected = [
        pts(&[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0)]),
        pts(&[(4.0, 6.0), (4.0, 8.0)]),
    ];
    assert_same(
        &built(&corner, 1.0, 0.0),
        &reference(&expected, Contour::Open),
    );

    let gap = dashed_line(&[(0.0, 0.0), (6.0, 0.0), (6.0, 4.0), (6.0, 10.0)]);
    let expected = [
        pts(&[(0.0, 0.0), (6.0, 0.0)]),
        pts(&[(6.0, 4.0), (6.0, 10.0)]),
    ];
    assert_same(&built(&gap, 1.0, 0.0), &reference(&expected, Contour::Open));
}

#[test]
fn solid_closed_strokes_close_only_when_whole() {
    let square = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
    let closed = solid_line(&square, Contour::Closed);
    assert_same(
        &built(&closed, 1.0, 0.0),
        &reference(&[pts(&square)], Contour::Closed),
    );
    assert_same(
        &built(&closed, 0.5, 0.0),
        &reference(&[pts(&square[..3])], Contour::Open),
    );
    let drawn = paths(&closed, Affine::default(), 0.0, 0.0, MAX_COMPOSED_SCALE)
        .expect("an unrevealed stroke builds");
    assert_eq!(drawn.len(), 0, "nothing of a stroke is revealed at draw 0");
}

/// 5,457 points cost 4 · 5,457 + 4 = 21,832, and each two-point run costs 12.
#[test]
fn chunks_split_one_over_the_build_limit_and_merge_in_order() {
    assert_eq!(BUILD_VERTEX_LIMIT, 21_845, "u16::MAX / 3");
    let long = pts(&zigzag(5_457, 0.0));
    let short = pts(&[(0.0, 10.0), (4.0, 10.0)]);
    let last = pts(&[(0.0, 20.0), (4.0, 20.0)]);
    let mut strokes = Strokes::new(px(2.0));
    strokes.add(&long, Contour::Open).expect("fits");
    strokes.add(&short, Contour::Open).expect("fits");
    assert_eq!(strokes.bound, 21_844, "the largest bound under the limit");
    assert!(
        strokes.merged.is_none(),
        "21_844 vertices stay in one build"
    );

    strokes.add(&last, Contour::Open).expect("fits");
    assert_eq!(strokes.bound, 12, "one run over starts a new chunk");
    let first = reference(&[long, short], Contour::Open);
    let second = reference(&[last], Contour::Open);
    assert_eq!(
        strokes.merged.as_ref().map(xy),
        Some(xy(&first)),
        "the full chunk built before the run that overflowed it"
    );

    let merged = strokes.finish().expect("both chunks build").expect("ink");
    let mut expected = xy(&first);
    expected.extend(xy(&second));
    assert_eq!(xy(&merged), expected, "chunks concatenate in order");
    assert_eq!(
        merged.bounds,
        first.bounds.union(&second.bounds),
        "bounds union"
    );
}

/// 3,277 two-point runs cost 39,324, so they take two chunks.
#[test]
fn a_long_dashed_line_merges_its_chunks_into_one_build() {
    let dashed = dashed_line(&[(0.0, 0.0), (32_768.0, 0.0)]);
    let intervals: Vec<_> = (0..3_277_u16)
        .map(|k| (10.0 * f32::from(k), 10.0 * f32::from(k) + 6.0))
        .collect();
    assert_same(
        &built(&dashed, 1.0, 0.0),
        &reference(&runs(&intervals), Contour::Open),
    );
}

#[test]
fn one_run_past_the_build_limit_is_refused() {
    let at_limit = solid_line(&zigzag(5_460, 0.0), Contour::Open);
    assert_eq!(
        paths(&at_limit, Affine::default(), 1.0, 0.0, MAX_COMPOSED_SCALE).map(|built| built.len()),
        Ok(1),
        "4 * 5_460 + 4 = 21_844 builds"
    );
    let over = solid_line(&zigzag(5_461, 0.0), Contour::Open);
    assert_eq!(
        paths(&over, Affine::default(), 1.0, 0.0, MAX_COMPOSED_SCALE).map(|built| built.len()),
        Err(BuildError::OverCharge),
        "4 * 5_461 + 4 = 21_848 is past the limit"
    );
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
    let fill = |count| Prim::Fill {
        points: circle(count, 10_000.0).into(),
        color: ink(),
    };
    assert_eq!(
        FILL_VERTEX_LIMIT, 21_845,
        "the fill limit is the build limit"
    );
    assert_eq!(
        paths(
            &fill(21_845),
            Affine::default(),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE
        )
        .map(|built| built.len()),
        Ok(1),
        "a fill at the limit builds"
    );
    assert_eq!(
        paths(
            &fill(21_846),
            Affine::default(),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE
        )
        .map(|built| built.len()),
        Err(BuildError::OverCharge),
        "one point over is refused"
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
fn sagitta(outline: &[Vec2], segments: usize, center: Vec2, radius: f64) -> f64 {
    let angle = |v: Vec2| {
        let [x, y] = [v.x - center.x, v.y - center.y].map(f64::from);
        y.atan2(x)
    };
    let step = outline
        .chunks(segments + 1)
        .flat_map(|corner| corner.windows(2))
        .map(|pair| (angle(pair[1]) - angle(pair[0])).rem_euclid(f64::from(TAU)))
        .fold(0.0, f64::max);
    radius * (1.0 - (step / 2.0).cos())
}

#[test]
fn corners_flatten_from_the_world_radius_without_coarsening() {
    let largest = Rect {
        origin: Vec2::new(-20_000.0, -20_000.0),
        size: Vec2::new(40_000.0, 40_000.0),
    };
    assert_eq!(corner_segments(20_000.0), 158, "the shared corner count");
    let outline = rounded(largest, 20_000.0, 158);
    assert_eq!(outline.len(), 636, "four corners of 159 points");
    let center = Vec2::new(0.0, 0.0);
    assert!(
        sagitta(&outline, 158, center, 20_000.0) <= 0.25,
        "158 chords hold 0.25"
    );
    let coarse = rounded(largest, 20_000.0, 157);
    assert!(
        sagitta(&coarse, 157, center, 20_000.0) > 0.25,
        "157 chords break 0.25"
    );

    let rect = Prim::Rect {
        rect: largest,
        radius: 20_000.0,
        fill: Some(ink()),
        border: Some((ink(), 2.0)),
    };
    assert_eq!(
        paths(&rect, Affine::default(), 0.5, 0.0, MAX_COMPOSED_SCALE).map(|built| built.len()),
        Ok(2),
        "the largest rect's fill and partly drawn border build"
    );

    let square = Rect {
        origin: Vec2::new(0.0, 0.0),
        size: Vec2::new(1_000.0, 1_000.0),
    };
    assert_eq!(corner_segments(0.0), 0, "radius 0 needs no chords");
    let sharp = rounded(square, 0.0, 0);
    let corners = [
        (1_000.0, 0.0),
        (1_000.0, 1_000.0),
        (0.0, 1_000.0),
        (0.0, 0.0),
    ];
    assert_eq!(
        sharp,
        corners.map(|(x, y)| Vec2::new(x, y)),
        "radius 0 is four corners"
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
    assert_eq!(
        [2.0, 2.07, 2.08].map(|zoom: f32| corner_segments(400.0 * f64::from(zoom))),
        [32, 32, 33],
        "2.07 stays within the pieces charged at 2 and 2.08 crosses a subdivision"
    );
    let charge = fallback_corner_segments(400.0, 2.0);
    let mut scratch = super::super::paint_stroke::Scratch::new();
    for (zoom, pieces, built) in [
        (2.0, Ok(32), Ok(2)),
        (2.07, Ok(32), Ok(2)),
        (
            2.08,
            Err(BuildError::OverCharge),
            Err(BuildError::OverCharge),
        ),
    ] {
        // Turned and half drawn, so the painter takes the path fallback, not a native quad.
        let turned = Affine::pose(zoom, 0.5, Vec2::new(0.0, 0.0));
        assert_eq!(
            corners(rect, 400.0, scale(turned), charge).map(|(_, segments)| segments),
            pieces
        );
        assert_eq!(
            super::super::paint_geometry::paths(&mut scratch, &prim, turned, 0.5, 0.0, 2.0)
                .map(|paths| paths.into_iter().flatten().count()),
            built,
            "a pose needing more pieces than charged isn't coarsened to fit"
        );
    }
}

#[test]
fn text_is_refused_and_an_unusable_phase_paints_nothing() {
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
        paths(&text, Affine::default(), 1.0, 0.0, MAX_COMPOSED_SCALE).map(|built| built.len()),
        Err(BuildError::Text),
        "text is painted outside"
    );
    let dashed = dashed_line(&[(0.0, 0.0), (128.0, 0.0)]);
    assert_eq!(
        paths(
            &dashed,
            Affine::default(),
            1.0,
            f32::NAN,
            MAX_COMPOSED_SCALE
        )
        .map(|built| built.len()),
        Ok(0),
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
        paths(
            &empty,
            Affine::pose(65.0, 0.0, Vec2::default()),
            1.0,
            0.0,
            MAX_COMPOSED_SCALE,
        )
    });
    assert_eq!(result.map(|built| built.len()), Ok(0));
    assert_eq!(
        (counts, bytes),
        ([0, 0], [0, 0]),
        "an unpainted rect does no uncharged tessellation work"
    );
}

/// Whether a non-degenerate triangle of the path covers `at`, edges included.
fn covers(path: &Path<Pixels>, at: Vec2) -> bool {
    let side = |p: Vec2, q: Vec2, r: Vec2| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
    path.vertices.chunks(3).any(|triangle| {
        let [a, b, c] = [0, 1, 2].map(|i| world(triangle[i].xy_position));
        let sides = [side(a, b, at), side(b, c, at), side(c, a, at)];
        side(a, b, c).abs() > 1e-6
            && (sides.iter().all(|s| *s >= -1e-3) || sides.iter().all(|s| *s <= 1e-3))
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
    let mut built =
        paths(prim, transform, 1.0, 0.0, MAX_COMPOSED_SCALE).expect("a bordered rect builds");
    assert_eq!(built.len(), 1, "the primitive builds one path");
    built.remove(0).0
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
}

/// A border narrower than half the rect covers the band inside the rect, not the interior.
#[test]
fn a_border_covers_its_inner_band() {
    let path = border_path(&border(40.0, 0.0, 4.0), Affine::default());
    for vertex in &path.vertices {
        let at = world(vertex.xy_position);
        assert!(
            (0.0..=40.0).contains(&at.x) && (0.0..=40.0).contains(&at.y),
            "every vertex stays inside the rect"
        );
    }
    for (x, y) in [(2.0, 20.0), (20.0, 2.0), (38.0, 20.0), (1.0, 1.0)] {
        assert!(covers(&path, Vec2::new(x, y)), "the band is border");
    }
    for (x, y) in [(20.0, 20.0), (5.0, 20.0), (20.0, 35.0), (5.0, 5.0)] {
        assert!(!covers(&path, Vec2::new(x, y)), "the interior isn't");
    }
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

/// A radius under half the border keeps the outer corner round, as the quad's does. The inset
/// stroke this replaced mitred it sharp and covered (0.1, 0.1) at radius 1 and width 4.
#[test]
fn a_narrow_radius_border_keeps_its_outer_corners_round() {
    let path = border_path(&border(40.0, 1.0, 4.0), Affine::default());
    for (x, y) in [(0.1, 0.1), (39.9, 0.1), (39.9, 39.9), (0.1, 39.9)] {
        assert!(
            !covers(&path, Vec2::new(x, y)),
            "outside the rounded corner"
        );
    }
    for (x, y) in [(0.5, 1.0), (1.0, 0.5), (3.0, 3.0)] {
        assert!(covers(&path, Vec2::new(x, y)), "the band at the corner");
    }
    assert_quad_border(Vec2::new(40.0, 40.0), 1.0, 4.0);
    assert_quad_border(Vec2::new(40.0, 24.0), 0.5, 3.0);
}

#[test]
fn a_border_covers_what_its_quad_would() {
    for (radius, width) in [
        (0.0, 4.0),
        (3.0, 4.0),
        (4.0, 4.0),
        (10.0, 4.0),
        (12.0, 12.0),
    ] {
        assert_quad_border(Vec2::new(40.0, 24.0), radius, width);
    }
}

fn drawn(prim: &Prim, draw: f32) -> Option<Path<Pixels>> {
    let mut built = paths(prim, Affine::default(), draw, 0.0, MAX_COMPOSED_SCALE)
        .expect("a bordered rect builds");
    assert!(built.len() <= 1, "a border builds at most one path");
    built.pop().map(|(path, _)| path)
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
    let sector = |draw| xy(&drawn(&square, draw).expect("a revealed sector"));
    assert_eq!(
        sector(0.25),
        xy(&polygon(&[
            (40.0, 0.0),
            (40.0, 40.0),
            (36.0, 36.0),
            (36.0, 4.0)
        ])),
        "the right side, mitred at both ends"
    );
    assert_eq!(
        sector(0.3125),
        xy(&polygon(&[
            (40.0, 0.0),
            (40.0, 40.0),
            (30.0, 40.0),
            (28.0, 36.0),
            (36.0, 36.0),
            (36.0, 4.0),
        ])),
        "a quarter of the bottom edge, paired with a quarter of the inner one"
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
    let n = f32::from(u16::try_from(corner_segments(10.0)).expect("a few chords"));
    let arc = n * 20.0 * (FRAC_PI_4 / n).sin();
    let cut = 10.0 + 0.2 * (80.0 + 4.0 * arc) - arc;
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
    let mut positions: Vec<_> = xy(path)
        .into_iter()
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
        (40_000.0, 2.0),
        (40_000.0, 20_000.0),
        (20_000.0, 10_000.0),
        (40_000.0, 3e-3),
        (40_000.0, 1e-3),
    ];
    for (index, (height, width)) in cases.into_iter().enumerate() {
        for draw in [1.0, 0.999, 0.5] {
            let built = paths(
                &largest(height, width),
                charged,
                draw,
                0.0,
                MAX_COMPOSED_SCALE,
            )
            .expect("the largest border builds");
            if index < 3 {
                assert_eq!(built.len(), 1, "a control border builds one path");
            }
            for (path, _) in &built {
                assert!(
                    distinct(path) <= FILL_VERTEX_LIMIT,
                    "lyon's vertices stay within one fill's limit"
                );
            }
        }
    }
    let past = Affine::pose(65.0, 0.3, Vec2::new(0.0, 0.0));
    assert_eq!(
        paths(&largest(40_000.0, 2.0), past, 1.0, 0.0, MAX_COMPOSED_SCALE).map(|built| built.len()),
        Err(BuildError::OverCharge),
        "a scale past the charge isn't coarsened"
    );
}

/// Tessellation sees prim points, so a tiny scale at the canvas edge, where f32 world points
/// collapse into each other, places the same triangles rather than retriangulating them.
#[test]
fn crossing_geometry_is_placed_not_retriangulated() {
    let star = Prim::Fill {
        points: (0..5_u8)
            .map(|i| {
                let angle = TAU * f32::from(i * 2 % 5) / 5.0;
                Vec2::new(angle.cos(), angle.sin()) * 10.0
            })
            .collect(),
        color: ink(),
    };
    let baseline = border_path(&star, Affine::default());
    assert!(
        baseline.vertices.len() >= 15,
        "a star's five points tessellate"
    );
    let transform = Affine::pose(1e-4, 0.3, Vec2::new(20_000.0, 20_000.0));
    let placed = border_path(&star, transform);
    let expected: Vec<_> = xy(&baseline)
        .into_iter()
        .map(|at| transform.point(world(at)))
        .collect();
    assert_eq!(
        xy(&placed).into_iter().map(world).collect::<Vec<_>>(),
        expected,
        "the baseline triangles, each vertex transformed"
    );
    let (min, max) = expected
        .iter()
        .fold((expected[0], expected[0]), |(min, max), at| {
            (
                Vec2::new(min.x.min(at.x), min.y.min(at.y)),
                Vec2::new(max.x.max(at.x), max.y.max(at.y)),
            )
        });
    let size = placed.bounds.size;
    assert_eq!(
        (world(placed.bounds.origin), size.width, size.height),
        (min, px(max.x - min.x), px(max.y - min.y)),
        "the exact bounds of the transformed vertices"
    );
}

#[test]
fn a_pentagram_fills_its_centre_nonzero() {
    let star = Prim::Fill {
        points: (0..5_u8)
            .map(|i| {
                let angle = TAU * f32::from(i * 2 % 5) / 5.0;
                Vec2::new(angle.cos(), angle.sin()) * 10.0
            })
            .collect(),
        color: ink(),
    };
    let path = border_path(&star, Affine::default());
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
