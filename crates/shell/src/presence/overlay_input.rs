//! Selection endings restore native input before notifying the future picker.

use gpui_kit::{Context, EventEmitter};

use super::{OpenError, Presence};
use crate::presenter::overlay_input::{Effect, ExitCause};

impl EventEmitter<Effect> for Presence {}

impl Presence {
    pub(super) fn end_selection(&mut self, cause: ExitCause, cx: &mut Context<'_, Self>) {
        let Some(gesture) = self.presenter.input.selection() else {
            return;
        };
        let effects = self.presenter.input.end_selection(gesture, cause);
        if let Err(error) = self.selection_ending(effects, cx) {
            tracing::warn!(%error, "can't restore overlay input");
        }
    }

    pub(super) fn selection_ending(
        &mut self,
        effects: Vec<Effect>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        for effect in effects {
            if effect == Effect::Configure {
                self.presenter.input.configure(&mut self.input_windows)?;
            } else {
                cx.emit(effect);
            }
        }
        Ok(())
    }
}
