//! One pictured submit and its explicit, plain-text recovery.

use std::time::Instant;

use kumochi_protocol::geometry::{ReleasePointer, TypedDisplay};
use kumochi_protocol::message::{
    AudioSession, CaptureFor, Content, ControlToBrain, DataToBrain, ErrorKey, RunId, ScreenChoice,
    ThreadId, TypedScreenDraft, TypedScreenOffer, TypedScreenSubmit,
};

use crate::link::{LinkEvent, LinkHandle, Session};
use crate::presence::exclusion::machine::{Error, Exclusion, Key, Surfaces};

use super::{Refused, Runs, Submit};

const TYPED_SUBMIT_OVERHEAD: usize = 160;

#[cfg(test)]
mod tests;

/// What the composer saw at Send, before the exclusion setters run.
#[derive(Clone, Copy, Debug)]
pub struct TypedScreenInput {
    /// The composer's display at Send.
    pub display: TypedDisplay,
    /// A physical point on that display.
    pub point: ReleasePointer,
    /// The Send attempt's counter stamp.
    pub submitted_qpc: u64,
    /// The Send attempt's local time.
    pub now: Instant,
}

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
    display: TypedDisplay,
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

struct Words {
    request: Request,
    text: Content,
}

struct InFlight {
    capture: Capture,
    words: Words,
    run: Option<RunId>,
    confirmed: bool,
}

struct Retirement {
    capture: Capture,
    cancel: bool,
}

#[derive(Default)]
pub(super) struct TypedScreen {
    generation: u64,
    revision: u64,
    offer: Option<Offer>,
    in_flight: Option<InFlight>,
    recovery: Option<Words>,
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
        if let Some(picture) = self.in_flight.take() {
            self.retiring = Some(Retirement {
                capture: picture.capture,
                cancel: true,
            });
            self.recovery = Some(picture.words);
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.revoke();
        self.recovery = None;
    }

    pub(super) fn target_changed(&mut self, target: Option<ThreadId>) {
        if self
            .recovery
            .as_ref()
            .is_none_or(|words| words.request.thread != target)
        {
            self.invalidate();
        }
    }

    pub(super) fn revoke_turn(&mut self, turn: u32) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|picture| picture.capture.turn == turn)
        {
            self.revoke();
        }
    }

    pub(super) fn blocks_plain(&mut self) -> bool {
        if self.offer.is_some() || self.in_flight.is_some() || self.retiring.is_some() {
            self.revoke();
            return true;
        }
        self.recovery.is_some()
    }

    pub(super) fn refused(&mut self, turn: u32, thread: Option<ThreadId>) {
        if let Some(picture) = self.in_flight.take_if(|picture| {
            picture.run.is_none()
                && picture.capture.turn == turn
                && picture.words.request.thread == thread
        }) {
            self.recovery = Some(picture.words);
            if picture.capture.key.is_some() {
                self.retiring = Some(Retirement {
                    capture: picture.capture,
                    cancel: false,
                });
            }
        }
    }

    pub(super) fn name(
        &mut self,
        turn: u32,
        draft: TypedScreenDraft,
        thread: ThreadId,
        run: RunId,
    ) -> bool {
        let Some(picture) = self.in_flight.as_mut().filter(|picture| {
            picture.capture.turn == turn
                && picture.words.request.draft == draft
                && picture.confirmed
                && picture.run.is_none()
        }) else {
            return false;
        };
        picture.run = Some(run);
        picture.words.request.thread = Some(thread);
        true
    }

    pub(super) fn finished(&mut self, run: RunId, error: Option<ErrorKey>) {
        let Some(picture) = self.in_flight.take_if(|picture| picture.run == Some(run)) else {
            return;
        };
        if picture.capture.key.is_some() {
            self.retiring = Some(Retirement {
                capture: picture.capture,
                cancel: false,
            });
        }
        if error == Some(ErrorKey::ScreenUnavailable) {
            self.recovery = Some(picture.words);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn kept(&self) -> Option<usize> {
        self.recovery
            .as_ref()
            .or(self.in_flight.as_ref().map(|picture| &picture.words))
            .map(|words| words.text.len())
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

    /// Requests one offer for the current draft, selection, next turn and exact display.
    pub fn request_typed_screen(
        &mut self,
        link: &LinkHandle,
        session: &Session,
        display: TypedDisplay,
    ) -> bool {
        let thread = self.thread();
        let state = &self.typed_screen;
        if !self.connected
            || self.main.is_none()
            || state.revision == u64::MAX
            || state.generation != session.generation()
            || thread.is_some_and(|thread| self.holds(thread))
            || state.offer.is_some()
            || state.in_flight.is_some()
            || state.recovery.is_some()
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
                display,
            },
        ) {
            return false;
        }
        self.typed_screen.offer = Some(Offer::Waiting {
            request: Request {
                turn,
                thread,
                draft,
                display,
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
            && (request.turn, request.thread, request.draft) == (turn, thread, draft)
        {
            let armed = offer.filter(|_| !*withdrawn).map(|offer| Offer::Armed {
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
                (request.turn, request.thread, request.draft, Some(*armed)) == (turn, thread, draft, offer))
        }) || state.in_flight.as_ref().is_some_and(|picture| {
            picture.capture.turn == turn && Some(picture.capture.offer) == offer
        })
        {
            return false;
        }
        if let Some(offer) = offer {
            link.cancel_typed_screen(session, turn, offer);
        }
        false
    }

    /// Spends the offer on this attempt; never falls back to a plain submit.
    /// The stamp is read only after successful surface setters.
    ///
    /// # Errors
    /// The turn was not queued; any acquired lease is revoked and the original words stay local.
    pub fn submit_typed_screen(
        &mut self,
        text: &str,
        input: TypedScreenInput,
        io: &mut TypedScreenIo<'_, impl Surfaces>,
        stamp: impl FnOnce() -> Option<u64>,
    ) -> Result<(), Refused> {
        let Some(Offer::Armed { request, offer }) = self.typed_screen.offer else {
            if let Some(Offer::Waiting { withdrawn, .. }) = &mut self.typed_screen.offer {
                *withdrawn = true;
            }
            return Err(Refused::Busy);
        };
        self.typed_screen.offer = None;
        let generation = self.typed_screen.generation;
        let mut capture = Capture {
            generation,
            turn: request.turn,
            offer,
            key: None,
        };
        let queued = self.queue_typed_screen(text, input, io, request, &mut capture, stamp);
        let words = Words {
            request,
            text: text.into(),
        };
        if queued.is_ok() {
            self.typed_screen.in_flight = Some(InFlight {
                capture,
                words,
                run: None,
                confirmed: false,
            });
        } else {
            self.typed_screen.recovery = Some(words);
            self.typed_screen.retiring = Some(Retirement {
                capture,
                cancel: true,
            });
            if self.flush_typed_screen_cancel(io).is_err() {
                return Err(Refused::Offline);
            }
        }
        queued
    }

    fn queue_typed_screen(
        &mut self,
        text: &str,
        input: TypedScreenInput,
        io: &mut TypedScreenIo<'_, impl Surfaces>,
        request: Request,
        capture: &mut Capture,
        stamp: impl FnOnce() -> Option<u64>,
    ) -> Result<(), Refused> {
        self.cancel_hold();
        let turn = self.prepare_submit(text, TYPED_SUBMIT_OVERHEAD)?;
        if capture.generation != io.session.generation()
            || !io.session.open()
            || request.turn != turn
            || request.thread != self.thread()
            || request.draft != self.typed_draft()
            || request.display != input.display
            || !request.display.contains(input.point.0)
            || io.exclusion.deadline().is_some()
        {
            return Err(Refused::Busy);
        }
        let lease = io.exclusion.acquire(
            capture.generation,
            CaptureFor::Turn(turn),
            input.now,
            io.surfaces,
        );
        let Ok(key) = lease else {
            capture.key = io.exclusion.key(capture.generation, CaptureFor::Turn(turn));
            return Err(Refused::Busy);
        };
        capture.key = Some(key);
        let excluded_qpc = stamp()
            .filter(|excluded| input.submitted_qpc != 0 && *excluded >= input.submitted_qpc)
            .ok_or(Refused::Busy)?;
        let screen = ScreenChoice::Typed(TypedScreenSubmit {
            offer: capture.offer,
            draft: request.draft,
            lease: key.generation(),
            display: request.display,
            point: input.point,
            submitted_qpc: input.submitted_qpc,
            excluded_qpc,
        });
        if !io.link.send_data(
            io.session,
            DataToBrain::TextSubmit {
                turn,
                screen,
                thread: request.thread,
                text: text.into(),
            },
        ) {
            return Err(Refused::Offline);
        }
        self.forget_texts(|_| true);
        self.record_submit(turn, Submit::Picture);
        Ok(())
    }

    /// Queues positive proof before restoring, or cancels an exact turn that cannot prove coverage.
    ///
    /// # Errors
    /// Returns a restoration debt; the lease must be restored before another capture.
    pub fn confirm_typed_screen(
        &mut self,
        event: &LinkEvent,
        display: Option<TypedDisplay>,
        now: Instant,
        io: &mut TypedScreenIo<'_, impl Surfaces>,
    ) -> Result<bool, Error> {
        let LinkEvent::TypedScreenHeld { generation, proof } = *event else {
            return Ok(false);
        };
        let Some(picture) = self.typed_screen.in_flight.as_mut().filter(|picture| {
            generation == io.session.generation()
                && picture.capture.generation == generation
                && picture.capture.turn == proof.turn
                && picture.capture.offer == proof.offer
                && !picture.confirmed
        }) else {
            return Ok(false);
        };
        let Some(key) = picture
            .capture
            .key
            .filter(|key| key.generation() == proof.lease)
        else {
            return Ok(false);
        };
        if display != Some(picture.words.request.display)
            || !io.exclusion.confirms(key, now, io.surfaces)
            || !io
                .link
                .send_control(io.session, ControlToBrain::ConfirmTypedScreen(proof))
        {
            self.revoke_typed_screen();
            self.flush_typed_screen_cancel(io)?;
            return Ok(false);
        }
        picture.confirmed = true;
        if let Err(error) = io.exclusion.restore_matching(key, io.surfaces) {
            self.revoke_typed_screen();
            io.link
                .cancel_typed_screen(io.session, proof.turn, proof.offer);
            if let Some(retiring) = &mut self.typed_screen.retiring {
                retiring.cancel = false;
            }
            return Err(error);
        }
        picture.capture.key = None;
        Ok(true)
    }

    /// Revokes a live picture before restoring an expired lease.
    ///
    /// # Errors
    /// Returns a restoration debt; the lease owner must retry restoring it.
    pub fn expire_typed_screen(
        &mut self,
        now: Instant,
        io: &mut TypedScreenIo<'_, impl Surfaces>,
    ) -> Result<(), Error> {
        if self.typed_screen.in_flight.as_ref().is_some_and(|picture| {
            picture
                .capture
                .key
                .is_some_and(|key| io.exclusion.expired(key, now))
        }) {
            self.revoke_typed_screen();
        }
        self.flush_typed_screen_cancel(io)
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

    /// Whether words from a failed pictured send still belong to the current draft and target.
    pub fn typed_screen_retry_ready(&self) -> bool {
        self.connected
            && self.main.is_some()
            && self.typed_screen.retiring.is_none()
            && self.typed_screen.recovery.as_ref().is_some_and(|words| {
                words.request.draft == self.typed_draft()
                    && words.request.thread == self.thread()
                    && !words
                        .request
                        .thread
                        .is_some_and(|thread| self.holds(thread))
            })
    }

    /// Explicitly sends the original words without an image. A refused queue does not consume them.
    ///
    /// # Errors
    /// The recovery is no longer current or the plain turn could not be queued.
    pub fn retry_typed_screen(
        &mut self,
        link: &LinkHandle,
        session: &Session,
    ) -> Result<(), Refused> {
        if !self.typed_screen_retry_ready() || self.typed_screen.generation != session.generation()
        {
            return Err(Refused::Busy);
        }
        let words = self.typed_screen.recovery.take().ok_or(Refused::Busy)?;
        let queued = self.submit_with_audio(&words.text, AudioSession::QueuedRequired, |turn| {
            link.send_data(session, turn)
        });
        if queued.is_err() {
            self.typed_screen.recovery = Some(words);
        }
        queued
    }
}
