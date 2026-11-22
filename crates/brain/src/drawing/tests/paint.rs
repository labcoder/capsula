//! The brain lays every step out with the wide measure before writing it, and refuses one that
//! takes what the shell may paint past the caps.

use std::sync::atomic::Ordering;

use super::*;
use crate::drawing::prepare::{MAX_RETAINED_BYTES, Paint};
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

const TOO_MUCH: Option<CallError> = Some(CallError::TooMuchToPaint { cleared: false });

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
    assert_eq!(out.calls[0].error, TOO_MUCH);
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
    assert_eq!(out.calls[0].error, TOO_MUCH);
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
    assert_eq!(out.calls[0].error, TOO_MUCH);
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
    assert_eq!(out.calls[0].error, TOO_MUCH);
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
    for (call_id, record, index, id, result) in [
        ("c1", far, 0, "one", "drawn: [one]"),
        ("c2", circles("two", 0, 43), 1, "two", "drawn: [two]"),
    ] {
        let out = store.run(&mut drawing, call(1, call_id, args("s1", &[record])));
        let [ended] = out.calls.as_slice() else {
            panic!("one result for {call_id}");
        };
        assert_eq!(ended.tool_text(), result);
        let forwarded: Vec<_> = out
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Data(DataToShell::SceneStepCommit {
                    scene, index, step, ..
                }) => Some((*scene, *index, step.id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(forwarded, [(SceneId(1), index, id)]);
    }
    let [scene] = store.scenes.as_slice() else {
        panic!("both writes belong to one scene");
    };
    let [
        SceneEvent::Step {
            revision: 1,
            step: first,
        },
        SceneEvent::Step {
            revision: 2,
            step: second,
        },
    ] = scene.log.as_slice()
    else {
        panic!("both step records must be durable");
    };
    assert_eq!([first.id.as_str(), second.id.as_str()], ["one", "two"]);
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
    let closed = drawing.handle(closed(1)).expect("the run closed");
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
    assert_eq!(out.calls[0].error, TOO_MUCH);
}

/// The race the cancel checks exist for: a layout that finished before its cancel is delivered
/// after it, and the step is still never written.
#[test]
fn a_layout_finished_before_its_cancel_is_never_written() {
    let failing: Vec<_> = ["refused", "deleted", "cleared", "newcomers"]
        .into_iter()
        .filter(|source| std::panic::catch_unwind(|| cancel_race(source)).is_err())
        .collect();
    assert_eq!(
        failing,
        Vec::<&str>::new(),
        "every source runs before the test fails"
    );
}

fn cancel_race(source: &str) {
    let mut drawing = Drawing {
        defer_preparation: true,
        ..Drawing::default()
    };
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 2)])),
    );
    let scene = if source == "newcomers" { "s2" } else { "s1" };
    let out = drawing
        .handle(call(1, "c2", args(scene, &[circles("two", 2, 2)])))
        .expect("a fresh call");
    // A new scene's name is looked up before its step is laid out.
    let out = match sent(&out) {
        Some((serial, write @ Write::Lookup { .. })) => drawing
            .handle(Input::Written {
                serial,
                reply: Ok(store.answer(&write)),
            })
            .expect("the lookup"),
        _ => out,
    };
    let finished = job(&out).run();
    assert!(
        matches!(
            finished,
            Input::Prepared {
                outcome: Prepared::Layout { .. },
                ..
            }
        ),
        "{source}: the layout finished"
    );
    let cancel = match source {
        "refused" => Input::CallFinished {
            thread: THREAD,
            run: RunId(1),
            call: CallId::new("c2").expect("a short ASCII id"),
            error: Some(CallError::Interrupted),
        },
        "deleted" => Input::ThreadDeleted { thread: THREAD },
        _ => Input::Clear {
            scenes: vec![SceneId(1)],
        },
    };
    let mut out = drawing.handle(cancel).expect("a cancel");
    merge(&mut out, drawing.handle(finished).expect("the held layout"));
    let out = store.settle(&mut drawing, out);
    assert!(
        !trace(&out)
            .iter()
            .any(|line| line.starts_with("write step")),
        "{source}: {:?}",
        trace(&out)
    );
    match source {
        "deleted" => {
            let mut inline = Drawing::default();
            let mut reference = Store::default();
            reference.run(
                &mut inline,
                call(1, "c1", args("s1", &[circles("one", 0, 2)])),
            );
            let mut pending = inline
                .handle(call(1, "c2", args("s1", &[circles("two", 2, 2)])))
                .expect("a fresh call");
            merge(
                &mut pending,
                inline
                    .handle(Input::ThreadDeleted { thread: THREAD })
                    .expect("a deletion"),
            );
            let written = reference.settle(&mut inline, pending);
            let errors =
                |ends: &[CallEnd]| ends.iter().map(|end| end.error.clone()).collect::<Vec<_>>();
            assert_eq!(
                errors(&out.calls),
                errors(&written.calls),
                "as a fenced Wait::Step ends"
            );
            assert!(out.calls[0].drawn.is_empty(), "nothing was written");
        }
        "cleared" => {
            assert_eq!(out.calls.len(), 1);
            assert_eq!(error(&out.calls[0]), ErrorCode::SceneCleared);
        }
        _ => assert_eq!(
            out.calls
                .iter()
                .map(|end| end.error.clone())
                .collect::<Vec<_>>(),
            [Some(CallError::Interrupted)],
            "{source}"
        ),
    }
    assert!(drawing.settled(), "{source}");
}

/// What every counted scene's layouts retain.
fn retained(drawing: &Drawing) -> usize {
    drawing
        .scenes
        .iter()
        .filter_map(|held| held.paint.as_ref())
        .map(Paint::resident_bytes)
        .sum()
}

/// A text with a sixty-character label.
fn label(id: &str, y: usize) -> String {
    format!(
        r#"{{"text":{{"id":"{id}","text":"A label of sixty characters for the retention fixture, pad","at":[10,{y}]}}}}"#
    )
}

/// Step `step` of a scene that removes the label it put last and puts a new one.
fn swap(step: usize) -> String {
    format!(
        r#"{{"id":"w{step}","remove":["k{}"],"put":[{}]}}"#,
        step - 1,
        label(&format!("k{step}"), 5)
    )
}

/// The heaviest scene the budgets admit, shown: 4,000 table cells, three 4,000-point line
/// charts and 50 labels, each step in its own run so no run passes its call limit.
fn heaviest_scene(drawing: &mut Drawing, store: &mut Store) {
    let cols = r#"["Name","Owner","State","Due","Note","Area","Risk","Size"]"#;
    let cells = r#"["Cell text one","Cell text two","Cell text 3","Cell text 4","Cell text 5","Cell text 6","Cell text 7","Cell text 8"]"#;
    let mut steps: Vec<String> = (0..13)
        .map(|table| {
            let mut put = vec![format!(
                r#"{{"table":{{"id":"tb{table}","box":[0,0,900,600],"cols":{cols}}}}}"#
            )];
            put.extend((0..if table == 12 { 20 } else { 40 }).map(|row| {
                format!(r#"{{"row":{{"id":"tb{table}r{row}","in":"tb{table}","cells":{cells}}}}}"#)
            }));
            format!(r#"{{"id":"g{table}","put":[{}]}}"#, put.join(","))
        })
        .collect();
    let points = vec!["1"; 500].join(",");
    steps.extend((0..3).map(|chart| {
        let mut put = vec![format!(
            r#"{{"chart":{{"id":"ch{chart}","kind":"line","box":[0,0,900,600]}}}}"#
        )];
        put.extend((0..8).map(|series| {
            format!(
                r#"{{"series":{{"id":"ch{chart}s{series}","in":"ch{chart}","data":[{points}]}}}}"#
            )
        }));
        format!(r#"{{"id":"h{chart}","put":[{}]}}"#, put.join(","))
    }));
    let labels: Vec<String> = (0..49).map(|n| label(&format!("t{n}"), n * 12)).collect();
    steps.push(format!(
        r#"{{"id":"labels","put":[{},{}]}}"#,
        labels.join(","),
        label("k0", 5)
    ));
    for (index, step) in (0..).zip(&steps) {
        let run = u64::from(index) + 1;
        let out = store.run(
            drawing,
            call(
                run,
                &format!("r{run}"),
                args("s1", std::slice::from_ref(step)),
            ),
        );
        assert_eq!(out.calls[0].error, None, "run {run} draws");
        let shown = Input::StepShown {
            run: RunId(run),
            scene: SceneId(1),
            index,
        };
        store.run(drawing, shown);
    }
    let usage = drawing.global.usage();
    assert_eq!((usage.cells, usage.marks), (4_000, 12_000));
}

/// The ceiling: the heaviest scene the budgets admit stays counted with eight unshown steps.
#[test]
fn the_heaviest_scene_keeps_eight_unshown_steps_under_the_retained_cap() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    heaviest_scene(&mut drawing, &mut store);
    let swaps: Vec<String> = (1..=8).map(swap).collect();
    let out = store.run(&mut drawing, call(18, "swaps", args("s1", &swaps)));
    assert_eq!(out.calls[0].error, None);
    assert!(drawing.scenes[0].paint.is_some(), "still counted");
    let bytes = retained(&drawing);
    assert!(bytes <= MAX_RETAINED_BYTES, "{bytes} retained");
}

/// Labels put up front and swapped one at a time, so every later layout holds them all: the
/// scene crosses the cap, gives its layouts back and stays uncounted, while others are counted.
#[test]
fn a_scene_past_the_retained_cap_goes_uncounted_and_gives_its_layouts_back() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "c1", args("small", &[circles("one", 0, 2)])),
    );
    let small = retained(&drawing);
    let put = |range: std::ops::Range<usize>, last: &str| {
        let mut put: Vec<String> = range.map(|n| label(&format!("t{n}"), n % 900)).collect();
        put.push(label(last, 5));
        put.join(",")
    };
    let mut steps = vec![
        format!(r#"{{"id":"w0","put":[{}]}}"#, put(0..184, "k0x")),
        format!(
            r#"{{"id":"w1","remove":["k0x"],"put":[{}]}}"#,
            put(184..369, "k1")
        ),
    ];
    steps.extend((2..64).map(swap));
    let mut crossed = None;
    for (offset, step) in (2..).zip(&steps) {
        let input = call(
            offset,
            &format!("r{offset}"),
            args("big", std::slice::from_ref(step)),
        );
        let out = store.run(&mut drawing, input);
        assert_eq!(
            out.calls[0].error, None,
            "run {offset} is written, never refused"
        );
        let bytes = retained(&drawing);
        assert!(
            bytes <= MAX_RETAINED_BYTES,
            "run {offset} leaves {bytes} retained"
        );
        if crossed.is_none() && drawing.scenes[1].paint.is_none() {
            crossed = Some(offset);
            assert_eq!(bytes, small, "its layouts are given back");
        }
    }
    assert!(crossed.is_some_and(|run| run > 3), "crossed at {crossed:?}");
    assert_eq!(fills(&drawing), [Some(96), None], "it stays uncounted");
    store.run(
        &mut drawing,
        call(70, "c70", args("later", &[circles("one", 0, 3)])),
    );
    assert_eq!(fills(&drawing), [Some(96), None, Some(144)]);
}

/// A shown step, a Stop's cut and a clear each leave the scene with no unshown layout.
#[test]
fn retained_layouts_leave_when_shown_cut_or_cleared() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let latest = |drawing: &Drawing| {
        drawing.scenes[0]
            .paint
            .as_ref()
            .map_or(0, |paint| paint.latest().resident_bytes())
    };
    store.run(
        &mut drawing,
        call(1, "c1", args("s1", &[circles("one", 0, 20)])),
    );
    let shown = |index| Input::StepShown {
        run: RunId(1),
        scene: SceneId(1),
        index,
    };
    store.run(&mut drawing, shown(0));
    assert_eq!(
        retained(&drawing),
        latest(&drawing),
        "only the shown layout"
    );
    store.run(
        &mut drawing,
        call(1, "c2", args("s1", &[circles("two", 20, 10)])),
    );
    assert!(retained(&drawing) > latest(&drawing), "the unshown one too");
    store.run(&mut drawing, shown(1));
    assert_eq!(retained(&drawing), latest(&drawing));
    store.run(
        &mut drawing,
        call(1, "c3", args("s1", &[circles("three", 30, 5)])),
    );
    assert!(retained(&drawing) > latest(&drawing));
    let revealed = (0..30)
        .map(|index| handle(&drawing, 1, &format!("c{index}")))
        .collect();
    let out = drawing
        .handle(stop(1, 1, cut(2, revealed)))
        .expect("a Stop of the drawing run");
    store.settle(&mut drawing, out);
    assert_eq!(
        fills(&drawing),
        [Some(30 * 48)],
        "cut back to the shown steps"
    );
    assert_eq!(retained(&drawing), latest(&drawing));
    store.run(
        &mut drawing,
        Input::Clear {
            scenes: vec![SceneId(1)],
        },
    );
    assert!(drawing.scenes.is_empty());
    assert_eq!(retained(&drawing), 0);
}
