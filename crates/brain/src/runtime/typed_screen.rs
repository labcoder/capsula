//! A paired connection's one typed screen offer, before capture admission.

use std::num::NonZeroU64;
use std::time::Duration;

use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, SettingsOp, ThreadId, ThreadOp, TypedScreenDraft,
    TypedScreenOffer, TypedScreenRecipient, TypedScreenRevision,
};

use super::{Brain, Ended, Full, Link, Registration};
use crate::context::{Origin, PictureRecipient, RunSnapshot};
use crate::coordinator::StoreOp;
use crate::coordinator::typed_screen::{ReadTarget, TypedKey};
use crate::recall::control::{CancelOnDrop, Control, READ_TIME};

pub(super) type Clock = fn(u64) -> Option<Duration>;

struct Offer {
    key: TypedKey,
    thread: Option<ThreadId>,
    draft: TypedScreenDraft,
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

#[derive(Default)]
pub(super) struct State {
    serial: u64,
    recipient_serial: u64,
    recipient: Option<(TypedScreenRecipient, PictureRecipient)>,
    offer: Option<Offer>,
}

impl State {
    fn recipient_revision(&self) -> Option<TypedScreenRevision> {
        self.recipient.as_ref().map(|(view, _)| view.revision)
    }
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

    pub(super) fn typed_screen_settings_changed(
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        if let Some(link) = link {
            if link.typed_screen.recipient.take().is_some() {
                link.control(&ControlToShell::TypedScreenRecipient(None))?;
            }
            Self::drop_typed_offer(link).map_err(|_| Full)?;
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
                ..
            } => {
                self.typed_screen_offer(turn, thread, draft, recipient, link)?;
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
            }
            ControlToBrain::Settings(SettingsOp::SetShowScreen(false)) => {
                Self::typed_screen_settings_changed(Some(link)).map_err(|Full| Ended::Lost)?;
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
}
