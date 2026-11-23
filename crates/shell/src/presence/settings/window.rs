//! Settings window lifetime and nonblocking edits over the current session.

#[cfg(test)]
mod tests;

use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Context, Entity, Subscription};
use kumochi_protocol::message::{AccountOp, AssistantName, ControlToBrain, MaxFrameRate};

use super::super::{
    Foreground, OpenError, Presence, PresenceSurface, configure_surface, options, platform,
};
use super::{SettingsTab, SettingsView};
use crate::settings::{AfterSave, BeforeSave, Field, Save, SaveOutcome};
use crate::strings::{Strings, Text};

pub(in crate::presence) struct SettingsPanel {
    pub(in crate::presence) window: AnyWindowHandle,
    pub(in crate::presence) view: Entity<SettingsView>,
    visible: bool,
    focused: bool,
    pub(in crate::presence) bounds: [i32; 4],
    previous: Option<Foreground>,
    activation: Option<Subscription>,
}

impl Presence {
    pub(in crate::presence) fn request_save(
        &mut self,
        save: Save,
        cx: &mut Context<'_, Self>,
    ) -> SaveOutcome {
        match save.before() {
            Some(BeforeSave::CancelVoice) => self.cancel_voice(cx),
            Some(BeforeSave::RevokeScreenOffer) => {
                self.voice.gates.publish(None);
            }
            None => {}
        }
        let pair = self.session.as_ref().map(|session| (&self.link, session));
        let outcome = self.settings.update(cx, |state, cx| {
            let outcome = state.apply_save(save, pair);
            if outcome.notify {
                cx.notify();
            }
            outcome
        });
        match outcome.after {
            Some(AfterSave::FollowAndCancelVoice) => {
                // The next press reads the mirror before any link batch refreshes it.
                self.follow_voice_input(cx);
                self.cancel_voice(cx);
            }
            Some(AfterSave::RefreshScreenOffer { clear_marks }) => {
                if clear_marks {
                    self.marks_off(cx);
                }
                self.publish_offer(cx);
            }
            Some(AfterSave::SyncTaskbar) => self.sync_taskbar(cx),
            None => {}
        }
        outcome
    }

    pub(in crate::presence) fn speech_input_action(
        &mut self,
        intent: crate::settings::SpeechInputIntent,
        cx: &mut Context<'_, Self>,
    ) {
        self.request_save(Save::SpeechInput(intent), cx);
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the checkbox and wire setter carry this boolean unchanged"
    )]
    pub(in crate::presence) fn save_hold_to_talk(
        &mut self,
        on: bool,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.request_save(Save::HoldToTalk(on), cx)
            .result
            .unwrap_or(false)
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the checkbox and wire setter carry this boolean unchanged"
    )]
    pub(in crate::presence) fn save_show_screen(
        &mut self,
        on: bool,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.request_save(Save::ShowScreen(on), cx)
            .result
            .unwrap_or(false)
    }

    pub(in crate::presence) fn open_accounts(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.open_settings_tab(SettingsTab::Accounts, cx)
    }

    pub(in crate::presence) fn open_general(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.open_settings_tab(SettingsTab::General, cx)
    }

    fn open_settings_tab(
        &mut self,
        tab: SettingsTab,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if !self.settings_open() {
            self.toggle_settings(cx)?;
        }
        let panel = self.settings_panel.as_ref().expect("settings opened above");
        panel
            .window
            .update(cx, |_, window, cx| {
                panel
                    .view
                    .update(cx, |view, cx| view.select_tab(tab, window, cx));
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
        Ok(())
    }

    /// A Claude Code op from any surface: the attempt is the user's to steer there now, so an
    /// owed cancel is dropped.
    pub(in crate::presence) fn claude_op(
        &mut self,
        op: kumochi_protocol::message::ConnectionOp,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.onboarding_claude.superseded();
        self.settings_claude.superseded();
        self.cancel_voice(cx);
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let sent = state.claude.send(op, |op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(session, ControlToBrain::Connection(op))
                })
            });
            cx.notify();
            sent
        })
    }
    pub(in crate::presence) fn sync_taskbar(&mut self, cx: &mut Context<'_, Self>) {
        let native = &mut self.native;
        self.settings.update(cx, |state, cx| {
            state.apply_taskbar(|choice| native.set_taskbar_button(choice).is_ok());
            cx.notify();
        });
        self.refresh_dock(cx);
    }

    pub(in crate::presence) fn dismiss_account_confirmation(
        &self,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.settings_panel.as_ref().is_some_and(|panel| {
            panel
                .window
                .update(cx, |_, window, cx| {
                    if window.has_active_dialog(cx) {
                        panel.view.update(cx, |view, _| view.confirmation = None);
                        crate::confirmation::retire_in(window, cx);
                        window.close_dialog(cx);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        })
    }
    /// An account op from any surface: the attempt is the user's to steer there now, so an owed
    /// cancel is dropped.
    pub(in crate::presence) fn account_op(
        &mut self,
        op: AccountOp,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.onboarding_chatgpt.superseded();
        self.settings_chatgpt.superseded();
        self.cancel_voice(cx);
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            let sent = state.accounts.send(op, |op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(session, ControlToBrain::AccountOp(op))
                })
            });
            cx.notify();
            sent
        })
    }
    pub(in crate::presence) fn sync_settings_notice(&mut self, cx: &mut Context<'_, Self>) {
        let previous_error = self.error.clone();
        self.error = self
            .settings
            .read(cx)
            .notice()
            .map(|error| cx.global::<Strings>().error(error).into());
        self.notices_changed(previous_error.as_ref(), cx);
    }

    pub(in crate::presence) fn settings_open(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|panel| panel.visible)
    }

    pub(in crate::presence) fn settings_focused(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|panel| panel.visible && panel.focused)
    }

    pub(in crate::presence) fn toggle_settings(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if self.settings_open() {
            return self.dismiss_settings(cx);
        }
        self.dismiss_text(cx)?;
        self.dismiss_threads(cx)?;
        if self.settings_panel.is_none() {
            self.settings_panel = Some(self.create_settings(cx)?);
        }
        self.protect_surface(super::super::exclusion::Slot::Settings, cx)?;
        self.pointer_settle = None;
        self.set_panel(super::super::PanelVisibility::Hidden, cx)?;
        let area = platform::work_area()?;
        let panel = self.settings_panel.as_mut().expect("created above");
        let rem = panel
            .window
            .update(cx, |_, window, _| f32::from(window.rem_size()))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        panel.bounds = self.layout.anchored_panel(
            area,
            super::super::threads::PANEL_REMS
                .map(|value| super::super::scaled(value * rem, self.scale)),
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
        self.place_drawings(cx.weak_entity(), cx);
        Ok(())
    }

    fn create_settings(&mut self, cx: &mut Context<'_, Self>) -> Result<SettingsPanel, OpenError> {
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
                self.input_windows
                    .bind(crate::presenter::overlay_input::Layer::Panels, window)?;
                Ok::<_, platform::PresenceError>(())
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

    pub(in crate::presence) fn dismiss_settings(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if !self.settings_open() {
            return Ok(());
        }
        self.dismiss_github_settings(cx);
        self.cancel_settings_sign_ins(cx);
        if let Some(field) = self
            .settings_panel
            .as_ref()
            .filter(|panel| panel.visible)
            .and_then(|panel| panel.view.read(cx).editing())
        {
            self.save_setting(field, cx);
        }
        let panel = self
            .settings_panel
            .as_mut()
            .expect("the Settings panel is visible");
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
        self.place_drawings(cx.weak_entity(), cx);
        self.pointer_changed(cx)
    }

    pub(in crate::presence) fn cancel_settings_edit(
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

    pub(in crate::presence) fn rename_assistant(
        &mut self,
        name: AssistantName,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Text> {
        self.request_save(Save::RenameAssistant(name), cx)
            .result
            .map(|_| ())
    }

    pub(in crate::presence) fn save_max_frame_rate(
        &mut self,
        rate: MaxFrameRate,
        cx: &mut Context<'_, Self>,
    ) {
        self.request_save(Save::FrameRate(rate), cx);
    }

    pub(in crate::presence) fn save_default_model(
        &mut self,
        connection: kumochi_protocol::message::ConnectionKind,
        model: kumochi_protocol::message::ModelSlug,
        cx: &mut Context<'_, Self>,
    ) {
        self.request_save(Save::DefaultModel(connection, model), cx);
    }

    pub(in crate::presence) fn save_setting(
        &mut self,
        field: Field,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.request_save(Save::Name(field), cx)
            .result
            .unwrap_or(false)
    }
}
