use kumochi_protocol::message::{
    ControlToBrain, MAX_STEP_LABEL, MAX_STEPS, MAX_VISIBLE_SCENES, RunId, SceneDescriptor, SceneId,
    Step,
};

use super::{Broken, Chunk, Drawing, Effect};

/// Every snapshot frame in these tests is this size on the pipe.
const BYTES: u32 = 10;

fn named(scene: u64, revision: u32, committed: u32) -> SceneDescriptor {
    SceneDescriptor {
        scene: SceneId(scene),
        revision,
        committed,
    }
}

/// A chunk that isn't its scene's last.
fn chunk(scene: u64, revision: u32, labels: &[&str]) -> Chunk {
    Chunk {
        scene: SceneId(scene),
        revision,
        steps: labels
            .iter()
            .map(|label| Step {
                label: (*label).to_owned(),
            })
            .collect(),
        last: false,
        bytes: BYTES,
    }
}

fn last(scene: u64, revision: u32, labels: &[&str]) -> Chunk {
    Chunk {
        last: true,
        ..chunk(scene, revision, labels)
    }
}

fn clear_of(ids: &[u64]) -> ControlToBrain {
    ControlToBrain::ClearDrawing {
        scene_ids: ids.iter().copied().map(SceneId).collect(),
    }
}

/// The shown scenes and their labels, oldest first.
fn shown(drawing: &Drawing) -> Vec<(u64, Vec<&str>)> {
    drawing
        .shown()
        .map(|(id, scene)| {
            let labels = scene.steps.iter().map(|(_, step)| step.label.as_str());
            (id.0, labels.collect())
        })
        .collect()
}

/// A live step `label` at `index` of `scene`, drawn by run `scene`.
fn live(drawing: &mut Drawing, scene: u64, index: u32, label: &str) {
    let step = Step {
        label: label.to_owned(),
    };
    drawing.step(RunId(scene), SceneId(scene), index, step);
}

/// Connected as `generation`, with the scenes recovery found already applied.
fn received(generation: u64, scenes: &[SceneDescriptor]) -> Drawing {
    let mut drawing = Drawing::default();
    drawing.connected(generation);
    drawing.apply_scenes(scenes).expect("valid scenes");
    drawing
}

#[test]
fn chunks_that_arrive_before_scenes_are_held_then_install_whole() {
    let mut drawing = Drawing::default();
    drawing.connected(1);
    assert_eq!(drawing.apply_snapshot(1, chunk(7, 4, &["a", "b"])), Ok(0));
    assert_eq!(drawing.apply_snapshot(1, last(7, 4, &["c"])), Ok(0));
    assert!(shown(&drawing).is_empty(), "nothing installs before Scenes");
    assert_eq!(
        drawing.apply_scenes(&[named(7, 4, 3)]),
        Ok(Effect {
            clear: None,
            credit: 2 * BYTES
        }),
        "each held frame is credited once, when Scenes classifies it"
    );
    assert_eq!(shown(&drawing), [(7, vec!["a", "b", "c"])]);
    let indices: Vec<u32> = drawing
        .shown()
        .flat_map(|(_, scene)| scene.steps.iter().map(|(index, _)| *index))
        .collect();
    assert_eq!(indices, [0, 1, 2]);
    assert!(
        drawing.revealed(RunId(7)).is_empty(),
        "a restored scene belongs to no run"
    );
}

#[test]
fn a_clear_before_scenes_suppresses_a_scene_never_shown() {
    let mut drawing = Drawing::default();
    drawing.connected(1);
    assert_eq!(drawing.apply_snapshot(1, last(9, 1, &["x"])), Ok(0));
    assert!(drawing.active(), "recovery is awaiting");
    assert_eq!(
        drawing.clear(),
        Effect {
            clear: None,
            credit: BYTES
        },
        "the held chunk is dropped and credited; the ids aren't known yet"
    );
    assert!(!drawing.active(), "Esc changes the shell's state at once");
    assert_eq!(
        drawing.apply_snapshot(1, last(9, 1, &["x"])),
        Ok(BYTES),
        "a later chunk is dropped too"
    );
    assert_eq!(
        drawing.apply_scenes(&[named(9, 1, 1)]),
        Ok(Effect {
            clear: Some(clear_of(&[9])),
            credit: 0
        })
    );
    assert_eq!(drawing.apply_snapshot(1, last(9, 1, &["x"])), Ok(BYTES));
    assert!(shown(&drawing).is_empty());
    assert!(!drawing.active());
}

#[test]
fn an_old_session_s_last_chunk_never_touches_the_new_recovery() {
    let mut drawing = received(1, &[named(7, 3, 3)]);
    assert_eq!(
        drawing.apply_snapshot(1, chunk(7, 3, &["a", "b"])),
        Ok(BYTES)
    );
    drawing.disconnected();
    drawing.connected(2);
    drawing.apply_scenes(&[named(7, 3, 3)]).expect("valid");
    assert_eq!(
        drawing.apply_snapshot(1, last(7, 3, &["c"])),
        Ok(0),
        "dropped, and never credited through the new session"
    );
    assert!(shown(&drawing).is_empty());
    assert_eq!(
        drawing.apply_snapshot(2, last(7, 3, &["a", "b", "c"])),
        Ok(BYTES)
    );
    assert_eq!(shown(&drawing), [(7, vec!["a", "b", "c"])]);
}

#[test]
fn a_rejected_frame_from_an_older_session_is_never_granted() {
    let mut drawing = received(1, &[named(7, 1, 1)]);
    drawing.disconnected();
    drawing.connected(2);
    let long = "x".repeat(MAX_STEP_LABEL + 1);
    assert_eq!(drawing.apply_snapshot(1, last(7, 1, &[&long])), Ok(0));
    assert_eq!(
        drawing.apply_snapshot(2, last(7, 1, &[&long])),
        Err(Broken::Label(MAX_STEP_LABEL + 1))
    );
}

#[test]
fn a_disjoint_reconnect_with_a_clear_leaves_nothing_stale_and_clears_at_most_four() {
    let new_four = [
        named(5, 1, 1),
        named(6, 1, 1),
        named(7, 1, 1),
        named(8, 1, 1),
    ];
    for clear_while_awaiting in [false, true] {
        let mut drawing = received(1, &[]);
        for scene in 1..=4 {
            live(&mut drawing, scene, 0, "old");
        }
        drawing.disconnected();
        drawing.connected(2);
        let clear = if clear_while_awaiting {
            let effect = drawing.clear();
            assert_eq!(effect, Effect::default(), "the ids aren't known yet");
            drawing.apply_scenes(&new_four).expect("valid").clear
        } else {
            assert!(
                drawing
                    .apply_scenes(&new_four)
                    .expect("valid")
                    .clear
                    .is_none()
            );
            assert!(
                shown(&drawing).is_empty(),
                "scenes Scenes doesn't name go at once"
            );
            assert_eq!(drawing.apply_snapshot(2, last(5, 1, &["new"])), Ok(BYTES));
            assert_eq!(shown(&drawing), [(5, vec!["new"])]);
            drawing.clear().clear
        };
        assert_eq!(
            clear,
            Some(clear_of(&[5, 6, 7, 8])),
            "never the old ids too"
        );
        assert_eq!(drawing.apply_snapshot(2, last(6, 1, &["new"])), Ok(BYTES));
        assert!(shown(&drawing).is_empty());
        assert!(!drawing.active());
    }
}

#[test]
fn a_same_revision_with_fewer_steps_kept_replaces_the_shown_scene() {
    let mut drawing = received(1, &[]);
    for index in 0..3 {
        live(&mut drawing, 3, index, &format!("step {index}"));
    }
    drawing.disconnected();
    drawing.connected(2);
    drawing.apply_scenes(&[named(3, 3, 2)]).expect("valid");
    assert_eq!(
        shown(&drawing),
        [(3, vec!["step 0", "step 1", "step 2"])],
        "a named scene stays until its snapshot installs"
    );
    drawing
        .apply_snapshot(2, last(3, 3, &["step 0", "step 1"]))
        .expect("valid");
    assert_eq!(shown(&drawing), [(3, vec!["step 0", "step 1"])]);
}

#[test]
fn a_clear_while_receiving_never_clears_a_later_scene() {
    let mut drawing = received(1, &[named(1, 2, 2)]);
    drawing
        .apply_snapshot(1, chunk(1, 2, &["a"]))
        .expect("valid");
    assert_eq!(drawing.clear().clear, Some(clear_of(&[1])));
    assert_eq!(drawing.apply_snapshot(1, last(1, 2, &["b"])), Ok(BYTES));
    live(&mut drawing, 2, 0, "b");
    drawing.disconnected();
    drawing.connected(2);
    assert_eq!(
        drawing.apply_scenes(&[named(2, 1, 1)]),
        Ok(Effect::default()),
        "no clear for a scene made after the Esc"
    );
    drawing
        .apply_snapshot(2, last(2, 1, &["b"]))
        .expect("valid");
    assert_eq!(shown(&drawing), [(2, vec!["b"])]);
}

#[test]
fn an_awaiting_clear_survives_losses_and_clears_exactly_what_the_next_scenes_names() {
    let mut drawing = received(1, &[]);
    live(&mut drawing, 2, 0, "shown");
    drawing.disconnected();
    drawing.connected(2);
    assert_eq!(drawing.clear(), Effect::default());
    assert!(
        shown(&drawing).is_empty(),
        "Esc changes the shell's state at once"
    );
    // A second loss within the repeat window: the link waits for the user's Retry.
    drawing.disconnected();
    drawing.connected(3);
    drawing.disconnected();
    drawing.connected(4);
    assert_eq!(
        drawing.apply_scenes(&[named(2, 1, 1), named(5, 2, 2)]),
        Ok(Effect {
            clear: Some(clear_of(&[2, 5])),
            credit: 0
        })
    );
    assert!(!drawing.active());
    live(&mut drawing, 6, 0, "later");
    drawing.disconnected();
    drawing.connected(5);
    assert_eq!(
        drawing.apply_scenes(&[named(6, 1, 1)]),
        Ok(Effect::default()),
        "the intent was used up"
    );
    assert_eq!(shown(&drawing), [(6, vec!["later"])]);
}

#[test]
fn a_cleared_id_scenes_names_again_is_resent_and_never_shown() {
    let mut drawing = received(1, &[]);
    live(&mut drawing, 4, 0, "cleared offline");
    drawing.disconnected();
    assert_eq!(
        drawing.clear().clear,
        Some(clear_of(&[4])),
        "its send fails offline"
    );
    drawing.connected(2);
    assert_eq!(
        drawing
            .apply_scenes(&[named(4, 1, 1)])
            .expect("valid")
            .clear,
        Some(clear_of(&[4]))
    );
    assert_eq!(drawing.apply_snapshot(2, last(4, 1, &["x"])), Ok(BYTES));
    assert!(shown(&drawing).is_empty());
    live(&mut drawing, 4, 1, "late");
    assert!(
        shown(&drawing).is_empty(),
        "a cleared scene's late steps are dropped"
    );
}

#[test]
fn a_live_scene_admits_the_newest_four() {
    let mut drawing = received(1, &[]);
    for scene in 1..=5 {
        live(&mut drawing, scene, 0, "s");
    }
    let ids: Vec<u64> = shown(&drawing).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, [2, 3, 4, 5]);
    live(&mut drawing, 1, 1, "older than all four");
    let ids: Vec<u64> = shown(&drawing).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, [2, 3, 4, 5]);
    assert_eq!(drawing.clear().clear, Some(clear_of(&[2, 3, 4, 5])));
}

#[test]
fn a_live_scene_evicts_one_still_recovering() {
    let four = [
        named(1, 1, 1),
        named(2, 1, 1),
        named(3, 1, 1),
        named(4, 1, 1),
    ];
    let mut drawing = received(1, &four);
    live(&mut drawing, 5, 0, "new");
    assert_eq!(drawing.apply_snapshot(1, last(1, 1, &["x"])), Ok(BYTES));
    assert_eq!(drawing.clear().clear, Some(clear_of(&[2, 3, 4, 5])));
}

#[test]
fn scenes_and_chunks_that_break_the_protocol_end_the_connection() {
    let mut awaiting = Drawing::default();
    awaiting.connected(1);
    let five: Vec<_> = (1..=5).map(|scene| named(scene, 1, 1)).collect();
    assert_eq!(awaiting.apply_scenes(&five), Err(Broken::Scenes(5)));
    assert_eq!(
        awaiting.apply_scenes(&[named(1, 1, 1), named(1, 1, 1)]),
        Err(Broken::Scenes(2))
    );
    let max = u32::try_from(MAX_STEPS).expect("small");
    for (revision, committed) in [(1, 2), (max + 1, 1), (u32::MAX, u32::MAX), (0, u32::MAX)] {
        assert_eq!(
            awaiting.apply_scenes(&[named(1, revision, committed)]),
            Err(Broken::Counts),
            "revision {revision}, committed {committed}"
        );
    }
    let sentinel = "zqxsentinellabel";
    let long = sentinel.repeat(MAX_STEP_LABEL / sentinel.len() + 1);
    let rejected = awaiting
        .apply_snapshot(1, last(1, 1, &[&long]))
        .expect_err("checked even before Scenes");
    assert_eq!(rejected, Broken::Label(long.len()));
    for shown in [rejected.to_string(), format!("{rejected:?}")] {
        assert!(!shown.contains(sentinel), "{shown}");
    }
    let mut receiving = received(1, &[named(1, 3, 2)]);
    assert_eq!(
        receiving.apply_snapshot(1, chunk(1, 3, &["a", "b", "c"])),
        Err(Broken::Overrun(2))
    );
    assert!(shown(&receiving).is_empty(), "nothing was kept");
}

#[test]
fn the_exact_limits_are_accepted() {
    let max = u32::try_from(MAX_STEPS).expect("small");
    let four: Vec<_> = (1..=4).map(|scene| named(scene, max, max)).collect();
    assert_eq!(four.len(), MAX_VISIBLE_SCENES);
    let mut drawing = received(1, &four);
    let label = "x".repeat(MAX_STEP_LABEL);
    let labels = vec![label.as_str(); MAX_STEPS];
    for scene in 1..=4 {
        assert_eq!(
            drawing.apply_snapshot(1, last(scene, max, &labels)),
            Ok(BYTES)
        );
    }
    let counts: Vec<usize> = shown(&drawing)
        .iter()
        .map(|(_, labels)| labels.len())
        .collect();
    assert_eq!(counts, [MAX_STEPS; 4]);
}

#[test]
fn a_snapshot_bigger_than_the_credit_window_completes_with_credit_per_chunk() {
    let window = 3 * BYTES;
    let mut drawing = received(1, &[named(1, 8, 8)]);
    let mut credit = window;
    for index in 0..8 {
        credit -= BYTES;
        credit += drawing
            .apply_snapshot(
                1,
                Chunk {
                    last: index == 7,
                    ..chunk(1, 8, &["s"])
                },
            )
            .expect("valid");
        assert_eq!(
            credit, window,
            "chunk {index} is credited as it's classified"
        );
    }
    assert_eq!(shown(&drawing), [(1, vec!["s"; 8])]);
}

#[test]
fn a_mismatched_or_short_snapshot_never_installs() {
    let mut drawing = received(1, &[named(1, 2, 2), named(2, 2, 2)]);
    drawing
        .apply_snapshot(1, last(1, 1, &["a", "b"]))
        .expect("valid");
    drawing
        .apply_snapshot(1, last(2, 2, &["a"]))
        .expect("valid");
    assert!(shown(&drawing).is_empty());
    assert!(!drawing.active(), "recovery is over");
}

#[test]
fn a_wrong_revision_chunk_spoils_its_snapshot_live_or_held() {
    let mut drawing = received(1, &[named(1, 2, 2)]);
    assert_eq!(drawing.apply_snapshot(1, chunk(1, 1, &["old"])), Ok(BYTES));
    assert_eq!(drawing.apply_snapshot(1, last(1, 2, &["new"])), Ok(BYTES));
    assert!(
        shown(&drawing).is_empty(),
        "a live wrong chunk never installs"
    );
    assert!(!drawing.active(), "recovery is over");

    let mut drawing = Drawing::default();
    drawing.connected(1);
    assert_eq!(drawing.apply_snapshot(1, chunk(1, 1, &["old"])), Ok(0));
    assert_eq!(drawing.apply_snapshot(1, last(1, 2, &["new"])), Ok(0));
    assert_eq!(
        drawing.apply_scenes(&[named(1, 2, 2)]),
        Ok(Effect {
            clear: None,
            credit: 2 * BYTES
        })
    );
    assert!(
        shown(&drawing).is_empty(),
        "a held wrong chunk never installs"
    );
    assert!(!drawing.active(), "recovery is over");
}

/// The per-step cost of a live step for a scene already shown, with four shown: the reducer
/// work every placeholder `StepCommit` pays, not typed v1 admission. Build release tests below
/// normal priority before acquiring the measuring lock. After 30 seconds without cargo/rustc,
/// run the built test binary at normal priority with `--ignored --exact --nocapture
/// drawing::tests::measure_live_step_admission --test-threads=1` while holding the lock.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
fn measure_live_step_admission() {
    const CALLS: u32 = 1_000_000;
    let mut drawing = received(1, &[]);
    for scene in 1..=4 {
        live(&mut drawing, scene, 0, "");
    }
    let mut samples: Vec<f64> = (0..7)
        .map(|_| {
            let started = std::time::Instant::now();
            for index in 0..CALLS {
                let step = Step {
                    label: String::new(),
                };
                drawing.step(RunId(4), SceneId(4), index, step);
            }
            started.elapsed().as_secs_f64() * 1e9 / f64::from(CALLS)
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    println!(
        "live step admission: median {:.1} ns, min {:.1} ns, max {:.1} ns per step",
        samples[3], samples[0], samples[6]
    );
}
