use gpui_kit::component::input::Input;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme as _, Disableable as _};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, Window, div,
};
use kumochi_protocol::{
    BUILD_ID,
    message::{DockPosition, MaxFrameRate, NameError},
};

use super::super::{PresenceEvent, frame_rate, send};
use super::SettingsView;
use crate::settings::{Error, Field, Settings, TaskbarChoice};
use crate::strings::{Strings, Text};

impl SettingsView {
    pub(super) fn general(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let quit = self.events.clone();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(
                self.model
                    .read(cx)
                    .view()
                    .is_none()
                    .then(|| div().text_sm().child(strings.get(Text::SettingsOffline))),
            )
            .children(self.general_notice.map(|notice| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(notice))
            }))
            .children(self.model.read(cx).notice().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error))
            }))
            .child(self.field(Field::Assistant, cx))
            .child(self.field(Field::User, cx))
            .child(self.frame_rate(window, cx))
            .child(self.dock_positions(cx))
            .child(self.taskbar(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::SettingsSaveHelp)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child(strings.get(Text::SettingsVersion)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(BUILD_ID),
                            ),
                    )
                    .child(
                        Clipboard::new("settings-copy-build")
                            .value(BUILD_ID)
                            .accessibility_label(strings.get(Text::SettingsCopyBuild)),
                    ),
            )
            .child(
                Button::new("settings-quit")
                    .outline()
                    .label(strings.get(Text::Quit))
                    .on_click(move |_, _, _| send(&quit, PresenceEvent::Quit)),
            )
    }

    fn dock_positions(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let selected = state.dock_position_choice();
        let disabled = state.view().is_none();
        let label = strings.get(Text::DockPosition);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().child(label.clone()))
            .child(
                PositionGroup::new("settings-dock-position")
                    .aria_label(label)
                    .grid()
                    .grid_cols(3)
                    .gap_2()
                    .children(
                        DockPosition::ALL
                            .into_iter()
                            .zip(Text::POSITIONS)
                            .enumerate()
                            .map(|(ix, (position, label))| {
                                Radio::new(("dock-position", ix))
                                    .label(strings.get(label))
                                    .checked(selected == Some(position))
                                    .disabled(disabled)
                                    .on_change(cx.listener(move |view, _, _, cx| {
                                        let moved = view
                                            .owner
                                            .update(cx, |owner, cx| {
                                                owner.save_dock_position(position, cx)
                                            })
                                            .is_ok_and(|result| result.is_ok());
                                        view.general_notice =
                                            (!moved).then_some(Text::SettingsPositionUnavailable);
                                        cx.notify();
                                    }))
                            }),
                    ),
            )
    }

    fn taskbar(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        Checkbox::new("settings-taskbar")
            .label(cx.global::<Strings>().taskbar())
            .checked(state.taskbar_choice() == Some(TaskbarChoice::Shown))
            .disabled(state.view().is_none())
            .on_change(cx.listener(|view, checked, _, cx| {
                let choice = if *checked {
                    TaskbarChoice::Shown
                } else {
                    TaskbarChoice::Hidden
                };
                if view.model.read(cx).taskbar_choice() == Some(choice) {
                    return;
                }
                let sent = view
                    .owner
                    .update(cx, |owner, cx| owner.save_taskbar(choice, cx))
                    .unwrap_or(false);
                view.general_notice = (!sent).then_some(Text::SettingsUnavailable);
                cx.notify();
            }))
    }

    fn field(&self, field: Field, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let label = strings.get(match field {
            Field::Assistant => Text::AssistantName,
            Field::User => Text::UserName,
        });
        let error = state.field(field).error.as_ref().map(|error| match error {
            Error::Validation(NameError::Empty) => strings.get(Text::RenameEmpty),
            Error::Validation(NameError::TooLong) => strings.get(Text::RenameNameTooLong),
            Error::Validation(NameError::NotOneLine) => strings.get(Text::RenameOneLine),
            Error::Brain(error) => strings.error(error),
            Error::Offline => strings.get(if state.view().is_none() {
                Text::SettingsOffline
            } else {
                Text::SettingsUnavailable
            }),
        });
        let ix = usize::from(field == Field::User);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(label.clone()))
            .child(
                Input::new(&self.inputs[ix])
                    .aria_label(label)
                    .disabled(state.view().is_none())
                    .readonly(state.field(field).saving),
            )
            .children((field == Field::User).then(|| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::UserNameHelp))
            }))
            .children(
                state
                    .field(field)
                    .saving
                    .then(|| div().text_xs().child(strings.get(Text::SettingsSaving))),
            )
            .children(error.map(|error| div().text_sm().text_color(cx.theme().danger).child(error)))
    }

    /// Automatic, then each cap, with the rate a paced animation draws at now.
    fn frame_rate(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let view = self.model.read(cx).view();
        let cap = view.map(|view| view.max_frame_rate).unwrap_or_default();
        let selected = choice_index(self.model.read(cx));
        let label = strings.get(Text::MaxFrameRate);
        let owner = self.owner.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(label))
            .child(
                RadioGroup::horizontal("max-frame-rate")
                    .child(Radio::new("fps-auto").label(strings.get(Text::FrameRateAutomatic)))
                    .children(
                        MaxFrameRate::CAPS.map(|fps| {
                            Radio::new(("fps", usize::from(fps))).label(fps.to_string())
                        }),
                    )
                    .selected_index(Some(selected))
                    .disabled(view.is_none())
                    .on_click(move |&ix, _, cx| {
                        if let Some(rate) = rate_at(ix) {
                            let _gone =
                                owner.update(cx, |owner, cx| owner.save_max_frame_rate(rate, cx));
                        }
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    // The panel opens beside the dock, so it's on the dock's display.
                    .child(
                        strings.frame_rate(frame_rate::effective(cap, window.refresh_interval(cx))),
                    ),
            )
    }
}

/// The radio showing the newest choice, queued or saved: Kit gives a checked radio no click
/// handler, so showing the saved cap while a new one is queued would swallow a quick reversal.
fn choice_index(model: &Settings) -> usize {
    model
        .max_frame_rate_choice()
        .and_then(MaxFrameRate::fps)
        .and_then(|fps| MaxFrameRate::CAPS.iter().position(|&each| each == fps))
        .map_or(0, |ix| ix + 1)
}

/// The cap behind radio `ix`: Automatic, then each of [`MaxFrameRate::CAPS`].
fn rate_at(ix: usize) -> Option<MaxFrameRate> {
    match ix.checked_sub(1) {
        None => Some(MaxFrameRate::AUTOMATIC),
        Some(ix) => MaxFrameRate::CAPS
            .get(ix)
            .and_then(|&fps| MaxFrameRate::cap(fps).ok()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WeakEntity,
        WindowBounds, WindowOptions, px, size,
    };
    use kumochi_protocol::message::{AssistantName, MaxFrameRate, SettingsView, ThreadId};

    use super::{choice_index, rate_at};
    use crate::settings::{Settings, Update};
    use crate::strings::Strings;

    fn saved() -> SettingsView {
        SettingsView {
            assistant_name: AssistantName::new("Kumo").expect("valid"),
            user_name: None,
            onboarded: true,
            main_thread: ThreadId(1),
            max_frame_rate: MaxFrameRate::AUTOMATIC,
            default_model: None,
            dock_position: kumochi_protocol::message::DockPosition::TopCenter,
            show_in_taskbar: false,
        }
    }

    /// The Settings window as the shell opens it, over a saved Automatic cap and no owner.
    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<super::SettingsView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| {
                let mut model = Settings::default();
                assert_eq!(model.set_view(saved(), Update::Unsolicited), None);
                model
            });
            let (events, _) = async_channel::unbounded();
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(480.), px(640.)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    super::SettingsView::new(model, WeakEntity::new_invalid(), events, window, cx)
                })
            })
            .expect("a window")
        })
    }

    #[gpui::test]
    fn a_refresh_only_change_redraws_the_effective_rate(cx: &mut TestAppContext) {
        let (window, view) = open(cx);
        let display = cx
            .update_window(window, |_, window, cx| {
                window.display(cx).map(|display| display.id())
            })
            .expect("the window")
            .expect("a display");
        let notified = Rc::new(Cell::new(0));
        let _watch = cx.update(|cx| {
            let notified = Rc::clone(&notified);
            cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
        });
        let hertz_144 = Duration::from_secs(1) / 144;
        cx.simulate_display_refresh_interval_change(display, Some(hertz_144));
        assert_eq!(notified.get(), 1, "the view redraws its effective rate");
        let refresh = cx
            .update_window(window, |_, window, cx| window.refresh_interval(cx))
            .expect("the window");
        assert_eq!(refresh, Some(hertz_144));
    }

    #[gpui::test]
    fn account_confirmation_closes_on_disconnect_state_loss_or_new_session(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::component::WindowExt as _;
        use kumochi_protocol::message::{AccountView, PlanState};
        let (window, view) = open(cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        let model = cx.update(|cx| view.read(cx).model.clone());
        let signed_in = AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None,
        };
        for outcome in 0..3 {
            cx.update(|cx| {
                model.update(cx, |state, cx| {
                    assert!(state.accounts.receive(Some(1), 1, signed_in.clone()));
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.confirm_sign_out(window, cx));
                assert!(window.has_active_dialog(cx));
            })
            .expect("settings window");
            cx.update(|cx| {
                model.update(cx, |state, cx| {
                    match outcome {
                        0 => state.accounts.disconnected(),
                        1 => {
                            assert!(state.accounts.receive(Some(1), 1, AccountView::Busy));
                        }
                        _ => {
                            assert!(state.accounts.receive(Some(2), 2, signed_in.clone()));
                        }
                    }
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                assert!(!window.has_active_dialog(cx));
                window.draw(cx).clear(cx);
            })
            .expect("settings window");
        }
    }

    #[test]
    fn a_quick_reversal_lands_on_an_unchecked_radio() {
        let mut model = Settings::default();
        assert_eq!(model.set_view(saved(), Update::Unsolicited), None);
        assert_eq!(choice_index(&model), 0);
        let rate = rate_at(5).expect("a radio");
        assert_eq!(rate.fps(), Some(144));
        assert!(model.send_max_frame_rate(rate, |_| true));
        assert_eq!(
            choice_index(&model),
            5,
            "144 shows checked before the answer"
        );
        let automatic = rate_at(0).expect("a radio");
        assert_ne!(
            choice_index(&model),
            0,
            "so Automatic is unchecked and takes the click"
        );
        assert!(model.send_max_frame_rate(automatic, |_| true));
        assert_eq!(choice_index(&model), 0);
        assert_eq!(rate_at(6), None);
    }
}
use gpui_kit::base::RadioGroup as PositionGroup;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::clipboard::Clipboard;
