use crate::state_followups_fixture::{commit, scene, step};
use kumochi_scene::GlobalBudget;

pub(super) fn measure_whole_scene_removal_admit() {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };
    let mut state = scene();
    let mut global = GlobalBudget::default();
    for (name, range) in [("first", 0..300), ("second", 300..600)] {
        let operations = range
            .map(|n| format!(r#"{{"text":{{"id":"t{n}","at":[0,0],"text":""}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        commit(
            &mut state,
            &mut global,
            &format!(r#"{{"id":"{name}","put":[{operations}]}}"#),
        );
    }
    let ids = (0..600)
        .map(|n| format!(r#""t{n}""#))
        .collect::<Vec<_>>()
        .join(",");
    let removal = step(&format!(r#"{{"id":"remove","remove":[{ids}]}}"#));
    assert_eq!(state.object_count(), 600, "the full scene is present");
    for _ in 0..5 {
        black_box(state.admit(&removal, &global).expect("valid full removal"));
    }
    let mut total = Duration::ZERO;
    let mut longest = Duration::ZERO;
    for _ in 0..128 {
        let started = Instant::now();
        let candidate = state.admit(&removal, &global).expect("valid full removal");
        let elapsed = started.elapsed();
        total += elapsed;
        longest = longest.max(elapsed);
        black_box(candidate);
    }
    assert_eq!(
        (state.revision(), state.object_count()),
        (2, 600),
        "admission leaves the scene unchanged"
    );
    eprintln!(
        "admit: objects=600 removals=600 warmups=5 samples=128 mean_us={:.3} max_us={:.3}",
        total.as_secs_f64() * 1_000_000.0 / 128.0,
        longest.as_secs_f64() * 1_000_000.0
    );
}
