//! What the core loop asks of the store thread, and the answer a failed write gets.

use capsula_protocol::message::{DataToShell, PageOf, RunId, SceneId, Step, ThreadId};

use super::Input;
use crate::store::{RunEnd, SceneTarget};

/// A store operation: an autocommit read, or a write in one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOp {
    /// An autocommit read that neither stops nor postpones a purge.
    ReadPage {
        /// The connection that requested the page.
        epoch: u64,
        /// The shell's request counter.
        request: u32,
        /// The page to read.
        page: PageOf,
    },
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
}

impl StoreOp {
    /// The run a write belongs to; page reads and clears have none.
    pub fn run(&self) -> Option<RunId> {
        match self {
            Self::ReadPage { .. } | Self::ClearScenes { .. } | Self::KeepScene { .. } => None,
            Self::BeginTurn { run, .. }
            | Self::CommitStep { run, .. }
            | Self::EndRun { run, .. } => Some(*run),
        }
    }

    /// Answers an operation that couldn't reach the store.
    pub fn failed(&self) -> Input {
        match self {
            Self::ReadPage { epoch, request, .. } => Input::PageRead {
                epoch: *epoch,
                request: *request,
                reply: DataToShell::PageFailed { request: *request },
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
        }
    }
}
