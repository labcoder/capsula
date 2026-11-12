//! The first-run window: the welcome and the names, over [`crate::onboarding`]'s plain state.
//! The names go through the Settings model and its save path; nothing here keeps or logs one.

#[cfg(test)]
mod tests;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, Styled as _,
    Subscription, WeakEntity, Window, div, rems,
};
use kumochi_protocol::constants;
use kumochi_protocol::message::ControlToBrain;

use super::settings::{field_status, name_input_event};
use super::{
    Foreground, LinkState, OpenError, PanelVisibility, Presence, PresenceEvent, PresenceSurface,
    configure_surface, options, platform, send, status_text,
};
use crate::onboarding::{Landing, Next, Onboarding, Step};
use crate::settings::{Field, Settings};
use crate::strings::{Strings, Text};

/// The window's width and height in rem.
const SIZE_REMS: [f32; 2] = [30.0, 22.0];

pub(super) struct OnboardingPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<OnboardingView>,
    pub(super) bounds: [i32; 4],
    previous: Option<Foreground>,
}

pub(super) struct OnboardingView {
    pub(super) state: Onboarding,
    /// The link's state, for the status line while there's no settings view.
    link: LinkState,
    /// The last finish couldn't be sent.
    unsent: bool,
    model: Entity<Settings>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    inputs: [Entity<InputState>; 2],
    focus: FocusHandle,
    editing: Option<Field>,
    _events: Vec<Subscription>,
    _watch: Subscription,
}

impl OnboardingView {
    fn new(
        state: Onboarding,
        model: Entity<Settings>,
        owner: WeakEntity<Presence>,
        events: async_channel::Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([KeyBinding::new("escape", Escape, Some("OnboardingPanel"))]);
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
                    let focus = input.read(cx).focus_handle(cx);
                    name_input_event(
                        field,
                        input,
                        event,
                        &mut view.editing,
                        &focus,
                        &view.model,
                        &view.owner,
                        window,
                        cx,
                    );
                    if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                        go_on(&view.owner, cx);
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
            cx.notify();
        });
        Self {
            state,
            link: LinkState::Connected,
            unsent: false,
            model,
            owner,
            events,
            inputs,
            focus: cx.focus_handle(),
            editing: None,
            _events: subscriptions,
            _watch: watch,
        }
    }

    /// Whether the step's save or the finish is in flight.
    fn busy(&self, cx: &App) -> bool {
        self.model.read(cx).field(self.state.step().field()).saving || self.state.finishing()
    }

    /// The step's words around its input, then its help line.
    fn body(&self, cx: &Context<'_, Self>) -> Div {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let input = Input::new(self.input())
            .aria_label(strings.get(match self.state.step().field() {
                Field::Assistant => Text::AssistantName,
                Field::User => Text::UserName,
            }))
            .disabled(state.view().is_none())
            .readonly(self.busy(cx));
        let help = |text: String| {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        match self.state.step() {
            Step::Welcome => {
                let (before, after) = strings.onboarding_welcome();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_baseline()
                            .gap_x_2()
                            .text_2xl()
                            .child(before)
                            .child(div().w(rems(12.0)).child(input))
                            .child(after),
                    )
                    .child(help(strings.get(Text::OnboardingWelcomeHelp)))
            }
            Step::YourName => {
                let [question, user_help] = strings.onboarding_user(state.value(Field::Assistant));
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_xl().child(question))
                    .child(input)
                    .child(help(user_help))
            }
        }
    }

    fn input(&self) -> &Entity<InputState> {
        &self.inputs[usize::from(self.state.step() == Step::YourName)]
    }

    /// Focus goes into the step's input; `select` selects its text.
    pub(super) fn focus_input(
        &self,
        select: Select,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.input().update(cx, |input, cx| {
            input.focus(window, cx);
            if select == Select::All {
                input.select_all(window, cx);
            }
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Select {
    All,
    Keep,
}

/// Continue, from the button or Enter, once the current event is done.
fn go_on(owner: &WeakEntity<Presence>, cx: &mut gpui_kit::App) {
    let owner = owner.clone();
    cx.defer(move |cx| {
        if let Ok(Err(error)) = owner.update(cx, Presence::onboarding_continue) {
            tracing::error!(%error, "can't go on in onboarding");
        }
    });
}

/// Finish later, from the button or the native close, once the current event is done.
fn put_off(owner: &WeakEntity<Presence>, cx: &mut gpui_kit::App) {
    let owner = owner.clone();
    cx.defer(move |cx| {
        if let Ok(Err(error)) = owner.update(cx, Presence::dismiss_onboarding) {
            tracing::error!(%error, "can't close onboarding");
        }
    });
}

impl Render for OnboardingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let step = self.state.step();
        let field = step.field();
        let online = state.view().is_some();
        let busy = self.busy(cx);
        let (saving, error) = field_status(state, field, strings);
        let failed = error.is_some() || state.finish_refusal().is_some() || self.unsent;
        let status = saving
            .or(error)
            .or_else(|| state.finish_refusal().map(|error| strings.error(error)))
            .or_else(|| self.unsent.then(|| strings.get(Text::SettingsUnavailable)))
            .or_else(|| {
                (!online && self.link != LinkState::Connected)
                    .then(|| strings.get(status_text(self.link)))
            });
        let body = self.body(cx);
        let events = self.events.clone();
        let (close, next) = (self.owner.clone(), self.owner.clone());
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded_lg()
            .border_1()
            .border_color(if self.focus.contains_focused(window, cx) {
                cx.theme().ring
            } else {
                cx.theme().border
            })
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .key_context("OnboardingPanel")
            .track_focus(&self.focus)
            .on_action(move |_: &Escape, _, _| send(&events, PresenceEvent::Escape))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().child(strings.onboarding_title()))
                    .child(
                        Button::new("onboarding-close")
                            .debug_selector(|| "onboarding-close".into())
                            .ghost()
                            .tab_index(2)
                            .label(strings.get(Text::OnboardingClose))
                            .on_click(move |_, _, cx| put_off(&close, cx)),
                    ),
            )
            .child(
                div().flex_1().flex().flex_col().gap_2().child(body).child(
                    div()
                        .h(rems(1.25))
                        .text_sm()
                        .text_color(if failed {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .children(status),
                ),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("onboarding-continue")
                        .debug_selector(|| "onboarding-continue".into())
                        .primary()
                        .tab_index(1)
                        .label(strings.get(Text::OnboardingContinue))
                        .loading(busy)
                        .disabled(!online)
                        .on_click(move |_, _, cx| go_on(&next, cx)),
                ),
            )
    }
}

impl Presence {
    /// The launch decision and the window's follow-up, after a settings view or a link change.
    pub(super) fn onboarding_changed(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let settings = self.settings.clone();
        let link = self.state;
        if let Some(panel) = &self.onboarding {
            let next = panel.view.update(cx, |view, cx| {
                view.link = link;
                cx.notify();
                view.state.settings_changed(settings.read(cx))
            });
            return self.onboarding_next(next, cx);
        }
        match settings.read(cx).view().and_then(Onboarding::begin) {
            Some(state) if !self.onboarding_later => {
                let opened = self.open_onboarding(state, cx);
                if opened.is_err() {
                    self.onboarding_later = true;
                    self.close_onboarding(cx)?;
                }
                opened
            }
            _ => Ok(()),
        }
    }

    /// Continue or Enter: the step's name is saved, then the plain state decides.
    pub(super) fn onboarding_continue(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        if self.settings.read(cx).view().is_none() {
            return Ok(());
        }
        let view = panel.view.clone();
        self.save_setting(view.read(cx).state.step().field(), cx);
        let settings = self.settings.clone();
        let next = view.update(cx, |view, cx| {
            view.unsent = false;
            cx.notify();
            view.state.submit(settings.read(cx))
        });
        self.onboarding_next(next, cx)
    }

    fn onboarding_next(&mut self, next: Next, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        match next {
            Next::Stay => Ok(()),
            Next::Moved => {
                tracing::info!(step = ?panel.view.read(cx).state.step(), "onboarding moved on");
                panel
                    .window
                    .update(cx, |_, window, cx| {
                        panel
                            .view
                            .update(cx, |view, cx| view.focus_input(Select::Keep, window, cx));
                    })
                    .map_err(|error| OpenError::Gpui(error.into()))
            }
            Next::Finish => {
                let view = panel.view.clone();
                let (link, session) = (&self.link, &self.session);
                let queued = self.settings.update(cx, |state, cx| {
                    cx.notify();
                    state.finish_onboarding(|op| {
                        session.as_ref().is_some_and(|session| {
                            link.send_control(session, ControlToBrain::Settings(op))
                        })
                    })
                });
                view.update(cx, |view, cx| {
                    if queued {
                        view.state.finish_queued(Landing::Peek);
                    }
                    view.unsent = !queued;
                    cx.notify();
                });
                Ok(())
            }
            Next::Close(landing) => {
                self.close_onboarding(cx)?;
                if landing == Landing::Peek {
                    self.set_panel(PanelVisibility::Visible, cx)?;
                }
                Ok(())
            }
        }
    }

    fn open_onboarding(
        &mut self,
        state: Onboarding,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.dismiss_text(cx)?;
        self.dismiss_threads(cx)?;
        self.dismiss_settings(cx)?;
        let model = self.settings.clone();
        let owner = cx.weak_entity();
        let events = self.events.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(constants::DISPLAY_NAME);
            let close = owner.clone();
            window.on_window_should_close(cx, move |_, cx| {
                put_off(&close, cx);
                false
            });
            cx.new(|cx| OnboardingView::new(state, model, owner, events, window, cx))
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        self.onboarding = Some(OnboardingPanel {
            window,
            view,
            bounds: [0; 4],
            previous: None,
        });
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, self.events.clone(), cx)?;
                self.input_windows
                    .bind(crate::presenter::overlay_input::Layer::Panels, window)?;
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.protect_surface(super::exclusion::Slot::Onboarding, cx)?;
        self.pointer_settle = None;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        tracing::info!("onboarding opened");
        self.show_onboarding(Select::All, cx)
    }

    /// Places the window under the dock and brings it to the front with focus in its input. If
    /// Windows refuses the activation, it shows unfocused and a click focuses it.
    pub(super) fn show_onboarding(
        &mut self,
        select: Select,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let area = platform::work_area()?;
        let (layout, scale) = (self.layout, self.scale);
        let Some(panel) = self.onboarding.as_mut() else {
            return Ok(());
        };
        panel
            .window
            .update(cx, |_, window, cx| {
                let rem = f32::from(window.rem_size());
                panel.bounds = layout.anchored_panel(
                    area,
                    SIZE_REMS.map(|value| super::scaled(value * rem, scale)),
                );
                platform::place_presence_window(window, panel.bounds)?;
                window.bounds_changed(cx);
                match platform::activate_presence_window(window) {
                    Ok(previous) => {
                        if previous.is_some() {
                            panel.previous = previous;
                        }
                        panel
                            .view
                            .update(cx, |view, cx| view.focus_input(select, window, cx));
                    }
                    Err(error) => tracing::warn!(%error, "can't focus the onboarding window"),
                }
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.place_drawings(cx.weak_entity(), cx);
        Ok(())
    }

    /// Finish later, Esc at the top level or the native close: the window goes for this run and
    /// no finish is sent. A name edit still in the field is saved, as closing Settings saves one.
    pub(super) fn dismiss_onboarding(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        let field = panel.view.read(cx).state.step().field();
        if self.settings.read(cx).editing(field) {
            self.save_setting(field, cx);
        }
        self.onboarding_later = true;
        tracing::info!("onboarding put off for this run");
        self.close_onboarding(cx)
    }

    /// Esc with an unsaved name: the field goes back to the saved name and keeps focus.
    pub(super) fn cancel_onboarding_edit(&mut self, cx: &mut Context<'_, Self>) {
        let Some(panel) = &self.onboarding else {
            return;
        };
        let field = panel.view.read(cx).state.step().field();
        self.settings.update(cx, |state, cx| {
            state.cancel(field);
            cx.notify();
        });
    }

    /// Whether Esc cancels the onboarding step's unsaved name.
    pub(super) fn onboarding_editing(&self, cx: &gpui_kit::App) -> bool {
        self.onboarding.as_ref().is_some_and(|panel| {
            self.settings
                .read(cx)
                .editing(panel.view.read(cx).state.step().field())
        })
    }

    fn close_onboarding(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(panel) = self.onboarding.take() else {
            return Ok(());
        };
        panel
            .window
            .update(cx, |_, window, _| {
                if let Some(previous) = panel.previous {
                    platform::restore_foreground(window, previous);
                }
                window.remove_window();
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        tracing::info!("onboarding closed");
        self.place_drawings(cx.weak_entity(), cx);
        self.pointer_changed(cx)
    }
}
