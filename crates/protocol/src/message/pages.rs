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
    /// A live thread's nearest newer events, with an exclusive lower bound.
    EventsAfter {
        /// The thread to read.
        thread: ThreadId,
        /// The newest event already held by the reader.
        after: EventId,
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
            Self::EventsAfter { thread, after } => f
                .debug_struct("EventsAfter")
                .field("thread", thread)
                .field("after", after)
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
    /// Saved task facts as of this page read; absent for Kumo’s chat.
    pub task: Option<TaskFact>,
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

/// The latest saved facts for a task, independent of any live run announcement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFact {
    /// The task's latest stored run, if it has started one.
    pub latest_run: Option<TaskRun>,
    /// Unix milliseconds of the last real run or conversation activity.
    pub last_activity_ms: i64,
    /// Derived on the read, never written as task state.
    pub state: TaskState,
}

/// A saved run reference and its stored outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    /// Its store id.
    pub id: RunId,
    /// The persisted outcome, even when the task has aged into Sleeping.
    pub outcome: TaskRunOutcome,
}

/// The stored run's outcome. Active is not evidence that a task is Working.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskRunOutcome {
    /// The store has no terminal outcome for this run.
    Active,
    /// The run completed.
    Done,
    /// The run was stopped before it completed.
    Interrupted,
    /// The run failed, including one recovered after a crash.
    Failed,
}

/// What persisted evidence alone can say about a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// No saved completion or explicit need for attention.
    Earlier,
    /// A saved interruption or failure needs attention.
    NeedsYou,
    /// The latest saved run completed.
    Done,
    /// Its last real activity was over five days ago.
    Sleeping,
}

/// A connection failure recorded for a user turn with known delivery evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFailure {
    /// The closed connection reason, not provider text.
    pub cause: HistoryFailureCause,
    /// Evidence recorded at the original attempt, not inferred on read.
    pub delivery: DeliveryProof,
}

/// Connection failures that can be named in saved history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryFailureCause {
    /// The ChatGPT plan connection requires sign-in again.
    ChatGptSignIn,
    /// The ChatGPT plan reached its usage limit.
    ChatGptUsageLimit,
    /// Anthropic refused the configured key.
    AnthropicKeyRejected,
    /// The Anthropic account ran out of credit.
    AnthropicCredit,
    /// Claude Code confirmed it was signed out.
    ClaudeCodeSignedOut,
}

/// Facts about delivery that distinguish a saved failure from an ambiguous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryProof {
    /// The request never left the app.
    NotSent,
    /// The provider explicitly rejected the first request after receiving it, before any answer
    /// or accepted effect.
    NotAccepted,
    /// An answer was received but broke off.
    PartialAnswer,
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
    /// The stored drawing is not reconstructed in history; this is not a live scene failure.
    NoVisual {
        /// The scene identified by the stored step, not a provider call id.
        scene: super::SceneId,
    },
    /// The user's turn of a screen hold, with what the hold carried into it; not whether a request
    /// carrying it left the machine. Appended.
    HeldTurn {
        /// The image, or why not.
        screen: super::HeldScreen,
    },
    /// A user turn whose run is stored as failed, including after crash recovery.
    /// Says nothing about delivery or whether retrying would be safe.
    FailedUser,
    /// A failed run's held turn, with its original screen metadata.
    FailedHeldTurn {
        /// The image, or why not.
        screen: super::HeldScreen,
    },
    /// A failed user turn with a known connection reason and delivery evidence.
    ConnectionFailed {
        /// The recorded outcome.
        failure: HistoryFailure,
        /// The original held-screen status, if this was a screen hold.
        screen: Option<super::HeldScreen>,
    },
}

/// An event as the thread panel reads it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEvent {
    /// Its store id.
    pub id: EventId,
    /// Unix milliseconds when the event was stored, for display rather than ordering.
    pub created_ms: i64,
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
    fn history_unavailable_appends_its_tag_and_round_trips_a_bounded_page() {
        use crate::frame::{self, Pipe};
        use crate::message::{ControlToShell, DraftId, SceneId};

        for (tag, kind) in [
            PageEventKind::User,
            PageEventKind::AssistantText,
            PageEventKind::SceneStep,
            PageEventKind::Issue { draft: DraftId(9) },
            PageEventKind::TaskDeleted,
            PageEventKind::NoVisual {
                scene: SceneId(u64::MAX),
            },
            PageEventKind::HeldTurn {
                screen: crate::message::HeldScreen::Attached,
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                usize::from(postcard::to_allocvec(&kind).expect("encodes")[0]),
                tag
            );
        }
        assert_eq!(
            postcard::to_allocvec(&ControlToShell::NoVisual { run: RunId(1) })
                .expect("legacy control")[0],
            10
        );
        let page = DataToShell::Page {
            request: u32::MAX,
            thread: ThreadId(u64::MAX),
            events: vec![
                PageEvent {
                    id: EventId(u64::MAX),
                    created_ms: i64::MAX,
                    run: Some(RunId(u64::MAX)),
                    kind: PageEventKind::NoVisual {
                        scene: SceneId(u64::MAX)
                    },
                    text: String::new(),
                    cut: false,
                };
                crate::message::MAX_PAGE_ROWS
            ],
            next: Some(EventId(u64::MAX)),
        };
        let encoded = frame::encode(&page, Pipe::Data).expect("bounded Page frame");
        assert_eq!(
            encoded.len(),
            frame::HEADER_LEN + 1 + 5 + 10 + 1 + 50 * 44 + 11
        );
        assert!(
            encoded.len() <= crate::message::MAX_PAGE_BYTES,
            "the new scene identity is charged within the page frame cap"
        );
        assert_eq!(
            frame::read::<DataToShell>(&mut encoded.as_slice(), Pipe::Data).expect("decoded Page"),
            Some(page)
        );
    }

    #[test]
    fn saved_failure_kinds_append_their_wire_tags_and_round_trip() {
        use crate::frame::{self, Pipe};
        use crate::message::HeldScreen;

        let kinds = [
            (PageEventKind::FailedUser, &[7][..]),
            (
                PageEventKind::FailedHeldTurn {
                    screen: HeldScreen::Screen,
                },
                &[8, 3],
            ),
            (
                PageEventKind::ConnectionFailed {
                    failure: HistoryFailure {
                        cause: HistoryFailureCause::ChatGptSignIn,
                        delivery: DeliveryProof::NotSent,
                    },
                    screen: None,
                },
                &[9, 0, 0, 0],
            ),
            (
                PageEventKind::ConnectionFailed {
                    failure: HistoryFailure {
                        cause: HistoryFailureCause::AnthropicKeyRejected,
                        delivery: DeliveryProof::NotAccepted,
                    },
                    screen: Some(HeldScreen::Screen),
                },
                &[9, 2, 1, 1, 3],
            ),
            (
                PageEventKind::ConnectionFailed {
                    failure: HistoryFailure {
                        cause: HistoryFailureCause::ClaudeCodeSignedOut,
                        delivery: DeliveryProof::PartialAnswer,
                    },
                    screen: None,
                },
                &[9, 4, 2, 0],
            ),
        ];
        let events = kinds
            .into_iter()
            .zip(1..)
            .map(|((kind, bytes), id)| {
                assert_eq!(postcard::to_allocvec(&kind).expect("kind"), bytes);
                PageEvent {
                    id: EventId(id),
                    created_ms: 0,
                    run: Some(RunId(id)),
                    kind,
                    text: "user text".to_owned(),
                    cut: false,
                }
            })
            .collect();
        let page = DataToShell::Page {
            request: 1,
            thread: ThreadId(1),
            events,
            next: None,
        };
        let encoded = frame::encode(&page, Pipe::Data).expect("page");
        assert_eq!(
            frame::read::<DataToShell>(&mut encoded.as_slice(), Pipe::Data).expect("decoded page"),
            Some(page)
        );
    }

    #[test]
    fn page_debug_never_contains_search_titles_or_text() {
        let debug = |[words, title, text, issue_title, slug]: [&str; 5]| {
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
                task: Some(TaskFact {
                    latest_run: Some(TaskRun {
                        id: RunId(4),
                        outcome: TaskRunOutcome::Failed,
                    }),
                    last_activity_ms: 2,
                    state: TaskState::NeedsYou,
                }),
            };
            let event = PageEvent {
                id: EventId(3),
                created_ms: 0,
                run: Some(RunId(4)),
                kind: PageEventKind::User,
                text: text.to_owned(),
                cut: false,
            };
            let issue = PageEvent {
                id: EventId(1),
                created_ms: 0,
                run: None,
                kind: PageEventKind::Issue {
                    draft: crate::message::DraftId(2),
                },
                text: issue_title.to_owned(),
                cut: false,
            };
            let issue_debug = format!("{issue:?}");
            format!(
                "{page:?} {row:?} {event:?} {issue_debug} {set_model:?} {:?} {:?} {:?}",
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
                    events: vec![event.clone(), issue],
                    next: None
                }
            )
        };
        let original = [
            "private search words",
            "private thread title",
            "private event text",
            "zqx-private-title",
            "zqxprivatemodel",
        ];
        let other = [
            "another query phrase",
            "another dialog label",
            "another post entry",
            "abc-queued-letter",
            "abc-another-llm",
        ];
        let output = debug(original);
        assert!(output == debug(other), "stored content affected Debug");
        for content in original {
            assert!(!output.contains(content));
            assert!(!output.contains(&format!("{:?}", content.as_bytes())));
        }
    }
}
