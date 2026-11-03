//! General fields and the reserved Accounts tab, composed from retained Kit controls.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, scroll::ScrollableElement as _};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, Styled as _, Subscription, WeakEntity, Window, div,
};
use kumochi_protocol::message::NameError;

use super::{Presence, PresenceEvent, send};
use crate::settings::{Error, Field, Settings};
use crate::strings::{Strings, Text};

pub(super) struct SettingsView {
    model: Entity<Settings>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    inputs: [Entity<InputState>; 2],
    focus: FocusHandle,
    editing: Option<Field>,
    tab: usize,
    pub(super) stop: bool,
    _events: Vec<Subscription>,
    _watch: Subscription,
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
        Self {
            model,
            owner,
            events,
            inputs,
            focus: cx.focus_handle().tab_stop(true),
            editing: None,
            tab: 0,
            stop: false,
            _events: subscriptions,
            _watch: watch,
        }
    }

    pub(super) fn editing(&self) -> Option<Field> {
        self.editing
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.focus(&self.focus, cx);
    }

    pub(super) fn finish_edit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
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
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
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
                        .on_click(cx.listener(|view, tab, _, cx| {
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
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(strings.get(Text::SettingsSaveHelp)),
                                    )
                            })),
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
