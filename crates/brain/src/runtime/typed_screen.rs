//! A paired connection's one typed offer and its bounded capture, before a run exists.

use std::future::{Future, pending};
use std::num::NonZeroU64;
use std::pin::Pin;
use std::time::Duration;

use kumochi_protocol::geometry::TypedDisplay;
use kumochi_protocol::message::{
    Content, ControlToBrain, ControlToShell, SettingsOp, ThreadId, ThreadOp, TypedScreenDraft,
    TypedScreenOffer, TypedScreenProof, TypedScreenRecipient, TypedScreenRevision,
    TypedScreenSubmit,
};
use tokio::time::Instant;

use super::{Brain, Ended, Full, Link, Registration};
use crate::context::{Origin, PictureRecipient, RunSnapshot};
use crate::coordinator::StoreOp;
use crate::coordinator::typed_screen::{ReadTarget, TypedKey};
use crate::recall::control::{CancelOnDrop, Control, READ_TIME};
use crate::screen::{Closed, MAX_TTL, ScreenContext, TypedHeld};

const LEASE: Duration = Duration::from_secs(3);
const LATEST_START: Duration = LEASE.saturating_sub(MAX_TTL);
pub(super) type Clock = fn(u64) -> Option<Duration>;
type Work<T> = Pin<Box<dyn Future<Output = Result<T, Closed>> + Send>>;

struct Offer {
    key: TypedKey,
    thread: Option<ThreadId>,
    draft: TypedScreenDraft,
    display: TypedDisplay,
    recipient_revision: TypedScreenRevision,
    snapshot: RunSnapshot,
    recipient: PictureRecipient,
    reading: Option<CancelOnDrop>,
}

impl Offer {
    fn answer(&self, offer: Option<TypedScreenOffer>) -> ControlToShell {
        ControlToShell::TypedScreenOffered {
            turn: self.key.turn,
            thread: self.thread,
            draft: self.draft,
            recipient: self.recipient_revision,
            offer,
        }
    }
}

enum Capture {
    Acquiring(Work<TypedHeld>),
    Confirming(TypedScreenProof, TypedHeld),
    Sealing(Work<ScreenContext>),
}

struct Pending {
    offer: Offer,
    text: Content,
    request: TypedScreenSubmit,
    until: Instant,
    capture: Capture,
}

#[derive(Default)]
pub(super) struct State {
    serial: u64,
    recipient_serial: u64,
    recipient: Option<(TypedScreenRecipient, PictureRecipient)>,
    lapsed_lease: u32,
    offer: Option<Offer>,
    pending: Option<Box<Pending>>,
}

pub(super) enum Progress {
    Held(TypedHeld),
    Sealed(ScreenContext),
    Refused,
}

impl State {
    fn recipient_revision(&self) -> Option<TypedScreenRevision> {
        self.recipient.as_ref().map(|(view, _)| view.revision)
    }

    pub(super) fn capturing(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) async fn next(&mut self) -> Progress {
        let Some(active) = &mut self.pending else {
            return pending().await;
        };
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(active.until) => Progress::Refused,
            result = async {
                match &mut active.capture {
                    Capture::Acquiring(work) => work.await.map(Progress::Held),
                    Capture::Confirming(..) => pending().await,
                    Capture::Sealing(work) => work.await.map(Progress::Sealed),
                }
            } => result.unwrap_or(Progress::Refused)
        }
    }
}

fn refuse(link: &mut Link<'_, '_>, turn: u32, thread: Option<ThreadId>) -> Result<(), Ended> {
    link.control(&ControlToShell::TurnRefused { turn, thread })
        .map_err(|Full| Ended::Lost)
}

impl Brain {
    fn typed_screen_current(&self, offer: &Offer, revision: Option<TypedScreenRevision>) -> bool {
        offer.key.epoch == self.epoch
            && Some(offer.recipient_revision) == revision
            && self.show_screen_enabled()
            && !self.changing_model()
            && self.coordinator.typed_target(offer.thread)
            && self
                .snapshot(Origin::Kumo)
                .choice
                .as_ref()
                .is_some_and(|choice| {
                    offer.snapshot.choice.as_ref() == Some(choice)
                        && self.coordinator.typed_recipient(choice).as_ref()
                            == Some(&offer.recipient)
                })
    }

    pub(super) fn typed_screen_sync(&self, link: &mut Link<'_, '_>) -> Result<(), Ended> {
        self.typed_screen_advertise(link)?;
        let revision = link.typed_screen.recipient_revision();
        if link.typed_screen.offer.as_ref().is_some_and(|offer| {
            !self.typed_screen_current(offer, revision)
                || self.coordinator.next_typed_turn() != Some(offer.key.turn)
        }) {
            Self::drop_typed_offer(link)?;
        }
        if link
            .typed_screen
            .pending
            .as_ref()
            .is_some_and(|active| !self.typed_screen_current(&active.offer, revision))
        {
            Self::refuse_typed_pending(link)?;
        }
        Ok(())
    }

    fn typed_screen_advertise(&self, link: &mut Link<'_, '_>) -> Result<(), Ended> {
        let eligible = self.typed_screen_clock.is_some()
            && self.holds.screen.is_some()
            && link
                .registration
                .is_some_and(|registration| Registration::admits(Some(registration)))
            && self.show_screen_enabled()
            && !self.changing_model();
        let state = &mut link.typed_screen;
        if !eligible && state.recipient.is_none() {
            return Ok(());
        }
        let candidate = eligible
            .then(|| self.snapshot(Origin::Kumo).choice)
            .flatten()
            .and_then(|choice| {
                self.coordinator
                    .typed_recipient(&choice)
                    .map(|account| (choice, account))
            });
        if state
            .recipient
            .as_ref()
            .map(|(view, account)| (&view.choice, account))
            == candidate
                .as_ref()
                .map(|(choice, account)| (choice, account))
        {
            return Ok(());
        }
        state.recipient = if let Some((choice, account)) = candidate {
            let revision = state
                .recipient_serial
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or(Ended::Broken("typed screen recipient revisions exhausted"))?;
            state.recipient_serial = revision.get();
            Some((
                TypedScreenRecipient {
                    revision: TypedScreenRevision(revision),
                    choice,
                },
                account,
            ))
        } else {
            None
        };
        let recipient = state.recipient.as_ref().map(|(view, _)| view.clone());
        link.control(&ControlToShell::TypedScreenRecipient(recipient))
            .map_err(|Full| Ended::Lost)
    }

    fn drop_typed_offer(link: &mut Link<'_, '_>) -> Result<(), Ended> {
        if let Some(offer) = link.typed_screen.offer.take()
            && offer.reading.is_some()
        {
            link.control(&offer.answer(None))
                .map_err(|Full| Ended::Lost)?;
        }
        Ok(())
    }

    fn refuse_typed_pending(link: &mut Link<'_, '_>) -> Result<(), Ended> {
        if let Some(active) = link.typed_screen.pending.take() {
            let (turn, thread) = (active.offer.key.turn, active.offer.thread);
            drop(active);
            refuse(link, turn, thread)?;
        }
        Ok(())
    }

    pub(super) fn typed_screen_settings_changed(
        &mut self,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let effects = self.coordinator.cancel_typed_picture(None, None);
        self.apply(effects, link.as_deref_mut()).map_err(|_| Full)?;
        if let Some(link) = link {
            if link.typed_screen.recipient.take().is_some() {
                link.control(&ControlToShell::TypedScreenRecipient(None))?;
            }
            Self::drop_typed_offer(link).map_err(|_| Full)?;
            Self::refuse_typed_pending(link).map_err(|_| Full)?;
        }
        Ok(())
    }

    pub(super) fn typed_screen_control(
        &mut self,
        message: &ControlToBrain,
        link: &mut Link<'_, '_>,
    ) -> Result<bool, Ended> {
        match *message {
            ControlToBrain::RequestTypedScreenOffer {
                turn,
                thread,
                draft,
                recipient,
                ref display,
            } => {
                self.typed_screen_offer(turn, thread, draft, recipient, **display, link)?;
            }
            ControlToBrain::CancelTypedScreen { turn, offer } => {
                let key = TypedKey {
                    epoch: self.epoch,
                    turn,
                    offer,
                };
                if link
                    .typed_screen
                    .offer
                    .as_ref()
                    .is_some_and(|offer| offer.key == key)
                {
                    Self::drop_typed_offer(link)?;
                }
                if link
                    .typed_screen
                    .pending
                    .as_ref()
                    .is_some_and(|active| active.offer.key == key)
                {
                    Self::refuse_typed_pending(link)?;
                }
                let effects = self.coordinator.cancel_typed_picture(Some(key), None);
                self.apply(effects, Some(link))?;
            }
            ControlToBrain::ConfirmTypedScreen(proof) => {
                if let Some(active) = &mut link.typed_screen.pending
                    && Instant::now() < active.until
                    && matches!(&active.capture, Capture::Confirming(held, _) if *held == proof)
                    && let Capture::Confirming(_, held) = std::mem::replace(
                        &mut active.capture,
                        Capture::Sealing(Box::pin(pending())),
                    )
                {
                    active.capture = Capture::Sealing(Box::pin(held.seal()));
                }
            }
            ControlToBrain::ExclusionLapsed { lease } => {
                if self.typed_screen_clock.is_some() {
                    link.typed_screen.lapsed_lease = link.typed_screen.lapsed_lease.max(lease.0);
                    if link.typed_screen.pending.as_ref().is_some_and(|active| {
                        active.request.lease.0 <= link.typed_screen.lapsed_lease
                    }) {
                        Self::refuse_typed_pending(link)?;
                    }
                }
                return Ok(false);
            }
            ControlToBrain::Settings(SettingsOp::SetShowScreen(false)) => {
                self.typed_screen_settings_changed(Some(link))
                    .map_err(|Full| Ended::Lost)?;
                return Ok(false);
            }
            ControlToBrain::ThreadOp(ThreadOp::SetModel { thread, .. })
            | ControlToBrain::DeleteThread { thread, .. } => {
                if link
                    .typed_screen
                    .offer
                    .as_ref()
                    .is_some_and(|offer| offer.thread == Some(thread))
                {
                    Self::drop_typed_offer(link)?;
                }
                if link
                    .typed_screen
                    .pending
                    .as_ref()
                    .is_some_and(|active| active.offer.thread == Some(thread))
                {
                    Self::refuse_typed_pending(link)?;
                }
                if matches!(message, ControlToBrain::ThreadOp(_)) {
                    let effects = self.coordinator.cancel_typed_picture(None, Some(thread));
                    self.apply(effects, Some(link))?;
                }
                return Ok(false);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn typed_screen_offer(
        &mut self,
        turn: u32,
        thread: Option<ThreadId>,
        draft: TypedScreenDraft,
        recipient_revision: TypedScreenRevision,
        display: TypedDisplay,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Ended> {
        let refused = ControlToShell::TypedScreenOffered {
            turn,
            thread,
            draft,
            recipient: recipient_revision,
            offer: None,
        };
        if self.typed_screen_clock.is_none()
            || self.holds.screen.is_none()
            || !link
                .registration
                .is_some_and(|registration| Registration::admits(Some(registration)))
            || link.typed_screen.capturing()
            || self.coordinator.next_typed_turn() != Some(turn)
            || link.typed_screen.recipient_revision() != Some(recipient_revision)
        {
            return link.control(&refused).map_err(|Full| Ended::Lost);
        }
        Self::drop_typed_offer(link)?;
        let snapshot = self.snapshot(Origin::Kumo);
        let Some(recipient) = snapshot
            .choice
            .as_ref()
            .and_then(|choice| self.coordinator.typed_recipient(choice))
        else {
            return link.control(&refused).map_err(|Full| Ended::Lost);
        };
        let Some(serial) = link
            .typed_screen
            .serial
            .checked_add(1)
            .and_then(NonZeroU64::new)
        else {
            return Err(Ended::Broken("typed screen offer numbers exhausted"));
        };
        let key = TypedKey {
            epoch: self.epoch,
            turn,
            offer: TypedScreenOffer(serial),
        };
        let mut offer = Offer {
            key,
            thread,
            draft,
            display,
            recipient_revision,
            snapshot,
            recipient,
            reading: None,
        };
        if !self.typed_screen_current(&offer, link.typed_screen.recipient_revision()) {
            return link.control(&refused).map_err(|Full| Ended::Lost);
        }
        link.typed_screen.serial = serial.get();
        if let Some(thread) = thread {
            let control = Control::new(std::time::Instant::now() + READ_TIME);
            offer.reading = Some(CancelOnDrop(control.clone()));
            let op = StoreOp::ReadTypedTarget(ReadTarget {
                control,
                epoch: self.epoch,
                offer: key.offer,
                thread,
            });
            if self.store.as_ref().is_none_or(|store| !store.send(op)) {
                return link.control(&refused).map_err(|Full| Ended::Lost);
            }
        } else {
            link.control(&offer.answer(Some(offer.key.offer)))
                .map_err(|Full| Ended::Lost)?;
        }
        link.typed_screen.offer = Some(offer);
        Ok(())
    }

    pub(super) fn typed_screen_read(
        &self,
        epoch: u64,
        key: TypedScreenOffer,
        result: Result<(), ()>,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Ended> {
        if let Some(mut offer) = link.typed_screen.offer.take_if(|offer| {
            offer.key.epoch == epoch && offer.key.offer == key && offer.reading.is_some()
        }) {
            drop(offer.reading.take());
            let current = result.is_ok()
                && self.typed_screen_current(&offer, link.typed_screen.recipient_revision())
                && self.coordinator.next_typed_turn() == Some(offer.key.turn);
            link.control(&offer.answer(current.then_some(key)))
                .map_err(|Full| Ended::Lost)?;
            if current {
                link.typed_screen.offer = Some(offer);
            }
        }
        Ok(())
    }

    pub(super) fn typed_screen_submit(
        &mut self,
        turn: u32,
        thread: Option<ThreadId>,
        text: Content,
        request: TypedScreenSubmit,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Ended> {
        let expected = self.coordinator.next_typed_turn();
        let mut effects = Vec::new();
        let admitted = self.coordinator.typed_attempt(thread, &mut effects);
        self.apply(effects, Some(link))?;
        let offer = link
            .typed_screen
            .offer
            .take_if(|offer| offer.key.offer == request.offer);
        let Some(offer) = offer.filter(|offer| {
            admitted
                && expected == Some(turn)
                && offer.key.turn == turn
                && offer.thread == thread
                && offer.draft == request.draft
                && offer.display == request.display
                && offer.reading.is_none()
                && !text.trim().is_empty()
                && self.typed_screen_current(offer, link.typed_screen.recipient_revision())
        }) else {
            return refuse(link, turn, thread);
        };
        let Some(age) = self.typed_screen_clock else {
            return refuse(link, turn, thread);
        };
        let now = Instant::now();
        if request.lease.0 <= link.typed_screen.lapsed_lease
            || !request.display.contains(request.point.0)
            || request.display.dpi == 0
            || request.submitted_qpc == 0
            || request.excluded_qpc < request.submitted_qpc
            || age(request.submitted_qpc).is_none_or(|age| age > LATEST_START)
            || age(request.excluded_qpc).is_none()
        {
            return refuse(link, turn, thread);
        }
        let Some(client) = self.holds.screen.clone() else {
            return refuse(link, turn, thread);
        };
        let until = now + MAX_TTL;
        let capture = Capture::Acquiring(Box::pin(async move {
            client
                .typed_display(
                    request.point.0,
                    request.display,
                    request.excluded_qpc,
                    until,
                )
                .await
        }));
        link.typed_screen.pending = Some(Box::new(Pending {
            offer,
            text,
            request,
            until,
            capture,
        }));
        Ok(())
    }

    pub(super) fn typed_screen_progress(
        &mut self,
        progress: Progress,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Ended> {
        let Some(mut active) = link.typed_screen.pending.take() else {
            return Ok(());
        };
        let Offer { key, thread, .. } = &active.offer;
        if Instant::now() < active.until
            && self.typed_screen_current(&active.offer, link.typed_screen.recipient_revision())
        {
            match progress {
                Progress::Held(held) => {
                    if let Some(capture) = NonZeroU64::new(held.capture_id().0) {
                        let proof = TypedScreenProof {
                            turn: key.turn,
                            offer: key.offer,
                            lease: active.request.lease,
                            capture,
                        };
                        active.capture = Capture::Confirming(proof, held);
                        link.control(&ControlToShell::TypedScreenHeld(proof))
                            .map_err(|Full| Ended::Lost)?;
                        link.typed_screen.pending = Some(active);
                        return Ok(());
                    }
                }
                Progress::Sealed(frame) => {
                    let effects = self.coordinator.start_typed_picture(
                        *key,
                        *thread,
                        active.text,
                        active.offer.snapshot,
                        active.offer.recipient,
                        frame.into_jpeg(),
                    );
                    return self.apply(effects, Some(link));
                }
                Progress::Refused => {}
            }
        }
        refuse(link, key.turn, *thread)
    }
}
