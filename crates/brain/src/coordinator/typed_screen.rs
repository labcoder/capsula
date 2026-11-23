//! Exact recipient authority for a typed screenshot before a run exists.

use kumochi_protocol::message::{ConnectionKind, ModelChoice, ThreadId, TypedScreenOffer};

use super::{Availability, Coordinator, Input};
use crate::context::{PictureRecipient, Route};
use crate::recall::control::Control;

/// One eligible existing chat's cancellable lookup of its committed model inheritance.
#[derive(Clone, Debug)]
pub struct ReadTarget {
    pub(crate) control: Control,
    pub(crate) epoch: u64,
    pub(crate) offer: TypedScreenOffer,
    pub(crate) thread: ThreadId,
}

impl PartialEq for ReadTarget {
    fn eq(&self, other: &Self) -> bool {
        (self.epoch, self.offer, self.thread) == (other.epoch, other.offer, other.thread)
    }
}

impl ReadTarget {
    pub(crate) fn reply(&self, result: Result<(), ()>) -> Input {
        Input::TypedTargetRead {
            epoch: self.epoch,
            offer: self.offer,
            result,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypedKey {
    pub(crate) epoch: u64,
    pub(crate) turn: u32,
    pub(crate) offer: TypedScreenOffer,
}

impl Coordinator {
    pub(crate) fn typed_recipient(&self, choice: &ModelChoice) -> Option<PictureRecipient> {
        if choice.connection != ConnectionKind::ChatGpt {
            return None;
        }
        let Availability::Ready(ready) = self.availability_of(choice.connection) else {
            return None;
        };
        let route = Route {
            kind: choice.connection,
            tag: ready.connection,
        };
        (ready.models.iter().any(|model| model.slug == choice.model)
            && self.pictured(route, Some(&choice.model)))
        .then(|| PictureRecipient {
            route,
            model: choice.model.clone(),
        })
    }

    pub(crate) fn typed_target(&self, thread: Option<ThreadId>) -> bool {
        thread.is_none_or(|thread| thread == self.main_thread && !self.deleting_thread(thread))
    }

    pub(crate) fn next_typed_turn(&self) -> Option<u32> {
        self.turns_read.checked_add(1)
    }
}
