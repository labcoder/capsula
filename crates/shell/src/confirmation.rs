//! Cancel-first confirmation for consequential shell actions.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
};
use gpui_kit::prelude::*;
use gpui_kit::{App, SharedString, Window};

/// Opens an alert with Cancel focused and no dialog-wide Enter confirmation.
/// The caller can dismiss a stale confirmation with `WindowExt::close_dialog`.
pub(crate) fn open(
    window: &mut Window,
    cx: &mut App,
    title: SharedString,
    body: SharedString,
    cancel: SharedString,
    confirm: SharedString,
    on_confirm: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let cancel_focus = cx.focus_handle().tab_stop(true);
    let focus = cancel_focus.clone();
    let pending = Rc::new(RefCell::new(Some(on_confirm)));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let cancelled = Rc::clone(&pending);
        let confirmed = Rc::clone(&pending);
        let dismissed = Rc::clone(&pending);
        let enter_pending = Rc::clone(&pending);
        let enter_focus = cancel_focus.clone();
        alert
            .title(title.clone())
            .description(body.clone())
            .on_ok(move |_, window, cx| {
                if enter_focus.is_focused(window) {
                    drop(enter_pending.borrow_mut().take());
                    window.close_dialog(cx);
                }
                false
            })
            .on_cancel(move |_, _, _| {
                drop(dismissed.borrow_mut().take());
                true
            })
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("confirmation-cancel")
                            .label(cancel.clone())
                            .track_focus(&cancel_focus)
                            .on_click(move |_, window, cx| {
                                drop(cancelled.borrow_mut().take());
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("confirmation-confirm")
                            .label(confirm.clone())
                            .danger()
                            .on_click(move |_, window, cx| {
                                let callback = confirmed.borrow_mut().take();
                                if let Some(callback) = callback {
                                    window.close_dialog(cx);
                                    callback(window, cx);
                                }
                            }),
                    ),
            )
    });
    focus.focus(window, cx);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui_kit::{
        Context, KeyDownEvent, KeyUpEvent, Keystroke, Render, TestAppContext, VisualTestContext,
        div, px, size,
    };

    use super::*;

    struct Host;

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn setup(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let host = cx.new(|_| Host);
            gpui_kit::component::Root::new(host, window, cx)
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        let sends = Rc::new(Cell::new(0));
        let counter = Rc::clone(&sends);
        cx.update(|window, cx| {
            open(
                window,
                cx,
                "Sign out?".into(),
                "Remove this account.".into(),
                "Cancel".into(),
                "Sign out".into(),
                move |_, _| counter.set(counter.get() + 1),
            );
        });
        (cx, sends)
    }

    fn paint(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn press(cx: &mut VisualTestContext, key: &'static str) {
        let keystroke = Keystroke::parse(key).expect("test key");
        cx.simulate_event(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(KeyUpEvent { keystroke });
        paint(cx);
    }

    #[gpui::test]
    fn immediate_and_held_enter_send_nothing(cx: &mut TestAppContext) {
        let (cx, sends) = setup(cx);
        for is_held in [false, true, true] {
            cx.simulate_event(KeyDownEvent {
                keystroke: Keystroke::parse("enter").unwrap(),
                is_held,
                prefer_character_input: false,
            });
        }
        paint(cx);
        assert_eq!(sends.get(), 0);
    }

    #[gpui::test]
    fn enter_on_cancel_cancels(cx: &mut TestAppContext) {
        let (cx, sends) = setup(cx);
        paint(cx);
        let cancel = cx.update(|window, cx| window.focused(cx));
        assert!(cancel.is_some(), "Cancel has focus after opening");
        paint(cx);
        assert_eq!(cx.update(|window, cx| window.focused(cx)), cancel);
        cx.simulate_keystrokes("tab");
        assert_ne!(cx.update(|window, cx| window.focused(cx)), cancel);
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), cancel);
        press(cx, "enter");
        cx.run_until_parked();
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn escape_cancels(cx: &mut TestAppContext) {
        let (cx, sends) = setup(cx);
        paint(cx);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn danger_activation_sends_exactly_once(cx: &mut TestAppContext) {
        let (cx, sends) = setup(cx);
        paint(cx);
        press(cx, "tab");
        press(cx, "space");
        cx.run_until_parked();
        assert_eq!(sends.get(), 1);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        press(cx, "space");
        press(cx, "enter");
        assert_eq!(sends.get(), 1);
    }
}
