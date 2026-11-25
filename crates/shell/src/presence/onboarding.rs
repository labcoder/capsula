//! The first-run window: the welcome, the names, the accounts and the shortcut, over
//! [`crate::onboarding`]'s plain state. The names go through the Settings model and its save path;
//! nothing here keeps or logs one.

mod accounts;
#[cfg(test)]
mod close_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, Entity, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render,
    RenderImage, ScrollHandle, Styled as _, Subscription, Task, WeakEntity, Window, div, img, px,
    rems,
};
use kumochi_protocol::constants;
use kumochi_protocol::message::ControlToBrain;

use super::kumo_view::{SIZE, still};
use super::settings::name_input_event;
use super::{
    Foreground, LinkState, OpenError, PanelVisibility, Presence, PresenceEvent, PresenceSurface,
    configure_surface, options, platform, send, status_text,
};
use crate::onboarding::{Exit, Landing, Leave, Next, Onboarding, Step, ThisRun};
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
    model: Entity<Settings>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    inputs: [Entity<InputState>; 2],
    focus: FocusHandle,
    /// Kept for the shared name handler; Esc reads the Settings draft instead.
    editing: Option<Field>,
    /// Kumo's still pose under the welcome, once its bake lands.
    kumo: Option<Arc<RenderImage>>,
    _kumo_bake: Task<()>,
    /// A `ChatGPT` sign-in this window asked for completed: the plan line shows.
    plan: bool,
    account_scroll: ScrollHandle,
    _events: Vec<Subscription>,
    _watch: Subscription,
    _focus_lost: Subscription,
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
                    // Enter keeps focus in the input, so an error can be fixed in place.
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
        let scale = window.scale_factor();
        let kumo_bake = cx.spawn(async move |view, cx| {
            let started = Instant::now();
            let image = cx
                .background_executor()
                .spawn(async move { still(scale) })
                .await;
            let baked_us = started.elapsed().as_micros();
            let kept = view.update(cx, |view, cx| {
                view.kumo = image.map(Arc::new);
                if view.kumo.is_some() {
                    super::diagnostics::record(
                        cx,
                        format_args!("onboarding kumo baked us={baked_us}"),
                    );
                }
                cx.notify();
            });
            if kept.is_err() {
                tracing::debug!("onboarding closed before Kumo's bake landed");
            }
        });
        let focus_lost = cx.on_focus_lost(window, |view, window, cx| {
            super::settings::restore_lost_focus(&view.focus, window, cx);
            cx.notify();
        });
        Self {
            state,
            link: LinkState::Connected,
            model,
            owner,
            events,
            inputs,
            focus: cx.focus_handle(),
            editing: None,
            kumo: None,
            _kumo_bake: kumo_bake,
            plan: false,
            account_scroll: ScrollHandle::new(),
            _events: subscriptions,
            _watch: watch,
            _focus_lost: focus_lost,
        }
    }

    /// Whether the step's save or the finish is in flight.
    fn busy(&self, cx: &App) -> bool {
        self.state
            .step()
            .field()
            .is_some_and(|field| self.model.read(cx).field(field).saving)
            || self.state.finishing()
    }

    /// The step's words around its input, then its help line; the account step's sections; the
    /// shortcut.
    fn body(&self, cx: &Context<'_, Self>) -> AnyElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let (Some(field), Some(input)) = (self.state.step().field(), self.input()) else {
            return if self.state.step() == Step::Shortcut {
                self.shortcut(cx).into_any_element()
            } else {
                self.accounts(cx).into_any_element()
            };
        };
        let input = Input::new(input)
            .aria_label(strings.get(match field {
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
        match field {
            Field::Assistant => {
                let (before, after) = strings.onboarding_welcome();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        // Centred: taffy gets no text baselines from GPUI, only box bottoms.
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_x_2()
                            .text_xl()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(before)
                            .child(div().flex_1().min_w(rems(12.0)).child(input.large()))
                            .children((!after.is_empty()).then_some(after)),
                    )
                    .child(
                        // Reserved before the bake lands, so the welcome doesn't move.
                        div()
                            .size(px(SIZE))
                            .flex_none()
                            .self_center()
                            .children(self.kumo.clone().map(|image| img(image).size_full())),
                    )
                    .child(help(strings.get(Text::OnboardingWelcomeHelp)))
                    .into_any_element()
            }
            Field::User => {
                let [question, user_help] = strings.onboarding_user(state.value(Field::Assistant));
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_xl().child(question))
                    .child(input)
                    .child(help(user_help))
                    .into_any_element()
            }
        }
    }

    /// The shortcut's sentence with its keys as a keycap, then its help line.
    fn shortcut(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let [before, after, help] =
            strings.onboarding_shortcut(self.model.read(cx).value(Field::Assistant));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .text_xl()
                    .child(before)
                    .child(
                        div()
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().border)
                            .child(strings.get(Text::ShortcutKeys)),
                    )
                    .children((!after.is_empty()).then_some(after)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(help),
            )
    }

    /// The plan line, then the one button: Continue; Not now on the account step until a model
    /// connection is usable; Finish, not primary, at the shortcut, whose real action is the
    /// double-tap.
    fn footer(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let step = self.state.step();
        let skip = step == Step::Accounts && !accounts::usable(state);
        let next = self.owner.clone();
        let button = Button::new("onboarding-continue")
            .debug_selector(|| "onboarding-continue".into())
            .tab_index(1)
            .label(strings.get(if step == Step::Shortcut {
                Text::OnboardingFinish
            } else if skip {
                Text::OnboardingNotNow
            } else {
                Text::OnboardingContinue
            }))
            .loading(self.busy(cx))
            .disabled(state.view().is_none())
            .on_click(move |_, _, cx| go_on(&next, cx));
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap_3()
            .children(self.plan.then(|| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::OnboardingPlan))
            }))
            .child(if skip {
                button.ghost()
            } else if step == Step::Shortcut {
                button
            } else {
                button.primary()
            })
    }

    fn input(&self) -> Option<&Entity<InputState>> {
        let field = self.state.step().field()?;
        Some(&self.inputs[usize::from(field == Field::User)])
    }

    /// Focus goes into the step's input, `select` selecting its text; a step with none focuses
    /// the window, so Enter starts nothing there.
    pub(super) fn focus_input(
        &self,
        select: Select,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(input) = self.input() else {
            window.focus(&self.focus, cx);
            return;
        };
        input.update(cx, |input, cx| {
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
        let online = state.view().is_some();
        let (saving, error) = step
            .field()
            .map_or((None, None), |field| state.field_status(field, strings));
        let unsent = self.state.unsent();
        let failed = error.is_some() || state.finish_refusal().is_some() || unsent;
        let status = saving
            .or(error)
            .or_else(|| state.finish_refusal().map(|error| strings.error(error)))
            .or_else(|| unsent.then(|| strings.get(Text::SettingsUnavailable)))
            .or_else(|| {
                (!online && self.link != LinkState::Connected)
                    .then(|| strings.get(status_text(self.link)))
            });
        let body = self.body(cx);
        let events = self.events.clone();
        let close = self.owner.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded_lg()
            .border_1()
            .border_color(if self.focus.is_focused(window) {
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
                    .child(
                        div()
                            .text_lg()
                            .text_color(cx.theme().muted_foreground)
                            .child(strings.onboarding_title()),
                    )
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
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(body)
                    .children((step != Step::Accounts || status.is_some()).then(|| {
                        div()
                            .h(rems(1.25))
                            .text_sm()
                            .text_color(if failed {
                                cx.theme().danger
                            } else {
                                cx.theme().muted_foreground
                            })
                            .children(status)
                    })),
            )
            .child(self.footer(cx))
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
            let (from, next) = panel.view.update(cx, |view, cx| {
                view.link = link;
                cx.notify();
                (
                    view.state.step(),
                    view.state.settings_changed(settings.read(cx)),
                )
            });
            return self.onboarding_next(from, next, cx);
        }
        let Some(state) = settings
            .read(cx)
            .view()
            .and_then(|view| Onboarding::begin(view, self.onboarding_run))
        else {
            return Ok(());
        };
        let opened = self.open_onboarding(state, cx);
        if opened.is_err() {
            self.onboarding_run = ThisRun::PutOff;
            self.close_onboarding(cx)?;
        }
        opened
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
        if let Some(field) = view.read(cx).state.step().field() {
            self.save_setting(field, cx);
        }
        let settings = self.settings.clone();
        let (from, next) = view.update(cx, |view, cx| {
            cx.notify();
            (view.state.step(), view.state.submit(settings.read(cx)))
        });
        self.onboarding_next(from, next, cx)
    }

    /// A double-tap while the window shows: used up, and at the shortcut it asks for the finish
    /// that opens typing.
    pub(super) fn onboarding_double_tap(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        let state = &panel.view.read(cx).state;
        let (from, next) = (state.step(), state.double_tap(self.settings.read(cx)));
        self.onboarding_next(from, next, cx)
    }

    /// What the plain state decided, `from` being the step that was showing when it decided.
    fn onboarding_next(
        &mut self,
        from: Step,
        next: Next,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        if next == Next::Moved {
            let (link, session) = (&self.link, &self.session);
            let recorded = self.settings.update(cx, |state, _| {
                state.complete_onboarding_step(from.recorded(), |op| {
                    session.as_ref().is_some_and(|session| {
                        link.send_control(session, ControlToBrain::Settings(op))
                    })
                })
            });
            if !recorded {
                tracing::info!(step = ?from, "onboarding step not recorded");
            }
        }
        match next {
            Next::Stay => Ok(()),
            Next::Moved => {
                if from.hands_over_sign_ins() {
                    self.onboarding_chatgpt.hand_over();
                    self.onboarding_claude.hand_over();
                }
                tracing::info!(step = ?panel.view.read(cx).state.step(), "onboarding moved on");
                panel
                    .window
                    .update(cx, |_, window, cx| {
                        panel
                            .view
                            .update(cx, |view, cx| view.focus_input(Select::All, window, cx));
                    })
                    .map_err(|error| OpenError::Gpui(error.into()))
            }
            Next::Finish(landing) => {
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
                        view.state.finish_queued(landing);
                    } else {
                        view.state.finish_unsent();
                    }
                    cx.notify();
                });
                Ok(())
            }
            Next::Close(landing) => {
                let plan =
                    (panel.view.read(cx).state).leave(Exit::Close(landing), self.settings.read(cx));
                self.leave_onboarding(plan, cx)
            }
        }
    }

    /// Runs the plain state's plan for the window going, in its order.
    fn leave_onboarding(
        &mut self,
        plan: Vec<Leave>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        for step in plan {
            match step {
                Leave::Save(field) => {
                    self.save_setting(field, cx);
                }
                Leave::CancelEdit => self.cancel_onboarding_edit(cx),
                Leave::CancelSignIns => self.cancel_onboarding_sign_ins(cx),
                Leave::HandOverSignIns => {
                    self.onboarding_chatgpt.hand_over();
                    self.onboarding_claude.hand_over();
                }
                Leave::PutOff => {
                    self.onboarding_run = ThisRun::PutOff;
                    tracing::info!("onboarding put off for this run");
                }
                Leave::Close => self.close_onboarding(cx)?,
                Leave::Land(Landing::Peek) => self.set_panel(PanelVisibility::Visible, cx)?,
                Leave::Land(Landing::Typing) => self.focus_text(cx)?,
                Leave::Land(Landing::Quiet) => {}
            }
        }
        Ok(())
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
        self.set_onboarding(
            OnboardingPanel {
                window,
                view,
                bounds: [0; 4],
                previous: None,
            },
            cx,
        );
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

    /// Holds the window, pauses holds while it shows and adopts a sign-in already waiting. A hold
    /// already recognised ends through `cancel_voice` with input cleared, so Esc and the next hold
    /// work after it: cancelled at once if not yet recording, stopping until its microphone joins
    /// if it is.
    fn set_onboarding(&mut self, panel: OnboardingPanel, cx: &mut Context<'_, Self>) {
        self.onboarding = Some(panel);
        self.voice.paused.set(true);
        self.cancel_voice(cx);
        self.adopt_onboarding_sign_ins(cx);
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
                        // A taskbar click would otherwise make Kumochi's own window the one restored.
                        if panel.previous.is_none() {
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
    /// no finish is sent. A name edit still in the field is saved, as closing Settings saves one;
    /// one that can't be sent is dropped, so no draft or notice outlives the window. A sign-in
    /// this window asked for that hasn't ended is cancelled, or the cancel stays owed.
    pub(super) fn dismiss_onboarding(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        let plan = (panel.view.read(cx).state).leave(Exit::Dismiss, self.settings.read(cx));
        self.leave_onboarding(plan, cx)
    }

    /// Esc with an unsaved name: the field goes back to the saved name and keeps focus.
    pub(super) fn cancel_onboarding_edit(&mut self, cx: &mut Context<'_, Self>) {
        let Some(panel) = &self.onboarding else {
            return;
        };
        let Some(field) = panel.view.read(cx).state.step().field() else {
            return;
        };
        self.settings.update(cx, |state, cx| {
            state.cancel(field);
            cx.notify();
        });
    }

    /// Whether Esc cancels the onboarding step's unsaved name.
    pub(super) fn onboarding_editing(&self, cx: &gpui_kit::App) -> bool {
        self.onboarding.as_ref().is_some_and(|panel| {
            panel
                .view
                .read(cx)
                .state
                .step()
                .field()
                .is_some_and(|field| self.settings.read(cx).editing(field))
        })
    }

    /// Removes the window first and only then lets go of it and unpauses holds, as `hide_text`
    /// does, so a close that fails keeps both and a later one can try again.
    fn close_onboarding(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(panel) = &self.onboarding else {
            return Ok(());
        };
        let previous = panel.previous;
        panel
            .window
            .update(cx, |_, window, _| {
                if let Some(previous) = previous {
                    platform::restore_foreground(window, previous);
                }
                window.remove_window();
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        self.onboarding = None;
        self.voice.paused.set(false);
        tracing::info!("onboarding closed");
        self.place_drawings(cx.weak_entity(), cx);
        self.pointer_changed(cx)
    }
}
