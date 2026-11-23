//! A press on the peek's button under Kumo (Stop when it shows, else Chat) is a press on that
//! button: it never grabs Kumo, while a press on Kumo in the same peek does, with Kumo's picture
//! showing.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    Bounds, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext, WindowBounds,
    WindowOptions, point, px, size,
};

use super::kumo::Motion;
use super::kumo_view::{PullChanged, PullSite};
use super::test_fixture;
use crate::platform::input::{Button, input};
use crate::platform::work_area_fixture;

const LEFT: MouseButton = MouseButton::Left;

/// One left press and release at `at`, each forwarded as the panel's native subclass does.
fn click(frame: &mut VisualTestContext, at: Point<Pixels>) {
    input(|input| input.press(|| frame.simulate_mouse_down(at, LEFT, Modifiers::default())));
    input(|input| {
        input.release(Button::Left, || {
            frame.simulate_mouse_up(at, LEFT, Modifiers::default());
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
fn a_press_on_stop_never_grabs_kumo_but_a_press_on_kumo_does_while_its_picture_shows(
    cx: &mut TestAppContext,
) {
    for side in [
        Side::Left,
        Side::Right,
        Side::Top,
        Side::Bottom,
        Side::UpperBody,
    ] {
        press_under_kumo(&mut cx.new_app(), "presence-stop", side);
    }
}

#[gpui::test]
fn a_press_on_chat_never_grabs_kumo_but_a_press_on_kumo_does_while_its_picture_shows(
    cx: &mut TestAppContext,
) {
    press_under_kumo(cx, "presence-chat", Side::UpperBody);
}

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
    UpperBody,
}

/// Presses the named button, then checks the chosen part of Kumo's drawn hit region.
fn press_under_kumo(cx: &mut TestAppContext, selector: &'static str, side: Side) {
    let _area = work_area_fixture::set([0, 0, 2560, 1392]);
    let presence = test_fixture::presence(cx);
    let (panel, kumo) = presence.update(cx, |this, cx| {
        this.follow_kumo(cx);
        (this.panel.clone(), this.kumo.clone())
    });
    panel.update(cx, |panel, _| panel.stop = selector == "presence-stop");
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
    let mut frame = VisualTestContext::from_window(window, cx);
    kumo.update(&mut frame, |kumo, cx| {
        kumo.rebake(1.0, cx);
        kumo.show(Motion::Reduced, cx);
    });
    frame.run_until_parked();
    frame.update(|window, cx| {
        kumo.update(cx, |kumo, cx| kumo.step(0.0, window, cx));
        window.draw(cx).clear(cx);
    });
    let button = frame.debug_bounds(selector).expect("the button shows");
    let body = frame.debug_bounds("presence-kumo").expect("Kumo shows");
    assert_eq!(
        frame.debug_bounds("presence-kumo-picture"),
        Some(body),
        "Kumo's picture fills its square in the peek"
    );

    click(&mut frame, button.center());
    assert_eq!(grabs(&seen), 0, "{selector} must not grab Kumo");

    let far = body.bottom_right();
    let center = body.center();
    let pixel = px(1.0);
    let (outside, inside) = match side {
        Side::Left => (
            Some(point(body.origin.x - pixel, center.y)),
            point(body.origin.x + pixel, center.y),
        ),
        Side::Right => (
            Some(point(far.x + pixel, center.y)),
            point(far.x - pixel, center.y),
        ),
        Side::Top => (
            Some(point(center.x, body.origin.y - pixel)),
            point(center.x, body.origin.y + pixel),
        ),
        Side::Bottom => (
            Some(point(center.x, far.y + pixel)),
            point(center.x, far.y - pixel),
        ),
        Side::UpperBody => (None, point(center.x, body.origin.y + px(8.0))),
    };
    if let Some(outside) = outside {
        click(&mut frame, outside);
        assert_eq!(grabs(&seen), 0, "outside the drawn body at {outside:?}");
    }
    click(&mut frame, inside);
    assert_eq!(grabs(&seen), 1, "inside the drawn body at {inside:?}");
}
