//! The reusable typing surface and its activation lifetime.

use async_channel::Sender;
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputEvent};
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Window, div,
};

use super::{
    Context, Diagnostics, Entity, Foreground, InputState, OpenError, PanelVisibility, Presence,
    PresenceEvent, PresenceSurface, SharedString, configure_surface, constants, diagnostics,
    options, platform, send,
};
use crate::strings::{Strings, Text};

#[derive(Clone, Copy)]
enum Hide {
    Dismiss,
    Deactivated,
}

#[derive(Default)]
pub(super) struct Typing {
    pub(super) visible: bool,
    active_seen: bool,
}

impl Typing {
    fn open(&mut self) {
        if !self.visible {
            self.active_seen = false;
        }
        self.visible = true;
    }

    fn activated(&mut self) {
        if self.visible {
            self.active_seen = true;
        }
    }

    fn close(&mut self, reason: Hide) -> bool {
        if !self.visible || matches!(reason, Hide::Deactivated) && !self.active_seen {
            return false;
        }
        self.visible = false;
        self.active_seen = false;
        true
    }
}

/// The typing surface: the one window Capsula activates, and only on a double tap.
pub(super) struct TextPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<TextView>,
    pub(super) typing: Typing,
    pub(super) input: Entity<InputState>,
    /// The window in front before the panel took focus, given back when it closes.
    previous: Option<Foreground>,
    /// Input diagnostics only: the draft at the last dismiss, compared on the next open.
    draft: Option<SharedString>,
    activation: Option<gpui_kit::Subscription>,
    /// Enter sends the draft as a turn.
    _submit: gpui_kit::Subscription,
}

impl Presence {
    pub(super) fn open_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if self.text.is_none() {
            self.text = Some(self.create_text(cx)?);
        }
        self.pointer_settle = None;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        let bounds = self.layout.panel;
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
        text.typing.open();
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
        // The placeholder steps count as a drawing only while the panel shows them.
        self.runs_changed(cx)
    }

    fn create_text(&self, cx: &mut Context<'_, Self>) -> Result<TextPanel, OpenError> {
        let stop = self.escape_hold.shows_stop();
        let events = self.events.clone();
        let close_events = self.events.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(constants::DISPLAY_NAME);
            window.on_window_should_close(cx, move |_, _| {
                send(&close_events, PresenceEvent::DismissPanel);
                false
            });
            let input = cx.new(|cx| InputState::new(window, cx));
            cx.new(|_| TextView {
                input,
                stop,
                steps: Vec::new(),
                answer: SharedString::default(),
                notice: None,
                events,
            })
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, events, cx)
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let result = window.update(cx, |_, window, cx| {
                owner.update(cx, |this, cx| {
                    let activation = cx.observe_window_activation(window, |this, window, cx| {
                        if window.is_window_active() {
                            if let Some(text) = &mut this.text {
                                text.typing.activated();
                            }
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
                        if window.is_window_active() {
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
        let bounds = self.layout.panel;
        let Some(text) = self.text.as_mut() else {
            return Ok(());
        };
        // shortcut: rapid native away/back within one queued GPUI activation can hide an active box, revisit if reproducible.
        if !text.typing.close(reason) {
            return Ok(());
        }
        if cx.has_global::<Diagnostics>() {
            text.draft = Some(text.input.read(cx).value());
        }
        let previous = text
            .previous
            .take()
            .filter(|_| matches!(reason, Hide::Dismiss));
        text.window
            .update(cx, |_, window, cx| {
                if let Some(previous) = previous {
                    platform::restore_foreground(window, previous);
                }
                platform::place_hidden_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        diagnostics::record(cx, format_args!("input dismissed"));
        // The typing panel was covering the status panel's Stop.
        if self.escape_hold.shows_stop() {
            self.pointer_changed(cx)?;
        }
        self.runs_changed(cx)
    }
}

impl Presence {
    /// Shows the newest run's steps and the end of its answer in the typing panel.
    pub(super) fn refresh_conversation(&self, cx: &mut Context<'_, Self>) {
        let Some(text) = &self.text else {
            return;
        };
        let (steps, answer) = self.runs.shown().map_or_else(Default::default, |shown| {
            let steps: Vec<SharedString> = shown
                .steps
                .iter()
                .map(|(_, step)| step.label.clone().into())
                .collect();
            (steps, answer_tail(&shown.text).into())
        });
        text.view.update(cx, |view, cx| {
            if view.steps != steps || view.answer != answer {
                view.steps = steps;
                view.answer = answer;
                cx.notify();
            }
        });
    }
}

/// The anchored typing surface: the newest run's placeholder steps and the end of its answer,
/// one focused input, and why a turn wasn't sent. shortcut: stands in for the threads panel.
pub(super) struct TextView {
    pub(super) input: Entity<InputState>,
    /// The Stop fallback, here too because this panel covers the status panel's.
    pub(super) stop: bool,
    steps: Vec<SharedString>,
    answer: SharedString,
    pub(super) notice: Option<SharedString>,
    events: Sender<PresenceEvent>,
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let events = self.events.clone();
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            Button::new("stop")
                .label(cx.global::<Strings>().get(Text::Stop))
                .on_click(move |_, _, _| send(&events, PresenceEvent::Escape))
        });
        let steps = (!self.steps.is_empty()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children(self.steps.iter().map(|label| {
                    div()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(label.clone())
                }))
        });
        div()
            .size_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .flex()
            .flex_col()
            .gap_2()
            .on_action(move |_: &EscapeKey, _, _| send(&events, PresenceEvent::Escape))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .gap_1()
                    .text_xs()
                    .children(steps)
                    .child(self.answer.clone()),
            )
            .children(
                self.notice
                    .clone()
                    .map(|notice| div().text_xs().child(notice)),
            )
            .child(Input::new(&self.input))
            .children(stop)
    }
}

/// The end of an answer the typing panel has room for [guess], cut at a character boundary.
fn answer_tail(text: &str) -> &str {
    const SHOWN: usize = 280;
    let mut start = text.len().saturating_sub(SHOWN);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::{Hide, Typing, answer_tail};

    #[test]
    fn a_short_answer_shows_whole() {
        assert_eq!(answer_tail("hi"), "hi");
    }

    #[test]
    fn a_long_answer_shows_its_last_280_bytes() {
        let text = format!("{}end", "a".repeat(300));
        assert_eq!(answer_tail(&text), format!("{}end", "a".repeat(277)));
    }

    #[test]
    fn a_long_answer_is_cut_after_a_character_split_by_the_limit() {
        let text = format!("{}!", "é".repeat(141));
        assert_eq!(answer_tail(&text), format!("{}!", "é".repeat(139)));
    }

    #[test]
    fn reopening_active_input_keeps_blur_dismissal() {
        let mut typing = Typing::default();
        typing.open();
        typing.activated();
        typing.open();
        assert!(typing.close(Hide::Deactivated));
        assert!(!typing.visible);
        typing.open();
        assert!(!typing.close(Hide::Deactivated));
        assert!(typing.visible);
        assert!(typing.close(Hide::Dismiss));
        typing.activated();
        assert!(!typing.close(Hide::Deactivated));
    }
}
