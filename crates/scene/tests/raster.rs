//! Coverage, depth, translucency, ids, antialiasing, bounds and cancellation of the raster.
use kumochi_scene::three::raster::{
    BandScratch, COORD_LIMIT, Fragment, FrameScratch, PushError, Quality, RasterError, ScreenPoly,
    Target, raster,
};
use std::sync::atomic::AtomicBool;

#[path = "support/allocation.rs"]
mod allocation;

const FLAT: [f32; 3] = [0.0, 0.0, 0.5];
const RED: [u8; 3] = [255, 0, 0];
const WHITE: [u8; 3] = [255, 255, 255];

/// Axis-aligned rectangle corners in 1/256 texel.
fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> [[i32; 2]; 4] {
    [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

fn texels(v: i32) -> i32 {
    v * 256
}

fn frag(
    points: &[[i32; 2]],
    plane: [f32; 3],
    rgb: [u8; 3],
    opacity: f32,
    solid: u8,
) -> Fragment<'_> {
    Fragment {
        points,
        plane,
        rgb,
        opacity,
        solid,
        centroid_z: 1.0,
    }
}

fn push(frame: &mut FrameScratch, fragment: &Fragment<'_>) {
    assert_eq!(
        frame.push(fragment),
        Ok(true),
        "the fixture polygon is kept"
    );
}

/// Rasterises into fresh buffers and returns texels as [r, g, b, a] and ids.
fn run(
    frame: &mut FrameScratch,
    quality: Quality,
    width: u32,
    height: u32,
) -> (Vec<[u8; 4]>, Vec<u8>) {
    let texels = usize::try_from(width * height).expect("small fixture");
    let (mut color, mut ids) = (vec![0; texels], vec![0; texels]);
    let mut band = BandScratch::new(width).expect("valid width");
    let target = Target {
        width,
        height,
        color: &mut color,
        ids: &mut ids,
    };
    raster(frame, quality, &mut band, target, &AtomicBool::new(false)).expect("frame rasterises");
    let rgba = color
        .iter()
        .map(|c| {
            let [blue, green, red, alpha] = c.to_le_bytes();
            [red, green, blue, alpha]
        })
        .collect();
    (rgba, ids)
}

#[test]
fn a_screen_polygon_is_84_bytes() {
    assert_eq!(
        std::mem::size_of::<ScreenPoly>(),
        84,
        "the residency ledger charges 84 + 8 per triangle"
    );
}

#[test]
fn shared_edges_cover_every_sample_exactly_once() {
    // An 8 × 8 grid of 8-texel cells split on their diagonals, drawn at opacity 0.5: a sample
    // covered once has alpha 128 and twice 192. The straight grid puts diagonals exactly through
    // sample centres; the jittered one moves interior vertices off the lattice.
    for jitter in [0, 1] {
        let vertex = |i: i32, j: i32| {
            let inner = i > 0 && i < 8 && j > 0 && j < 8;
            let wobble = if inner {
                jitter * ((i * 37 + j * 91) % 255 - 127)
            } else {
                0
            };
            [texels(8 * i) + wobble, texels(8 * j) - wobble / 2]
        };
        for quality in [Quality::Motion, Quality::Rest] {
            let mut frame = FrameScratch::with_capacity(128);
            for i in 0..8 {
                for j in 0..8 {
                    let [a, b, c, d] = [
                        vertex(i, j),
                        vertex(i + 1, j),
                        vertex(i + 1, j + 1),
                        vertex(i, j + 1),
                    ];
                    push(&mut frame, &frag(&[a, b, c], FLAT, WHITE, 0.5, 1));
                    push(&mut frame, &frag(&[a, c, d], FLAT, WHITE, 0.5, 2));
                }
            }
            let (rgba, _) = run(&mut frame, quality, 64, 64);
            assert!(
                rgba.iter().all(|p| p[3] == 128),
                "every texel is covered exactly once ({jitter}, {quality:?})"
            );
        }
    }
}

#[test]
fn samples_on_top_and_left_edges_belong_to_the_polygon() {
    // Edges at x = 0.5 and 2.5 and y = 0.5 and 2.5 pass through texel centres.
    let mut frame = FrameScratch::with_capacity(1);
    push(
        &mut frame,
        &frag(&rect(128, 128, 640, 640), FLAT, RED, 1.0, 1),
    );
    let (rgba, ids) = run(&mut frame, Quality::Motion, 4, 4);
    let covered: Vec<u8> = rgba.iter().map(|p| u8::from(p[3] == 255)).collect();
    assert_eq!(
        covered,
        [1, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        "left and top inclusive"
    );
    assert_eq!(ids, covered, "ids follow coverage");
}

#[test]
fn the_nearer_of_two_intersecting_faces_wins_each_sample() {
    // Reciprocal depths 0.5 + 0.01x and 0.6 - 0.01x cross at x = 5.
    let mut frame = FrameScratch::with_capacity(2);
    let full = rect(0, 0, texels(16), texels(1));
    push(&mut frame, &frag(&full, [0.01, 0.0, 0.5], RED, 1.0, 1));
    push(&mut frame, &frag(&full, [-0.01, 0.0, 0.6], WHITE, 1.0, 2));
    let (_, ids) = run(&mut frame, Quality::Motion, 16, 1);
    assert_eq!(
        ids,
        [2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
        "ids switch at the intersection"
    );
}

#[test]
fn depths_in_one_bin_tie_to_the_earlier_input() {
    let full = rect(0, 0, texels(4), texels(1));
    let depth = f32::from_bits;
    // (first, second, expected id): one bin ties either way; a bin boundary one ulp wide doesn't.
    for (first, second, id) in [
        (0x3f00_0000, 0x3f00_0001, 1),
        (0x3f00_0001, 0x3f00_0000, 1),
        (0x3f00_0000, 0x3f00_0007, 1),
        (0x3f00_0007, 0x3f00_0008, 2),
        (0x3f00_0008, 0x3f00_0007, 1),
    ] {
        let mut frame = FrameScratch::with_capacity(2);
        push(
            &mut frame,
            &frag(&full, [0.0, 0.0, depth(first)], RED, 1.0, 1),
        );
        push(
            &mut frame,
            &frag(&full, [0.0, 0.0, depth(second)], WHITE, 1.0, 2),
        );
        let (_, ids) = run(&mut frame, Quality::Motion, 4, 1);
        assert_eq!(ids, [id; 4], "{first:#x} then {second:#x}");
    }
}

#[test]
fn translucent_layers_blend_back_to_front_over_opaque() {
    let full = rect(0, 0, texels(2), texels(1));
    let behind = rect(texels(1), 0, texels(2), texels(1));
    let mut frame = FrameScratch::with_capacity(5);
    push(&mut frame, &frag(&full, [0.0, 0.0, 0.1], RED, 1.0, 1));
    let layer = |rgb, opacity, solid, centroid_z| Fragment {
        points: &full,
        plane: [0.0, 0.0, 0.5],
        rgb,
        opacity,
        solid,
        centroid_z,
    };
    // Pushed out of order: the sort draws green (z 5), then blue (z 4), then white (z 3).
    push(&mut frame, &layer(WHITE, 0.6, 4, 3.0));
    push(&mut frame, &layer([0, 255, 0], 0.5, 2, 5.0));
    push(&mut frame, &layer([0, 0, 255], 0.25, 3, 4.0));
    // Farther than the opaque face in its texel, so it never shows there.
    push(
        &mut frame,
        &Fragment {
            points: &behind,
            plane: [0.0, 0.0, 0.05],
            rgb: WHITE,
            opacity: 0.9,
            solid: 5,
            centroid_z: 9.0,
        },
    );
    let (rgba, ids) = run(&mut frame, Quality::Motion, 2, 1);
    assert_eq!(
        rgba,
        [[191, 191, 179, 255]; 2],
        "hand-computed source-over of all three layers"
    );
    assert_eq!(ids, [4, 4], "the last layer at 0.5 or more owns the id");
}

#[test]
fn translucent_faces_never_write_depth() {
    // The nearer green face sorts first (centroid 9); the farther blue one still blends over it.
    // This is OSC-218's accepted order, pinned exactly.
    let full = rect(0, 0, texels(1), texels(1));
    let mut frame = FrameScratch::with_capacity(2);
    push(
        &mut frame,
        &Fragment {
            points: &full,
            plane: [0.0, 0.0, 0.9],
            rgb: [0, 255, 0],
            opacity: 0.5,
            solid: 1,
            centroid_z: 9.0,
        },
    );
    push(
        &mut frame,
        &Fragment {
            points: &full,
            plane: [0.0, 0.0, 0.5],
            rgb: [0, 0, 255],
            opacity: 0.25,
            solid: 2,
            centroid_z: 1.0,
        },
    );
    let (rgba, ids) = run(&mut frame, Quality::Motion, 1, 1);
    assert_eq!(
        rgba,
        [[0, 153, 102, 160]],
        "blue over green over clear, unpremultiplied"
    );
    assert_eq!(ids, [1], "only the 0.5 layer picks");
}

#[test]
fn authored_opacity_decides_picking() {
    let full = rect(0, 0, texels(1), texels(1));
    for (opacity, id) in [(0.5, 2), (0.499_99, 1)] {
        let mut frame = FrameScratch::with_capacity(2);
        push(&mut frame, &frag(&full, [0.0, 0.0, 0.1], RED, 1.0, 1));
        push(&mut frame, &frag(&full, [0.0, 0.0, 0.5], WHITE, opacity, 2));
        assert_eq!(
            run(&mut frame, Quality::Motion, 1, 1).1,
            [id],
            "opacity {opacity}"
        );
    }
}

#[test]
fn rest_frames_resolve_exact_sample_coverage() {
    // Texel 1's samples sit at x = 352, 480, 288 and 416 and y = 288, 352, 416 and 480.
    let cases = [
        (rect(0, 0, 352, texels(1)), [255, 0, 0, 64], 0),
        (rect(0, 0, 400, texels(1)), [255, 0, 0, 128], 1),
        (rect(400, 0, texels(3), texels(1)), [255, 0, 0, 128], 0),
    ];
    for (points, texel, id) in cases {
        let mut frame = FrameScratch::with_capacity(1);
        push(&mut frame, &frag(&points, FLAT, RED, 1.0, 1));
        let (rgba, ids) = run(&mut frame, Quality::Rest, 3, 1);
        assert_eq!(
            (rgba[1], ids[1]),
            (texel, id),
            "vertical edge of {points:?}"
        );
    }
    let mut frame = FrameScratch::with_capacity(1);
    push(
        &mut frame,
        &frag(&rect(0, 0, texels(1), 352), FLAT, RED, 1.0, 1),
    );
    let (rgba, _) = run(&mut frame, Quality::Rest, 1, 3);
    assert_eq!(
        rgba[1],
        [255, 0, 0, 64],
        "a bottom edge on a sample excludes it"
    );
}

#[test]
fn a_diagonal_edge_covers_half_of_each_texel_it_crosses() {
    let mut frame = FrameScratch::with_capacity(1);
    push(
        &mut frame,
        &frag(
            &[[0, 0], [texels(8), texels(8)], [0, texels(8)]],
            FLAT,
            RED,
            1.0,
            1,
        ),
    );
    let (rgba, _) = run(&mut frame, Quality::Rest, 8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let expected = match x.cmp(&y) {
                std::cmp::Ordering::Less => 255,
                std::cmp::Ordering::Equal => 128,
                std::cmp::Ordering::Greater => 0,
            };
            assert_eq!(rgba[y * 8 + x][3], expected, "texel ({x}, {y})");
        }
    }
}

#[test]
fn clear_texels_take_their_first_covered_neighbour_colour() {
    let mut frame = FrameScratch::with_capacity(2);
    push(
        &mut frame,
        &frag(&rect(0, 0, texels(1), texels(1)), FLAT, RED, 1.0, 1),
    );
    push(
        &mut frame,
        &frag(
            &rect(texels(2), 0, texels(3), texels(1)),
            FLAT,
            [0, 0, 255],
            1.0,
            2,
        ),
    );
    let (rgba, ids) = run(&mut frame, Quality::Motion, 3, 2);
    assert_eq!(rgba[1], [255, 0, 0, 0], "left wins over right");
    assert_eq!(rgba[3], [255, 0, 0, 0], "up when nothing is beside it");
    assert_eq!(
        rgba[4],
        [0, 0, 0, 0],
        "neighbours that were themselves bled don't count"
    );
    assert_eq!(ids, [1, 0, 2, 0, 0, 0], "bleeding leaves ids alone");
}

#[test]
fn an_empty_frame_is_clear() {
    let (rgba, ids) = run(&mut FrameScratch::with_capacity(0), Quality::Rest, 2, 2);
    assert_eq!((rgba, ids), (vec![[0; 4]; 4], vec![0; 4]), "nothing drawn");
}

#[test]
fn targets_and_bands_outside_one_to_4096_texels_are_refused() {
    assert_eq!(
        BandScratch::new(0).err(),
        Some(RasterError::Size),
        "zero width"
    );
    assert_eq!(
        BandScratch::new(4097).err(),
        Some(RasterError::Size),
        "one over"
    );
    let mut band = BandScratch::new(4096).expect("the limit");
    let mut frame = FrameScratch::with_capacity(0);
    let stop = AtomicBool::new(false);
    for (width, height, len) in [(0, 1, 0), (4096, 4097, 4096 * 4097), (2, 2, 3)] {
        let (mut color, mut ids) = (vec![0; len], vec![0; len]);
        let target = Target {
            width,
            height,
            color: &mut color,
            ids: &mut ids,
        };
        assert_eq!(
            raster(&mut frame, Quality::Motion, &mut band, target, &stop),
            Err(RasterError::Size),
            "{width} × {height} with {len} texels"
        );
    }
    let (mut color, mut ids) = (vec![0; 4096], vec![0; 4096]);
    let target = Target {
        width: 4096,
        height: 1,
        color: &mut color,
        ids: &mut ids,
    };
    assert_eq!(
        raster(&mut frame, Quality::Motion, &mut band, target, &stop),
        Ok(()),
        "4,096 wide"
    );
    let mut narrow = BandScratch::new(2).expect("narrow band");
    let (mut color, mut ids) = (vec![0; 3], vec![0; 3]);
    let target = Target {
        width: 3,
        height: 1,
        color: &mut color,
        ids: &mut ids,
    };
    assert_eq!(
        raster(&mut frame, Quality::Motion, &mut narrow, target, &stop),
        Err(RasterError::Size),
        "wider than its band"
    );
}

#[test]
fn pushes_check_capacity_range_and_numbers() {
    let mut frame = FrameScratch::with_capacity(1);
    let nine = [[0, 0]; 9];
    assert_eq!(
        frame.push(&frag(&nine, FLAT, RED, 1.0, 1)),
        Err(PushError::Full),
        "nine vertices"
    );
    let far = rect(0, 0, COORD_LIMIT + 1, 256);
    assert_eq!(
        frame.push(&frag(&far, FLAT, RED, 1.0, 1)),
        Err(PushError::Invalid),
        "past the guard band"
    );
    let square = rect(0, 0, 256, 256);
    for (opacity, solid) in [(1.5, 1), (f32::NAN, 1), (1.0, 0), (1.0, 161)] {
        assert_eq!(
            frame.push(&frag(&square, FLAT, RED, opacity, solid)),
            Err(PushError::Invalid),
            "{opacity} {solid}"
        );
    }
    for plane in [
        [f32::INFINITY, 0.0, 0.5],
        [0.0, 0.0, -0.5],
        [0.0, -1.0, 0.5],
    ] {
        assert_eq!(
            frame.push(&frag(&square, plane, RED, 1.0, 1)),
            Err(PushError::Numeric),
            "{plane:?}"
        );
    }
    let bad_key = Fragment {
        centroid_z: f32::NAN,
        ..frag(&square, FLAT, RED, 0.5, 1)
    };
    assert_eq!(
        frame.push(&bad_key),
        Err(PushError::Numeric),
        "translucent sort key"
    );
    assert_eq!(
        frame.push(&frag(&square, FLAT, RED, 0.0, 1)),
        Ok(false),
        "fully transparent"
    );
    assert_eq!(
        (frame.len(), frame.discarded()),
        (0, 0),
        "nothing written by refusals"
    );
    assert_eq!(
        frame.push(&frag(&square, FLAT, RED, 1.0, 1)),
        Ok(true),
        "the capacity"
    );
    assert_eq!(
        frame.push(&frag(&square, FLAT, RED, 1.0, 1)),
        Err(PushError::Full),
        "one over"
    );
    assert_eq!(frame.len(), 1, "the refused push wrote nothing");
}

#[test]
fn collapsed_and_reflex_loops_are_discarded_and_counted() {
    let mut frame = FrameScratch::with_capacity(4);
    let point = [[5, 5]; 3];
    let line = [[0, 0], [256, 256], [512, 512]];
    let reflex = [[0, 0], [1024, 0], [256, 256], [0, 1024]];
    for points in [&point[..], &line[..], &reflex[..]] {
        assert_eq!(
            frame.push(&frag(points, FLAT, RED, 1.0, 1)),
            Ok(false),
            "{points:?}"
        );
    }
    assert_eq!(frame.discarded(), 3, "each discard is counted");
    // Duplicate and collinear vertices are removed, keeping the square.
    let messy = [
        [0, 0],
        [0, 0],
        [256, 0],
        [512, 0],
        [512, 512],
        [0, 512],
        [0, 0],
    ];
    push(&mut frame, &frag(&messy, FLAT, RED, 1.0, 1));
    let (rgba, _) = run(&mut frame, Quality::Motion, 3, 3);
    let covered: Vec<u8> = rgba.iter().map(|p| p[3]).collect();
    assert_eq!(
        covered,
        [255, 255, 0, 255, 255, 0, 0, 0, 0],
        "the deduplicated square"
    );
}

#[test]
fn guard_band_extremes_do_not_overflow() {
    let l = COORD_LIMIT;
    let mut frame = FrameScratch::with_capacity(1);
    push(&mut frame, &frag(&rect(-l, -l, l, l), FLAT, RED, 1.0, 1));
    let (rgba, ids) = run(&mut frame, Quality::Rest, 4, 4);
    assert_eq!(
        (rgba, ids),
        (vec![[255, 0, 0, 255]; 16], vec![1; 16]),
        "full cover at the limit"
    );
}

#[test]
fn cancellation_is_seen_at_entry() {
    let mut frame = FrameScratch::with_capacity(2_000);
    for _ in 0..2_000 {
        push(
            &mut frame,
            &frag(&rect(0, 0, texels(4096), texels(32)), FLAT, RED, 1.0, 1),
        );
    }
    let mut band = BandScratch::new(4096).expect("band");
    let (mut color, mut ids) = (vec![0; 4096 * 32], vec![0; 4096 * 32]);
    let target = Target {
        width: 4096,
        height: 32,
        color: &mut color,
        ids: &mut ids,
    };
    let set = AtomicBool::new(true);
    assert_eq!(
        raster(&mut frame, Quality::Rest, &mut band, target, &set),
        Err(RasterError::Cancelled),
        "at entry"
    );
}

#[test]
fn a_warm_frame_allocates_nothing() {
    let mut frame = FrameScratch::with_capacity(4);
    let full = rect(0, 0, texels(64), texels(64));
    push(&mut frame, &frag(&full, FLAT, RED, 1.0, 1));
    push(
        &mut frame,
        &Fragment {
            centroid_z: 2.0,
            ..frag(&full, [0.0, 0.0, 0.6], WHITE, 0.5, 2)
        },
    );
    push(
        &mut frame,
        &Fragment {
            centroid_z: 3.0,
            ..frag(&full, [0.0, 0.0, 0.7], WHITE, 0.3, 3)
        },
    );
    let mut band = BandScratch::new(64).expect("band");
    let (mut color, mut ids) = (vec![0; 64 * 64], vec![0; 64 * 64]);
    let stop = AtomicBool::new(false);
    for warm in [false, true] {
        let target = Target {
            width: 64,
            height: 64,
            color: &mut color,
            ids: &mut ids,
        };
        let (result, allocations) =
            allocation::count(|| raster(&mut frame, Quality::Rest, &mut band, target, &stop));
        assert_eq!(result, Ok(()), "frame rasterises");
        if warm {
            assert_eq!(allocations, 0, "no allocation after warm-up");
        }
    }
}

#[test]
fn row_spans_match_a_pointwise_top_left_oracle() {
    // Random triangles, half snapped to a coarse lattice so edges hit sample positions, against
    // a per-sample test that applies the top-left rule point by point instead of solving spans.
    let mut seed = 0x1234_5678_u32;
    let mut next = move |range: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        i32::try_from(seed % range).expect("small range")
    };
    let offsets = [[96, 32], [224, 96], [32, 160], [160, 224]];
    let mut checked = 0;
    for round in 0..200 {
        let snap = |v: i32| if round % 2 == 0 { v } else { v / 32 * 32 };
        let mut corner = || [snap(next(6000) - 1000), snap(next(6000) - 1000)];
        let triangle = [corner(), corner(), corner()];
        let mut frame = FrameScratch::with_capacity(1);
        if frame.push(&frag(&triangle, FLAT, WHITE, 1.0, 1)) != Ok(true) {
            continue;
        }
        checked += 1;
        let (rgba, _) = run(&mut frame, Quality::Rest, 16, 16);
        let area: i64 = (0..3)
            .map(|i| {
                let (a, b) = (triangle[i], triangle[(i + 1) % 3]);
                i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1])
            })
            .sum();
        let mut ccw = triangle;
        if area < 0 {
            ccw.reverse();
        }
        let inside = |x: i32, y: i32| {
            (0..3).all(|i| {
                let (a, b) = (ccw[i], ccw[(i + 1) % 3]);
                let (dx, dy) = (i64::from(b[0] - a[0]), i64::from(b[1] - a[1]));
                let e = dx * i64::from(y - a[1]) - dy * i64::from(x - a[0]);
                e > 0 || (e == 0 && (dy < 0 || (dy == 0 && dx > 0)))
            })
        };
        for ty in 0..16 {
            for tx in 0..16 {
                let count = offsets
                    .iter()
                    .filter(|o| inside(tx * 256 + o[0], ty * 256 + o[1]))
                    .count();
                let alpha = u8::try_from((255 * count + 2) / 4).expect("byte");
                let at = usize::try_from(ty * 16 + tx).expect("index");
                assert_eq!(
                    rgba[at][3], alpha,
                    "round {round}, texel ({tx}, {ty}), {triangle:?}"
                );
            }
        }
    }
    assert!(
        checked > 150,
        "most random triangles are non-degenerate, {checked} checked"
    );
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "manual Rez measurement with synthetic geometry"
)]
mod measure {
    use super::{AtomicBool, BandScratch, Fragment, FrameScratch, Quality, Target, raster};
    use std::time::Instant;

    fn stats(mut ms: Vec<f64>) -> String {
        ms.sort_by(f64::total_cmp);
        let rank = |p: f64| ms[((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1];
        format!(
            "p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms, n {}",
            rank(0.5),
            rank(0.95),
            ms[ms.len() - 1],
            ms.len()
        )
    }

    /// Synthetic projected triangles: overdraw about 2 on the target, 70% opaque, 30% translucent
    /// (half of those under 0.5), each at its own constant depth.
    fn synthetic(count: usize, width: i32, height: i32, opaque_only: Option<bool>) -> FrameScratch {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let area = 2.0 * f64::from(width * height) / count as f64;
        // An equilateral triangle of circumradius R covers 1.299·R², so the target is covered
        // about twice.
        let radius = (area / 1.299).sqrt() * 256.0;
        let mut frame = FrameScratch::with_capacity(count);
        while frame.len() < count {
            let centre = [
                (next() % (width as u32 * 256)) as i32,
                (next() % (height as u32 * 256)) as i32,
            ];
            let corner = |angle: f64| {
                [
                    centre[0] + (radius * angle.cos()) as i32,
                    centre[1] + (radius * angle.sin()) as i32,
                ]
            };
            let turn = f64::from(next() % 628) / 100.0;
            let points = [corner(turn), corner(turn + 2.1), corner(turn + 4.2)];
            let kind = next() % 10;
            let opaque = kind < 7;
            if opaque_only.is_some_and(|only| only != opaque) {
                continue;
            }
            let opacity = if opaque {
                1.0
            } else if kind < 9 {
                0.3
            } else {
                0.6
            };
            let depth = 0.1 + f32::from(u16::try_from(next() % 800).expect("small")) / 1000.0;
            let solid = u8::try_from(next() % 160 + 1).expect("solid id");
            let fragment = Fragment {
                points: &points,
                plane: [0.0, 0.0, depth],
                rgb: [200, 100, 50],
                opacity,
                solid,
                centroid_z: 1.0 / depth,
            };
            frame.push(&fragment).expect("synthetic polygon fits");
        }
        frame
    }

    #[test]
    #[ignore = "release timing under the Rez quiet lock: --release --exact --ignored --nocapture"]
    fn measure_raster_and_tessellation() {
        let stop = AtomicBool::new(false);
        // A full-overlay view at k = 8, then half-resolution motion of the same view.
        for (width, height, qualities) in [
            (1280_u32, 720_u32, &[Quality::Motion, Quality::Rest][..]),
            (640, 360, &[Quality::Motion][..]),
        ] {
            let mut band = BandScratch::new(width).expect("band");
            let texels = (width * height) as usize;
            let (mut color, mut ids) = (vec![0; texels], vec![0; texels]);
            for count in [2_000, 6_000, 24_000] {
                for (label, only) in [
                    ("mixed", None),
                    ("opaque only", Some(true)),
                    ("translucent only", Some(false)),
                ] {
                    let mut frame = synthetic(count, width as i32, height as i32, only);
                    for &quality in qualities {
                        let mut times = Vec::new();
                        for sample in 0..23 {
                            let target = Target {
                                width,
                                height,
                                color: &mut color,
                                ids: &mut ids,
                            };
                            let start = Instant::now();
                            raster(&mut frame, quality, &mut band, target, &stop).expect("frame");
                            if sample >= 3 {
                                times.push(start.elapsed().as_secs_f64() * 1e3);
                            }
                        }
                        println!(
                            "raster {width}x{height}, {count} triangles {label} ({} kept), {quality:?}: {}",
                            frame.len(),
                            stats(times)
                        );
                    }
                }
            }
        }
        let lathe: kumochi_scene::record::Solid = serde_json::from_value(serde_json::json!({
            "in": "view", "id": "lathe", "shape": "lathe", "segments": 48,
            "points": (0..251).map(|i| [1.0 + f64::from(i % 7) * 0.1, f64::from(i) * 0.05]).collect::<Vec<_>>()
        }))
        .expect("lathe fixture");
        let count = kumochi_scene::three::size::triangles(&lathe).expect("sized");
        let mut out = Vec::with_capacity(count as usize);
        let mut scratch = kumochi_scene::three::mesh::MeshScratch::new();
        let world = kumochi_scene::three::Transform::local([0.0; 3], [0.0; 3], 1.0);
        let mut times = Vec::new();
        for sample in 0..23 {
            out.clear();
            let start = Instant::now();
            kumochi_scene::three::mesh::build(&lathe, &world, 1, &mut out, &stop, &mut scratch)
                .expect("builds");
            if sample >= 3 {
                times.push(start.elapsed().as_secs_f64() * 1e3);
            }
        }
        println!(
            "mesh::build lathe of {count} candidates ({} written): {}",
            out.len(),
            stats(times)
        );
    }
}
