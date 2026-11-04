//! Settings window lifetime and nonblocking edits over the current session.

use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Context, Entity, Subscription};
use kumochi_protocol::message::{AccountOp, AssistantName, ControlToBrain, MaxFrameRate};

use super::settings_view::SettingsView;
use super::{
    Foreground, OpenError, Presence, PresenceSurface, configure_surface, options, platform,
};
use crate::settings::Field;
use crate::strings::{Strings, Text};

pub(super) struct SettingsPanel {
    pub(super) window: AnyWindowHandle,
    pub(super) view: Entity<SettingsView>,
    visible: bool,
    focused: bool,
    bounds: [i32; 4],
    previous: Option<Foreground>,
    activation: Option<Subscription>,
}

impl Presence {
    pub(super) fn dismiss_account_confirmation(&self, cx: &mut Context<'_, Self>) -> bool {
        self.settings_panel.as_ref().is_some_and(|panel| {
            panel
                .window
                .update(cx, |_, window, cx| {
                    if window.has_active_dialog(cx) {
                        window.close_dialog(cx);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        })
    }
    pub(super) fn account_op(&mut self, op: AccountOp, cx: &mut Context<'_, Self>) {
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let _sent = state.accounts.send(op, |op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(session, ControlToBrain::AccountOp(op))
                })
            });
            cx.notify();
        });
    }
    pub(super) fn sync_settings_notice(&mut self, cx: &mut Context<'_, Self>) {
        let previous = (self.error.clone(), self.refusal);
        self.error = self
            .settings
            .read(cx)
            .notice()
            .map(|error| cx.global::<Strings>().error(error).into());
        self.notices_changed(&previous, cx);
    }

    pub(super) fn settings_open(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|panel| panel.visible)
    }

    pub(super) fn settings_focused(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|panel| panel.visible && panel.focused)
    }

    pub(super) fn toggle_settings(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if self.settings_open() {
            return self.dismiss_settings(cx);
        }
        self.dismiss_text(cx)?;
        self.dismiss_threads(cx)?;
        if self.settings_panel.is_none() {
            self.settings_panel = Some(self.create_settings(cx)?);
        }
        self.pointer_settle = None;
        self.set_panel(super::PanelVisibility::Hidden, cx)?;
        let area = platform::work_area()?;
        let panel = self.settings_panel.as_mut().expect("created above");
        let rem = panel
            .window
            .update(cx, |_, window, _| f32::from(window.rem_size()))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        panel.bounds = self.layout.anchored_panel(
            area,
            super::threads::PANEL_REMS.map(|value| super::scaled(value * rem, self.scale)),
        );
        panel.visible = true;
        panel.view.update(cx, |view, cx| {
            view.stop = self.escape_hold.shows_stop();
            cx.notify();
        });
        panel
            .window
            .update(cx, |_, window, cx| {
                platform::place_presence_window(window, panel.bounds)?;
                window.bounds_changed(cx);
                match platform::activate_presence_window(window) {
                    Ok(previous) => {
                        if previous.is_some() {
                            panel.previous = previous;
                        }
                        panel.view.update(cx, |view, cx| view.focus(window, cx));
                    }
                    Err(error) => tracing::warn!(%error, "can't focus the settings panel"),
                }
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        Ok(())
    }

    fn create_settings(&self, cx: &mut Context<'_, Self>) -> Result<SettingsPanel, OpenError> {
        let model = self.settings.clone();
        let owner = cx.weak_entity();
        let close_owner = owner.clone();
        let events = self.events.clone();
        let (window, view) = gpui_kit::open_window(options(), cx, |window, cx| {
            window.set_window_title(&cx.global::<Strings>().get(Text::Settings));
            window.on_window_should_close(cx, move |_, cx| {
                let owner = close_owner.clone();
                cx.defer(move |cx| {
                    if let Ok(Err(error)) = owner.update(cx, Presence::dismiss_settings) {
                        tracing::error!(%error, "can't dismiss settings");
                    }
                });
                false
            });
            cx.new(|cx| SettingsView::new(model, owner, events, window, cx))
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
        window
            .update(cx, |_, window, cx| {
                configure_surface(window, PresenceSurface::Text, self.events.clone(), cx)?;
                platform::set_capture(
                    window,
                    if self.gesture.get().holding() {
                        super::Capture::Excluded
                    } else {
                        super::Capture::Allowed
                    },
                )
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let result = window.update(cx, |_, window, cx| {
                owner.update(cx, |this, cx| {
                    let activation = cx.observe_window_activation(window, |this, window, _| {
                        if let Some(panel) = &mut this.settings_panel {
                            panel.focused = window.is_window_active();
                        }
                    });
                    if let Some(panel) = &mut this.settings_panel {
                        panel.focused = window.is_window_active();
                        panel.activation = Some(activation);
                    }
                })
            });
            if let Err(error) = result {
                tracing::error!(%error, "can't observe settings activation");
            }
        });
        Ok(SettingsPanel {
            window,
            view,
            visible: false,
            focused: false,
            bounds: [0; 4],
            previous: None,
            activation: None,
        })
    }

    pub(super) fn dismiss_settings(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if let Some(field) = self
            .settings_panel
            .as_ref()
            .filter(|panel| panel.visible)
            .and_then(|panel| panel.view.read(cx).editing())
        {
            self.save_setting(field, cx);
        }
        let Some(panel) = &mut self.settings_panel else {
            return Ok(());
        };
        if !panel.visible {
            return Ok(());
        }
        panel.visible = false;
        panel.focused = false;
        let previous = panel.previous.take();
        panel
            .window
            .update(cx, |_, window, cx| {
                panel
                    .view
                    .update(cx, |view, cx| view.finish_edit(window, cx));
                if let Some(previous) = previous {
                    platform::restore_foreground(window, previous);
                }
                platform::hide_presence_window(window)?;
                Ok::<_, platform::PresenceError>(())
            })
            .map_err(|error| OpenError::Gpui(error.into()))??;
        self.pointer_changed(cx)
    }

    pub(super) fn cancel_settings_edit(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if let Some(panel) = &self.settings_panel {
            panel
                .window
                .update(cx, |_, window, cx| {
                    panel
                        .view
                        .update(cx, |view, cx| view.cancel_edit(window, cx));
                })
                .map_err(|error| OpenError::Gpui(error.into()))?;
        }
        Ok(())
    }

    pub(super) fn rename_assistant(
        &mut self,
        name: AssistantName,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Text> {
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let sent = state.send_assistant(name, |op| {
                session
                    .as_ref()
                    .is_some_and(|session| link.send_control(session, ControlToBrain::Settings(op)))
            });
            cx.notify();
            if sent {
                Ok(())
            } else {
                Err(state.rename_refusal())
            }
        })
    }

    pub(super) fn save_max_frame_rate(&mut self, rate: MaxFrameRate, cx: &mut Context<'_, Self>) {
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let sent = state.send_max_frame_rate(rate, |op| {
                session
                    .as_ref()
                    .is_some_and(|session| link.send_control(session, ControlToBrain::Settings(op)))
            });
            if sent {
                cx.notify();
            }
        });
    }

    pub(super) fn save_setting(&mut self, field: Field, cx: &mut Context<'_, Self>) -> bool {
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let sent = state.save(field, |op| {
                session
                    .as_ref()
                    .is_some_and(|session| link.send_control(session, ControlToBrain::Settings(op)))
            });
            cx.notify();
            sent
        })
    }
}
