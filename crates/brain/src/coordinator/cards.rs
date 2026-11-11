//! One admitted card batch and the deletion stamps of its bounded sources.

use std::sync::{Arc, OnceLock};

use kumochi_protocol::message::{DraftId, IssueCard, MAX_ISSUE_CARDS, ThreadId};

use super::PageKey;
use crate::store::Generation;

/// A batch's original admission, retained until its terminal is written.
#[derive(Debug)]
pub struct Cards {
    pub(crate) key: PageKey,
    pub(crate) sources: Vec<Arc<CardSource>>,
}

impl PartialEq for Cards {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for Cards {}

impl Cards {
    pub(crate) fn new(
        key: PageKey,
        drafts: &kumochi_scene::text::Bounded<DraftId, MAX_ISSUE_CARDS>,
    ) -> Self {
        let mut sources: Vec<Arc<CardSource>> = Vec::new();
        for &draft in drafts.iter() {
            if !sources.iter().any(|source| source.draft == draft) {
                sources.push(Arc::new(CardSource {
                    draft,
                    stamp: OnceLock::new(),
                    control: key.control.child(),
                }));
            }
        }
        Self { key, sources }
    }
}

#[derive(Debug)]
pub(crate) struct CardSource {
    pub(crate) draft: DraftId,
    stamp: OnceLock<(ThreadId, Generation)>,
    control: crate::recall::control::Control,
}

impl PartialEq for CardSource {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for CardSource {}

impl CardSource {
    pub(crate) fn bind(&self, thread: ThreadId, generation: Generation) -> bool {
        self.stamp.get_or_init(|| (thread, generation)) == &(thread, generation)
            && self.control.check().is_ok()
    }

    pub(crate) fn live(&self, thread: ThreadId, generation: Generation) -> bool {
        self.stamp.get() == Some(&(thread, generation)) && self.control.check().is_ok()
    }

    pub(crate) fn delete(&self) {
        self.control.cancel();
    }
}

/// A full card and the exact source generation in its snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRead {
    pub(crate) card: IssueCard,
    pub(crate) generation: Generation,
    pub(crate) source: Arc<CardSource>,
}

impl CardRead {
    pub(crate) fn live(&self) -> bool {
        self.source.live(self.card.thread, self.generation)
    }
}
