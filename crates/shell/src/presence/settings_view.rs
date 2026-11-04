//! General fields and the Accounts tab, composed from retained Kit controls.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, scroll::ScrollableElement as _};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, div,
};
use kumochi_protocol::message::{MaxFrameRate, NameError};

use super::{Presence, PresenceEvent, frame_rate, send};
use crate::settings::{Error, Field, Settings};
use crate::strings::{Strings, Text};

pub(super) struct SettingsView {
    pub(super) model: Entity<Settings>,
    pub(super) owner: WeakEntity<Presence>,
    pub(super) confirmation: Option<u64>,
    events: async_channel::Sender<PresenceEvent>,
    inputs: [Entity<InputState>; 2],
    focus: FocusHandle,
    editing: Option<Field>,
    tab: usize,
    pub(super) stop: bool,
    _events: Vec<Subscription>,
    _watch: Subscription,
    /// Redraws the effective rate when the window's display or its refresh changes.
    _display: Subscription,
}

impl SettingsView {
    pub(super) fn new(
        model: Entity<Settings>,
        owner: WeakEntity<Presence>,
        events: async_channel::Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([KeyBinding::new("escape", Escape, Some("SettingsPanel"))]);
        let inputs = [Field::Assistant, Field::User].map(|field| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                let value = model.read(cx).value(field).to_owned();
                input.set_value(value, window, cx);
                input
            })
        });
        let subscriptions = [Field::Assistant, Field::User]
            .into_iter()
            .zip(&inputs)
            .map(|(field, input)| {
                cx.subscribe_in(input, window, move |view, input, event, window, cx| {
                    match event {
                        InputEvent::Focus => view.editing = Some(field),
                        InputEvent::Change => {
                            let value = input.read(cx).value();
                            view.model.update(cx, |state, cx| {
                                state.change(field, &value);
                                cx.notify();
                            });
                        }
                        InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur => {
                            if view.editing == Some(field) {
                                view.editing = None;
                            }
                            let _gone = view
                                .owner
                                .update(cx, |owner, cx| owner.save_setting(field, cx));
                            if matches!(event, InputEvent::PressEnter { .. }) {
                                window.focus(&view.focus, cx);
                            }
                        }
                        InputEvent::PressEnter { .. } => {}
                    }
                    cx.notify();
                })
            })
            .collect();
        let watch = cx.observe_in(&model, window, |view, _, window, cx| {
            let account = &view.model.read(cx).accounts;
            if view.confirmation.is_some_and(|generation| {
                account.generation != Some(generation)
                    || !matches!(
                        account.view,
                        Some(kumochi_protocol::message::AccountView::SignedIn { .. })
                    )
            }) {
                view.confirmation = None;
                window.close_dialog(cx);
                window.focus(&view.focus, cx);
            }
            for (field, input) in [Field::Assistant, Field::User]
                .into_iter()
                .zip(&view.inputs)
            {
                let value = view.model.read(cx).value(field);
                if input.read(cx).value().as_ref() != value {
                    let value = value.to_owned();
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                }
            }
            let _gone = view.owner.update(cx, |owner, cx| owner.refresh_dock(cx));
            cx.notify();
        });
        let view = cx.entity_id();
        let display = window.observe_window_display(move |_, cx| cx.notify(view));
        Self {
            model,
            owner,
            confirmation: None,
            events,
            inputs,
            focus: cx.focus_handle().tab_stop(true),
            editing: None,
            tab: 0,
            stop: false,
            _events: subscriptions,
            _watch: watch,
            _display: display,
        }
    }

    pub(super) fn editing(&self) -> Option<Field> {
        self.editing
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.focus(&self.focus, cx);
    }

    pub(super) fn finish_edit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.confirmation = None;
        window.close_all_dialogs(cx);
        self.editing = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(super) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(field) = self.editing.take() {
            self.model.update(cx, |state, cx| {
                state.cancel(field);
                cx.notify();
            });
        }
        window.focus(&self.focus, cx);
        cx.notify();
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

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let events = self.events.clone();
        let close = self.owner.clone();
        let stop_events = self.events.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .key_context("SettingsPanel")
            .track_focus(&self.focus)
            .on_action(move |_: &Escape, _, _| send(&events, PresenceEvent::Escape))
            .child(
                div()
                    .p_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().child(strings.get(Text::Settings)))
                    .child(
                        Button::new("close-settings")
                            .ghost()
                            .label(strings.get(Text::CloseSettings))
                            .on_click(move |_, _, cx| {
                                let owner = close.clone();
                                cx.defer(move |cx| {
                                    if let Ok(Err(error)) =
                                        owner.update(cx, Presence::dismiss_settings)
                                    {
                                        tracing::error!(%error, "can't dismiss settings");
                                    }
                                });
                            }),
                    ),
            )
            .child(
                div().px_4().child(
                    TabBar::new("settings-tabs")
                        .selected_index(self.tab)
                        .child(Tab::new().label(strings.get(Text::General)))
                        .child(Tab::new().label(strings.get(Text::Accounts)))
                        .on_click(cx.listener(|view, tab, window, cx| {
                            view.confirmation = None;
                            window.close_all_dialogs(cx);
                            view.tab = *tab;
                            cx.notify();
                        })),
                ),
            )
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .children((self.tab == 0).then(|| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_4()
                                    .children(self.model.read(cx).view().is_none().then(|| {
                                        div().text_sm().child(strings.get(Text::SettingsOffline))
                                    }))
                                    .child(self.field(Field::Assistant, cx))
                                    .child(self.field(Field::User, cx))
                                    .child(self.frame_rate(window, cx))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(strings.get(Text::SettingsSaveHelp)),
                                    )
                            }))
                            .children((self.tab == 1).then(|| self.accounts(cx))),
                    ),
            )
            .children(self.stop.then(|| {
                div().p_4().child(
                    Button::new("stop")
                        .label(strings.get(Text::Stop))
                        .on_click(move |_, _, _| send(&stop_events, PresenceEvent::Escape)),
                )
            }))
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
