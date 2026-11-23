use std::hint::black_box;
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::PhysicalRect;

use super::*;

/// Rez's display.
const REZ: [u32; 2] = [2560, 1440];

fn recorder(size: [u32; 2]) -> PathRecorder {
    let display = PhysicalRect {
        origin: [0, 0],
        size,
    };
    PathRecorder::new(PhysicalPoint([0, 0]), 1.0, display)
}

/// A path's left, top, right and bottom in logical pixels.
fn sides(path: &Path<Pixels>) -> [f32; 4] {
    let (near, far) = (path.bounds.origin, path.bounds.bottom_right());
    [near.x, near.y, far.x, far.y].map(f32::from)
}

/// A line that turns back on itself at every point: the most vertices a round join takes.
fn zigzag(points: usize) -> Vec<Vec2> {
    (0..points)
        .map(|index| {
            let down = u16::try_from(index).expect("at most a path's points");
            Vec2::new(
                if index % 2 == 0 { 100.5 } else { 400.5 },
                100.5 + f32::from(down),
            )
        })
        .collect()
}

#[test]
fn a_display_s_shrink_follows_the_picture_s_fit_and_never_enlarges() {
    for (display, expected) in [
        (REZ, 2.0_f32),
        ([1280, 720], 1.0),
        ([800, 600], 1.0),
        ([3840, 1080], 3.0),
        ([1280, 1440], 2.0),
        ([7680, 4320], 6.0),
    ] {
        assert_eq!(
            shrink(display).to_bits(),
            expected.to_bits(),
            "a {display:?} display"
        );
    }
}

#[test]
fn the_line_is_a_white_edge_under_a_pink_core_at_the_picture_s_widths() {
    let mut scratch = Scratch::new();
    // On Rez one image pixel is two physical ones: 10 of white under 6 of pink. A 3840 by 2160
    // display at 150% shrinks by three: 15 and 9 physical, which are 10 and 6 logical.
    for (display, scale) in [(REZ, 1.0_f32), ([3840, 2160], 1.5)] {
        let points = [100.5, 200.5].map(|x| Vec2::new(x * scale, 100.5 * scale));
        let [(edge, white), (core, pink)] = strokes(&mut scratch, &points, display, scale)
            .expect("a two-point line builds")
            .expect("a segment draws both strokes");
        assert_eq!(white, rgb(0x00ff_ffff), "the edge is painted first");
        assert_eq!(pink, rgb(0x00ff_1493), "the core is painted over it");
        for (path, half) in [(&edge, 5.0_f32), (&core, 3.0)] {
            let [left, top, right, bottom] = sides(path);
            assert!(
                (top - (100.5 - half)).abs() < 1e-3 && (bottom - (100.5 + half)).abs() < 1e-3,
                "{half} either side of the line at {scale}: {top} to {bottom}"
            );
            // A round end reaches as far as the half-width, less lyon's 0.1 flattening.
            assert!(
                (100.5 - half - 1e-3..100.5 - half + 0.11).contains(&left)
                    && (200.5 + half - 0.11..200.5 + half + 1e-3).contains(&right),
                "round ends {half} past the line's ends at {scale}: {left} to {right}"
            );
        }
    }
}

#[test]
fn a_corner_is_round_like_the_painter_s_capsules() {
    let mut scratch = Scratch::new();
    // A right angle on Rez, where the white edge is 5 logical pixels either side.
    let corner = Vec2::new(200.5, 100.5);
    let points = [Vec2::new(100.5, 100.5), corner, Vec2::new(200.5, 200.5)];
    let [(edge, _), _] = strokes(&mut scratch, &points, REZ, 1.0)
        .expect("the corner builds")
        .expect("two segments draw");
    // Past both segments, on the corner's outer side.
    let outside: Vec<f32> = edge
        .vertices
        .iter()
        .map(|vertex| {
            let at = vertex.xy_position;
            (f32::from(at.x) - corner.x, f32::from(at.y) - corner.y)
        })
        .filter(|(right, down)| *right > 1.0 && *down < -1.0)
        .map(|(right, down)| right.hypot(down))
        .collect();
    assert!(
        !outside.is_empty(),
        "a round join has points along its arc, where a bevelled one has none"
    );
    for reach in outside {
        assert!(
            (4.89..5.001).contains(&reach),
            "the arc stays the half-width from the corner, where a mitre's tip is 7.07 away: {reach}"
        );
    }
}

#[test]
fn nothing_to_draw_builds_nothing_and_allocates_nothing() {
    let mut scratch = Scratch::new();
    let segment = [Vec2::new(10.5, 10.5), Vec2::new(90.5, 10.5)];
    let still = [segment[0], segment[0]];
    for (points, scale) in [
        (&[][..], 1.0_f32),
        (&segment[..1], 1.0),
        (&still[..], 1.0),
        (&segment[..], 0.0),
        (&segment[..], -1.0),
        (&segment[..], f32::NAN),
    ] {
        let (built, counts) =
            crate::allocation::activity(|| strokes(&mut scratch, points, REZ, scale));
        assert!(
            matches!(built, Ok(None)),
            "{} points at scale {scale}",
            points.len()
        );
        assert_eq!(counts, [0, 0], "{} points at scale {scale}", points.len());
    }
}

#[test]
fn the_longest_line_at_an_8k_display_s_width_fits_the_scratch_as_two_handoffs() {
    let mut scratch = Scratch::new();
    let points = zigzag(MAX_PATH_POINTS);
    let (built, counts) =
        crate::allocation::activity(|| strokes(&mut scratch, &points, [7680, 4320], 1.0));
    let strokes = built
        .expect("the scratch holds the longest line's round joins")
        .expect("2,047 segments draw");
    assert_eq!(counts, [2, 0], "one exactly sized path for each stroke");
    for (path, _) in &strokes {
        assert_eq!(path.vertices.len() % 3, 0, "whole triangles");
        assert!(
            path.vertices.len() >= (MAX_PATH_POINTS - 1) * 6,
            "at least two triangles a segment: {}",
            path.vertices.len()
        );
        assert_eq!(
            path.vertices.len(),
            path.vertices.capacity(),
            "sized exactly"
        );
    }
}

#[test]
fn the_line_is_the_recorder_s_points_and_then_the_pointer_as_pixel_centres() {
    let start = Instant::now();
    let after = |millis: u64| start + Duration::from_millis(millis);
    let mut sampler = Sampler::new(GestureId(3), recorder(REZ));
    assert_eq!(sampler.gesture(), GestureId(3));
    assert!(
        !sampler.sample(PhysicalPoint([10, 0]), start),
        "inside the dead zone"
    );
    assert!(sampler.line().is_empty());
    // One time constant on, the eased line has gone 63.2% of the way: 19 of 30 and 25 of 40.
    assert!(
        sampler.sample(PhysicalPoint([30, 40]), after(12)),
        "50 pixels from where it was"
    );
    let kept = [Vec2::new(0.5, 0.5), Vec2::new(19.5, 25.5)];
    assert_eq!(
        sampler.line(),
        [kept[0], kept[1], Vec2::new(30.5, 40.5)],
        "the line reaches the pointer"
    );
    // In no time nothing more is kept, but the line's end goes where the pointer does.
    assert!(sampler.sample(PhysicalPoint([31, 40]), after(12)));
    assert_eq!(sampler.line(), [kept[0], kept[1], Vec2::new(31.5, 40.5)]);
    assert!(
        !sampler.sample(PhysicalPoint([31, 40]), after(12)),
        "a still pointer changes nothing"
    );
    // A second later the eased line is at the pointer, which isn't drawn twice.
    assert!(sampler.sample(PhysicalPoint([31, 40]), after(1_012)));
    assert_eq!(sampler.line(), [kept[0], kept[1], Vec2::new(31.5, 40.5)]);
    assert!(!sampler.sample(PhysicalPoint([31, 40]), after(2_012)));
    assert_eq!(
        sampler.finish(PhysicalPoint([33, 44])),
        PointerPath(vec![
            PhysicalPoint([0, 0]),
            PhysicalPoint([19, 25]),
            PhysicalPoint([31, 40]),
            PhysicalPoint([33, 44])
        ])
    );
}

#[test]
fn sampling_past_a_full_path_thins_the_line_with_it_and_allocates_nothing() {
    let mut sampler = Sampler::new(GestureId(3), recorder([8192, 100]));
    // A second apart, so the eased line is at every sample and the samples are what is kept.
    let start = Instant::now();
    let at = |step: i32| {
        let now = start + Duration::from_secs(1) * step.unsigned_abs();
        (PhysicalPoint([100 + 2 * step, 0]), now)
    };
    let ((), counts) = crate::allocation::activity(|| {
        // The first sample starts the path with where the pointer was, so 2,046 samples
        // leave it one point short of full.
        for step in 0..2_046 {
            let (point, now) = at(step);
            assert!(sampler.sample(point, now));
        }
        assert_eq!(sampler.line().len(), MAX_PATH_POINTS - 1);
        let (point, now) = at(2_046);
        assert!(sampler.sample(point, now));
    });
    assert_eq!(
        counts,
        [0, 0],
        "the line was reserved when sampling started"
    );
    let line = sampler.line();
    assert_eq!(
        line.len(),
        1_025,
        "every second point of a straight line went"
    );
    assert_eq!(
        [line[0], line[1], line[2], line[1_024]],
        [
            Vec2::new(0.5, 0.5),
            Vec2::new(102.5, 0.5),
            Vec2::new(106.5, 0.5),
            Vec2::new(4_192.5, 0.5)
        ],
        "the first and the newest stay"
    );
}

/// p50, p95 and maximum.
fn stats(mut samples: Vec<u128>) -> String {
    samples.sort_unstable();
    format!(
        "{}/{}/{}",
        samples[samples.len() / 2 - 1],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1]
    )
}

/// A line that wanders the way a hand does: `points` points about three pixels apart.
fn wander(points: usize) -> Vec<Vec2> {
    (0..points)
        .map(|index| {
            let along = f32::from(u16::try_from(index).expect("at most a path's points"));
            Vec2::new(
                1280.5 + 900.0 * (along * 0.0031).sin(),
                720.5 + 500.0 * (along * 0.0047).cos(),
            )
        })
        .collect()
}

#[test]
#[ignore = "quiet-Rez timing; run with --exact --ignored alone in its own process"]
fn measure_the_hold_line() {
    const WARMUPS: usize = 8;
    const SAMPLES: usize = 256;
    let mut scratch = Scratch::new();
    for points in [2, 256, MAX_PATH_POINTS] {
        let line = wander(points);
        let mut spent = Vec::new();
        let mut handed = [0, 0];
        for sample in 0..WARMUPS + SAMPLES {
            let start = Instant::now();
            let built = strokes(&mut scratch, black_box(&line), REZ, 1.0)
                .expect("the line builds")
                .expect("the line draws");
            let took = start.elapsed().as_nanos();
            handed = [0, 1].map(|stroke| built[stroke].0.vertices.len());
            black_box(built);
            if sample >= WARMUPS {
                spent.push(took);
            }
        }
        println!(
            "hold_line_strokes points={points} samples={SAMPLES} warmups={WARMUPS} \
             frame_ns_p50/p95/max={} edge_vertices={} core_vertices={} handoff_bytes={}; \
             both paths built, their two allocations included, their frees and GPUI's paint not",
            stats(spent),
            handed[0],
            handed[1],
            (handed[0] + handed[1]) * size_of::<gpui_kit::PathVertex<Pixels>>()
        );
    }
    let mut copies = Vec::new();
    for sample in 0..WARMUPS + SAMPLES {
        let mut path = recorder([8192, 100]);
        let mut line = Vec::with_capacity(MAX_PATH_POINTS + 1);
        let start = Instant::now();
        for step in 0..2_047_u32 {
            let now = start + Duration::from_secs(1) * step;
            if path.sample(PhysicalPoint([100 + 2 * step.cast_signed(), 0]), now) {
                line.clear();
                line.extend(path.points().iter().copied().map(centre));
            }
        }
        let took = start.elapsed().as_nanos();
        black_box(line);
        if sample >= WARMUPS {
            copies.push(took);
        }
    }
    println!(
        "hold_line_samples ticks=2047 samples={SAMPLES} warmups={WARMUPS} \
         batch_ns_p50/p95/max={}; every tick adds a point and copies the line so far",
        stats(copies)
    );
}
