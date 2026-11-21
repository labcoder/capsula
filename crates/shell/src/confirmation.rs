//! Cancel-first confirmation for consequential shell actions.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{AlertDialog, DialogFooter},
};
use gpui_kit::prelude::*;
use gpui_kit::{App, FocusHandle, Global, SharedString, Window, WindowId};

#[derive(Default)]
struct Confirmations(Vec<(WindowId, Weak<()>)>);

impl Global for Confirmations {}

pub(crate) struct Handle {
    cancel: FocusHandle,
    modal: FocusHandle,
    lifetime: Weak<()>,
}

impl Handle {
    pub(crate) fn live(&self) -> bool {
        self.lifetime.strong_count() > 0
    }

    pub(crate) fn dismiss(&self, window: &mut Window, cx: &mut App) {
        if self.live()
            && (self.cancel.is_focused(window) || self.modal.contains_focused(window, cx))
            && window.has_active_dialog(cx)
        {
            window.close_dialog(cx);
        }
    }
}

/// Opens an alert with Cancel focused; Enter activates only a focused button.
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
    open_content(
        window,
        cx,
        title,
        move |alert, _, _| alert.description(body.clone()),
        cancel,
        confirm,
        move |confirmed, window, cx| {
            if confirmed {
                on_confirm(window, cx);
            }
        },
    )
    .map(|handle| handle.cancel)
}

pub(crate) fn open_content(
    window: &mut Window,
    cx: &mut App,
    title: SharedString,
    content: impl Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static,
    cancel: SharedString,
    confirm: SharedString,
    on_decision: impl FnOnce(bool, &mut Window, &mut App) + 'static,
) -> Option<Handle> {
    open_dialog(
        window,
        cx,
        title,
        content,
        (cancel, confirm),
        false,
        on_decision,
    )
}

/// Opens a new confirmation above an inert exit transition, never above a live dialog.
pub(crate) fn open_during_exit(
    window: &mut Window,
    cx: &mut App,
    title: SharedString,
    body: SharedString,
    cancel: SharedString,
    confirm: SharedString,
    on_decision: impl FnOnce(bool, &mut Window, &mut App) + 'static,
) -> Option<Handle> {
    open_dialog(
        window,
        cx,
        title,
        move |alert, _, _| alert.description(body.clone()),
        (cancel, confirm),
        true,
        on_decision,
    )
}

fn open_dialog(
    window: &mut Window,
    cx: &mut App,
    title: SharedString,
    content: impl Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static,
    (cancel, confirm): (SharedString, SharedString),
    allow_during_exit: bool,
    on_decision: impl FnOnce(bool, &mut Window, &mut App) + 'static,
) -> Option<Handle> {
    if window.has_active_dialog(cx) {
        return None;
    }
    let id = window.window_handle().window_id();
    let guards = cx.default_global::<Confirmations>();
    guards.0.retain(|(_, guard)| guard.strong_count() > 0);
    if !allow_during_exit && guards.0.iter().any(|(window, _)| *window == id) {
        return None;
    }
    let lifetime = Rc::new(());
    let weak = Rc::downgrade(&lifetime);
    guards.0.push((id, Weak::clone(&weak)));
    let cancel_focus = cx.focus_handle().tab_stop(true);
    let confirm_focus = cx.focus_handle().tab_stop(true);
    let focus = cancel_focus.clone();
    let pending = Rc::new(RefCell::new(Some(on_decision)));
    let finish = Rc::new(move |confirmed: bool, window: &mut Window, cx: &mut App| {
        let callback = pending.borrow_mut().take();
        if let Some(callback) = callback {
            window.close_dialog(cx);
            callback(confirmed, window, cx);
        }
    });
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let _lifetime = Rc::clone(&lifetime);
        let enter = Rc::clone(&finish);
        let dismissed = Rc::clone(&finish);
        let cancelled = Rc::clone(&finish);
        let confirmed = Rc::clone(&finish);
        let enter_focus = cancel_focus.clone();
        let enter_confirm_focus = confirm_focus.clone();
        content(alert.title(title.clone()), window, cx)
            .on_ok(move |_, window, cx| {
                if enter_focus.is_focused(window) {
                    enter(false, window, cx);
                } else if enter_confirm_focus.is_focused(window) {
                    enter(true, window, cx);
                }
                false
            })
            .on_cancel(move |_, window, cx| {
                dismissed(false, window, cx);
                false
            })
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("confirmation-cancel")
                            .label(cancel.clone())
                            .track_focus(&cancel_focus)
                            .on_click(move |_, window, cx| cancelled(false, window, cx))
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
                            .on_click(move |_, window, cx| confirmed(true, window, cx))
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
    let modal = window
        .focused(cx)
        .expect("opening a dialog focuses its shell");
    focus.focus(window, cx);
    Some(Handle {
        cancel: focus,
        modal,
        lifetime: weak,
    })
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
