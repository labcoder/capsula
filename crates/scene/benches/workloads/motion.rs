use crate::{
    allocation,
    motion_fixture::{Fixture, four_views, gliding_scene, mixed_track, sample},
    prepared,
};
use kumochi_scene::{Frame, GlobalBudget};
use serde_json::json;

pub(super) fn frame_four_views_max() {
    use std::{hint::black_box, time::Instant};
    for fixture in [Fixture::Control, Fixture::Glide] {
        let mixed = matches!(fixture, Fixture::Glide);
        let views = four_views(fixture);
        let mut frames: Vec<_> = (0..4).map(|_| Frame::new()).collect();
        for early in [true, false] {
            for tick in 0..64 {
                let now = if early { 100 } else { tick * 16 };
                for (view, frame) in views.iter().zip(&mut frames) {
                    view.frame(now, frame).expect("warmup frame");
                }
            }
            let mut samples = Vec::with_capacity(1024);
            let ((), [allocations, frees]) = allocation::activity(|| {
                for tick in 0..1024 {
                    let now = if early { 100 } else { (tick + 64) * 16 };
                    let start = Instant::now();
                    for (view, frame) in views.iter().zip(&mut frames) {
                        view.frame(black_box(now), black_box(frame))
                            .expect("timed frame");
                        if mixed {
                            black_box(view.animating(now));
                        }
                    }
                    samples.push(start.elapsed().as_secs_f64() * 1e6);
                }
            });
            assert_eq!(
                allocations, 0,
                "all four views allocate nothing while sampling"
            );
            assert_eq!(frees, 0, "all four views free nothing while sampling");
            let mut sorted = samples.clone();
            sorted.sort_by(f64::total_cmp);
            println!(
                "workload={} samples=1024 allocations={allocations} frees={frees} p50_us={} p95_us={} p99_us={} max_us={}",
                match (mixed, early) {
                    (false, true) => "early-all-entrances",
                    (false, false) => "long-window",
                    (true, true) => "glide-property-mix-early",
                    (true, false) => "glide-property-mix-long",
                },
                sorted[511],
                sorted[972],
                sorted[1013],
                sorted[1023]
            );
            for (index, sample) in samples.into_iter().enumerate() {
                println!("sample {index} {sample:.3}");
            }
            assert_eq!(
                frames
                    .iter()
                    .map(|frame| frame.items().len())
                    .sum::<usize>(),
                if mixed { 750 } else { 1488 },
                "empty diagram items are omitted"
            );
        }
    }
}

pub(super) fn activation_at_caps() {
    use std::{hint::black_box, time::Instant};
    let mut samples = Vec::with_capacity(64);
    let mut allocations = Vec::with_capacity(64);
    for iteration in 0..69 {
        let mut budget = GlobalBudget::default();
        let (mut scene, mut view) = gliding_scene("activation", 600, &mut budget);
        let put: Vec<_> = (0..256)
            .map(|index| mixed_track(index, 2, Some(index % 200)))
            .collect();
        let step = prepared(
            &mut scene,
            &mut budget,
            view.layout(),
            json!({"id":"activate","say":vec!["a";200].join(" "),"put":put}),
        );
        let mut frame = sample(&view, 100);
        let start = Instant::now();
        let ((), count) = allocation::count(|| {
            view.activate(black_box(step), 100, 72_728, &mut frame)
                .expect("caps activation");
        });
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        if iteration >= 5 {
            samples.push(elapsed);
            allocations.push(count);
        }
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "workload=activation-600-objects-256-replacement-tracks-200-words samples=64 p50_us={} p95_us={} p99_us={} max_us={}",
        sorted[31], sorted[60], sorted[63], sorted[63]
    );
    for (index, (sample, allocations)) in samples.into_iter().zip(allocations).enumerate() {
        println!("sample {index} {sample:.3} allocations={allocations}");
    }
}
