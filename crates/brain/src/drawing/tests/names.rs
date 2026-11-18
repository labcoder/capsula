use super::*;
use crate::context::Geometry;
use kumochi_scene::record::Surface;

fn named(name: &str, surface: Surface) -> String {
    serde_json::json!({"begin":{"scene":name,"surface":surface,"others":"keep","alt":"test"}})
        .to_string()
}

fn mark(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","put":[{{"highlight":{{"id":"{id}","target":{{"box":[10,20,30,40]}},"style":"box"}}}}]}}"#
    )
}

#[test]
fn a_screen_name_does_not_change_an_overlay_draw_live_or_cleared() {
    for cleared in [false, true] {
        for screen_name in ["unrelated", "guessed"] {
            let mut drawing = Drawing::default();
            drawing.canvas(RunId(1), Ok(Geometry::FIXTURE));
            let mut store = Store::default();
            let screen = envelope(&named(screen_name, Surface::Screen), &[mark("mark")]);
            let screen = store.run(&mut drawing, call(1, "screen", screen));
            assert!(screen.calls[0].complete());
            if cleared {
                store.run(
                    &mut drawing,
                    Input::Clear {
                        scenes: vec![SceneId(1)],
                    },
                );
            }

            let overlay = args("guessed", &[text("card", "label")]);
            let overlay = store.run(&mut drawing, call(1, "overlay", overlay));
            assert_eq!(
                overlay.calls[0].tool_text(),
                "drawn: [card]",
                "screen name {screen_name}, cleared {cleared}"
            );
            assert_eq!(
                trace(&overlay),
                [
                    "lookup",
                    "write step 1",
                    "begin 2",
                    "step 2#0",
                    "end 2 true"
                ]
            );
            assert_eq!(store.scenes[0].header.begin.surface, Surface::Screen);
            assert_eq!(store.scenes[0].log.len(), 1);
            assert_eq!(store.scenes[1].header.begin.surface, Surface::Overlay);
            assert_eq!(store.scenes[1].log.len(), 1);

            let continued = store.run(
                &mut drawing,
                call(1, "more-overlay", args("guessed", &[text("next", "extra")])),
            );
            assert_eq!(continued.calls[0].tool_text(), "drawn: [next]");
            assert_eq!(
                trace(&continued),
                ["write step 2", "step 2#1", "end 2 true"]
            );
            if !cleared {
                let more = envelope(&named(screen_name, Surface::Screen), &[mark("next-mark")]);
                let continued = store.run(&mut drawing, call(1, "more-screen", more));
                assert!(continued.calls[0].complete());
                assert_eq!(
                    trace(&continued),
                    ["write step 2", "step 1#1", "end 1 true"]
                );
            }
        }
    }
}

#[test]
fn pending_names_join_only_the_same_surfaces_first_write() {
    for (winner, other) in [
        (Surface::Overlay, Surface::Screen),
        (Surface::Screen, Surface::Overlay),
    ] {
        let mut drawing = Drawing::default();
        drawing.canvas(RunId(1), Ok(Geometry::FIXTURE));
        let mut store = Store::default();
        for (call, surface) in [("first", winner), ("same", winner), ("other", other)] {
            let out = store.run(&mut drawing, streamed(call, 0, &named("shared", surface)));
            assert_eq!(trace(&out), ["lookup"]);
            assert!(out.calls.is_empty());
        }
        let record = |surface, id| {
            step(&if surface == Surface::Screen {
                mark(id)
            } else {
                text(id, id)
            })
        };

        let first = drawing
            .handle(streamed("first", 1, &record(winner, "a")))
            .expect("first step");
        let queued = drawing
            .handle(streamed("other", 1, &record(other, "b")))
            .expect("other step while the first is held");
        assert!(queued.effects.is_empty());
        let out = store.settle(&mut drawing, first);
        assert!(
            out.calls.is_empty(),
            "neither call is refused: {:?}",
            out.calls
        );
        assert_eq!(
            trace(&out),
            [
                "write step 1",
                "begin 1",
                "step 1#0",
                "write step 1",
                "begin 2",
                "step 2#0"
            ]
        );
        let same = store.run(&mut drawing, streamed("same", 1, &record(winner, "c")));
        assert_eq!(trace(&same), ["write step 2", "step 1#1"]);
        assert_eq!(store.scenes[0].header.begin.surface, winner);
        assert_eq!(store.scenes[0].log.len(), 2);
        assert_eq!(store.scenes[1].header.begin.surface, other);
        assert_eq!(store.scenes[1].log.len(), 1);
    }
}
