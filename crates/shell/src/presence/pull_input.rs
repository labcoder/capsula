//! The pull's input from the native bridge: capture losses, and the reconcile that follows a
//! loss once the release that caused it has been forwarded. How it fits:
//! docs/development/kumo.md, "The native bridge".

use gpui_kit::{Context, MouseButton};

use super::Presence;
use super::drag::Event;
use crate::platform::input::{Input, input};

impl Presence {
    /// A presence window lost capture. The drag decides whether the loss ends it; one its own
    /// button's release caused is reconciled only after that release's forward has returned.
    pub(super) fn capture_lost(
        &mut self,
        seq: u64,
        released: Option<MouseButton>,
        cx: &mut Context<'_, Self>,
    ) {
        self.pulling
            .handle(Event::CaptureLost { seq, released }, [0, 0]);
        if input(Input::reconcile_after_release) {
            let owner = cx.entity().downgrade();
            cx.defer(move |cx| {
                if owner.update(cx, |this, _| this.reconcile_pull()).is_err() {
                    tracing::debug!("the presence closed before the drag reconciled");
                }
            });
        }
    }

    /// The release that took capture has been forwarded: a drag still waiting for it is lost.
    pub(super) fn reconcile_pull(&mut self) {
        self.pulling.reconcile();
    }
}
