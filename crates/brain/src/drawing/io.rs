//! The drawing system's store contract: the writes it asks for and the answers it expects.
//!
//! Every write is one transaction, run in the order the core loop queued it, and answered once.
//! Arguments carry drawing content, so nothing here may be logged; `Debug` prints only ids and
//! counts.

use std::{fmt, sync::Arc};

use kumochi_protocol::message::{RunId, SceneHeader, SceneId, ThreadId};
use kumochi_scene::{SceneEvent, Step, StopCut, WireId, record::Surface};

/// The scene a step's write goes into.
#[derive(Clone, PartialEq)]
pub enum Target {
    /// A scene's first step: the same transaction writes the `scene_begin` (header, layout
    /// version and the model's scene name) whose event id becomes the scene's id.
    New(SceneHeader),
    /// A later step of a scene the store already allocated.
    Existing(SceneId),
}

/// One store operation of the drawing system.
#[derive(Clone, PartialEq)]
pub enum Write {
    /// An autocommit read: whether the scene name ever began on this thread and surface.
    Lookup {
        /// The live thread.
        thread: ThreadId,
        /// The scene's typed surface.
        surface: Surface,
        /// The model's scene name.
        name: WireId,
    },
    /// One committed step as a `scene_step` event with its typed JSON in the body, never in
    /// the `text` column. A new scene's `scene_begin` goes in the same transaction.
    Step {
        /// The live thread.
        thread: ThreadId,
        /// The run that drew it.
        run: RunId,
        /// The scene.
        target: Target,
        /// Frozen victims cleared in the new scene's first-step transaction, at most four.
        clear: Vec<SceneId>,
        /// One-based lifetime commit revision.
        revision: u32,
        /// The layout version the scene was begun with.
        layout_version: u32,
        /// The original typed step.
        step: Arc<Step>,
    },
    /// An effective cut as a `scene_cut` event, ordered after every step it follows.
    Cut {
        /// The scene.
        scene: SceneId,
        /// The last committed revision before the cut.
        revision: u32,
        /// The authoritative cut.
        cut: StopCut,
    },
}

/// The store's answer to a committed [`Write`]. A failed write answers `Err(())`, after which
/// nothing of it is in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The scene that began with the given name on the given thread and surface, if any.
    Lookup(Option<SceneId>),
    /// The scene the step was written into; a new scene's id is allocated here.
    Step(SceneId),
    /// The cut is durable.
    Cut,
}

/// A visible scene read at start: the newest four scenes of live threads with no
/// `scene_cleared`, each with its complete log of steps and cuts in event order.
#[derive(Clone, PartialEq)]
pub struct StoredScene {
    /// The scene.
    pub scene: SceneId,
    /// Its thread.
    pub thread: ThreadId,
    /// The header and layout version it began with.
    pub header: SceneHeader,
    /// Every committed step at its revision, interleaved with every effective cut.
    pub log: Vec<SceneEvent>,
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::New(header) => f.debug_tuple("New").field(header).finish(),
            Self::Existing(scene) => f.debug_tuple("Existing").field(scene).finish(),
        }
    }
}

impl fmt::Debug for Write {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lookup { thread, .. } => f
                .debug_struct("Lookup")
                .field("thread", thread)
                .finish_non_exhaustive(),
            Self::Step {
                thread,
                run,
                target,
                revision,
                layout_version,
                clear,
                ..
            } => f
                .debug_struct("Step")
                .field("thread", thread)
                .field("run", run)
                .field("target", target)
                .field("revision", revision)
                .field("layout_version", layout_version)
                .field("cleared", &clear.len())
                .finish_non_exhaustive(),
            Self::Cut {
                scene,
                revision,
                cut,
            } => f
                .debug_struct("Cut")
                .field("scene", scene)
                .field("revision", revision)
                .field("activated", &cut.activated)
                .field("revealed", &cut.revealed.len())
                .finish(),
        }
    }
}

impl fmt::Debug for StoredScene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredScene")
            .field("scene", &self.scene)
            .field("thread", &self.thread)
            .field("header", &self.header)
            .field("log", &self.log.len())
            .finish()
    }
}
