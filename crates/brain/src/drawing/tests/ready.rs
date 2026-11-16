//! A finished call's result waits for the shell to prepare or refuse its scene's last step.

use kumochi_protocol::message::{RunId, SceneFailure, SceneId};

use super::{
    CallEnd, CallError, Drawing, Input, Output, Remaining, Store, THREAD, args, begin, call,
    closed, cut, numbered, step, streamed, text,
};
use crate::coordinator::Stale;
use crate::engines::CallId;

fn silent() -> Store {
    Store {
        unprepared: true,
        ..Store::default()
    }
}

fn ready(scene: u64, index: u32) -> Input {
    Input::StepReady {
        run: RunId(1),
        scene: SceneId(scene),
        index,
    }
}

fn refused(index: u32) -> Input {
    Input::StepFailed {
        run: RunId(1),
        scene: SceneId(1),
        index,
        reason: SceneFailure::PaintWork,
        cut: cut(0, vec![]),
    }
}

fn texts(out: &Output) -> Vec<String> {
    out.calls.iter().map(CallEnd::tool_text).collect()
}

/// One held call: `two` drew steps a and b in scene 1 and waits for step 1.
fn held(store: &mut Store, drawing: &mut Drawing) {
    let out = store.run(
        drawing,
        call(1, "two", args("two", &[text("a", "a"), text("b", "b")])),
    );
    assert_eq!(out.calls, []);
    assert!(drawing.holding());
}

#[test]
fn a_finished_call_waits_and_a_refusal_of_its_step_asks_the_model_to_redraw() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    let ended = store.run(
        &mut drawing,
        call(1, "done", args("done", &[text("s1", "a")])),
    );
    assert_eq!(ended.calls, []);
    let failed = store.run(&mut drawing, refused(0));
    assert_eq!(
        texts(&failed),
        [
            "drawn: [s1]; stopped: drawing.TooMuchToPaint; nothing of this scene was shown, so it was removed; everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows; begin a new scene with a new name and fewer or simpler shapes"
        ]
    );
    assert_eq!(failed.calls[0].remaining, Remaining::Steps);
    assert!(drawing.drew_nothing(RunId(1)));
    assert!(!drawing.holding());
    assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
    assert_eq!(store.run(&mut drawing, Input::ReadyWait).calls, []);
}

#[test]
fn a_held_call_ends_once_at_its_last_step_ready_or_shown() {
    for shown in [false, true] {
        let mut drawing = Drawing::default();
        let mut store = silent();
        held(&mut store, &mut drawing);
        assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
        let last = if shown {
            Input::StepShown {
                run: RunId(1),
                scene: SceneId(1),
                index: 1,
            }
        } else {
            ready(1, 1)
        };
        assert_eq!(texts(&store.run(&mut drawing, last)), ["drawn: [a, b]"]);
        assert!(!drawing.holding());
        assert_eq!(store.run(&mut drawing, ready(1, 1)).calls, []);
        assert_eq!(store.run(&mut drawing, Input::ReadyWait).calls, []);
    }
}

#[test]
fn a_streamed_call_whose_steps_were_ready_before_its_end_returns_at_once() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    store.run(&mut drawing, streamed("s", 0, &begin("live", "")));
    store.run(&mut drawing, streamed("s", 1, &step(&text("a", "a"))));
    store.run(&mut drawing, streamed("s", 2, &step(&text("b", "b"))));
    assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
    assert_eq!(store.run(&mut drawing, ready(1, 1)).calls, []);
    store.run(&mut drawing, streamed("s", 3, r#"{"end":{}}"#));
    let finished = store.run(
        &mut drawing,
        Input::CallFinished {
            thread: THREAD,
            run: RunId(1),
            call: CallId::new("s").expect("call id"),
            error: None,
        },
    );
    assert_eq!(texts(&finished), ["drawn: [a, b]"]);
    assert!(!drawing.holding());
}

#[test]
fn only_a_last_step_inside_the_ready_window_is_waited_for() {
    for (steps, waits) in [(8, true), (9, false)] {
        let mut drawing = Drawing::default();
        let mut store = silent();
        let ended = store.run(&mut drawing, call(1, "w", args("w", &numbered("s", steps))));
        assert_eq!(drawing.holding(), waits, "{steps} steps");
        let drawn = (0..steps).map(|n| format!("s{n}")).collect::<Vec<_>>();
        let expected = if waits {
            Vec::new()
        } else {
            vec![format!("drawn: [{}]", drawn.join(", "))]
        };
        assert_eq!(texts(&ended), expected, "{steps} steps");
    }
}

#[test]
fn the_wait_ends_after_a_whole_tick_with_no_answer_and_an_answer_restarts_it() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    held(&mut store, &mut drawing);
    assert_eq!(store.run(&mut drawing, Input::ReadyWait).calls, []);
    assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
    assert_eq!(store.run(&mut drawing, Input::ReadyWait).calls, []);
    assert_eq!(
        texts(&store.run(&mut drawing, Input::ReadyWait)),
        ["drawn: [a, b]"]
    );
    assert!(!drawing.holding());
    assert_eq!(store.run(&mut drawing, refused(1)).calls, []);
}

#[test]
fn every_end_path_ends_a_held_call_once() {
    let ends: [(&str, Input, Option<CallError>); 3] = [
        ("closed", closed(1), None),
        (
            "cleared",
            Input::Clear {
                scenes: vec![SceneId(1)],
            },
            None,
        ),
        ("deleted", Input::ThreadDeleted { thread: THREAD }, None),
    ];
    for (case, input, error) in ends {
        let mut drawing = Drawing::default();
        let mut store = silent();
        held(&mut store, &mut drawing);
        let ended = store.run(&mut drawing, input);
        assert_eq!(ended.calls.len(), 1, "{case}");
        assert_eq!(ended.calls[0].error, error, "{case}");
        assert!(!drawing.holding(), "{case}");
        assert_eq!(store.run(&mut drawing, ready(1, 1)).calls, [], "{case}");
        assert_eq!(
            store.run(&mut drawing, Input::ReadyWait).calls,
            [],
            "{case}"
        );
    }
}

#[test]
fn an_evicted_scene_s_held_call_gets_its_result_and_the_newcomer_waits() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    for n in 1..=4 {
        let input = call(
            1,
            &format!("c{n}"),
            args(&format!("s{n}"), &[text("a", "x")]),
        );
        assert_eq!(store.run(&mut drawing, input).calls, []);
    }
    let fifth = store.run(&mut drawing, call(1, "c5", args("s5", &[text("e", "x")])));
    assert_eq!(texts(&fifth), ["drawn: [a]"]);
    assert_eq!(fifth.calls[0].call, CallId::new("c1").expect("call id"));
    assert!(drawing.holding());
    assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
}

#[test]
fn a_second_call_in_the_held_scene_waits_for_its_own_step() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    assert_eq!(
        store
            .run(
                &mut drawing,
                call(1, "first", args("one", &[text("a", "a")]))
            )
            .calls,
        []
    );
    assert_eq!(
        store
            .run(
                &mut drawing,
                call(1, "second", args("one", &[text("b", "b")]))
            )
            .calls,
        []
    );
    let first = store.run(&mut drawing, ready(1, 0));
    assert_eq!(texts(&first), ["drawn: [a]"]);
    assert_eq!(first.calls[0].call, CallId::new("first").expect("call id"));
    let second = store.run(&mut drawing, ready(1, 1));
    assert_eq!(texts(&second), ["drawn: [b]"]);
    assert!(!drawing.holding());
}

#[test]
fn a_ready_step_its_run_never_committed_breaks_the_session() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    held(&mut store, &mut drawing);
    let wrong = store.run(
        &mut drawing,
        Input::StepReady {
            run: RunId(2),
            scene: SceneId(1),
            index: 1,
        },
    );
    assert_eq!(wrong.calls, []);
    assert!(wrong.effects.iter().any(|effect| matches!(
        effect,
        crate::coordinator::Effect::Broken("a step its run never committed")
    )));
}

const PAINT: &str = "drawn: [a, b]; stopped: drawing.TooMuchToPaint; nothing of this scene was shown, so it was removed; everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows; begin a new scene with a new name and fewer or simpler shapes";

fn finished(call: &str) -> Input {
    Input::CallFinished {
        thread: THREAD,
        run: RunId(1),
        call: CallId::new(call).expect("call id"),
        error: None,
    }
}

/// The held call's last step refused while another scene's lookup is still pending, so the
/// refusal's cut waits in the queue. The pending work, to settle later.
fn refused_behind_other_work(store: &mut Store, drawing: &mut Drawing) -> Output {
    held(store, drawing);
    assert_eq!(store.run(drawing, Input::ReadyWait).calls, []);
    let other = drawing
        .handle(streamed("other", 0, &begin("other", "")))
        .expect("a fresh call");
    assert_eq!(
        drawing.handle(refused(1)).expect("a committed step").calls,
        []
    );
    other
}

#[test]
fn a_refusal_received_behind_other_work_wins_over_the_tick() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    let other = refused_behind_other_work(&mut store, &mut drawing);
    let tick = drawing.handle(Input::ReadyWait).expect("a tick");
    assert_eq!(texts(&tick), [PAINT]);
    assert!(!drawing.holding());
    assert_eq!(store.settle(&mut drawing, other).calls, []);
}

#[test]
fn a_refusal_received_behind_other_work_wins_over_the_run_closing() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    let other = refused_behind_other_work(&mut store, &mut drawing);
    let closing = drawing.handle(closed(1)).expect("a close");
    let two = CallId::new("two").expect("call id");
    let ended: Vec<_> = closing.calls.iter().filter(|end| end.call == two).collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(
        ended[0].error,
        Some(CallError::TooMuchToPaint { cleared: true })
    );
    assert_eq!(ended[0].drawn.len(), 2);
    assert!(
        store
            .settle(&mut drawing, other)
            .calls
            .iter()
            .all(|end| end.call != two)
    );
}

#[test]
fn a_call_waits_for_its_own_last_step_not_a_later_call_s() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    store.run(&mut drawing, streamed("first", 0, &begin("live", "")));
    store.run(&mut drawing, streamed("first", 1, &step(&text("a", "a"))));
    assert_eq!(store.run(&mut drawing, ready(1, 0)).calls, []);
    store.run(&mut drawing, streamed("second", 0, &begin("live", "")));
    store.run(&mut drawing, streamed("second", 1, &step(&text("b", "b"))));
    store.run(&mut drawing, streamed("first", 2, r#"{"end":{}}"#));
    let first = store.run(&mut drawing, finished("first"));
    assert_eq!(texts(&first), ["drawn: [a]"]);
    assert_eq!(first.calls[0].call, CallId::new("first").expect("call id"));
    store.run(&mut drawing, streamed("second", 2, r#"{"end":{}}"#));
    assert_eq!(store.run(&mut drawing, finished("second")).calls, []);
    assert_eq!(texts(&store.run(&mut drawing, ready(1, 1))), ["drawn: [b]"]);
}

#[test]
fn a_held_call_takes_no_late_record_or_second_finish() {
    let mut drawing = Drawing::default();
    let mut store = silent();
    store.run(&mut drawing, streamed("s", 0, &begin("live", "")));
    store.run(&mut drawing, streamed("s", 1, &step(&text("a", "a"))));
    store.run(&mut drawing, streamed("s", 2, r#"{"end":{}}"#));
    assert_eq!(store.run(&mut drawing, finished("s")).calls, []);
    assert!(matches!(
        drawing.handle(streamed("s", 3, &step(&text("b", "b")))),
        Err(Stale)
    ));
    assert!(matches!(drawing.handle(finished("s")), Err(Stale)));
    assert!(drawing.holding());
    assert_eq!(drawing.ledger(RunId(1)).steps, 1);
    assert_eq!(texts(&store.run(&mut drawing, ready(1, 0))), ["drawn: [a]"]);
}
