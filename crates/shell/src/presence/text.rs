//! The reusable typing surface and its activation lifetime.

use async_channel::Sender;
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputEvent};
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, WeakEntity, Window, div,
};

use super::{
    Context, Diagnostics, Entity, Foreground, InputState, OpenError, PanelVisibility, Presence,
    PresenceEvent, PresenceSurface, SharedString, configure_surface, constants, diagnostics,
    options, platform, send,
};
use crate::runs::{Refused, Target};
use crate::strings::{Strings, Text};
use crate::threads::row_label;

#[derive(Clone, Copy)]
pub(super) enum Hide {
    Dismiss,
    Deactivated,
}

#[derive(Default)]
pub(super) struct Typing {
    pub(super) visible: bool,
    active_seen: bool,
}

impl Typing {
    pub(super) fn open(&mut self) {
        if !self.visible {
            self.active_seen = false;
        }
        self.visible = true;
    }

    pub(super) fn activated(&mut self) {
        if self.visible {
            self.active_seen = true;
        }
    }

    pub(super) fn close(&mut self, reason: Hide) -> bool {
        if !self.visible || matches!(reason, Hide::Deactivated) && !self.active_seen {
            return false;
        }
        self.visible = false;
        self.active_seen = false;
        true
    }
}

/// The typing surface: the one window Kumochi activates, and only on a double tap.
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
        self.dismiss_threads(cx)?;
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
        let owner = cx.weak_entity();
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
                conversation: Conversation::default(),
                owner,
                events,
            })
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        let events = self.events.clone();
        let capture = if self.gesture.get().holding() {
            super::Capture::Excluded
        } else {
            super::Capture::Allowed
        };
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, events, cx)?;
                platform::set_capture(window, capture)
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
    pub(super) fn submit(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let Some(text) = &self.text else {
            return Ok(());
        };
        let draft = text.input.read(cx).value();
        let (link, session) = (&self.link, &self.session);
        let result = self.runs.submit(&draft, |turn| {
            session
                .as_ref()
                .is_some_and(|session| link.send_data(session, turn))
        });
        diagnostics::record(cx, format_args!("submit sent={}", result.is_ok()));
        let previous = (self.error.clone(), self.refusal);
        submission_notice(&mut self.error, &mut self.refusal, result);
        self.notices_changed(&previous, cx);
        if result.is_ok() {
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
        previous: &(Option<SharedString>, Option<Text>),
        cx: &mut Context<'_, Self>,
    ) {
        let strings = cx.global::<Strings>();
        if typing_notice(previous.0.as_ref(), previous.1, strings)
            != typing_notice(self.error.as_ref(), self.refusal, strings)
            && let Some(text) = &self.text
        {
            text.view.update(cx, |_, cx| cx.notify());
        }
        if previous.0 != self.error {
            if let Some(panel) = &self.thread_panel {
                panel.view.update(cx, |_, cx| cx.notify());
            }
            self.refresh_dock(cx);
        }
    }

    pub(super) fn clear_refusal(&mut self, cx: &mut Context<'_, Self>) {
        let previous = (self.error.clone(), self.refusal.take());
        self.notices_changed(&previous, cx);
    }

    /// Shows the drawing's steps and the end of the newest answer in the typing panel.
    pub(super) fn refresh_conversation(&self, cx: &mut Context<'_, Self>) {
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
        let steps: Vec<SharedString> = self
            .runs
            .scenes()
            .flat_map(|scene| &scene.steps)
            .map(|(_, step)| step.label.clone().into())
            .collect();
        let answer: SharedString = self
            .runs
            .shown()
            .map_or_else(Default::default, |shown| answer_tail(&shown.text).into());
        let deleting = self.runs.shown().is_some_and(|shown| {
            shown
                .thread
                .is_some_and(|thread| self.runs.deleting().contains(&thread))
        });
        let threads = self.threads.read(cx);
        let strings = cx.global::<Strings>();
        let label = match self.runs.target() {
            Target::Kumo => None,
            Target::Task(thread) => Some(threads.row(thread).map_or_else(
                || strings.get(Text::TaskFallback),
                |row| row_label(row, threads.settings.as_ref()),
            )),
            Target::New => Some(strings.get(Text::NewThread)),
        };
        let context = label.map(|label| {
            let back_label = strings.back_to_kumo(
                threads
                    .settings
                    .as_ref()
                    .map_or(constants::DEFAULT_ASSISTANT_NAME, |settings| {
                        settings.assistant_name.as_str()
                    }),
            );
            (strings.focus(&label).into(), back_label.into())
        });
        let conversation = Conversation {
            steps,
            answer,
            deleting,
            context,
        };
        text.view.update(cx, |view, cx| {
            if view.conversation.update(conversation) {
                cx.notify();
            }
        });
    }
}

#[derive(Default, PartialEq, Eq)]
struct Conversation {
    steps: Vec<SharedString>,
    answer: SharedString,
    deleting: bool,
    context: Option<(SharedString, SharedString)>,
}

impl Conversation {
    fn update(&mut self, next: Self) -> bool {
        if *self == next {
            return false;
        }
        *self = next;
        true
    }
}

/// The anchored typing surface: the newest run's placeholder steps and the end of its answer,
/// one focused input, and why a turn wasn't sent.
pub(super) struct TextView {
    pub(super) input: Entity<InputState>,
    /// The Stop fallback, here too because this panel covers the status panel's.
    pub(super) stop: bool,
    conversation: Conversation,
    owner: WeakEntity<Presence>,
    events: Sender<PresenceEvent>,
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let events = self.events.clone();
        let owner = self.owner.upgrade();
        let presence = owner.as_ref().map(|owner| owner.read(cx));
        let notice = presence.and_then(|owner| {
            typing_notice(owner.error.as_ref(), owner.refusal, cx.global::<Strings>())
        });
        let context = self.conversation.context.clone();
        let back_owner = self.owner.clone();
        let stop = self.stop.then(|| {
            let events = self.events.clone();
            Button::new("stop")
                .label(cx.global::<Strings>().get(Text::Stop))
                .on_click(move |_, _, _| send(&events, PresenceEvent::Escape))
        });
        let steps = (!self.conversation.steps.is_empty()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children(self.conversation.steps.iter().map(|label| {
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
            .children(context.map(|(context, back_label)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().child(context))
                    .child(Button::new("back-to-kumo").label(back_label).on_click(
                        move |_, _, cx| {
                            let _gone = back_owner.update(cx, Presence::back_to_kumo);
                        },
                    ))
            }))
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
                    .text_color(if self.conversation.deleting {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().popover_foreground
                    })
                    .children(steps)
                    .child(self.conversation.answer.clone()),
            )
            .children(self.conversation.deleting.then(|| {
                div()
                    .text_xs()
                    .child(cx.global::<Strings>().get(Text::ThreadDeleting))
            }))
            .children(notice.map(|notice| div().text_xs().child(notice)))
            .child(Input::new(&self.input))
            .children(stop)
    }
}

fn submission_notice(
    error: &mut Option<SharedString>,
    refusal: &mut Option<Text>,
    result: Result<(), Refused>,
) {
    let text = match result {
        Ok(()) => {
            *error = None;
            None
        }
        Err(Refused::Blank) => return,
        Err(Refused::Busy) => Some(Text::TurnBusy),
        Err(Refused::Offline) => Some(Text::TurnOffline),
        Err(Refused::TooLong) => Some(Text::TurnTooLong),
    };
    *refusal = text;
}

fn typing_notice(
    error: Option<&SharedString>,
    refusal: Option<Text>,
    strings: &Strings,
) -> Option<SharedString> {
    refusal
        .map(|text| strings.get(text).into())
        .or_else(|| error.cloned())
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
    fn blank_enter_preserves_the_retained_notice_until_a_turn_is_sent() {
        let strings = crate::strings::Strings::english();
        let mut error = Some(gpui_kit::SharedString::from(
            "Couldn't save that setting. Try again.",
        ));
        let mut refusal = None;
        super::submission_notice(&mut error, &mut refusal, Err(crate::runs::Refused::Blank));
        assert_eq!(
            super::typing_notice(error.as_ref(), refusal, &strings).as_deref(),
            Some("Couldn't save that setting. Try again.")
        );
        assert_eq!(refusal, None);
        super::submission_notice(&mut error, &mut refusal, Ok(()));
        assert_eq!(error, None);
        assert_eq!(refusal, None);
    }

    #[test]
    fn a_local_refusal_preserves_the_dock_facing_brain_error() {
        let strings = crate::strings::Strings::english();
        let mut error = Some(gpui_kit::SharedString::from(
            "Your settings file can't be read, so changes aren't saved. Repair or delete config.toml, then restart.",
        ));
        let mut refusal = None;
        for (result, expected) in [
            (
                crate::runs::Refused::Offline,
                "Not sent: the background process isn't connected.",
            ),
            (
                crate::runs::Refused::TooLong,
                "That message is too long to send.",
            ),
        ] {
            super::submission_notice(&mut error, &mut refusal, Err(result));
            assert_eq!(
                error.as_deref(),
                Some(
                    "Your settings file can't be read, so changes aren't saved. Repair or delete config.toml, then restart."
                )
            );
            assert_eq!(
                super::typing_notice(error.as_ref(), refusal, &strings).as_deref(),
                Some(expected)
            );
        }
        super::submission_notice(&mut error, &mut refusal, Ok(()));
        assert_eq!(error, None);
        assert_eq!(refusal, None);
    }

    #[test]
    fn a_pre_settings_refusal_never_becomes_a_dock_or_tasks_error() {
        let strings = crate::strings::Strings::english();
        let mut runs = crate::runs::Runs::default();
        runs.connected(1);
        let result = runs.submit("typed", |_| panic!("not ready"));
        assert_eq!(result, Err(crate::runs::Refused::Offline));
        let mut error = None;
        let mut refusal = None;
        super::submission_notice(&mut error, &mut refusal, result);
        assert_eq!(error, None);
        assert_eq!(refusal, Some(crate::strings::Text::TurnOffline));
        assert_eq!(
            super::typing_notice(error.as_ref(), refusal, &strings).as_deref(),
            Some("Not sent: the background process isn't connected.")
        );
    }

    #[test]
    fn only_a_changed_conversation_requests_a_redraw() {
        use super::Conversation;
        let mut conversation = Conversation::default();
        assert!(!conversation.update(Conversation::default()));
        assert!(conversation.update(Conversation {
            answer: "hello".into(),
            ..Conversation::default()
        }));
        assert_eq!(conversation.answer.as_ref(), "hello");
        assert!(!conversation.update(Conversation {
            answer: "hello".into(),
            ..Conversation::default()
        }));
        assert!(conversation.update(Conversation {
            answer: "hello!".into(),
            ..Conversation::default()
        }));
        assert_eq!(conversation.answer.as_ref(), "hello!");
        assert!(conversation.update(Conversation {
            steps: vec!["step".into()],
            answer: "hello!".into(),
            deleting: true,
            context: Some(("You're in: Draft".into(), "Back to Kumo".into()))
        }));
        assert_eq!(
            conversation.steps.as_slice(),
            &[gpui_kit::SharedString::from("step")]
        );
        assert!(conversation.deleting);
        assert_eq!(
            conversation
                .context
                .as_ref()
                .map(|(label, back)| (label.as_ref(), back.as_ref())),
            Some(("You're in: Draft", "Back to Kumo"))
        );
        assert!(!conversation.update(Conversation {
            steps: vec!["step".into()],
            answer: "hello!".into(),
            deleting: true,
            context: Some(("You're in: Draft".into(), "Back to Kumo".into()))
        }));
    }

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
