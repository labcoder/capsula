//! One paired offer for the displayed recipient and current draft.

use kumochi_protocol::geometry::TypedDisplay;
use kumochi_protocol::message::{
    ControlToBrain, ThreadId, TypedScreenDraft, TypedScreenOffer, TypedScreenRevision,
};

use crate::link::{LinkEvent, LinkHandle, Session};
use crate::presence::exclusion::machine::{Error, Exclusion, Key, Surfaces};

use super::Runs;

#[cfg(test)]
mod tests;

/// The paired link and exact-surface lease owner for this capture.
pub struct TypedScreenIo<'a, S: Surfaces> {
    /// The link that holds this paired connection.
    pub link: &'a LinkHandle,
    /// The originating pair.
    pub session: &'a Session,
    /// The shell's single exclusion lease owner.
    pub exclusion: &'a mut Exclusion<S::Receipt>,
    /// The current surfaces and their native exclusion receipts.
    pub surfaces: &'a mut S,
}

#[derive(Clone, Copy)]
struct Request {
    turn: u32,
    thread: Option<ThreadId>,
    draft: TypedScreenDraft,
    recipient: TypedScreenRevision,
}

enum Offer {
    Waiting {
        request: Request,
        withdrawn: bool,
    },
    Armed {
        request: Request,
        offer: TypedScreenOffer,
    },
}

#[derive(Clone, Copy)]
struct Capture {
    generation: u64,
    turn: u32,
    offer: TypedScreenOffer,
    key: Option<Key>,
}

struct Retirement {
    capture: Capture,
    cancel: bool,
}

#[derive(Default)]
pub(super) struct TypedScreen {
    generation: u64,
    revision: u64,
    displayed: Option<TypedScreenRevision>,
    offer: Option<Offer>,
    retiring: Option<Retirement>,
}

impl TypedScreen {
    pub(super) fn connected(&mut self, generation: u64) {
        if self.generation != 0 && self.generation != generation {
            self.disconnected();
        }
        self.generation = generation;
    }

    pub(super) fn disconnected(&mut self) {
        self.revoke();
        self.offer = None;
        self.displayed = None;
        self.generation = 0;
    }

    pub(super) fn revoke(&mut self) {
        match self.offer.take() {
            Some(Offer::Waiting { request, .. }) => {
                self.offer = Some(Offer::Waiting {
                    request,
                    withdrawn: true,
                });
            }
            Some(Offer::Armed { request, offer }) => {
                self.retiring = Some(Retirement {
                    capture: Capture {
                        generation: self.generation,
                        turn: request.turn,
                        offer,
                        key: None,
                    },
                    cancel: true,
                });
            }
            None => {}
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.revoke();
    }

    pub(super) fn blocks_plain(&mut self) -> bool {
        if self.offer.is_some() || self.retiring.is_some() {
            self.revoke();
            return true;
        }
        false
    }
}

impl Runs {
    pub(super) fn typed_draft(&self) -> TypedScreenDraft {
        TypedScreenDraft {
            revision: self.typed_screen.revision,
            selection: self.revision,
        }
    }

    /// Changes the logical composer draft, not its automatic clear after Send.
    pub fn typed_draft_changed(&mut self) {
        self.typed_screen.invalidate();
        self.typed_screen.revision = self.typed_screen.revision.saturating_add(1);
    }

    /// Revokes the opt-in or in-flight capture; flush the revocation before another offer.
    pub fn revoke_typed_screen(&mut self) {
        self.typed_screen.revoke();
    }

    /// Records this event only once its model choice is actually displayed in the composer.
    pub fn typed_screen_recipient_displayed(&mut self, event: &LinkEvent) {
        let LinkEvent::TypedScreenRecipient {
            generation,
            recipient,
        } = event
        else {
            return;
        };
        let state = &mut self.typed_screen;
        if !self.connected || *generation != state.generation {
            return;
        }
        let displayed = recipient.as_ref().map(|recipient| recipient.revision);
        if state.displayed != displayed {
            if state.offer.is_some() {
                state.revoke();
            }
            state.displayed = displayed;
        }
    }

    /// Requests one offer for the current draft, selection, next turn and exact display.
    pub fn request_typed_screen(
        &mut self,
        link: &LinkHandle,
        session: &Session,
        display: TypedDisplay,
    ) -> bool {
        let thread = self.thread();
        let state = &self.typed_screen;
        let Some(recipient) = state.displayed else {
            return false;
        };
        if !self.connected
            || self.main.is_none()
            || state.revision == u64::MAX
            || state.generation != session.generation()
            || thread.is_some_and(|thread| self.holds(thread))
            || state.offer.is_some()
            || state.retiring.is_some()
            || display.dpi == 0
            || display.bounds.size.contains(&0)
        {
            return false;
        }
        let Some(turn) = self.turns_sent.checked_add(1) else {
            return false;
        };
        let draft = self.typed_draft();
        if !link.send_control(
            session,
            ControlToBrain::RequestTypedScreenOffer {
                turn,
                thread,
                draft,
                recipient,
                display: Box::new(display),
            },
        ) {
            return false;
        }
        self.typed_screen.offer = Some(Offer::Waiting {
            request: Request {
                turn,
                thread,
                draft,
                recipient,
            },
            withdrawn: false,
        });
        true
    }

    /// Accepts only the response to the still-current request, cancelling late positive offers.
    pub fn typed_screen_offered(
        &mut self,
        link: &LinkHandle,
        session: &Session,
        event: &LinkEvent,
    ) -> bool {
        let LinkEvent::TypedScreenOffered {
            generation,
            turn,
            thread,
            draft,
            recipient,
            offer,
        } = *event
        else {
            return false;
        };
        let state = &mut self.typed_screen;
        if generation != state.generation || generation != session.generation() || !session.open() {
            return false;
        }
        if let Some(Offer::Waiting { request, withdrawn }) = state.offer.as_ref()
            && (
                request.turn,
                request.thread,
                request.draft,
                request.recipient,
            ) == (turn, thread, draft, recipient)
        {
            let armed = offer
                .filter(|_| !*withdrawn && state.displayed == Some(recipient))
                .map(|offer| Offer::Armed {
                    request: *request,
                    offer,
                });
            let accepted = armed.is_some();
            state.offer = armed;
            if accepted {
                return true;
            }
        } else if state.offer.as_ref().is_some_and(|state| {
            matches!(state, Offer::Armed { request, offer: armed } if
                (request.turn, request.thread, request.draft, request.recipient, Some(*armed))
                    == (turn, thread, draft, recipient, offer))
        }) {
            return false;
        }
        if let Some(offer) = offer {
            link.cancel_typed_screen(session, turn, offer);
        }
        false
    }

    /// Flushes an exact revocation before restoring its lease; a full control queue ends the pair.
    ///
    /// # Errors
    /// Returns a restoration debt; call again to retry restoration.
    pub fn flush_typed_screen_cancel(
        &mut self,
        io: &mut TypedScreenIo<'_, impl Surfaces>,
    ) -> Result<(), Error> {
        let Some(retiring) = &mut self.typed_screen.retiring else {
            return Ok(());
        };
        let capture = retiring.capture;
        if std::mem::take(&mut retiring.cancel) && capture.generation == io.session.generation() {
            io.link
                .cancel_typed_screen(io.session, capture.turn, capture.offer);
        }
        if let Some(key) = capture.key {
            io.exclusion.restore_matching(key, io.surfaces)?;
        }
        self.typed_screen.retiring = None;
        Ok(())
    }
}
