//! The brain lays every step out with the wide measure before writing it, and refuses one that
//! takes what the shell may paint past the caps.

use std::sync::atomic::Ordering;

use super::*;
use crate::drawing::{PrepareJob, Prepared};

/// A step putting `count` filled circles, `c{from}` onwards, 48 fill vertices each: 42 fit
/// the caps and 43 don't.
fn circles(step: &str, from: usize, count: usize) -> String {
    let put: Vec<String> = (from..from + count)
        .map(|index| {
            let (x, y) = ((index % 30) * 30, 100 + (index / 30) * 30);
            format!(
                r#"{{"shape":{{"id":"c{index}","kind":"ellipse","box":[{x},{y},24,24],"fill":"blue"}}}}"#
            )
        })
        .collect();
    format!(r#"{{"id":"{step}","put":[{}]}}"#, put.join(","))
}

fn too_much() -> Option<CallError> {
    Some(CallError::TooMuchToPaint { cleared: false })
}

/// Each held scene's charge in fill vertices, `None` for an uncounted scene.
fn fills(drawing: &Drawing) -> Vec<Option<usize>> {
    drawing
        .scenes
        .iter()
        .map(|held| {
            held.paint
                .as_ref()
                .map(|paint| paint.charge().fill_vertices)
        })
        .collect()
}

fn job(out: &Output) -> PrepareJob {
    out.effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PrepareDrawing(job) => Some(job.clone()),
            _ => None,
        })
        .expect("a step to lay out")
}

#[test]
fn a_first_step_past_the_caps_writes_nothing_and_keeps_earlier_drawings() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 30)])),
    );
    let out = store.run(
        &mut drawing,
        call(1, "c2", args("s2", &[circles("one", 0, 13)])),
    );
    assert_eq!(trace(&out), ["lookup"]);
    assert_eq!(out.calls[0].error, too_much());
    assert!(out.calls[0].drawn.is_empty());
    assert_eq!(store.scenes.len(), 1, "no begin was written");
    assert_eq!(fills(&drawing), [Some(30 * 48)]);
    let out = store.run(
        &mut drawing,
        call(1, "c3", args("s2", &[circles("one", 0, 12)])),
    );
    assert_eq!(out.calls[0].error, None);
    assert_eq!(fills(&drawing), [Some(30 * 48), Some(12 * 48)]);
}

#[test]
fn a_replacement_is_counted_without_its_victims_and_a_refused_one_keeps_them() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 30)])),
    );
    let out = store.run(
        &mut drawing,
        call(1, "c2", replacement_args("s2", &[circles("one", 0, 43)])),
    );
    assert_eq!(trace(&out), ["lookup"], "no replacement fired");
    assert_eq!(out.calls[0].error, too_much());
    assert_eq!(fills(&drawing), [Some(30 * 48)]);
    let out = store.run(
        &mut drawing,
        call(1, "c3", replacement_args("s3", &[circles("one", 0, 40)])),
    );
    assert_eq!(out.calls[0].error, None);
    assert!(trace(&out).contains(&"replaced 1 by 2".to_owned()));
    assert_eq!(fills(&drawing), [Some(40 * 48)]);
}

#[test]
fn a_step_past_the_caps_in_a_held_scene_leaves_its_picture() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 30)])),
    );
    let out = store.run(
        &mut drawing,
        call(1, "c2", args("s1", &[circles("two", 30, 13)])),
    );
    assert_eq!(trace(&out), ["end 1 false"]);
    assert_eq!(out.calls[0].error, too_much());
    assert_eq!(drawing.scenes[0].state.revision(), 1);
    let out = store.run(
        &mut drawing,
        call(1, "c3", args("s1", &[circles("two", 30, 12)])),
    );
    assert_eq!(out.calls[0].error, None);
    assert_eq!(fills(&drawing), [Some(42 * 48)]);
}

/// The shell holds a step it hasn't shown beside the one it shows, so the brain charges the
/// larger of the two until the shell shows a later one.
#[test]
fn an_unshown_step_stays_charged_until_a_later_one_is_shown() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let removals: Vec<String> = (0..25).map(|index| format!("\"c{index}\"")).collect();
    let drop = format!(r#"{{"id":"two","remove":[{}]}}"#, removals.join(","));
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 30), drop])),
    );
    assert_eq!(fills(&drawing), [Some(30 * 48)]);
    let out = store.run(
        &mut drawing,
        call(1, "c2", args("s2", &[circles("one", 0, 15)])),
    );
    assert_eq!(out.calls[0].error, too_much());
    let shown = Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index: 1,
    };
    store.run(&mut drawing, shown);
    assert_eq!(fills(&drawing), [Some(5 * 48)]);
    let out = store.run(
        &mut drawing,
        call(1, "c3", args("s2", &[circles("one", 0, 15)])),
    );
    assert_eq!(out.calls[0].error, None);
    assert_eq!(fills(&drawing), [Some(5 * 48), Some(15 * 48)]);
}

/// Four scenes, a Stop cutting the first one's unshown step, and another scene's step admitted
/// while that cut is being written: the cut scene keeps its pre-cut charge until its
/// `SceneCutApplied` goes out, and the other step is laid out and charged only after it.
#[test]
fn a_cut_scene_keeps_its_charge_until_its_cut_is_applied() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 20)])),
    );
    let shown = Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index: 0,
    };
    store.run(&mut drawing, shown);
    store.run(
        &mut drawing,
        call(1, "c2", args("s1", &[circles("two", 20, 15)])),
    );
    for n in 2..=4 {
        // Another run's scenes, so the Stop names no unseen scene to cut to nothing.
        let input = call(
            2,
            &format!("c{}", n + 1),
            args(&format!("s{n}"), &[circles("one", 0, 2)]),
        );
        store.run(&mut drawing, input);
    }
    assert_eq!(
        fills(&drawing),
        [Some(35 * 48), Some(96), Some(96), Some(96)]
    );
    let revealed = (0..20)
        .map(|index| handle(&drawing, 1, &format!("c{index}")))
        .collect();
    let mut out = drawing
        .handle(stop(1, 1, cut(1, revealed)))
        .expect("a Stop of the drawing run");
    assert_eq!(trace(&out), ["write cut 1@2:1"]);
    assert_eq!(
        fills(&drawing)[0],
        Some(35 * 48),
        "held until the cut is applied"
    );
    let during = drawing
        .handle(call(3, "d1", args("s3", &[circles("two", 2, 6)])))
        .expect("a later run's call");
    assert!(
        during.effects.is_empty(),
        "the step waits behind the cut's write"
    );
    merge(&mut out, during);
    let out = store.settle(&mut drawing, out);
    assert_eq!(
        trace(&out),
        [
            "write cut 1@2:1",
            "cut 1@2:1",
            "write step 2",
            "step 3#1",
            "end 3 true"
        ]
    );
    assert_eq!(
        fills(&drawing),
        [Some(20 * 48), Some(96), Some(8 * 48), Some(96)]
    );
}

#[test]
fn a_layout_only_the_brain_cannot_place_is_written_uncounted() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let far = format!(
        r#"{{"id":"one","put":[{{"text":{{"id":"t","text":"{}","at":[19900,10]}}}}]}}"#,
        "m".repeat(40)
    );
    let out = store.run(&mut drawing, call(1, "c1", args("s1", &[far])));
    assert_eq!(out.calls[0].error, None);
    assert_eq!(fills(&drawing), [None]);
    let out = store.run(
        &mut drawing,
        call(1, "c2", args("s1", &[circles("two", 0, 43)])),
    );
    assert_eq!(
        out.calls[0].error, None,
        "the shell's guard is its only check"
    );
    assert_eq!(fills(&drawing), [None]);
}

#[test]
fn a_step_being_laid_out_seals_the_reducer_and_a_closed_run_cancels_it() {
    let mut drawing = Drawing {
        defer_preparation: true,
        ..Drawing::default()
    };
    let mut store = Store::default();
    let out = store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 2)])),
    );
    assert_eq!(out.calls[0].error, None, "the store double answers the job");
    let out = drawing
        .handle(call(1, "c2", args("s1", &[circles("two", 2, 2)])))
        .expect("a fresh call");
    let job = job(&out);
    assert!(sent(&out).is_none(), "nothing is written before the layout");
    assert!(drawing.in_flight() && !drawing.settled());
    let written = Input::Written {
        serial: job.serial,
        reply: Ok(Reply::Cut),
    };
    assert!(matches!(drawing.handle(written), Err(Stale)));
    let other = Input::Prepared {
        serial: job.serial + 1,
        outcome: Prepared::Failed,
    };
    assert!(matches!(drawing.handle(other), Err(Stale)));
    let shown = Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index: 0,
    };
    let out = drawing.handle(shown).expect("a shown step");
    assert!(out.effects.is_empty(), "it waits behind the layout");
    let closed = drawing
        .handle(Input::RunClosed { run: RunId(1) })
        .expect("the run closed");
    assert!(job.cancel.load(Ordering::Relaxed));
    assert!(
        closed.calls.is_empty(),
        "the call waits for its layout's answer"
    );
    let out = drawing.handle(job.run()).expect("the pending layout");
    assert_eq!(out.calls[0].error, Some(CallError::Interrupted));
    assert!(sent(&out).is_none(), "a cancelled step is never written");
    assert_eq!(drawing.scenes[0].state.revision(), 1);
    assert!(drawing.settled());
}

#[test]
fn recovered_scenes_are_charged_what_their_layouts_paint() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 30)])),
    );
    store.run(
        &mut drawing,
        call(1, "c2", args("s2", &[circles("one", 0, 12)])),
    );
    let (mut recovered, skipped) = Drawing::recover(store.visible());
    assert!(skipped.is_empty());
    assert_eq!(fills(&recovered), [Some(30 * 48), Some(12 * 48)]);
    let out = store.run(
        &mut recovered,
        call(2, "c3", args("s1", &[circles("two", 30, 1)])),
    );
    assert_eq!(out.calls[0].error, too_much());
}

/// A 36-box flow step and the four-scene text workload, for the Rez measurements.
fn flow(step: &str) -> String {
    let mut put = vec![r#"{"diagram":{"id":"d","dir":"down"}}"#.to_owned()];
    put.extend((0..36).map(|index| {
        format!(r#"{{"node":{{"id":"n{index}","in":"d","label":"Validate the input {index}"}}}}"#)
    }));
    put.extend((1..36).map(|index| {
        format!(
            r#"{{"edge":{{"id":"e{index}","from":"n{}","to":"n{index}","label":"then"}}}}"#,
            index - 1
        )
    }));
    format!(r#"{{"id":"{step}","put":[{}]}}"#, put.join(","))
}

fn spread(mut samples: Vec<u128>) -> String {
    samples.sort_unstable();
    format!(
        "samples={} p50_us={} p95_us={} max_us={}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1]
    )
}

#[test]
#[ignore = "a measurement for a quiet Rez: cargo test -p kumochi-brain --release --lib measure_preparation -- --ignored --nocapture"]
fn measure_preparation_its_cancellation_and_retained_layouts() {
    use std::time::{Duration, Instant};

    let setup = || {
        let mut drawing = Drawing {
            defer_preparation: true,
            ..Drawing::default()
        };
        let mut store = Store::default();
        store.run(
            &mut drawing,
            call(1, "c0", args("s1", &[circles("one", 0, 1)])),
        );
        let out = drawing
            .handle(call(1, "c1", args("s1", &[flow("two")])))
            .expect("a fresh call");
        let job = job(&out);
        (drawing, job)
    };
    let mut prepared = Vec::new();
    for sample in 0..40 {
        let (mut drawing, job) = setup();
        let start = Instant::now();
        let answer = job.run();
        let elapsed = start.elapsed().as_micros();
        let out = drawing.handle(answer).expect("the pending layout");
        assert!(sent(&out).is_some(), "the flow fits the caps");
        if sample >= 8 {
            prepared.push(elapsed);
        }
    }
    let half = {
        let mut sorted = prepared.clone();
        sorted.sort_unstable();
        u64::try_from(sorted[sorted.len() / 2] / 2).expect("a short layout")
    };
    println!("prepare fixture=flow36 thread=caller {}", spread(prepared));
    let (mut stopped, mut cancelled) = (Vec::new(), 0);
    for _ in 0..32 {
        let (mut drawing, job) = setup();
        let (answers, answer) = std::sync::mpsc::channel();
        let worker = job.clone();
        let thread = std::thread::Builder::new()
            .name("drawing-prepare".to_owned())
            .spawn(move || answers.send(worker.run()).expect("the measurement listens"))
            .expect("a preparation thread");
        std::thread::sleep(Duration::from_micros(half));
        let start = Instant::now();
        drawing
            .handle(Input::RunClosed { run: RunId(1) })
            .expect("the run closed");
        let answer = answer.recv().expect("an answer");
        stopped.push(start.elapsed().as_micros());
        thread.join().expect("the preparation thread ends");
        if matches!(
            answer,
            Input::Prepared {
                outcome: Prepared::Cancelled,
                ..
            }
        ) {
            cancelled += 1;
        }
        let out = drawing.handle(answer).expect("the pending layout");
        assert!(sent(&out).is_none(), "a closed run's step is never written");
    }
    println!(
        "stop_to_layout_end fixture=flow36 stop_after_us={half} cancelled={cancelled} {}",
        spread(stopped)
    );
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    for scene in 1..=4_u64 {
        let steps: Vec<String> = (0..64)
            .map(|step| {
                let put: Vec<String> = (0..5)
                    .map(|index| {
                        format!(
                            r#"{{"text":{{"id":"t{step}x{index}","text":"Scene {scene} step {step} label {index}: a sentence of sixty chars","at":[{},{}]}}}}"#,
                            40 + index * 180,
                            20 + step * 9
                        )
                    })
                    .collect();
                format!(r#"{{"id":"s{step}","put":[{}]}}"#, put.join(","))
            })
            .collect();
        let out = store.run(
            &mut drawing,
            call(
                scene,
                &format!("c{scene}"),
                args(&format!("s{scene}"), &steps),
            ),
        );
        assert_eq!(out.calls[0].error, None);
    }
    let retained = |drawing: &Drawing| -> usize {
        drawing
            .scenes
            .iter()
            .filter_map(|held| held.paint.as_ref())
            .map(super::super::prepare::Paint::resident_bytes)
            .sum()
    };
    let unshown = retained(&drawing);
    for scene in 1..=4 {
        let shown = Input::StepShown {
            run: RunId(scene),
            scene: SceneId(scene),
            index: 63,
        };
        store.run(&mut drawing, shown);
    }
    println!(
        "retained_layouts scenes=4 objects={} unshown_steps=256 resident_bytes={unshown} all_shown_resident_bytes={}",
        drawing.global.usage().objects,
        retained(&drawing)
    );
}
