//! A press on the peek's Stop button is a press on Stop: it never grabs Kumo, while a press on
//! Kumo in the same peek does.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    Bounds, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext, WindowBounds,
    WindowOptions, point, px, size,
};

use super::kumo_view::{PullChanged, PullSite};
use super::test_fixture;
use crate::platform::input::input;
use crate::platform::work_area_fixture;

const LEFT: MouseButton = MouseButton::Left;

/// One left press and release at `at`, each forwarded as the panel's native subclass does.
fn click(frame: &mut VisualTestContext, at: Point<Pixels>) {
    input(|input| input.press(|| frame.simulate_mouse_down(at, LEFT, Modifiers::default())));
    input(|input| {
        input.release(LEFT, || {
            frame.simulate_mouse_up(at, LEFT, Modifiers::default())
        })
    });
    frame.run_until_parked();
}

fn grabs(seen: &RefCell<Vec<PullChanged>>) -> usize {
    seen.borrow()
        .iter()
        .filter(|changed| matches!(changed, PullChanged::Grabbed(_)))
        .count()
}

#[gpui::test]
fn a_press_on_stop_never_grabs_kumo_but_a_press_on_kumo_does(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1392]);
    let presence = test_fixture::presence(cx);
    let (panel, kumo) = presence.update(cx, |this, cx| {
        this.follow_kumo(cx);
        (this.panel.clone(), this.kumo.clone())
    });
    panel.update(cx, |panel, _| panel.stop = true);
    kumo.update(cx, |kumo, _| {
        kumo.set_site(Some(PullSite {
            line: [1252, 1, 56, 19],
            slot: [1251, 0, 58, 21],
        }));
    });
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    cx.update(|cx| {
        cx.subscribe(&kumo, move |_, changed: &PullChanged, _| {
            log.borrow_mut().push(*changed);
        })
        .detach();
    });
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(380.0), px(124.0)),
        })),
        ..WindowOptions::default()
    };
    let shown = panel.clone();
    let window = cx
        .update(|cx| gpui_kit::open_window(options, cx, move |_, _| shown))
        .expect("a GPUI test window opens")
        .0;
    let mut frame = VisualTestContext::from_window(window.into(), cx);
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let stop = frame.debug_bounds("presence-stop").expect("Stop shows");
    let body = frame.debug_bounds("presence-kumo").expect("Kumo shows");

    click(&mut frame, stop.center());
    assert_eq!(
        grabs(&seen),
        0,
        "a press on Stop at {:?} grabbed Kumo at {body:?}",
        stop.center()
    );

    click(&mut frame, body.center());
    assert_eq!(grabs(&seen), 1, "a press on Kumo at {body:?} grabs it");
}
