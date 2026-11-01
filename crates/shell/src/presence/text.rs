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

struct Deactivation {
    visible: bool,
    active_seen: bool,
    active: bool,
    generation: u64,
}

impl Deactivation {
    fn should_hide(&self, pending_generation: u64) -> bool {
        self.visible && self.active_seen && !self.active && self.generation == pending_generation
    }
}

/// The typing surface: the one window Capsula activates, and only on a double tap.
pub(super) struct TextPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<TextView>,
    input: Entity<InputState>,
    /// The window in front before the panel took focus, given back when it closes.
    previous: Option<Foreground>,
    pub(super) visible: bool,
    /// Input diagnostics only: the draft at the last dismiss, compared on the next open.
    draft: Option<SharedString>,
    generation: u64,
    active_seen: bool,
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
        text.generation += 1;
        text.active_seen = false;
        text.window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, bounds)?;
                window.bounds_changed(cx);
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        text.visible = true;
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
                        let Some(text) = this.text.as_mut().filter(|text| text.visible) else {
                            return;
                        };
                        if window.is_window_active() {
                            text.active_seen = true;
                            return;
                        }
                        let generation = text.generation;
                        let owner = cx.weak_entity();
                        cx.defer(move |cx| {
                            let result =
                                owner.update(cx, |this, cx| this.deactivated(generation, cx));
                            if let Ok(Err(error)) = result {
                                tracing::error!(%error, "can't hide the inactive typing panel");
                            }
                        });
                    });
                    if let Some(text) = &mut this.text {
                        text.active_seen |= text.visible && window.is_window_active();
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
            input: view.read(cx).input.clone(),
            view,
            previous: None,
            visible: false,
            draft: None,
            generation: 0,
            active_seen: false,
            activation: None,
        })
    }

    /// Hides the typing panel, keeping its draft, and gives the front back if it still has it.
    pub(super) fn dismiss_text(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.hide_text(Hide::Dismiss, cx)
    }

    fn hide_text(&mut self, reason: Hide, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let bounds = self.layout.panel;
        let Some(text) = self.text.as_mut().filter(|text| text.visible) else {
            return Ok(());
        };
        text.visible = false;
        if cx.has_global::<Diagnostics>() {
            text.draft = Some(text.input.read(cx).value());
        }
        text.active_seen = false;
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
        // The typing panel was covering the hover panel's Stop.
        if self.escape_hold.shows_stop() {
            self.pointer_changed(cx)?;
        }
        Ok(())
    }

    fn deactivated(
        &mut self,
        generation: u64,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(text) = self.text.as_ref() else {
            return Ok(());
        };
        let active = text
            .window
            .update(cx, |_, window, _| window.is_window_active())
            .map_err(|error| OpenError::Gpui(error.into()))?;
        if (Deactivation {
            visible: text.visible,
            active_seen: text.active_seen,
            active,
            generation: text.generation,
        })
        .should_hide(generation)
        {
            self.hide_text(Hide::Deactivated, cx)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Deactivation;

    #[test]
    fn only_current_visible_previously_active_input_hides_on_blur() {
        let mut state = Deactivation {
            visible: true,
            active_seen: true,
            active: false,
            generation: 2,
        };
        assert!(state.should_hide(2));
        assert!(!state.should_hide(1));
        state.active = true;
        assert!(!state.should_hide(2));
        state.active = false;
        state.active_seen = false;
        assert!(!state.should_hide(2));
        state.active_seen = true;
        state.visible = false;
        assert!(!state.should_hide(2));
    }
}
