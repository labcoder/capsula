//! Content-bearing page records deliberately omit user content from `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ModelChoice, RunId, ThreadId, ThreadKind};

/// One stored event, ordered within a thread by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventId(pub u64);

/// The last thread sent: in descending updated-time and id order, or for a search, in rank
/// and descending id order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCursor {
    /// Unix milliseconds when the thread last changed.
    pub updated_ms: i64,
    /// Breaks timestamp and rank ties.
    pub id: ThreadId,
    /// A search's rank for the thread, as `f64` bits compared exactly; 0 for the plain list.
    pub rank_bits: u64,
}

/// Which list to read and where to continue it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageOf {
    /// Live threads, or phrase matches when search words are present.
    Threads {
        /// User content, never diagnostic text.
        search: Option<String>,
        /// The previous page's continuation key.
        after: Option<ThreadCursor>,
    },
    /// A live thread's events, newest first.
    Events {
        /// The thread to read.
        thread: ThreadId,
        /// The previous page's oldest id.
        before: Option<EventId>,
    },
}

impl fmt::Debug for PageOf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Threads { after, .. } => f
                .debug_struct("Threads")
                .field("after", after)
                .finish_non_exhaustive(),
            Self::Events { thread, before } => f
                .debug_struct("Events")
                .field("thread", thread)
                .field("before", before)
                .finish(),
        }
    }
}

/// A live thread's list row.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRow {
    /// Its store id.
    pub id: ThreadId,
    /// User content, never diagnostic text.
    pub title: String,
    /// Unix milliseconds when it last changed.
    pub updated_ms: i64,
    /// The main thread or a task; the shell labels the main thread from its settings, since its
    /// stored title is empty.
    pub kind: ThreadKind,
    /// The stored model and effort, even one the catalog no longer offers.
    pub choice: Option<ModelChoice>,
}

impl fmt::Debug for ThreadRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadRow")
            .field("id", &self.id)
            .field("updated_ms", &self.updated_ms)
            .field("kind", &self.kind)
            .field("choice", &self.choice)
            .finish_non_exhaustive()
    }
}

/// What a stored event records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageEventKind {
    /// The user's turn.
    User,
    /// The assistant's stored text.
    AssistantText,
    /// A committed drawing step.
    SceneStep,
    /// An issue draft or outcome, read back by its durable draft id.
    Issue {
        /// The durable action to read back.
        draft: super::DraftId,
    },
    /// Kumo's note that a task was deleted. Its text is empty: the note keeps no title.
    TaskDeleted,
}

/// An event as the thread panel reads it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEvent {
    /// Its store id.
    pub id: EventId,
    /// The run it belongs to, when present.
    pub run: Option<RunId>,
    /// What it records.
    pub kind: PageEventKind,
    /// User content, never diagnostic text.
    pub text: String,
    /// The frame budget cut the text at a character boundary.
    pub cut: bool,
}

impl fmt::Debug for PageEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageEvent")
            .field("id", &self.id)
            .field("run", &self.run)
            .field("kind", &self.kind)
            .field("cut", &self.cut)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{DataToBrain, DataToShell, Effort, ModelSlug, ThreadOp};

    #[test]
    fn page_debug_never_contains_search_titles_or_text() {
        let words = "private search words";
        let title = "private thread title";
        let text = "private event text";
        let slug = "zqxprivatemodel";
        let choice = ModelChoice {
            model: ModelSlug::new(slug).expect("valid"),
            effort: Effort::Medium,
            connection: crate::message::ConnectionKind::ChatGpt,
        };
        let set_model = ThreadOp::SetModel {
            thread: ThreadId(1),
            choice: choice.clone(),
        };
        let page = PageOf::Threads {
            search: Some(words.to_owned()),
            after: None,
        };
        let row = ThreadRow {
            id: ThreadId(1),
            title: title.to_owned(),
            updated_ms: 2,
            kind: ThreadKind::Thread,
            choice: Some(choice),
        };
        let event = PageEvent {
            id: EventId(3),
            run: Some(RunId(4)),
            kind: PageEventKind::User,
            text: text.to_owned(),
            cut: false,
        };
        assert_eq!(
            format!("{row:?}"),
            "ThreadRow { id: ThreadId(1), updated_ms: 2, kind: Thread, choice: Some(ModelChoice { model: ModelSlug(len=15), effort: Medium, connection: ChatGpt }), .. }"
        );
        assert_eq!(
            format!("{set_model:?}"),
            "SetModel { thread: ThreadId(1), choice: ModelChoice { model: ModelSlug(len=15), effort: Medium, connection: ChatGpt } }"
        );
        let output = format!(
            "{page:?} {row:?} {event:?} {set_model:?} {:?} {:?} {:?}",
            DataToBrain::PageRequest {
                request: 1,
                page: page.clone()
            },
            DataToShell::ThreadList {
                request: 2,
                threads: vec![row.clone()],
                next: None
            },
            DataToShell::Page {
                request: 3,
                thread: ThreadId(1),
                events: vec![event.clone()],
                before: None
            }
        );
        for content in [words, title, text, slug] {
            assert!(!output.contains(content));
        }
    }
}
