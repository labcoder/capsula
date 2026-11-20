//! What the core loop asks of the store thread, and the answer a failed write gets.

use kumochi_protocol::message::{
    Content, DataToShell, Deleted, EventId, HeldScreen, ModelChoice, PageOf, RunId, SceneId, Step,
    ThreadId, ThreadTitle,
};

use std::{fmt, ops::Deref};

use crate::store::{EventKind, NewEvent};

/// The unflushed answer. Delayed delivery doesn't delay acceptance into this buffer.
#[derive(Clone, Default, PartialEq)]
pub struct ReplyText {
    text: String,
    delayed: usize,
    narration: Vec<(std::ops::Range<usize>, String)>,
}

impl ReplyText {
    pub(super) fn accept(&mut self, text: &str) {
        self.text.push_str(text);
    }

    pub(super) fn defer(&mut self, bytes: usize) {
        self.delayed += bytes;
    }

    pub(super) fn released(&mut self, bytes: usize) {
        self.delayed -= bytes;
    }

    pub(super) fn narrate(&mut self, line: &str, call: Option<&str>) {
        let at = self.text.len() - self.delayed;
        self.text.insert_str(at, line);
        if let Some(call) = call {
            let body = serde_json::json!({ "draw_call": call }).to_string();
            self.narration.push((at..at + line.len(), body));
        }
    }

    pub(super) fn take(&mut self) -> Option<Self> {
        self.delayed = 0;
        (!self.text.is_empty()).then(|| std::mem::take(self))
    }

    pub(super) fn into_plain(self) -> Content {
        debug_assert!(
            self.narration.is_empty(),
            "legacy steps do not carry V1 narration"
        );
        self.text.into()
    }

    pub(crate) fn events(&self) -> Vec<NewEvent<'_>> {
        let mut events = Vec::new();
        let mut at = 0;
        for (range, body) in &self.narration {
            if at < range.start {
                events.push(assistant(&self.text[at..range.start], None));
            }
            events.push(assistant(&self.text[range.clone()], Some(body)));
            at = range.end;
        }
        if at < self.text.len() {
            events.push(assistant(&self.text[at..], None));
        }
        events
    }
}

fn assistant<'a>(text: &'a str, body: Option<&'a str>) -> NewEvent<'a> {
    NewEvent {
        kind: EventKind::AssistantText,
        text: Some(text),
        body,
    }
}

impl Deref for ReplyText {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<&str> for ReplyText {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<String> for ReplyText {
    fn from(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}

impl fmt::Debug for ReplyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyText")
            .field("bytes", &self.text.len())
            .field("delayed_bytes", &self.delayed)
            .field("narration", &self.narration.len())
            .finish()
    }
}

#[test]
fn unspoken_chat_debug_redacts_text_and_bytes() {
    const MARKER: &str = "private-narration";
    let key = super::BuildKey {
        run: RunId(1),
        round: 1,
        attempt: 1,
    };
    let mut reply = ReplyText::from(MARKER);
    reply.narrate(MARKER, Some(MARKER));
    let printed = format!(
        "{:?}",
        [
            super::Effect::Store(StoreOp::EndRun {
                thread: ThreadId(1),
                run: key.run,
                text: Some(reply),
                revealed: None,
                clear: None,
                end: RunEnd::Done,
            }),
            super::Effect::DrawingReplyText {
                key,
                text: MARKER.into()
            },
        ]
    );
    assert!(!printed.contains(MARKER));
    assert!(!printed.contains(&format!("{:?}", MARKER.as_bytes())));
}

use super::Input;
use crate::store::{Generation, RunEnd, SceneTarget, TitleSource};

/// A store operation: a bounded side read, or a write in one transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreOp {
    /// One issue-owner operation on the existing ordered store thread.
    Issue {
        /// The issue owner's outstanding operation identity.
        serial: u64,
        /// Typed data with content-free diagnostics.
        write: crate::issues::Write,
        /// The admitted operation's original cancellation and deadline, if it may advance.
        scope: Option<crate::issues::WriteScope>,
    },
    /// A v1 drawing read or durable transition; its arguments never enter logs.
    Drawing {
        /// The drawing reducer's outstanding operation identity.
        serial: u64,
        /// Typed data with content-free diagnostics.
        write: crate::drawing::Write,
    },
    /// A bounded read that neither stops nor postpones a purge.
    ReadPage {
        /// The admitted read.
        key: super::PageKey,
        /// The page to read.
        page: PageOf,
    },
    /// Read authoritative cards on the page reader, in one snapshot.
    ReadCards {
        /// The original admission and requested source identities.
        cards: std::sync::Arc<super::Cards>,
    },
    /// Build a round's context on the page reader, in one read transaction.
    BuildContext(super::BuildContext),
    /// Read a background job's request on the page reader, in one read transaction.
    ReadJob(super::ReadJob),
    /// Select a history call on the stamped page reader.
    ReadCall(crate::dispatch::ReadCall),
    /// Capture a hold's target choice and live deletion identity on the stamped reader.
    ReadHold(super::HoldRead),
    /// Read an inline compaction's request on the page reader, in one read transaction.
    CompactContext(super::CompactContext),
    /// The turn, its run, and the thread when `title` names a new one.
    BeginTurn {
        /// The thread.
        thread: ThreadId,
        /// The new thread's title, or `None` for an existing thread.
        title: Option<Content>,
        /// The run.
        run: RunId,
        /// The user's text.
        text: Content,
        /// What a screen hold carried into the turn; `None` for every other turn.
        screen: Option<HeldScreen>,
    },
    /// The text streamed since the last write, then one step.
    CommitStep {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed before the step, if any.
        text: Option<Content>,
        /// A new scene for a run's first step, or the run's scene.
        scene: SceneTarget,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
    },
    /// A round's text, native items and results.
    RoundItems(super::RoundItems),
    /// The text streamed since the last write, the revealed set, then the run's outcome.
    EndRun {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed since the last step, if any.
        text: Option<ReplyText>,
        /// The steps that stayed on screen, when the user stopped the run.
        revealed: Option<Vec<u32>>,
        /// The run's scene, when a Stop kept none of its steps.
        clear: Option<SceneId>,
        /// How it ended.
        end: RunEnd,
    },
    /// The scenes that left the visible set, in one transaction.
    ClearScenes {
        /// The scenes.
        scenes: Vec<SceneId>,
    },
    /// The steps of a scene's run that stayed, for a Stop no outcome recorded.
    KeepScene {
        /// The scene.
        scene: SceneId,
        /// Indices of the steps that stayed.
        revealed: Vec<u32>,
    },
    /// Delete a thread whatever its state, in up to two transactions.
    Delete {
        /// The thread.
        thread: ThreadId,
    },
    /// Retitle a live task. A user title always applies; a model title only over first words,
    /// and only in the deletion generation it names. `TitleSet` answers it.
    SetTitle {
        /// The task.
        thread: ThreadId,
        /// Its new title.
        title: ThreadTitle,
        /// Who set it, with a model's generation.
        source: TitleSource,
        /// The job that wrote it; `None` for the user's rename.
        job: Option<super::JobKey>,
    },
    /// Store a summary of a live thread's events, only in the deletion generation it names.
    /// `SummaryStored` answers it.
    StoreSummary {
        /// The job or build that wrote it.
        owner: super::SendKey,
        /// The thread.
        thread: ThreadId,
        /// The deletion generation it was read in.
        generation: Generation,
        /// The first and last events it covers.
        covers: (EventId, EventId),
        /// The summary.
        text: SummaryText,
        /// The model that wrote it; `None` when the engine took none.
        model: Option<String>,
    },
    /// Store a live thread's model and effort; `ModelSet` answers the result.
    SetModel {
        /// The thread.
        thread: ThreadId,
        /// The model and effort.
        choice: ModelChoice,
    },
    /// A tool call's write and its ledger row, in one transaction; answered by `CallCommitted`.
    CommitCall(crate::dispatch::CommitCall),
    /// How far the user heard a released spoken run. Nothing answers it: the run is gone.
    Heard {
        /// The run.
        run: RunId,
        /// The point.
        heard: crate::voice::reply::Heard,
    },
    /// The user withdrew a run's image before any request carried it, so its turn's pending
    /// picture becomes `withdrawn`. Nothing answers it: a failed write leaves it pending, which
    /// recovery marks sent, an over-report.
    WithdrawScreen {
        /// The run.
        run: RunId,
    },
    /// A run ended before any request carried its image, so its turn's pending picture becomes
    /// none. Nothing answers it: a failed write leaves it pending, which recovery marks sent, an
    /// over-report.
    ForgetScreen {
        /// The run.
        run: RunId,
    },
    /// A run's image couldn't go to its model before any request carried it, so its turn's
    /// pending picture becomes `model_cant_see`. Nothing answers it.
    CantSeeScreen {
        /// The run.
        run: RunId,
    },
    /// A send carrying a run's image may have written some of it, so its turn's pending picture
    /// becomes `attached` or `screen`. Nothing answers it: a failed write leaves it pending,
    /// which recovery marks sent.
    SentScreen {
        /// The run.
        run: RunId,
    },
}

/// A summary's text. Its `Debug` shows only the length, so formatting a write never prints it.
#[derive(Clone, PartialEq)]
pub struct SummaryText(pub String);

impl fmt::Debug for SummaryText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SummaryText").field(&self.0.len()).finish()
    }
}

impl StoreOp {
    /// The run a write belongs to; page reads and clears have none.
    pub fn run(&self) -> Option<RunId> {
        match self {
            Self::Drawing {
                write: crate::drawing::Write::Step { run, .. },
                ..
            }
            | Self::BeginTurn { run, .. }
            | Self::CommitStep { run, .. }
            | Self::EndRun { run, .. } => Some(*run),
            Self::CommitCall(call) => Some(call.key.run),
            Self::RoundItems(round) => Some(round.run),
            Self::Issue { .. }
            | Self::Drawing { .. }
            | Self::ReadPage { .. }
            | Self::ReadCards { .. }
            | Self::BuildContext(_)
            | Self::ReadJob(_)
            | Self::ReadCall(_)
            | Self::ReadHold(_)
            | Self::CompactContext(_)
            | Self::ClearScenes { .. }
            | Self::KeepScene { .. }
            | Self::Delete { .. }
            | Self::SetTitle { .. }
            | Self::StoreSummary { .. }
            | Self::SetModel { .. }
            | Self::Heard { .. }
            | Self::WithdrawScreen { .. }
            | Self::ForgetScreen { .. }
            | Self::CantSeeScreen { .. }
            | Self::SentScreen { .. } => None,
        }
    }

    /// Answers an operation that couldn't reach the store; a heard point has no answer.
    pub fn failed(&self) -> Option<Input> {
        Some(match self {
            Self::ReadHold(read) => Input::HoldPrepared {
                key: read.snapshot.key,
                result: Err(()),
            },
            Self::Issue { serial, .. } => Input::IssueWritten {
                serial: *serial,
                reply: Err(()),
            },
            Self::ReadCall(call) => Input::ReadFinished(crate::dispatch::ReadFinished {
                key: call.key.clone(),
                build: call.build,
                result: Err(crate::recall::Error::Read),
            }),
            Self::ReadCards { cards } => Input::CardsRead {
                cards: std::sync::Arc::clone(cards),
                reply: Err(()),
            },
            Self::Drawing { serial, .. } => Input::DrawingWritten {
                serial: *serial,
                reply: Err(()),
            },
            Self::ReadPage { key, .. } => Input::PageRead {
                key: key.clone(),
                reply: DataToShell::PageFailed {
                    request: key.request,
                },
            },
            Self::BuildContext(build) => Input::ContextBuilt(super::ContextBuilt {
                key: build.key,
                snapshot: build.snapshot.clone(),
                result: Err(super::Unbuilt::Failed),
            }),
            Self::CompactContext(read) => Input::CompactBuilt(super::CompactBuilt {
                key: read.key,
                result: Err(super::Unbuilt::Failed),
            }),
            Self::ReadJob(read) => Input::JobBuilt(super::JobBuilt {
                key: read.key,
                result: Err(super::Unbuilt::Failed),
            }),
            Self::SetTitle { job, .. } => Input::TitleSet {
                job: *job,
                applied: false,
            },
            Self::StoreSummary { owner, .. } => Input::SummaryStored {
                owner: *owner,
                applied: false,
            },
            Self::BeginTurn { run, .. }
            | Self::CommitStep { run, .. }
            | Self::EndRun { run, .. } => Input::StoreFailed { run: *run },
            Self::RoundItems(round) => Input::StoreFailed { run: round.run },
            Self::ClearScenes { scenes } => Input::ScenesCleared {
                scenes: scenes.clone(),
                ok: false,
            },
            Self::KeepScene { scene, .. } => Input::SceneKept {
                scene: *scene,
                ok: false,
            },
            Self::Delete { thread } => Input::Deleted {
                thread: *thread,
                outcome: Deleted::NotHidden,
                last_run: None,
            },
            Self::CommitCall(call) => Input::CallCommitted {
                key: call.key.clone(),
                outcome: crate::dispatch::Committed::Failed,
                then: None,
            },
            Self::SetModel { thread, choice } => Input::ModelSet {
                thread: *thread,
                choice: choice.clone(),
                applied: false,
            },
            Self::Heard { .. }
            | Self::WithdrawScreen { .. }
            | Self::ForgetScreen { .. }
            | Self::CantSeeScreen { .. }
            | Self::SentScreen { .. } => {
                return None;
            }
        })
    }
}
