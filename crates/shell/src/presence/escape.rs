//! Local Esc routing for the focused surface.

use super::{Context, Escape, EscapePanel, OpenError, Presence, diagnostics};
use crate::escape;

impl Presence {
    /// One Esc press, from the hotkey, the typing panel or Stop. The local effect happens here,
    /// and only then is `Stop` or `ClearDrawing` queued for the brain, which nothing waits on.
    pub(super) fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
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
        let action = escape::decide(self.activity, panel);
        let tell = match action {
            Escape::Stop => self.runs.stop(),
            Escape::Clear => {
                let effect = self.runs.clear();
                self.grant(effect.credit);
                effect.clear
            }
            Escape::Dismiss => {
                if panel == EscapePanel::Settings {
                    self.dismiss_settings(cx)?;
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
        self.activity = self.activity();
        self.refresh_conversation(cx);
        // Stamped once the local stop or clear is applied, for the Esc smoke's timing.
        diagnostics::record(cx, format_args!("escape action={action:?}"));
        if let Some(message) = tell {
            self.tell(message);
        }
        self.escape_fixture = false;
        self.sync_escape(cx)
    }
}
