use std::sync::atomic::AtomicBool;

use kumochi_scene::display::TextMetricsV1;

use super::*;

fn metrics(advance: u16) -> TextMetricsV1 {
    TextMetricsV1 {
        advances: [advance; 95],
        line_height: 1536,
        range_maxima: [1024, 1536, 2048],
    }
}

fn dashed(x: u16, max_width: u16) -> String {
    format!(
        r#"{{"id":"dash","put":[{{"text":{{"id":"caption","text":"{}","at":[{x},100],"align":"left","max_width":{max_width}}}}},{{"line":{{"id":"leader","from":[0,100],"to":{{"ref":"caption"}},"dash":true}}}}]}}"#,
        "W".repeat(380)
    )
}

fn shown(run: u64, index: u32) -> Input {
    Input::StepShown {
        run: RunId(run),
        scene: SceneId(1),
        index,
    }
}

fn wide_text(step: &str, x: u16, glyphs: usize) -> String {
    format!(
        r#"{{"id":"{step}","put":[{{"text":{{"id":"long","text":"{}","at":[{x},100],"align":"left"}}}}]}}"#,
        "W".repeat(glyphs)
    )
}

fn geometry_error(step: &str) -> CallError {
    CallError::Step(StepError {
        step: Some(wire(step)),
        path: kumochi_scene::FieldPath::default(),
        code: ErrorCode::OutOfRange,
    })
}

#[test]
fn negotiated_wrap_increases_a_dashed_leader_past_the_paint_cap_before_write() {
    for (profile, x, expected) in [
        (None, 115, None),
        (Some(metrics(2048)), 105, None),
        (
            Some(metrics(2048)),
            115,
            Some(CallError::TooMuchToPaint { cleared: false }),
        ),
    ] {
        let mut drawing = Drawing::default();
        assert!(drawing.reaccount(1, profile).is_none());
        let mut store = Store::default();
        store.run(
            &mut drawing,
            call(1, "base", args("one", &[text("base", "a")])),
        );
        store.run(&mut drawing, shown(1, 0));
        let first = drawing.snapshot().chunks;
        // At 2 em the text wraps against 10,000; its center puts the leader at
        // 5,105 or 5,115 px, costing exactly 512 or 513 pieces.
        let result = store.run(
            &mut drawing,
            call(1, "dash", args("one", &[dashed(x, 10_000)])),
        );
        assert_eq!(result.calls[0].error, expected);
        if expected.is_some() {
            assert_eq!(store.log(SceneId(1)).len(), 1);
            assert_eq!(drawing.snapshot().chunks, first);
        } else {
            assert_eq!(store.log(SceneId(1)).len(), 2);
        }
    }
}

#[test]
fn a_valid_extreme_advance_cannot_write_a_dimension_overflow_uncounted() {
    let profile = Some(metrics(8192));
    for timed_out in [false, true] {
        let mut drawing = Drawing::default();
        if !timed_out {
            assert!(drawing.reaccount(1, profile).is_none());
        }
        let mut store = Store::default();
        store.run(
            &mut drawing,
            call(1, "base", args("one", &[text("base", "a")])),
        );
        store.run(&mut drawing, shown(1, 0));
        if timed_out {
            drawing.use_uncounted_measure(profile);
            assert!(drawing.scenes[0].paint.is_none());
        }
        let first = drawing.snapshot().chunks;
        let log = store.log(SceneId(1)).clone();
        // 380 glyphs exceed the measured width cap; 100 fit individually but reach x=21,000.
        for (run, x, glyphs) in [(2, 0, 380), (3, 9000, 100)] {
            let refused = store.run(
                &mut drawing,
                call(
                    run,
                    "extreme",
                    args("one", &[wide_text("overflow", x, glyphs)]),
                ),
            );
            assert_eq!(refused.calls[0].error, Some(geometry_error("overflow")));
            assert_eq!(*store.log(SceneId(1)), log);
            assert_eq!(drawing.snapshot().chunks, first);
        }
        let accepted = store.run(
            &mut drawing,
            call(4, "valid", args("one", &[text("valid", "ordinary")])),
        );
        assert_eq!(accepted.calls[0].error, None);
        assert_eq!(accepted.calls[0].drawn, [wire("valid")]);
        assert_eq!(store.log(SceneId(1)).len(), 2);
        assert_eq!(drawing.scenes[0].paint.is_none(), timed_out);
    }
}

#[test]
fn a_job_that_cannot_start_does_not_write_a_negotiated_continuation() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "base", args("one", &[text("base", "a")])),
    );
    store.run(&mut drawing, shown(1, 0));
    drawing.use_uncounted_measure(Some(metrics(2048)));
    drawing.defer_preparation = true;
    let log = store.log(SceneId(1)).clone();

    let pending = drawing
        .handle(call(2, "later", args("one", &[text("later", "new")])))
        .expect("a fresh call");
    assert!(sent(&pending).is_none());
    let failure = pending
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PrepareDrawing(job) => Some(job.unprepared()),
            _ => None,
        })
        .expect("dispatching a preparation job");
    let refused = store.run(&mut drawing, failure);
    assert_eq!(refused.calls[0].error, Some(CallError::Failed));
    assert_eq!(*store.log(SceneId(1)), log);
}

#[test]
fn reconnect_preserves_canonical_cuts_and_installs_owners_that_can_step_and_stop() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    let out = store.run(
        &mut drawing,
        call(
            1,
            "original",
            args("one", &[dashed(115, 6200), text("two", "extra")]),
        ),
    );
    assert_eq!(out.calls[0].error, None);
    let reveal = vec![
        handle(&drawing, 1, "caption"),
        handle(&drawing, 1, "leader"),
    ];
    store.run(&mut drawing, stop(1, 1, cut(1, reveal.clone())));
    store.run(&mut drawing, closed(1));
    let original = drawing.snapshot();
    let profile = Some(metrics(2048));
    let replay = drawing.reaccount(2, profile).expect("changed profile");
    let result = replay.run(&AtomicBool::new(false)).expect("replay fits");
    assert!(drawing.install_reaccounted(2, &replay, Some(result)));
    assert_eq!(drawing.snapshot().scenes, original.scenes);
    assert_eq!(drawing.snapshot().chunks, original.chunks);
    assert_eq!(
        drawing.scenes[0]
            .paint
            .as_ref()
            .unwrap()
            .charge()
            .dash_pieces,
        323
    );
    assert!(
        drawing.reaccount(3, profile).is_none(),
        "same profile keeps its charges"
    );

    let out = store.run(
        &mut drawing,
        call(2, "later", args("one", &[text("three", "new")])),
    );
    assert_eq!(out.calls[0].error, None);
    assert!(trace(&out).contains(&"step 1#2".to_owned()));
    let out = store.run(&mut drawing, stop(2, 1, cut(1, reveal)));
    assert_eq!(trace(&out), ["write cut 1@3:1", "cut 1@3:1"]);
    assert_eq!(drawing.global.usage().objects, 2);
    assert_eq!(
        drawing.scenes[0]
            .paint
            .as_ref()
            .unwrap()
            .charge()
            .dash_pieces,
        323,
        "the continuation and Stop used the replayed layout owner"
    );
}

#[test]
fn changing_pair_or_cutting_or_clearing_a_scene_fences_the_replayed_snapshot() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(
            1,
            "base",
            args("one", &[text("base", "a"), text("next", "b")]),
        ),
    );
    store.run(
        &mut drawing,
        call(2, "other", args("two", &[text("base", "c")])),
    );
    store.run(&mut drawing, shown(1, 1));
    let profile = Some(metrics(2048));
    let replay = drawing.reaccount(2, profile).expect("changed profile");
    let result = replay.run(&AtomicBool::new(false)).expect("replay fits");
    assert!(!drawing.install_reaccounted(3, &replay, Some(result)));

    let replay = drawing
        .reaccount(3, profile)
        .expect("the new pair must replay");
    let result = replay.run(&AtomicBool::new(false)).expect("replay fits");
    let reveal = vec![handle(&drawing, 1, "a")];
    let cut = store.run(&mut drawing, stop(1, 1, cut(2, reveal)));
    assert_eq!(trace(&cut), ["write cut 1@2:2", "cut 1@2:2"]);
    assert!(!drawing.install_reaccounted(3, &replay, Some(result)));

    let replay = drawing
        .reaccount(3, profile)
        .expect("the cut belongs in the replay");
    let result = replay.run(&AtomicBool::new(false)).expect("replay fits");
    store.run(
        &mut drawing,
        Input::Clear {
            scenes: vec![SceneId(1)],
        },
    );
    assert!(!drawing.install_reaccounted(3, &replay, Some(result)));
    let replay = drawing
        .reaccount(3, profile)
        .expect("replay the surviving scene");
    let result = replay.run(&AtomicBool::new(false)).expect("survivor fits");
    assert!(drawing.install_reaccounted(3, &replay, Some(result)));
    let snapshot = drawing.snapshot();
    assert_eq!(
        snapshot
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [SceneId(2)]
    );
    assert_eq!(drawing.global.usage().objects, 1);
    assert_eq!(store.visible()[0].scene, SceneId(2));
}

#[test]
fn failed_reaccounting_keeps_the_scene_but_not_its_old_paint_charge() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "base", args("one", &[dashed(115, 10_000)])),
    );
    let original = drawing.snapshot();
    let replay = drawing
        .reaccount(2, Some(metrics(2048)))
        .expect("changed profile");
    let result = replay.run(&AtomicBool::new(false));
    assert!(
        result.is_none(),
        "the retained leader now exceeds the paint cap"
    );
    assert!(drawing.install_reaccounted(2, &replay, result));
    assert_eq!(drawing.snapshot().chunks, original.chunks);
    assert_eq!(store.visible()[0].scene, SceneId(1));

    let log = store.log(SceneId(1)).clone();
    let out = store.run(
        &mut drawing,
        call(2, "later", args("one", &[text("later", "new")])),
    );
    assert_eq!(
        out.calls[0].error,
        Some(CallError::TooMuchToPaint { cleared: false })
    );
    assert_eq!(*store.log(SceneId(1)), log);
    assert_eq!(drawing.snapshot().chunks, original.chunks);

    let fresh =
        r#"{"id":"fresh","put":[{"line":{"id":"long","from":[0,0],"to":[3000,0],"dash":true}}]}"#;
    let out = store.run(
        &mut drawing,
        call(3, "fresh", args("two", &[fresh.to_owned()])),
    );
    assert_eq!(
        out.calls[0].error, None,
        "the old charge can't refuse this scene"
    );
    assert_eq!(store.visible().len(), 2);
}

#[test]
fn a_retained_prefix_with_invalid_geometry_refuses_an_ordinary_continuation() {
    let mut drawing = Drawing::default();
    let mut store = Store::default();
    store.run(
        &mut drawing,
        call(1, "base", args("one", &[wide_text("base", 0, 380)])),
    );
    store.run(&mut drawing, shown(1, 0));
    let log = store.log(SceneId(1)).clone();
    let original = drawing.snapshot();
    let replay = drawing
        .reaccount(2, Some(metrics(8192)))
        .expect("changed profile");
    let result = replay.run(&AtomicBool::new(false));
    assert!(result.is_none());
    assert!(drawing.install_reaccounted(2, &replay, result));

    let refused = store.run(
        &mut drawing,
        call(2, "later", args("one", &[text("later", "new")])),
    );
    assert_eq!(refused.calls[0].error, Some(geometry_error("later")));
    assert_eq!(*store.log(SceneId(1)), log);
    assert_eq!(drawing.snapshot().chunks, original.chunks);
}
