//! Cancel-first confirmation for consequential shell actions.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
};
use gpui_kit::prelude::*;
use gpui_kit::{App, FocusHandle, Global, SharedString, Window, WindowId};

#[derive(Default)]
struct Confirmations(HashMap<WindowId, Weak<()>>);

impl Global for Confirmations {}

/// Opens an alert with Cancel focused; Enter activates only a focused button.
/// The caller can dismiss a stale confirmation with `WindowExt::close_dialog`.
/// Returns no handle while another dialog is active or this confirmation is exiting.
pub(crate) fn open(
    window: &mut Window,
    cx: &mut App,
    title: SharedString,
    body: SharedString,
    cancel: SharedString,
    confirm: SharedString,
    on_confirm: impl FnOnce(&mut Window, &mut App) + 'static,
) -> Option<FocusHandle> {
    if window.has_active_dialog(cx) {
        return None;
    }
    let id = window.window_handle().window_id();
    let guards = cx.default_global::<Confirmations>();
    guards.0.retain(|_, guard| guard.strong_count() > 0);
    if guards.0.contains_key(&id) {
        return None;
    }
    let lifetime = Rc::new(());
    guards.0.insert(id, Rc::downgrade(&lifetime));
    let cancel_focus = cx.focus_handle().tab_stop(true);
    let confirm_focus = cx.focus_handle().tab_stop(true);
    let focus = cancel_focus.clone();
    let pending = Rc::new(RefCell::new(Some(on_confirm)));
    let confirmed = Rc::clone(&pending);
    let confirm_action = Rc::new(move |window: &mut Window, cx: &mut App| {
        let callback = confirmed.borrow_mut().take();
        if let Some(callback) = callback {
            window.close_dialog(cx);
            callback(window, cx);
        }
    });
    window.open_alert_dialog(cx, move |alert, _, _| {
        let _lifetime = Rc::clone(&lifetime);
        let cancelled = Rc::clone(&pending);
        let confirmed = Rc::clone(&confirm_action);
        let dismissed = Rc::clone(&pending);
        let enter_pending = Rc::clone(&pending);
        let enter_focus = cancel_focus.clone();
        let enter_confirm_focus = confirm_focus.clone();
        let enter_confirmed = Rc::clone(&confirm_action);
        alert
            .title(title.clone())
            .description(body.clone())
            .on_ok(move |_, window, cx| {
                if enter_focus.is_focused(window) && enter_pending.borrow_mut().take().is_some() {
                    window.close_dialog(cx);
                } else if enter_confirm_focus.is_focused(window) {
                    enter_confirmed(window, cx);
                }
                false
            })
            .on_cancel(move |_, _, _| dismissed.borrow_mut().take().is_some())
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("confirmation-cancel")
                            .label(cancel.clone())
                            .track_focus(&cancel_focus)
                            .on_click(move |_, window, cx| {
                                if cancelled.borrow_mut().take().is_some() {
                                    window.close_dialog(cx);
                                }
                            })
                            .map(|button| {
                                #[cfg(test)]
                                {
                                    gpui_kit::div()
                                        .debug_selector(|| "confirmation-cancel".into())
                                        .child(button)
                                }
                                #[cfg(not(test))]
                                {
                                    button
                                }
                            }),
                    )
                    .child(
                        Button::new("confirmation-confirm")
                            .label(confirm.clone())
                            .danger()
                            .track_focus(&confirm_focus)
                            .on_click(move |_, window, cx| {
                                confirmed(window, cx);
                            })
                            .map(|button| {
                                #[cfg(test)]
                                {
                                    gpui_kit::div()
                                        .debug_selector(|| "confirmation-danger".into())
                                        .child(button)
                                }
                                #[cfg(not(test))]
                                {
                                    button
                                }
                            }),
                    ),
            )
    });
    focus.focus(window, cx);
    Some(focus)
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

    fn setup(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<Cell<usize>>, FocusHandle) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_reduce_motion(true);
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let host = cx.new(|_| Host);
            gpui_kit::component::Root::new(host, window, cx)
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        let sends = Rc::new(Cell::new(0));
        let counter = Rc::clone(&sends);
        let focus = cx.update(|window, cx| {
            open(
                window,
                cx,
                "Sign out?".into(),
                "Remove this account.".into(),
                "Cancel".into(),
                "Sign out".into(),
                move |_, _| counter.set(counter.get() + 1),
            )
            .expect("no prior confirmation")
        });
        (cx, sends, focus)
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
        let (cx, sends, _) = setup(cx);
        for is_held in [false, true, true] {
            cx.simulate_event(KeyDownEvent {
                keystroke: Keystroke::parse("enter").unwrap(),
                is_held,
                prefer_character_input: false,
            });
        }
        paint(cx);
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn enter_on_cancel_cancels(cx: &mut TestAppContext) {
        let (cx, sends, cancel) = setup(cx);
        paint(cx);
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(cancel.clone())
        );
        paint(cx);
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(cancel.clone())
        );
        cx.simulate_keystrokes("tab");
        assert_ne!(
            cx.update(|window, cx| window.focused(cx)),
            Some(cancel.clone())
        );
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(cancel));
        press(cx, "enter");
        cx.run_until_parked();
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn escape_cancels(cx: &mut TestAppContext) {
        let (cx, sends, _) = setup(cx);
        paint(cx);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn danger_activation_sends_exactly_once(cx: &mut TestAppContext) {
        let (cx, sends, _) = setup(cx);
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

    #[gpui::test]
    fn tab_enter_confirms_exactly_once(cx: &mut TestAppContext) {
        let (cx, sends, _) = setup(cx);
        paint(cx);
        press(cx, "tab");
        press(cx, "enter");
        assert_eq!(sends.get(), 1);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        press(cx, "enter");
        assert_eq!(sends.get(), 1);
    }

    #[gpui::test]
    fn pointer_cancel_closes_without_sending(cx: &mut TestAppContext) {
        let (cx, sends, _) = setup(cx);
        paint(cx);
        let point = cx
            .debug_bounds("confirmation-cancel")
            .expect("cancel button")
            .center();
        cx.simulate_click(point, gpui_kit::Modifiers::default());
        paint(cx);
        assert_eq!(sends.get(), 0);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
    }

    #[gpui::test]
    fn pointer_confirms_exactly_once(cx: &mut TestAppContext) {
        let (cx, sends, _) = setup(cx);
        paint(cx);
        let point = cx
            .debug_bounds("confirmation-danger")
            .expect("danger button")
            .center();
        cx.simulate_click(point, gpui_kit::Modifiers::default());
        paint(cx);
        assert_eq!(sends.get(), 1);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        cx.simulate_click(point, gpui_kit::Modifiers::default());
        assert_eq!(sends.get(), 1);
    }

    #[gpui::test]
    fn another_open_is_rejected_while_active_or_exiting(cx: &mut TestAppContext) {
        let (cx, sends, cancel) = setup(cx);
        cx.update(|_, cx| cx.set_reduce_motion(false));
        paint(cx);
        let reject = |window: &mut Window, cx: &mut App| {
            open(
                window,
                cx,
                "Other".into(),
                "Other".into(),
                "Cancel".into(),
                "Confirm".into(),
                |_, _| panic!("rejected callback must not run"),
            )
        };
        assert_eq!(cx.update(reject), None);
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(cancel));
        cx.update(gpui_kit::component::WindowExt::close_dialog);
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        assert_eq!(cx.update(reject), None);
        cx.update(|window, cx| window.open_alert_dialog(cx, |alert, _, _| alert.title("Newer")));
        assert_eq!(cx.update(reject), None);
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        assert_eq!(sends.get(), 0);
    }
}
