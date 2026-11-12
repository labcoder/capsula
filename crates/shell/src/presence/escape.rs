//! Local Esc routing for the focused surface.

use super::drag::Event;
use super::{Activity, Context, Escape, EscapePanel, OpenError, Presence, diagnostics};
use crate::escape;

impl Presence {
    /// One Esc press, from the hotkey, the typing panel or Stop. The local effect happens here,
    /// and only then is `Stop` or `ClearDrawing` queued for the brain, which nothing waits on.
    #[expect(
        clippy::too_many_lines,
        reason = "one Esc press keeps its local decision and every effect of it in order"
    )]
    pub(super) fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        if self.presenter.input.selection().is_some() {
            self.end_selection(crate::presenter::overlay_input::ExitCause::Escape, cx);
            return Ok(());
        }
        let panel = if self.settings_focused() {
            if self
                .settings_panel
                .as_ref()
                .is_some_and(|panel| panel.view.read(cx).editing().is_some())
            {
                EscapePanel::SettingsEdit
            } else {
                EscapePanel::Settings
            }
        } else if self.threads_focused() {
            if self
                .thread_panel
                .as_ref()
                .is_some_and(|panel| panel.view.read(cx).editing())
            {
                EscapePanel::Rename
            } else {
                EscapePanel::Threads
            }
        } else if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let action = local_decision(self.activity, panel, &self.presenter);
        let tell = match action {
            Escape::CancelPull => {
                self.kumo
                    .update(cx, |kumo, cx| kumo.pull_event(Event::Escape, cx));
                None
            }
            Escape::Stop => {
                if let Some(message) = self.runs.stop() {
                    self.stop_run(message, cx);
                } else if let Some(run) = self.presenter.stoppable_run()
                    && self.presenter.live_work(std::time::Instant::now())
                {
                    self.place_drawings(cx.weak_entity(), cx);
                    let effects = self.presenter.stop(run, std::time::Instant::now());
                    self.scene_effects(effects, cx);
                }
                None
            }
            Escape::Clear => {
                let effect = self.runs.clear();
                self.grant(effect.credit);
                if self.presenter.holds_scenes() {
                    let effects = self.presenter.clear(std::time::Instant::now());
                    self.scene_effects(effects, cx);
                    None
                } else {
                    effect.clear
                }
            }
            Escape::Dismiss => {
                self.runs.dismiss_retry();
                if panel == EscapePanel::Settings {
                    if !self.dismiss_account_confirmation(cx) {
                        self.dismiss_settings(cx)?;
                    }
                } else if panel == EscapePanel::Threads {
                    self.dismiss_threads(cx)?;
                } else {
                    self.dismiss_text(cx)?;
                }
                None
            }
            Escape::CancelEdit => {
                if panel == EscapePanel::SettingsEdit {
                    self.cancel_settings_edit(cx)?;
                } else if let Some(panel) = &self.thread_panel {
                    panel
                        .window
                        .update(cx, |_, window, cx| {
                            panel
                                .view
                                .update(cx, |view, cx| view.cancel_rename(window, cx));
                        })
                        .map_err(|error| OpenError::Gpui(error.into()))?;
                }
                None
            }
            Escape::Ignore => None,
        };
        let panel = if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        self.activity = Activity {
            pulling: self.kumo.read(cx).pull_live(),
            ..Self::activity(&self.runs, panel, &self.presenter)
        };
        self.refresh_conversation(cx);
        // Stamped once the local stop or clear is applied, for the Esc smoke's timing.
        diagnostics::record(cx, format_args!("escape action={action:?}"));
        if let Some(message) = tell {
            self.stop_run(message, cx);
        }
        self.escape_fixture = false;
        self.sync_escape(cx)
    }
}

pub(super) fn local_decision(
    mut activity: Activity,
    panel: EscapePanel,
    presenter: &crate::presenter::Presenter,
) -> Escape {
    activity.drawing |= matches!(panel, EscapePanel::Open) && presenter.holds_scenes();
    escape::decide(activity, panel)
}
