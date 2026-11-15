use std::cell::Cell;
use std::hint::black_box;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{AppContext as _, TestAppContext};
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
fn the_line_s_colours_and_widths_are_the_painter_s() {
    // The brain's painter holds the same literals in crates/brain/src/screen/ink.rs and
    // encode.rs; neither crate sees the other's, so each side pins them.
    assert_eq!(PINK, [255, 20, 147], "the painter's pink");
    assert_eq!(WHITE, [255, 255, 255], "the painter's edge");
    assert_eq!(
        [CORE.to_bits(), EDGE.to_bits()],
        [1.5_f32.to_bits(), 2.5_f32.to_bits()],
        "the painter's half-widths in image pixels"
    );
    assert_eq!(
        IMAGE_LIMIT,
        [1280, 720],
        "what the picture is shrunk to fit"
    );
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

#[gpui::test]
fn the_sampler_keeps_the_recorder_s_points_as_pixel_centres(cx: &mut TestAppContext) {
    let owner = cx.new(|_| ());
    let mut sampler = owner.update(cx, |(), cx| {
        Sampler::start(GestureId(3), recorder(REZ), cx, |(), _| true)
    });
    assert_eq!(sampler.gesture(), GestureId(3));
    assert!(
        !sampler.sample(PhysicalPoint([10, 0])),
        "inside the dead zone"
    );
    assert!(sampler.line().is_empty());
    assert!(
        sampler.sample(PhysicalPoint([30, 40])),
        "50 pixels from where it was"
    );
    let start = [Vec2::new(0.5, 0.5), Vec2::new(30.5, 40.5)];
    assert_eq!(sampler.line(), start);
    assert!(
        !sampler.sample(PhysicalPoint([31, 40])),
        "closer than the spacing"
    );
    assert_eq!(sampler.line(), start);
    assert!(sampler.sample(PhysicalPoint([30, 43])));
    assert_eq!(sampler.line(), [start[0], start[1], Vec2::new(30.5, 43.5)]);
    assert_eq!(
        sampler.finish(),
        PointerPath(vec![
            PhysicalPoint([0, 0]),
            PhysicalPoint([30, 40]),
            PhysicalPoint([30, 43])
        ])
    );
}

#[gpui::test]
fn sampling_past_a_full_path_halves_the_line_with_it_and_allocates_nothing(
    cx: &mut TestAppContext,
) {
    let owner = cx.new(|_| ());
    let mut sampler = owner.update(cx, |(), cx| {
        Sampler::start(GestureId(3), recorder([8192, 100]), cx, |(), _| true)
    });
    let ((), counts) = crate::allocation::activity(|| {
        // The first sample starts the path with where the pointer was, so 2,047 fill it.
        for step in 0..2_047 {
            assert!(sampler.sample(PhysicalPoint([100 + 2 * step, 0])));
        }
        assert_eq!(sampler.line().len(), MAX_PATH_POINTS);
        assert!(sampler.sample(PhysicalPoint([100 + 2 * 2_047, 0])));
    });
    assert_eq!(
        counts,
        [0, 0],
        "the line was reserved when sampling started"
    );
    let line = sampler.line();
    assert_eq!(line.len(), 1_025, "every second point went");
    assert_eq!(
        [line[0], line[1], line[2], line[1_024]],
        [
            Vec2::new(0.5, 0.5),
            Vec2::new(102.5, 0.5),
            Vec2::new(106.5, 0.5),
            Vec2::new(4_194.5, 0.5)
        ],
        "the first and the newest stay"
    );
}

#[gpui::test]
fn the_timer_ticks_every_16_ms_until_told_to_stop_dropped_or_ownerless(cx: &mut TestAppContext) {
    let owner = cx.new(|_| ());
    let ticks = Rc::new(Cell::new(0_u32));
    let start = |limit: u32, cx: &mut TestAppContext| {
        let ticks = Rc::clone(&ticks);
        let sampler = owner.update(cx, |(), cx| {
            Sampler::start(GestureId(1), recorder(REZ), cx, move |(), _| {
                ticks.set(ticks.get() + 1);
                ticks.get() < limit
            })
        });
        // The timer counts from its task's first poll.
        cx.run_until_parked();
        sampler
    };
    let advance = |millis: u64, cx: &mut TestAppContext| {
        cx.dispatcher
            .scheduler()
            .clock()
            .advance(Duration::from_millis(millis));
        cx.run_until_parked();
    };

    let stopped = start(3, cx);
    assert_eq!(
        Rc::strong_count(&ticks),
        2,
        "a running timer holds its tick"
    );
    advance(15, cx);
    assert_eq!(ticks.get(), 0, "nothing before the first 16 ms");
    advance(1, cx);
    assert_eq!(ticks.get(), 1);
    advance(16, cx);
    assert_eq!(ticks.get(), 2);
    advance(16, cx);
    assert_eq!(ticks.get(), 3, "the tick that answers false still ran");
    advance(160, cx);
    assert_eq!(ticks.get(), 3, "and was the last");
    assert_eq!(
        Rc::strong_count(&ticks),
        1,
        "the timer ended and let its tick go"
    );
    drop(stopped);

    let dropped = start(u32::MAX, cx);
    advance(16, cx);
    advance(16, cx);
    assert_eq!(ticks.get(), 5);
    drop(dropped);
    advance(160, cx);
    assert_eq!(ticks.get(), 5, "a dropped sampler ticks no more");
    assert_eq!(Rc::strong_count(&ticks), 1);

    let ownerless = start(u32::MAX, cx);
    drop(owner);
    advance(160, cx);
    assert_eq!(ticks.get(), 5, "no tick without the entity it updates");
    assert_eq!(
        Rc::strong_count(&ticks),
        1,
        "the timer ended, though its sampler is still held"
    );
    drop(ownerless);
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
        for step in 0..2_047 {
            if path.sample(PhysicalPoint([100 + 2 * step, 0])) {
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
