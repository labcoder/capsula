//! Exact recipient authority for a typed screenshot before a run exists.

use kumochi_protocol::message::{
    ConnectionKind, Content, ModelChoice, ThreadId, TypedScreenOffer, TypedScreenSubmit,
};

use super::{Availability, Coordinator, Effect, Input};
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

/// A typed attempt whose capture authority can only be established by the paired runtime.
#[derive(Clone, PartialEq)]
pub struct PicturedTurn {
    pub(crate) turn: u32,
    pub(crate) thread: Option<ThreadId>,
    pub(crate) text: Content,
    pub(crate) screen: TypedScreenSubmit,
}

impl std::fmt::Debug for PicturedTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PicturedTurn")
            .field("turn", &self.turn)
            .field("thread", &self.thread)
            .field("text_len", &self.text.len())
            .finish_non_exhaustive()
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

    pub(crate) fn typed_attempt(
        &mut self,
        thread: Option<ThreadId>,
        effects: &mut Vec<Effect>,
    ) -> bool {
        self.count(effects) && self.admits(thread)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    #[test]
    fn the_outer_pictured_input_never_formats_its_words_or_bytes() {
        use kumochi_protocol::geometry::{
            PhysicalPoint, PhysicalRect, ReleasePointer, TypedDisplay,
        };
        use kumochi_protocol::message::{LeaseGeneration, TypedScreenDraft};

        const WORDS: &str = "pictured-input-privacy-sentinel";
        let input = Input::PicturedTurn(PicturedTurn {
            turn: 1,
            thread: None,
            text: WORDS.into(),
            screen: TypedScreenSubmit {
                offer: TypedScreenOffer(NonZeroU64::MIN),
                draft: TypedScreenDraft {
                    revision: 1,
                    selection: 1,
                },
                lease: LeaseGeneration(1),
                display: TypedDisplay {
                    id: NonZeroU64::MIN,
                    bounds: PhysicalRect {
                        origin: [0, 0],
                        size: [1, 1],
                    },
                    dpi: 96,
                },
                point: ReleasePointer(PhysicalPoint([0, 0])),
                submitted_qpc: 1,
                excluded_qpc: 1,
            },
        });
        let printed = format!("{input:?}");
        assert!(!printed.contains(WORDS));
        assert!(!printed.contains(&format!("{:?}", WORDS.as_bytes())));
        assert!(printed.contains("text_len: 31"));
    }
}
