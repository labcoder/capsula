use std::{sync::Arc, time::Instant};

use kumochi_protocol::message::{DataToShell, RunId, SceneHeader, SceneId};
use kumochi_scene::{
    Record,
    deferred::{self, Deferred},
    layout::SurfaceGeometry,
    parse_record,
    text::Bounded,
    theme::Theme,
};

use super::{EscapePanel, Presence, Runs};
use crate::presenter::{Effect, Presenter};

fn mirrored_presenter() -> Presenter {
    deferred::start().expect("the dropper starts before intake");
    let mut presenter = Presenter::new(
        SurfaceGeometry::Overlay {
            logical_size: [1920.0, 1080.0],
        },
        Arc::new(Theme::default()),
    );
    let now = Instant::now();
    presenter.connected(1);
    presenter
        .described(1, &Bounded::default(), now)
        .expect("empty membership");
    let Record::Begin(begin) =
        parse_record(br#"{"begin":{"v":1,"scene":"unpainted","surface":"overlay","alt":"test"}}"#)
            .expect("header")
    else {
        panic!("begin record");
    };
    presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneBegin {
                    scene: SceneId(1),
                    header: SceneHeader {
                        begin,
                        layout_version: 1,
                    },
                },
                100,
            ),
            100,
            now,
        )
        .expect("begin scene");
    let Record::Step(step) = parse_record(br#"{"step":{"id":"first","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,10,10]}}]}}"#).expect("step") else {
        panic!("step record");
    };
    let effects = presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneStepCommit {
                    run: RunId(1),
                    scene: SceneId(1),
                    index: 0,
                    step,
                },
                100,
            ),
            100,
            now,
        )
        .expect("admit first step");
    let mut admissions = effects.into_iter().filter_map(|effect| match effect {
        Effect::Admit(job) => Some(job),
        _ => None,
    });
    let job = admissions.next().expect("one admission");
    assert!(admissions.next().is_none());
    let effects = presenter
        .completed(job.run(), now)
        .expect("mirror first step");
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Prepare(_)))
            .count(),
        1
    );
    assert!(presenter.holds_scenes());
    presenter
}

#[test]
fn a_first_mirrored_v1_step_without_paint_does_not_hold_global_escape() {
    let presenter = mirrored_presenter();
    assert!(presenter.holds_scenes());
    let runs = Runs::default();
    assert!(!runs.working());
    assert!(!runs.drawing());
    for panel in [EscapePanel::Closed, EscapePanel::Open] {
        let activity = Presence::activity(&runs, panel, &presenter);
        assert!(!activity.drawing);
        assert!(!activity.holds_global_escape());
    }
}

#[test]
fn focused_text_panel_escape_clears_idle_mirrored_scenes_without_a_global_hold() {
    use super::{Escape, escape::local_decision};

    let presenter = mirrored_presenter();
    let activity = Presence::activity(&Runs::default(), EscapePanel::Open, &presenter);
    assert!(!activity.holds_global_escape());
    assert_eq!(
        local_decision(activity, EscapePanel::Open, &presenter),
        Escape::Clear
    );
    assert_eq!(
        local_decision(activity, EscapePanel::SettingsEdit, &presenter),
        Escape::CancelEdit
    );
    assert_eq!(
        local_decision(activity, EscapePanel::Rename, &presenter),
        Escape::CancelEdit
    );
    assert!(!activity.holds_global_escape());
}

struct Blank;

impl gpui_kit::Render for Blank {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<'_, Self>,
    ) -> impl gpui_kit::IntoElement {
        gpui_kit::div()
    }
}

#[test]
#[ignore = "opens a real hidden window: cargo test -p kumochi-shell -- --ignored hidden_window"]
fn a_hidden_window_takes_its_requested_size() {
    use gpui_kit::AppContext as _;

    let viewport = std::rc::Rc::new(std::cell::Cell::new(None));
    let seen = std::rc::Rc::clone(&viewport);
    gpui_kit::application().run(move |cx| {
        let window = cx
            .open_window(super::options(), |_, cx| cx.new(|_| Blank))
            .expect("a hidden window opens");
        seen.set(Some(
            window
                .update(cx, |_, window, _| window.viewport_size())
                .expect("the window is open"),
        ));
        cx.quit();
    });
    assert_eq!(
        viewport.get(),
        Some(gpui_kit::size(gpui_kit::px(112.0), gpui_kit::px(48.0)))
    );
}
