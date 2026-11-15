//! Manual quiet-Rez timing of Kumo's animated frame as the peek drives it, from the shell's side
//! of the crate line: the animator's tick, and one frame's pixels allocated and shaded. No GPUI
//! image is made, so the frame's image wrap is not in these numbers. One test that always runs
//! holds the squares timed here, and the one the pull's own test lays out, to the views' sizes.

use std::hint::black_box;
use std::time::Instant;

use super::kumo::{Animator, CARRY_WIDTH, Look, Mood, Renderer, Silhouette};
use super::kumo_view::SIZE;
use super::pulling::PEEK_SQUARE;

const WARMUPS: usize = 8;
const SAMPLES: usize = 256;
/// Ticks timed as one sample, so a sample is far above the clock's 100 ns step.
const TICKS: usize = 4096;
/// One frame at Rez's 144 Hz.
const DT: f32 = 1.0 / 144.0;
/// The peek's and the carry's squares in device pixels at scale 1.0.
const PEEK: u16 = 64;
const CARRY: u16 = 84;

/// An animator that keeps moving: listening, with the blink that engaging starts.
fn animator() -> Animator {
    let mut animator = Animator::new(Mood::Idle, 1);
    animator.set_mood(Mood::Listening);
    animator.engage();
    animator
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

#[test]
fn the_tests_squares_follow_the_views_sizes() {
    assert!(
        (f32::from(PEEK) - SIZE).abs() < f32::EPSILON,
        "the timed peek square follows kumo_view::SIZE"
    );
    assert!(
        (f32::from(CARRY) - CARRY_WIDTH).abs() < f32::EPSILON,
        "the timed carry square follows CARRY_WIDTH"
    );
    assert!(
        (PEEK_SQUARE - SIZE).abs() < f32::EPSILON,
        "the pull test's peek square follows kumo_view::SIZE"
    );
}

#[test]
#[ignore = "quiet-Rez timing; run with --exact --ignored alone in its own process"]
fn measure_kumo_tick_and_shade() {
    let mut ticking = animator();
    let mut ticks = Vec::new();
    for sample in 0..WARMUPS + SAMPLES {
        let start = Instant::now();
        for _ in 0..TICKS {
            black_box(ticking.tick(DT, 0.0));
        }
        let spent = start.elapsed().as_nanos();
        if sample >= WARMUPS {
            ticks.push(spent);
        }
    }
    println!(
        "kumo_tick ticks={TICKS} samples={SAMPLES} warmups={WARMUPS} batch_ns_p50/p95/max={}",
        stats(ticks)
    );

    for (name, size) in [("peek", PEEK), ("carry", CARRY)] {
        let renderer = Renderer::new(&Silhouette::emoji(), size, Look::default());
        let mut animator = animator();
        let mut shades = Vec::new();
        for sample in 0..WARMUPS + SAMPLES {
            let frame = animator.tick(DT, 0.0);
            let start = Instant::now();
            let mut pixels = vec![0; renderer.image_len()];
            renderer
                .render(&frame, &mut pixels)
                .expect("the buffer has the renderer's size");
            let spent = start.elapsed().as_nanos();
            black_box(pixels);
            if sample >= WARMUPS {
                shades.push(spent);
            }
        }
        println!(
            "kumo_shade square={name} px={size} samples={SAMPLES} warmups={WARMUPS} \
             frame_ns_p50/p95/max={}; one frame: its pixels allocated and shaded, no image wrap",
            stats(shades)
        );
    }
}
