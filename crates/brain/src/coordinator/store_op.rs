//! What the core loop asks of the store thread, and the answer a failed write gets.

use kumochi_protocol::message::{
    DataToShell, Deleted, EventId, ModelChoice, PageOf, RunId, SceneId, Step, ThreadId, ThreadTitle,
};

use super::Input;
use crate::store::{Generation, RunEnd, SceneTarget, TitleSource};

/// A store operation: an autocommit read, or a write in one transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreOp {
    /// A v1 drawing read or durable transition; its arguments never enter logs.
    Drawing {
        /// The drawing reducer's outstanding operation identity.
        serial: u64,
        /// Typed data with content-free diagnostics.
        write: crate::drawing::Write,
    },
    /// An autocommit read that neither stops nor postpones a purge.
    ReadPage {
        /// The connection that requested the page.
        epoch: u64,
        /// The shell's request counter.
        request: u32,
        /// The page to read.
        page: PageOf,
    },
    /// Build a round's context on the page reader, in one read transaction.
    BuildContext(super::BuildContext),
    /// Read a background job's request on the page reader, in one read transaction.
    ReadJob(super::ReadJob),
    /// Read an inline compaction's request on the page reader, in one read transaction.
    CompactContext(super::CompactContext),
    /// The turn, its run, and the thread when `title` names a new one.
    BeginTurn {
        /// The thread.
        thread: ThreadId,
        /// The new thread's title, or `None` for an existing thread.
        title: Option<String>,
        /// The run.
        run: RunId,
        /// The user's text.
        text: String,
    },
    /// The text streamed since the last write, then one step.
    CommitStep {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed before the step, if any.
        text: Option<String>,
        /// A new scene for a run's first step, or the run's scene.
        scene: SceneTarget,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
    },
    /// The text streamed since the last write, the revealed set, then the run's outcome.
    EndRun {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// Text streamed since the last step, if any.
        text: Option<String>,
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
        text: String,
        /// The model that wrote it; `None` when the engine took none.
        model: Option<String>,
    },
    /// Store a live thread's model and effort. Nothing answers it.
    SetModel {
        /// The thread.
        thread: ThreadId,
        /// The model and effort.
        choice: ModelChoice,
    },
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
            Self::Drawing { .. }
            | Self::ReadPage { .. }
            | Self::BuildContext(_)
            | Self::ReadJob(_)
            | Self::CompactContext(_)
            | Self::ClearScenes { .. }
            | Self::KeepScene { .. }
            | Self::Delete { .. }
            | Self::SetTitle { .. }
            | Self::StoreSummary { .. }
            | Self::SetModel { .. } => None,
        }
    }

    /// Answers an operation that couldn't reach the store; `None` for one nothing waits on.
    pub fn failed(&self) -> Option<Input> {
        Some(match self {
            Self::Drawing { serial, .. } => Input::DrawingWritten {
                serial: *serial,
                reply: Err(()),
            },
            Self::ReadPage { epoch, request, .. } => Input::PageRead {
                epoch: *epoch,
                request: *request,
                reply: DataToShell::PageFailed { request: *request },
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
            Self::SetModel { .. } => return None,
        })
    }
}
