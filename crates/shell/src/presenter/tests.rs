//! The presenter's tests that stay with the shell: the measurements and the one that opens a
//! GPUI context.

use std::{sync::Arc, time::Instant};

use kumochi_scene::{layout::SurfaceGeometry, text::Bounded, theme::Theme};

use super::{Completion, Effect, Presenter};
pub(crate) use kumochi_shell_core::presenter::fixture::*;

#[path = "measure_tests.rs"]
mod measurements;

#[path = "cap_measure_tests.rs"]
pub(crate) mod cap_measurements;

#[path = "../presence/overlay_recovery_recording_tests.rs"]
pub(crate) mod recovery_recording;

#[gpui::test]
fn a_failed_weak_entity_update_retires_its_captured_completion(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    use std::{cell::Cell, rc::Rc};

    let theme = Arc::new(Theme::default());
    let mut presenter = Presenter::new(
        SurfaceGeometry::default(),
        Arc::clone(&theme),
        Box::new(Charged(0)),
    );
    presenter.connected(GENERATION);
    let now = Instant::now();
    presenter
        .described(GENERATION, &Bounded::default(), now)
        .expect("empty membership");
    begin(&mut presenter, LIVE, now);
    let job = only_job(commit(&mut presenter, LIVE, 0, rect("captured"), now).expect("a step"));
    let done = job.run(&mut Shaper);
    let Completion::Prepared { result, .. } = &done else {
        panic!("a preparation completion");
    };
    assert_eq!(result.as_ref().expect("prepared step").index, 0);
    let source = first_logged_step(&presenter, LIVE);
    assert!(drained(&source, 4));
    assert_eq!(
        Arc::strong_count(&source),
        4,
        "log, step, aliased run and probe"
    );
    assert_eq!(Arc::strong_count(&theme), 3, "presenter, result and probe");

    let recipient = cx.update(|cx| {
        let entity = cx.new(|_| ());
        let weak = entity.downgrade();
        drop(entity);
        weak
    });
    recipient.assert_released();
    let called = Rc::new(Cell::new(false));
    let observed = Rc::clone(&called);
    let release = hold_dropper();
    let error = recipient
        .update(cx, move |(), _| {
            observed.set(true);
            drop(done);
        })
        .expect_err("the actual entity recipient was released");
    assert_eq!(error.to_string(), "entity released");
    assert!(
        !called.get(),
        "the failed delivery never invokes its closure"
    );
    assert_eq!(
        workers(&presenter),
        0,
        "the discarded answer releases its slot"
    );
    assert_eq!(
        Arc::strong_count(&source),
        4,
        "closure disposal queues its source"
    );
    assert_eq!(
        Arc::strong_count(&theme),
        3,
        "closure disposal queues its theme"
    );
    drop(release);
    assert!(
        drained(&source, 2),
        "the dropper releases both ticket aliases"
    );
    assert!(
        drained(&theme, 2),
        "the dropper releases the result's theme"
    );
    assert_eq!(
        Arc::strong_count(&source),
        2,
        "only the log and probe remain"
    );
    assert_eq!(
        Arc::strong_count(&theme),
        2,
        "only the presenter and probe remain"
    );
}
