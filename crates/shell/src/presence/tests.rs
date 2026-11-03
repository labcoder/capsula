use std::{sync::Arc, time::Instant};

use kumochi_protocol::message::{DataToShell, RunId, SceneHeader, SceneId};
use kumochi_scene::{Record, layout::SurfaceGeometry, parse_record, text::Bounded, theme::Theme};

use super::{EscapePanel, Runs, escape_activity};
use crate::presenter::Presenter;

fn mirrored_presenter() -> Presenter {
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
            DataToShell::SceneBegin {
                scene: SceneId(1),
                header: SceneHeader {
                    begin,
                    layout_version: 1,
                },
            },
            100,
            now,
        )
        .expect("begin scene");
    let Record::Step(step) = parse_record(br#"{"step":{"id":"first","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,10,10]}}]}}"#).expect("step") else {
        panic!("step record");
    };
    presenter
        .data(
            1,
            DataToShell::SceneStepCommit {
                run: RunId(1),
                scene: SceneId(1),
                index: 0,
                step,
            },
            100,
            now,
        )
        .expect("mirror first step");
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
        let activity = escape_activity(&runs, panel);
        assert!(!activity.drawing);
        assert!(!activity.holds_global_escape());
    }
}

#[test]
fn focused_text_panel_escape_clears_idle_mirrored_scenes_without_a_global_hold() {
    use super::{Escape, escape::local_decision};

    let presenter = mirrored_presenter();
    let activity = escape_activity(&Runs::default(), EscapePanel::Open);
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
