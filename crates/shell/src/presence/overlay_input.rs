//! Paint surfaces share native input setup; selection endings restore it before notifying the picker.

use async_channel::Sender;
use gpui_kit::{App, Context, EventEmitter, Window};

use super::{OpenError, Presence};
use crate::platform::{self, PresenceEvent, PresenceSurface};
use crate::presenter::overlay_input::{Effect, ExitCause, Layer, OverlayOwner};

pub(super) fn configure_dock(
    window: &mut Window,
    events: Sender<PresenceEvent>,
    input: &OverlayOwner,
    windows: &mut platform::OverlayWindows,
    cx: &mut App,
) -> Result<(), platform::PresenceError> {
    super::configure_surface(window, PresenceSurface::Overlay, events, cx)?;
    windows.bind(Layer::Dock, window)?;
    input.configure(windows)
}

pub(super) fn configure_drawing(
    window: &mut Window,
    events: Sender<PresenceEvent>,
    input: &OverlayOwner,
    windows: &mut platform::OverlayWindows,
    cx: &mut App,
) -> Result<(), platform::PresenceError> {
    super::configure_surface(window, PresenceSurface::Overlay, events, cx)?;
    windows.bind(Layer::Overlay, window)?;
    input.configure(windows)
}

pub(super) fn show_drawing(
    window: gpui_kit::AnyWindowHandle,
    input: &OverlayOwner,
    windows: &mut platform::OverlayWindows,
    cx: &mut App,
) -> Result<(), OpenError> {
    super::overlay::show(window, cx)?;
    input.configure(windows)?;
    Ok(())
}

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
        self.publish_plain_escape(cx);
        run_effects(effects, |effect| {
            if effect == Effect::Configure {
                self.presenter.input.configure(&mut self.input_windows)?;
            } else {
                cx.emit(effect);
            }
            Ok(())
        })
    }
}

fn run_effects<E>(
    effects: Vec<Effect>,
    mut run: impl FnMut(Effect) -> Result<(), E>,
) -> Result<(), E> {
    let mut error = None;
    for effect in effects {
        if let Err(next) = run(effect) {
            error.get_or_insert(next);
        }
    }
    error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::GestureId;

    use super::*;
    use crate::presenter::overlay_input::{
        InputPlatform, OverlayInput, OverlayOwner, Regions, SelectionEnd,
    };

    #[derive(Default)]
    struct Platform {
        restores: usize,
    }

    impl InputPlatform for Platform {
        type Error = &'static str;

        fn configure(&mut self, mode: OverlayInput, _: &Regions) -> Result<(), Self::Error> {
            if mode == OverlayInput::Selecting {
                Ok(())
            } else {
                self.restores += 1;
                Err("restore failed")
            }
        }
    }

    #[test]
    fn failed_restore_still_emits_selection_ended_once() {
        let mut owner = OverlayOwner::default();
        let mut platform = Platform::default();
        let mut emitted = Vec::new();
        let gesture = GestureId(7);
        assert_eq!(
            crate::presenter::tests::select(&mut owner, gesture),
            vec![Effect::Configure]
        );
        assert_eq!(owner.configure(&mut platform), Ok(()));
        let effects = owner.end_selection(gesture, ExitCause::CaptureLost);
        assert_eq!(
            run_effects(effects, |effect| {
                if effect == Effect::Configure {
                    owner.configure(&mut platform)
                } else {
                    emitted.push(effect);
                    Ok(())
                }
            }),
            Err("restore failed")
        );
        let effects = owner.end_selection(gesture, ExitCause::BrainLost);
        assert_eq!(
            run_effects(effects, |effect| {
                emitted.push(effect);
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        assert_eq!(platform.restores, 1);
        assert_eq!(
            emitted,
            vec![Effect::SelectionEnded(
                gesture,
                ExitCause::CaptureLost,
                SelectionEnd::Cancelled
            )]
        );
        assert_eq!(owner.selection(), None);
    }
}
