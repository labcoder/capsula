//! A hold's screen part, and the release a waiting part owes the worker when it closes.

use std::sync::Arc;

use tokio::time::Instant;

use crate::context::{Screen, ScreenClass};
use crate::screen::{Generation, MAX_TTL, ScreenContext};

/// `None` is a voice hold. A screen hold starts at `Waiting`; its run takes what it ends with,
/// and every other closer leaves `Closed` or `Discarded`.
pub(super) enum ScreenPart {
    None,
    /// `generation` is the worker that captured, once one answered.
    Waiting {
        generation: Option<Generation>,
    },
    Ready(ScreenContext),
    Closed(ScreenClass),
    /// A ready image dropped, and so zeroed, because its hold ended without a run.
    Discarded,
}

impl std::fmt::Debug for ScreenPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Waiting { generation } => f
                .debug_struct("Waiting")
                .field("generation", generation)
                .finish(),
            Self::Ready(_) => f.write_str("Ready"),
            Self::Closed(class) => f.debug_tuple("Closed").field(class).finish(),
            Self::Discarded => f.write_str("Discarded"),
        }
    }
}

impl super::Holds {
    /// Closes the pending hold's screen part for a hold that ends without a run.
    pub(super) fn close_screen(&mut self) {
        self.retire_waiting();
        if let Some(hold) = &mut self.pending
            && matches!(hold.screen, ScreenPart::Ready(_))
        {
            hold.screen = ScreenPart::Discarded;
        }
    }

    /// What the pending hold's screen part hands its run. A part still waiting closes first.
    pub(super) fn submit_screen(&mut self) -> Option<Screen> {
        self.retire_waiting();
        let hold = self.pending.as_mut()?;
        match std::mem::replace(&mut hold.screen, ScreenPart::None) {
            ScreenPart::Ready(context) => Some(Screen::Image(context.into_jpeg())),
            ScreenPart::Closed(class) => Some(Screen::NotIncluded(class)),
            ScreenPart::None | ScreenPart::Waiting { .. } | ScreenPart::Discarded => None,
        }
    }

    /// A waiting part may hold a frame in the worker, so it closes as `NotReady` and the worker
    /// is told to release it.
    fn retire_waiting(&mut self) {
        let Some(hold) = &mut self.pending else {
            return;
        };
        let ScreenPart::Waiting { generation } = hold.screen else {
            return;
        };
        hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
        // No runtime means the brain is shutting down, and its job ends the worker.
        let (Some(client), Ok(runtime)) = (&self.screen, tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let client = Arc::clone(client);
        let owner = hold.key.into();
        drop(runtime.spawn(async move {
            if client
                .retire(owner, generation, Instant::now() + MAX_TTL)
                .await
                .is_err()
            {
                tracing::debug!("a held screen frame's release wasn't shown");
            }
        }));
    }
}
