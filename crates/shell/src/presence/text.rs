//! The reusable typing surface and its activation lifetime.

mod recovery;

use std::rc::Rc;

use async_channel::Sender;
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputEvent};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, WeakEntity, Window, div,
};
use kumochi_shell_core::presence::typing::{self, Admission, Submission};
pub(super) use kumochi_shell_core::presence::typing::{Hide, Typing};

use super::messages::Messages;
use super::{
    Context, Diagnostics, Entity, Foreground, InputState, OpenError, PanelVisibility, Presence,
    PresenceEvent, PresenceSurface, SharedString, configure_surface, constants, diagnostics,
    options, platform, send,
};
use crate::chat_notice::{Notice, Source};
use crate::runs::Target;
use crate::settings::Settings;
use crate::strings::{Strings, Text};
use crate::threads::{Threads, row_label};

/// The typing surface: the one window Kumochi activates, and only on a double tap.
pub(super) struct TextPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<TextView>,
    pub(super) typing: Typing,
    /// Its window is active, so it keeps Esc from speech.
    pub(super) focused: bool,
    pub(super) bounds: [i32; 4],
    pub(super) input: Entity<InputState>,
    /// The window in front before the panel took focus, given back when it closes.
    previous: Option<Foreground>,
    /// Input diagnostics only: the draft at the last dismiss, compared on the next open.
    draft: Option<SharedString>,
    activation: Option<gpui_kit::Subscription>,
    /// Enter sends the draft as a turn.
    _submit: gpui_kit::Subscription,
}

#[cfg(test)]
impl TextPanel {
    /// A hidden typing panel with its real view and input in a GPUI test window, without
    /// `create_text`'s native surface, capture, activation or Enter wiring.
    pub(super) fn detached(
        model: Entity<Threads>,
        settings: Entity<Settings>,
        cx: &mut Context<'_, Presence>,
    ) -> Self {
        let (events, _closed) = async_channel::unbounded();
        let owner = cx.weak_entity();
        let (window, view) =
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| TextView::new(model, settings, owner, events, window, cx))
            })
            .expect("a GPUI test window opens");
        let input = view.read(cx).input.clone();
        Self {
            window,
            view,
            typing: Typing::default(),
            focused: false,
            bounds: [0; 4],
            input,
            previous: None,
            draft: None,
            activation: None,
            _submit: gpui_kit::Subscription::new(|| {}),
        }
    }
}

impl Presence {
    pub(super) fn show_voice_answer(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.dismiss_settings(cx)?;
        self.dismiss_threads(cx)?;
        if self.text.is_none() {
            self.text = Some(self.create_text(cx)?);
        }
        self.protect_surface(super::exclusion::Slot::Text, cx)?;
        self.pointer_settle = None;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        let rem = self
            .text
            .as_ref()
            .expect("created above")
            .window
            .update(cx, |_, window, _| f32::from(window.rem_size()))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        let bounds = self.layout.anchored_panel(
            platform::work_area()?,
            [30.0, 34.0].map(|value| super::scaled(value * rem, self.scale)),
        );
        let Some(text) = self.text.as_mut() else {
            return Ok(());
        };
        text.window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        text.bounds = bounds;
        if text.typing.open() {
            self.chat.update(cx, |state, cx| {
                state.show_for(self.runs.thread());
                cx.notify();
            });
        }
        self.place_drawings(cx.weak_entity(), cx);
        self.runs_changed(cx)
    }

    pub(super) fn open_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.cancel_voice(cx);
        self.focus_text(cx)
    }

    pub(super) fn focus_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.show_voice_answer(cx)?;
        let text = self.text.as_mut().expect("created above");
        let draft = text
            .draft
            .take()
            .map(|saved| diagnostics::draft(&saved, &text.input.read(cx).value()));
        let input = text.input.clone();
        let activated = text
            .window
            .update(cx, |_, window, cx| {
                let previous = platform::activate_presence_window(window)?;
                input.update(cx, |state, cx| state.focus(window, cx));
                Ok::<_, platform::PresenceError>(previous)
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        let focused = match activated {
            Ok(Some(previous)) => {
                text.previous = Some(previous);
                true
            }
            Ok(None) => true,
            // Still visible; a click activates it.
            Err(error) => {
                tracing::warn!(%error, "can't focus the typing panel");
                false
            }
        };
        diagnostics::record(cx, format_args!("input opened focused={focused}"));
        if let Some([nonempty, preserved]) = draft {
            diagnostics::record(
                cx,
                format_args!("input draft nonempty={nonempty} preserved={preserved}"),
            );
        }
        self.runs_changed(cx)
    }

    fn create_text(&mut self, cx: &mut Context<'_, Self>) -> Result<TextPanel, OpenError> {
        let stop = self.escape_hold.shows_stop();
        let events = self.events.clone();
        let close_events = self.events.clone();
        let owner = cx.weak_entity();
        let model = self.chat.clone();
        let settings = self.settings.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(constants::DISPLAY_NAME);
            window.on_window_should_close(cx, move |_, _| {
                send(&close_events, PresenceEvent::DismissPanel);
                false
            });
            cx.new(|cx| {
                let mut view = TextView::new(model, settings, owner, events, window, cx);
                self.update_typing_notice(&mut view, cx);
                view.stop = stop;
                view
            })
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, events, cx)?;
                self.input_windows
                    .bind(crate::presenter::overlay_input::Layer::Panels, window)?;
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let result = window.update(cx, |_, window, cx| {
                owner.update(cx, |this, cx| {
                    let activation = cx.observe_window_activation(window, |this, window, cx| {
                        let active = window.is_window_active();
                        if let Some(text) = &mut this.text {
                            text.focused = active;
                            if active {
                                text.typing.activated();
                            }
                        }
                        this.focus_changed(cx);
                        if active {
                            return;
                        }
                        let owner = cx.weak_entity();
                        cx.defer(move |cx| {
                            let result =
                                owner.update(cx, |this, cx| this.hide_text(Hide::Deactivated, cx));
                            if let Ok(Err(error)) = result {
                                tracing::error!(%error, "can't hide the inactive typing panel");
                            }
                        });
                    });
                    if let Some(text) = &mut this.text {
                        text.focused = window.is_window_active();
                        if text.focused {
                            text.typing.activated();
                        }
                        text.activation = Some(activation);
                    }
                })
            });
            if let Err(error) = result {
                tracing::error!(%error, "can't observe typing panel activation");
            }
        });
        let input = view.read(cx).input.clone();
        let submit = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event
                && let Err(error) = this.submit(cx)
            {
                tracing::error!(%error, "can't send the typed turn");
            }
        });
        Ok(TextPanel {
            window,
            typing: Typing::default(),
            focused: false,
            bounds: [0; 4],
            input,
            view,
            previous: None,
            draft: None,
            activation: None,
            _submit: submit,
        })
    }

    /// Hides the typing panel, keeping its draft, and gives the front back if it still has it.
    pub(super) fn dismiss_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.hide_text(Hide::Dismiss, cx)
    }

    fn hide_text(&mut self, reason: Hide, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(text) = self.text.as_mut() else {
            return Ok(());
        };
        let shown = (text.typing, text.previous);
        // shortcut: rapid native away/back within one queued GPUI activation can hide an active box, revisit if reproducible.
        if !text.typing.close(reason) {
            return Ok(());
        }
        let previous = text
            .previous
            .take()
            .filter(|_| reason.restores_foreground());
        let hidden = text.window.update(cx, |_, window, _| {
            if let Some(previous) = previous {
                platform::restore_foreground(window, previous);
            }
            platform::hide_presence_window(window)
        });
        // A panel still on screen must stay open, so Esc and its close button can try again.
        if !matches!(hidden, Ok(Ok(()))) {
            (text.typing, text.previous) = shown;
        }
        hidden.map_err(|error| OpenError::Gpui(error.into()))??;
        self.chat.update(cx, |state, cx| {
            state.hide();
            cx.notify();
        });
        if cx.has_global::<Diagnostics>() {
            text.draft = Some(text.input.read(cx).value());
        }
        diagnostics::record(cx, format_args!("input dismissed"));
        self.place_drawings(cx.weak_entity(), cx);
        // The typing panel was covering the status panel's Stop.
        if self.escape_hold.shows_stop() {
            self.pointer_changed(cx)?;
        }
        self.runs_changed(cx)
    }
}

impl Presence {
    pub(super) fn submit(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        match typing::admit(&self.link, self.session.as_ref()) {
            Admission::Ignore => return Ok(()),
            Admission::Refused(text) => {
                let previous_error = self.error.clone();
                Submission::Refused(text).update_notice(&mut self.error, &mut self.refusal);
                self.notices_changed(previous_error.as_ref(), cx);
                return Ok(());
            }
            Admission::Proceed => {}
        }
        self.cancel_voice(cx);
        self.voice.lost = None;
        let Some(text) = &self.text else {
            return Ok(());
        };
        let draft = text.input.read(cx).value();
        let result = typing::submit(&mut self.runs, &self.link, self.session.as_ref(), &draft);
        let sent = result == Submission::Sent;
        diagnostics::record(cx, format_args!("submit sent={sent}"));
        let previous_error = self.error.clone();
        result.update_notice(&mut self.error, &mut self.refusal);
        if sent {
            self.chat_notice.clear();
            self.settings.update(cx, |state, _| state.clear_notice());
        }
        self.notices_changed(previous_error.as_ref(), cx);
        if sent {
            let input = text.input.clone();
            text.window
                .update(cx, |_, window, cx| {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                })
                .map_err(|error| OpenError::Gpui(error.into()))?;
        }
        self.runs_changed(cx)
    }

    pub(super) fn notices_changed(
        &self,
        previous_error: Option<&SharedString>,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(text) = &self.text {
            text.view.update(cx, |view, cx| {
                if self.update_typing_notice(view, cx) {
                    cx.notify();
                }
            });
        }
        if previous_error != self.error.as_ref() {
            if let Some(panel) = &self.thread_panel {
                let error = self.error.clone();
                panel.view.update(cx, |view, cx| {
                    view.error = error;
                    cx.notify();
                });
            }
            self.refresh_dock(cx);
        }
    }

    pub(super) fn clear_refusal(&mut self, cx: &mut Context<'_, Self>) {
        self.refusal = None;
        self.notices_changed(self.error.as_ref(), cx);
    }

    /// Follows the input target and pushes its bounded live answer into the retained chat.
    pub(super) fn refresh_conversation(&mut self, cx: &mut Context<'_, Self>) {
        self.refresh_screen(cx);
        self.voice
            .target
            .set((self.runs.thread(), self.runs.revision()));
        self.publish_offer(cx);
        if self.text.as_ref().is_some_and(|text| text.typing.visible) {
            self.chat.update(cx, |state, cx| {
                if state.follow(self.runs.thread()) {
                    cx.notify();
                }
                if let Some(shown) = self.runs.shown().filter(|shown| shown.done)
                    && let Some(thread) = shown.thread
                {
                    state.settled(thread, shown.run);
                }
            });
            self.request_pages(cx);
        }
        if let Some(panel) = &self.thread_panel {
            let target = self.runs.target();
            panel.view.update(cx, |view, cx| {
                if view.target != target {
                    view.target = target;
                    cx.notify();
                }
            });
        }
        let Some(text) = &self.text else {
            return;
        };
        let answer = self.runs.shown();
        let target = self.runs.target();
        text.view.update(cx, |view, cx| {
            view.messages
                .update(cx, |messages, cx| messages.live(answer, cx));
            let notice_changed = self.update_typing_notice(view, cx);
            if view.target != target || notice_changed {
                view.target = target;
                cx.notify();
            }
        });
    }

    fn typing_notice(&self) -> typing::Notice<&Rc<Notice>> {
        typing::notice(
            self.refusal,
            self.chat_notice.for_thread(self.runs.thread()),
        )
    }

    fn update_typing_notice(&self, view: &mut TextView, cx: &Context<'_, TextView>) -> bool {
        let strings = cx.global::<Strings>();
        let (notice, recovery) = match self.typing_notice() {
            typing::Notice::Refusal(text) => (Some(strings.get(text).into()), None),
            typing::Notice::Recovery(recovery) => (None, Some(recovery)),
            typing::Notice::Fallback => {
                let status = self
                    .voice
                    .lost
                    .or(self.voice.status)
                    .map(|status| strings.get(status).into())
                    // A hold's screen line outlives its voice status: the reason arrives with the run.
                    .or_else(|| {
                        let (text, model) = self.panel.read(cx).screen_line.as_ref()?;
                        Some(super::voice::screen_text(strings, *text, model.as_ref()).into())
                    });
                (status, None)
            }
        };
        let retry = recovery.is_some_and(
            |notice| matches!(notice.source, Source::Run(run) if self.runs.retry_ready(run)),
        );
        let recovery_changed = view.recovery.as_ref().map(Rc::as_ptr) != recovery.map(Rc::as_ptr);
        if recovery_changed {
            view.recovery_text = recovery.map(|notice| strings.error(&notice.error).into());
            view.recovery = recovery.cloned();
        }
        let changed = recovery_changed || view.retry != retry || view.notice != notice;
        view.retry = retry;
        view.notice = notice;
        changed
    }
}

/// Focused history, a bounded live preview and the retained composer.
pub(super) struct TextView {
    pub(super) input: Entity<InputState>,
    /// The Stop fallback, here too because this panel covers the status panel's.
    pub(super) stop: bool,
    model: Entity<Threads>,
    settings: Entity<Settings>,
    recovery: Option<Rc<Notice>>,
    recovery_text: Option<SharedString>,
    retry: bool,
    draft_empty: bool,
    messages: Entity<Messages>,
    target: Target,
    _watch: Vec<gpui_kit::Subscription>,
    /// Pushed by presence, never read from it: the first frame renders while presence is updating.
    pub(super) notice: Option<SharedString>,
    owner: WeakEntity<Presence>,
    events: Sender<PresenceEvent>,
}

impl TextView {
    fn new(
        model: Entity<Threads>,
        settings: Entity<Settings>,
        owner: WeakEntity<Presence>,
        events: Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        cx.bind_keys([gpui_kit::KeyBinding::new(
            "escape",
            EscapeKey,
            Some("ChatPanel"),
        )]);
        let messages = cx.new(|cx| Messages::new(model.clone(), owner.clone(), window, cx));
        let watch = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&input, |view, input, cx| {
                let empty = input.read(cx).value().is_empty();
                if view.draft_empty != empty {
                    view.draft_empty = empty;
                    cx.notify();
                }
            }),
        ];
        Self {
            input,
            model,
            settings,
            recovery: None,
            recovery_text: None,
            retry: false,
            draft_empty: true,
            messages,
            target: Target::Kumo,
            stop: false,
            notice: None,
            owner,
            events,
            _watch: watch,
        }
    }

    fn context(&self, cx: &gpui_kit::App) -> (SharedString, Option<SharedString>) {
        let state = self.model.read(cx);
        let strings = cx.global::<Strings>();
        let name = state
            .settings
            .as_ref()
            .map_or(constants::DEFAULT_ASSISTANT_NAME, |settings| {
                settings.assistant_name.as_str()
            });
        let label = match self.target {
            Target::Kumo => return (strings.assistant(name).into(), None),
            Target::Task(thread) => state.row(thread).map_or_else(
                || strings.get(Text::TaskFallback),
                |row| row_label(row, state.settings.as_ref()),
            ),
            Target::New => strings.get(Text::NewThread),
        };
        (
            strings.focus(&label).into(),
            Some(strings.back_to_kumo(name).into()),
        )
    }

    fn header(&self, cx: &Context<'_, Self>) -> gpui_kit::AnyElement {
        let (context, back_label) = self.context(cx);
        let back_owner = self.owner.clone();
        let tasks_owner = self.owner.clone();
        div()
            .p_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .max_w_full()
                    .truncate()
                    .text_sm()
                    .child(context),
            )
            .children(back_label.map(|back_label| {
                Button::new("back-to-kumo")
                    .label(back_label)
                    .on_click(move |_, _, cx| {
                        let _gone = back_owner.update(cx, Presence::back_to_kumo);
                    })
            }))
            .child(
                div().debug_selector(|| "chat-tasks-control".into()).child(
                    Button::new("chat-tasks")
                        .label(cx.global::<Strings>().get(Text::OpenThreads))
                        .on_click(move |_, _, cx| {
                            let owner = tasks_owner.clone();
                            // Opening Tasks hides this window, which GPUI can't update while it
                            // dispatches this click.
                            cx.defer(move |cx| {
                                if let Ok(Err(_)) = owner.update(cx, Presence::open_threads) {
                                    tracing::warn!("can't open tasks from chat");
                                }
                            });
                        }),
                ),
            )
            .into_any_element()
    }

    fn history_controls(&self, cx: &Context<'_, Self>) -> gpui_kit::AnyElement {
        let state = self.model.read(cx);
        let failed = state.failed;
        let more = state.before.is_some();
        let reading = state.reading();
        let online = state.online();
        let retry_owner = self.owner.clone();
        let messages = self.messages.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(failed.then(|| {
                div()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .child(cx.global::<Strings>().get(Text::ChatHistoryFailed)),
                    )
                    .child(
                        div().debug_selector(|| "chat-retry-control".into()).child(
                            Button::new("chat-history-retry")
                                .label(cx.global::<Strings>().get(Text::Retry))
                                .disabled(!online || reading)
                                .on_click(move |_, _, cx| {
                                    let _gone = retry_owner.update(cx, |owner, cx| {
                                        owner.chat.update(cx, |state, cx| {
                                            state.refresh();
                                            if let Some(thread) = state.open {
                                                state.run_state(thread);
                                            }
                                            cx.notify();
                                        });
                                        owner.request_pages(cx);
                                    });
                                }),
                        ),
                    )
            }))
            .children(
                more.then(|| {
                    div().px_3().child(
                        Button::new("chat-older-messages")
                            .label(cx.global::<Strings>().get(Text::OlderMessages))
                            .disabled(!online || reading)
                            .on_click(move |_, _, cx| {
                                messages.update(cx, Messages::older);
                            }),
                    )
                })
                .map(|control| control.debug_selector(|| "chat-older-control".into())),
            )
            .into_any_element()
    }

    /// Puts `text` in the input when it's empty, and says whether it did.
    pub(super) fn restore(
        &self,
        text: kumochi_protocol::message::Content,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(text) = typing::restore(&self.input.read(cx).value(), text) else {
            return false;
        };
        self.input.update(cx, |input, cx| {
            input.set_value(text.into_string(), window, cx);
        });
        true
    }
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let events = self.events.clone();
        let escape_owner = self.owner.clone();
        let notice = self.notice.clone();
        let recovery = self.recovery(cx);
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            let owner = self.owner.clone();
            Button::new("stop")
                .label(cx.global::<Strings>().get(Text::Stop))
                .on_click(move |_, _, cx| super::send_escape(&events, Some(&owner), cx))
        });
        let failed = self.model.read(cx).failed;
        let empty = self.messages.read(cx).is_empty().then(|| {
            let state = self.model.read(cx);
            cx.global::<Strings>().get(if !state.online() {
                Text::ThreadsOffline
            } else if state.reading() && !state.loaded {
                Text::ChatLoading
            } else {
                Text::ChatEmpty
            })
        });
        div()
            .size_full()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .flex()
            .flex_col()
            .key_context("ChatPanel")
            .on_action(move |_: &EscapeKey, _, cx| {
                super::send_escape(&events, Some(&escape_owner), cx);
            })
            .child(self.header(cx))
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(recovery)
                    .children(notice.map(|notice| {
                        div()
                            .debug_selector(|| format!("typing-notice {notice}"))
                            .text_sm()
                            .child(notice)
                    }))
                    .child(Input::new(&self.input))
                    .children(stop),
            )
            .child(self.history_controls(cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(self.messages.clone())
                    .children(empty.filter(|_| !failed).map(|empty| {
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .p_3()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(empty)
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Entity, SharedString, TestAppContext, VisualTestContext,
        WeakEntity, WindowOptions,
    };

    use super::{Presence, TextView};
    use crate::presence::threads::ThreadsPanel;
    use crate::strings::Strings;

    /// The typing panel as the shell opens it, with a pushed notice and no readable presence.
    fn open(
        cx: &mut TestAppContext,
        notice: Option<&'static str>,
    ) -> (AnyWindowHandle, Entity<TextView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let (events, _) = async_channel::unbounded();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let model = cx.new(|_| crate::threads::Threads::chat_for_tests());
                let settings = cx.new(|_| crate::settings::Settings::default());
                cx.new(|cx| {
                    let mut view = TextView::new(
                        model,
                        settings,
                        WeakEntity::new_invalid(),
                        events,
                        window,
                        cx,
                    );
                    view.notice = notice.map(SharedString::from);
                    view
                })
            })
            .expect("a window")
        })
    }

    const OFFLINE: &str = "typing-notice Kumo can't reach the brain.";
    const TOO_LONG: &str = "typing-notice That turn is too long.";

    #[gpui::test]
    fn history_controls_dispatch_the_focused_reader_without_touching_the_draft(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::{
            DataToBrain, DataToShell, EventId, Incarnation, PageOf, ThreadId,
        };
        for retry in [false, true] {
            let owner = crate::presence::test_fixture::presence(cx);
            let (window, view) = owner.update(cx, |owner, cx| {
                owner.session = Some(crate::link::Session::detached(1, Incarnation(1)));
                owner.chat.update(cx, |state, cx| {
                    state.connected();
                    state.show_for(Some(ThreadId(3)));
                    for request in state.requests() {
                        let DataToBrain::PageRequest { request, page } = request else {
                            panic!("page request");
                        };
                        state.apply_for_tests(match page {
                            PageOf::Threads { .. } => DataToShell::ThreadList {
                                request,
                                threads: Vec::new(),
                                next: None,
                            },
                            PageOf::Events { .. } | PageOf::EventsAfter { .. } => {
                                DataToShell::Page {
                                    request,
                                    thread: ThreadId(3),
                                    events: Vec::new(),
                                    next: (!retry).then_some(EventId(1)),
                                }
                            }
                        });
                    }
                    state.failed = retry;
                    cx.notify();
                });
                let text = owner.text.as_ref().expect("typing fixture");
                (text.window, text.view.clone())
            });
            window
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.input.update(cx, |input, cx| {
                            input.set_value("keep this draft", window, cx);
                        });
                    });
                })
                .expect("typing window");
            cx.run_until_parked();
            owner.read_with(cx, |owner, cx| assert!(owner.chat.read(cx).online()));
            {
                let mut frame = VisualTestContext::from_window(window, cx);
                let bounds = frame
                    .debug_bounds(if retry {
                        "chat-retry-control"
                    } else {
                        "chat-older-control"
                    })
                    .expect("history action");
                frame.simulate_click(
                    gpui_kit::point(bounds.left() + gpui_kit::px(20.0), bounds.center().y),
                    gpui_kit::Modifiers::default(),
                );
            }
            cx.run_until_parked();
            owner.read_with(cx, |owner, cx| {
                assert!(
                    !owner.chat.read(cx).online(),
                    "the real request reached the refusing fixture link"
                );
                assert_eq!(owner.chat.read(cx).open, Some(ThreadId(3)));
            });
            view.read_with(cx, |view, cx| {
                assert_eq!(view.input.read(cx).value().as_ref(), "keep this draft");
            });
        }
    }

    #[gpui::test]
    fn escape_from_input_messages_and_navigation_preserves_the_draft(cx: &mut TestAppContext) {
        let (window, view) = open(cx, None);
        let (events, received) = async_channel::unbounded();
        view.update(cx, |view, cx| {
            view.events = events;
            cx.notify();
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.input.update(cx, |input, cx| {
                        input.set_value("retained draft", window, cx);
                        input.focus(window, cx);
                    });
                });
                let _arena = window.draw(cx);
            })
            .expect("chat window");
        let input_focus = window
            .update(cx, |_, window, cx| window.focused(cx).expect("input focus"))
            .expect("chat window");
        let press_escape = |cx: &mut TestAppContext| {
            cx.simulate_keystrokes(window, "escape");
            assert!(matches!(
                received.try_recv(),
                Ok(super::PresenceEvent::Escape)
            ));
            assert!(matches!(
                received.try_recv(),
                Err(async_channel::TryRecvError::Empty)
            ));
            view.read_with(cx, |view, cx| {
                assert_eq!(view.input.read(cx).value().as_ref(), "retained draft");
            });
        };
        press_escape(cx);
        {
            let mut frame = VisualTestContext::from_window(window, cx);
            let bounds = frame
                .debug_bounds("chat-message-region")
                .expect("message region");
            frame.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        }
        let messages_focus = window
            .update(cx, |_, window, cx| {
                window.focused(cx).expect("message focus")
            })
            .expect("chat window");
        assert_ne!(messages_focus, input_focus);
        press_escape(cx);
        cx.simulate_keystrokes(window, "shift-tab shift-tab");
        let navigation_focus = window
            .update(cx, |_, window, cx| {
                window.focused(cx).expect("navigation focus")
            })
            .expect("chat window");
        assert_ne!(navigation_focus, input_focus);
        assert_ne!(navigation_focus, messages_focus);
        press_escape(cx);
    }

    /// Which of the two notices the window's latest frame drew.
    fn drawn(cx: &mut TestAppContext, window: AnyWindowHandle) -> [bool; 2] {
        cx.run_until_parked();
        let mut frame = VisualTestContext::from_window(window, cx);
        [OFFLINE, TOO_LONG].map(|notice| frame.debug_bounds(notice).is_some())
    }

    #[gpui::test]
    fn the_typing_notice_draws_from_its_pushed_copy(cx: &mut TestAppContext) {
        let (window, view) = open(cx, Some("Kumo can't reach the brain."));
        assert_eq!(drawn(cx, window), [true, false]);
        view.update(cx, |view, cx| {
            view.notice = Some("That turn is too long.".into());
            cx.notify();
        });
        assert_eq!(drawn(cx, window), [false, true]);
        view.update(cx, |view, cx| {
            view.notice = None;
            cx.notify();
        });
        assert_eq!(drawn(cx, window), [false, false]);
    }

    #[gpui::test]
    fn reconnect_restores_an_empty_composer_without_overwriting_a_new_draft(
        cx: &mut TestAppContext,
    ) {
        let (window, view) = open(cx, None);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.restore("Exact draft: café ✓".into(), window, cx);
                    assert_eq!(view.input.read(cx).value().as_ref(), "Exact draft: café ✓");
                    view.input
                        .update(cx, |input, cx| input.set_value("newer draft", window, cx));
                    view.restore("older failed turn".into(), window, cx);
                    assert_eq!(view.input.read(cx).value().as_ref(), "newer draft");
                    view.input
                        .update(cx, |input, cx| input.set_value(" ", window, cx));
                    view.restore("older failed turn".into(), window, cx);
                    assert_eq!(view.input.read(cx).value().as_ref(), " ");
                });
            })
            .expect("live typing window");
    }

    /// The headless presence with its chat open on task 3, as a double tap leaves it.
    fn chat_on_a_task(cx: &mut TestAppContext) -> (Entity<Presence>, AnyWindowHandle) {
        use kumochi_protocol::message::ThreadId;
        let owner = crate::presence::test_fixture::presence(cx);
        let window = owner.update(cx, |owner, cx| {
            owner.thread_panel = Some(ThreadsPanel::detached(owner.threads.clone(), cx));
            assert!(owner.runs.select_thread(Some(ThreadId(3))));
            owner.open_text(cx).expect("open chat");
            assert!(owner.text_open());
            assert_eq!(owner.chat.read(cx).open, Some(ThreadId(3)));
            owner.text.as_ref().expect("typing fixture").window
        });
        cx.run_until_parked();
        (owner, window)
    }
    #[gpui::test]
    fn tasks_in_the_chat_header_swaps_the_chat_for_the_task_list(cx: &mut TestAppContext) {
        let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
        let (owner, window) = chat_on_a_task(cx);
        owner.update(cx, |owner, cx| {
            let panel = owner.text.as_ref().expect("typing fixture");
            window
                .update(cx, |_, window, cx| {
                    panel.input.update(cx, |input, cx| {
                        input.set_value("unsent draft", window, cx);
                    });
                })
                .expect("typing window");
        });
        {
            let mut frame = VisualTestContext::from_window(window, cx);
            let bounds = frame
                .debug_bounds("chat-tasks-control")
                .expect("Tasks… in the chat header");
            frame.simulate_click(
                gpui_kit::point(bounds.left() + gpui_kit::px(10.0), bounds.center().y),
                gpui_kit::Modifiers::default(),
            );
        }
        cx.run_until_parked();
        owner.read_with(cx, |owner, cx| {
            assert!(owner.threads_open(), "the task list is shown");
            assert!(!owner.text_open(), "the chat is hidden");
            assert_eq!(owner.chat.read(cx).open, None);
        });
        owner.update(cx, |owner, cx| {
            owner.open_text(cx).expect("reopen chat");
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .expect("typing fixture")
                    .input
                    .read(cx)
                    .value()
                    .as_ref(),
                "unsent draft"
            );
        });
    }
    #[gpui::test]
    fn a_chat_that_failed_to_hide_keeps_its_task_and_esc_still_closes_it(cx: &mut TestAppContext) {
        let _area = crate::platform::work_area_fixture::set([0, 0, 2560, 1400]);
        let (owner, window) = chat_on_a_task(cx);
        // GPUI refuses to update a window from inside its own update, as a click on it would.
        let hidden = window
            .update(cx, |_, _, cx| owner.update(cx, Presence::dismiss_text))
            .expect("chat window");
        assert!(matches!(hidden, Err(super::OpenError::Gpui(_))));
        owner.update(cx, |owner, cx| {
            assert!(owner.text_open(), "the chat is still on screen");
            assert_eq!(
                owner.chat.read(cx).open,
                Some(kumochi_protocol::message::ThreadId(3))
            );
            owner.escape(cx).expect("Esc");
            assert!(!owner.text_open(), "Esc hid the chat");
            assert_eq!(owner.chat.read(cx).open, None);
        });
    }
}
