//! The reusable typing surface and its activation lifetime.

use super::{
    Context, Diagnostics, Entity, Foreground, InputState, OpenError, PanelVisibility, Presence,
    PresenceEvent, PresenceSurface, SharedString, TextView, configure_surface, constants,
    diagnostics, options, platform, send,
};
use gpui_kit::{AnyWindowHandle, AppContext as _};

#[derive(Clone, Copy)]
enum Hide {
    Dismiss,
    Deactivated,
}

pub(super) struct Typing<T> {
    input: T,
    pub(super) visible: bool,
    active_seen: bool,
}

impl<T> Typing<T> {
    fn new(input: T) -> Self {
        Self {
            input,
            visible: false,
            active_seen: false,
        }
    }

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
    pub(super) typing: Typing<Entity<InputState>>,
    /// The window in front before the panel took focus, given back when it closes.
    previous: Option<Foreground>,
    /// Input diagnostics only: the draft at the last dismiss, compared on the next open.
    draft: Option<SharedString>,
    activation: Option<gpui_kit::Subscription>,
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
            .map(|saved| diagnostics::draft(&saved, &text.typing.input.read(cx).value()));
        let input = text.typing.input.clone();
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
        Ok(())
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
        Ok(TextPanel {
            window,
            typing: Typing::new(view.read(cx).input.clone()),
            view,
            previous: None,
            draft: None,
            activation: None,
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
        // shortcut: rapid native away/back within one queued GPUI activation can hide an active box; revisit if reproducible.
        if !text.typing.close(reason) {
            return Ok(());
        }
        if cx.has_global::<Diagnostics>() {
            text.draft = Some(text.typing.input.read(cx).value());
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Hide, Typing};

    #[test]
    fn reopening_active_input_keeps_blur_dismissal_and_the_draft() {
        let mut typing = Typing::new(String::from("unfinished draft"));
        typing.open();
        typing.activated();
        typing.open();
        assert!(typing.close(Hide::Deactivated));
        assert!(!typing.visible);
        assert_eq!(typing.input, "unfinished draft");
        typing.open();
        assert!(!typing.close(Hide::Deactivated));
        assert!(typing.visible);
        assert!(typing.close(Hide::Dismiss));
        typing.activated();
        assert!(!typing.close(Hide::Deactivated));
    }
}
