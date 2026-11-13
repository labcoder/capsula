//! A hold's screen part, and the release a waiting part owes the worker when it closes.

use std::sync::Arc;

use tokio::time::Instant;

use crate::screen::{Generation, MAX_TTL, ScreenContext};

/// Why a screen hold's part closed without an image. The other classes come with the worker's
/// answers that produce them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// The hold closed, or submitted, while the part was still waiting.
    NotReady,
}

/// `None` is a voice hold. A screen hold starts at `Waiting`, and every closer leaves `Closed`.
pub(super) enum ScreenPart {
    None,
    /// `generation` is the worker that captured, once one answered.
    Waiting {
        generation: Option<Generation>,
    },
    Ready(ScreenContext),
    Closed(Class),
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
        }
    }
}

impl super::Holds {
    /// Closes the pending hold's screen part. A `Ready` context is zeroed as it drops; a waiting
    /// part may hold a frame in the worker, so the worker is told to release it.
    pub(super) fn close_screen(&mut self) {
        let Some(hold) = &mut self.pending else {
            return;
        };
        if matches!(hold.screen, ScreenPart::None | ScreenPart::Closed(_)) {
            return;
        }
        let ScreenPart::Waiting { generation } =
            std::mem::replace(&mut hold.screen, ScreenPart::Closed(Class::NotReady))
        else {
            return;
        };
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
