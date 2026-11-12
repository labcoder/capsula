//! The brain's canonical drawings: one [`SceneState`] per visible scene, one [`GlobalBudget`],
//! draw calls keyed by run and call id, and the order every change reaches the store and the
//! shell. Sans I/O: inputs come from the coordinator, which carries out the effects.
//!
//! A change runs admit, then one durable write, then the semantic commit, then the data FIFO.
//! At most one write is in the store at a time, and it seals the scene and the global budget:
//! every other mutation (records, cuts, retirements, clears) queues in arrival order until its
//! answer. Fences act at once: a closed run's queued records go, and a cleared or deleted scene's
//! later frames are never sent.

mod call;
pub mod io;
mod scenes;
mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use kumochi_protocol::message::{RunId, SceneCuts, SceneId, ThreadId};
use kumochi_scene::{Admitted, Begin, GlobalBudget, Record, SceneState, Step, StopCut};

use crate::coordinator::{Effect, Stale, StoreOp};
use crate::engines::CallId;

pub use call::{CallEnd, CallError};
pub use io::{Reply, StoredScene, Target, Write};
pub(crate) use snapshot::CHUNK_TARGET;
pub use snapshot::Snapshot;

use call::{Call, RunLedger};

/// The layout algorithm a new scene is begun with, persisted in its header.
pub const LAYOUT_VERSION: u32 = 2;

/// Jobs waiting behind the pending write: every record a run's calls may make, each call's
/// refusal and finish, and room for the shell's shown steps, cuts and stops (guess).
const MAX_QUEUED_JOBS: usize = call::MAX_CALLS_PER_RUN * (call::MAX_CALL_RECORDS + 2) + 256;

/// Something for the drawing system to act on.
#[derive(Clone, PartialEq)]
pub enum Input {
    /// A complete call whose preflight failed, charged without admitting a scene record.
    Rejected {
        /// The resolved thread.
        thread: ThreadId,
        /// The owning run.
        run: RunId,
        /// The host's internal call id.
        call: CallId,
        /// The first preflight failure.
        error: CallError,
    },
    /// A complete-only call: its whole arguments, checked as one envelope before anything runs.
    Call {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// The provider's call id.
        call: CallId,
        /// The arguments.
        args: Vec<u8>,
    },
    /// One closed record of a streamed call, in tokenizer order.
    Record {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// The provider's call id.
        call: CallId,
        /// The record's zero-based position in the call.
        index: u32,
        /// The record's JSON.
        bytes: Vec<u8>,
    },
    /// A streamed call's arguments ended; `error` when they were malformed or truncated.
    CallFinished {
        /// The thread.
        thread: ThreadId,
        /// The run.
        run: RunId,
        /// The provider's call id.
        call: CallId,
        /// The terminal ingress failure, or an interruption that discards queued records.
        error: Option<CallError>,
    },
    /// Cuts for every known visible scene, whichever runs committed its steps.
    /// An unseen, never-activated scene of `run` is cut to nothing after its write.
    StopScenes {
        /// The stopped run.
        run: RunId,
        /// At most four scenes.
        cuts: SceneCuts,
    },
    /// The first paint containing a step.
    StepShown {
        /// The run that committed it.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Its zero-based lifetime index.
        index: u32,
    },
    /// The shell couldn't prepare a step and stopped its scene at `cut`.
    StepFailed {
        /// The run that committed it.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Its zero-based lifetime index.
        index: u32,
        /// The cut at the last activated revision.
        cut: StopCut,
    },
    /// The user cleared these scenes; ids the drawing system doesn't hold are ignored.
    Clear {
        /// The scenes.
        scenes: Vec<SceneId>,
    },
    /// Quit: every scene is cleared once the write in the store is answered.
    ClearVisible,
    /// The run ended: its later input, and any earlier run's, is stale. Its scenes stay.
    RunClosed {
        /// The run.
        run: RunId,
    },
    /// The thread was hidden or removed: its scenes leave and their late frames are dropped.
    ThreadDeleted {
        /// The thread.
        thread: ThreadId,
    },
    /// The store answered the write `serial`.
    Written {
        /// The write.
        serial: u64,
        /// Its answer, or `Err` when nothing of it was stored.
        reply: Result<Reply, ()>,
    },
}

/// What one input produced.
#[derive(Debug, Default)]
pub struct Output {
    /// Effects for the coordinator to carry out, in order.
    pub effects: Vec<Effect>,
    /// Calls that ended.
    pub calls: Vec<CallEnd>,
}

/// The drawing system's state.
#[derive(Default)]
pub struct Drawing {
    unavailable: bool,
    global: GlobalBudget,
    /// Visible scenes, oldest first.
    scenes: Vec<Held>,
    calls: Vec<Call>,
    runs: Vec<RunLedger>,
    /// Runs up to this one are closed; the default is below every run.
    closed: RunId,
    /// At most [`MAX_QUEUED_JOBS`].
    queue: VecDeque<Job>,
    pending: Option<Pending>,
    serial: u64,
    /// A v1 clear isn't acknowledged yet, so no new scene's first step is written: the store then
    /// never holds more than four uncleared scenes.
    clearing: bool,
}

/// A visible scene.
struct Held {
    scene: SceneId,
    thread: ThreadId,
    layout_version: u32,
    state: SceneState,
    /// The run that committed each lifetime revision; `None` for recovered steps.
    committers: Vec<Option<RunId>>,
    /// Cleared or deleted: no frame of it goes out, and it leaves once no write is pending.
    gone: bool,
}

/// Work that waits for the store to answer.
enum Job {
    Record {
        run: RunId,
        call: CallId,
        record: Box<Record>,
    },
    Reject {
        run: RunId,
        call: CallId,
        error: CallError,
    },
    Finish {
        run: RunId,
        call: CallId,
        error: Option<CallError>,
    },
    Cut {
        scene: SceneId,
        cut: StopCut,
        kind: CutKind,
    },
    Shown {
        scene: SceneId,
        index: u32,
    },
    /// After a Stop's cuts: the stopped run's scenes the shell named no cut for.
    Unseen {
        run: RunId,
        named: Vec<SceneId>,
    },
    ClearVisible,
}

/// Why the shell cut a scene.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CutKind {
    /// A run stopped.
    Stop,
    /// The shell couldn't prepare a step, so the calls drawing in the scene end too.
    Failed,
}

/// The one write in the store.
struct Pending {
    serial: u64,
    /// False once it failed: a cut is owed until `retry` writes it again.
    in_flight: bool,
    wait: Wait,
}

enum Wait {
    Lookup {
        run: RunId,
        call: CallId,
        begin: Box<Begin>,
    },
    Step(Box<StepWait>),
    Cut {
        scene: SceneId,
        revision: u32,
        cut: StopCut,
    },
}

/// An admitted step in the store.
struct StepWait {
    run: RunId,
    call: CallId,
    thread: ThreadId,
    scene: Pend,
    admitted: Admitted,
    step: Arc<Step>,
    /// Its thread was deleted: the answer changes nothing.
    fenced: bool,
    interrupted: Option<CallError>,
}

/// The scene a pending step goes into.
enum Pend {
    Held(SceneId),
    New(Box<SceneState>),
}

impl Drawing {
    pub(crate) fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    pub(crate) fn available(&self) -> bool {
        !self.unavailable
    }

    /// Rebuilds the visible scenes the store kept, bound to one budget together. A scene is the
    /// unit of failure: one whose log doesn't replay, whose layout this build can't prepare or that
    /// doesn't fit beside the others is left out, and its id returned for a durable clear. So is
    /// a scene a crash left cut to nothing before its clear landed.
    pub fn recover(mut stored: Vec<StoredScene>) -> (Self, Vec<SceneId>) {
        stored.sort_by_key(|scene| scene.scene);
        let mut skipped = Vec::new();
        stored.retain(|scene| {
            let known = scene.header.layout_version == LAYOUT_VERSION;
            if !known {
                skipped.push(scene.scene);
            }
            known
        });
        let (states, global) = loop {
            let batch = {
                let sources: Vec<_> = stored
                    .iter()
                    .map(|scene| (&scene.header.begin, scene.log.as_slice()))
                    .collect();
                SceneState::replay_all(&sources)
            };
            if let Ok(batch) = batch {
                break batch;
            }
            // The scene that doesn't replay alone, else the oldest, which the others crowd out.
            let bad = stored
                .iter()
                .position(|scene| {
                    SceneState::replay(scene.header.begin.clone(), &scene.log).is_err()
                })
                .unwrap_or(0);
            skipped.push(stored.remove(bad).scene);
        };
        let mut scenes = Vec::new();
        for (stored, state) in stored.into_iter().zip(states) {
            if state.is_cleared() {
                skipped.push(stored.scene);
                continue;
            }
            scenes.push(Held {
                scene: stored.scene,
                thread: stored.thread,
                layout_version: stored.header.layout_version,
                committers: vec![None; usize::try_from(state.revision()).unwrap_or_default()],
                state,
                gone: false,
            });
        }
        skipped.sort_unstable();
        let drawing = Self {
            global,
            scenes,
            ..Self::default()
        };
        (drawing, skipped)
    }

    /// Acts on one input.
    ///
    /// # Errors
    /// [`Stale`] when the input names a closed run, repeats a call or record already taken,
    /// or answers no pending write; nothing changed.
    pub fn handle(&mut self, input: Input) -> Result<Output, Stale> {
        let mut out = Output::default();
        match input {
            Input::Rejected {
                thread,
                run,
                call,
                error,
            } => {
                self.rejected_call(thread, run, &call, error, &mut out)?;
            }
            Input::Call {
                thread,
                run,
                call,
                args,
            } => self.complete_call(thread, run, call, &args, &mut out)?,
            Input::Record {
                thread,
                run,
                call,
                index,
                bytes,
            } => self.closed_record(thread, run, &call, index, &bytes, &mut out)?,
            Input::CallFinished {
                thread,
                run,
                call,
                error,
            } => self.call_finished(thread, run, call, error, &mut out)?,
            Input::StopScenes { run, cuts } => {
                // shortcut: D1 has one run; OSC-56 needs per-run revocation for concurrency.
                for cut in cuts.iter() {
                    let job = Job::Cut {
                        scene: cut.scene,
                        cut: cut.cut.clone(),
                        kind: CutKind::Stop,
                    };
                    self.enqueue(job, &mut out);
                }
                let named = cuts.iter().map(|cut| cut.scene).collect();
                self.enqueue(Job::Unseen { run, named }, &mut out);
            }
            Input::StepShown { run, scene, index } => {
                if self.committed_by(run, scene, index, &mut out) {
                    self.enqueue(Job::Shown { scene, index }, &mut out);
                }
            }
            Input::StepFailed {
                run,
                scene,
                index,
                cut,
            } => {
                if self.committed_by(run, scene, index, &mut out) {
                    let job = Job::Cut {
                        scene,
                        cut,
                        kind: CutKind::Failed,
                    };
                    self.enqueue(job, &mut out);
                }
            }
            Input::Clear { scenes } => self.clear(&scenes, &mut out),
            Input::ClearVisible => {
                self.clear_visible(&mut out);
                if !self.settled() {
                    self.enqueue(Job::ClearVisible, &mut out);
                }
            }
            Input::RunClosed { run } => self.close(run, &mut out),
            Input::ThreadDeleted { thread } => self.deleted(thread, &mut out),
            Input::Written { serial, reply } => self.written(serial, reply, &mut out)?,
        }
        self.pump(&mut out);
        Ok(out)
    }

    /// An owed cut's write failed and none is in the store.
    pub fn needs_retry(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| !pending.in_flight)
    }

    /// Writes the owed cut again.
    pub fn retry(&mut self) -> Vec<Effect> {
        let Some(pending) = self.pending.as_mut().filter(|pending| !pending.in_flight) else {
            return Vec::new();
        };
        let Wait::Cut {
            scene,
            revision,
            cut,
        } = &pending.wait
        else {
            return Vec::new();
        };
        self.serial += 1;
        pending.serial = self.serial;
        pending.in_flight = true;
        let write = Write::Cut {
            scene: *scene,
            revision: *revision,
            cut: cut.clone(),
        };
        vec![Effect::Store(StoreOp::Drawing {
            serial: self.serial,
            write,
        })]
    }

    /// Work of `run` is in the store or queued behind it, so the run's outcome waits for it.
    pub fn pending_for(&self, run: RunId) -> bool {
        let writing = self
            .pending
            .as_ref()
            .is_some_and(|pending| match &pending.wait {
                Wait::Lookup { run: owner, .. } => *owner == run,
                Wait::Step(write) => write.run == run,
                Wait::Cut { .. } => false,
            });
        writing
            || self.queue.iter().any(|job| {
                matches!(job, Job::Record { run: owner, .. }
                    | Job::Reject { run: owner, .. }
                    | Job::Finish { run: owner, .. } if *owner == run)
            })
    }

    /// Nothing is in the store or waiting for it.
    pub fn settled(&self) -> bool {
        self.pending.is_none() && self.queue.is_empty()
    }

    /// A write is in the store; an owed cut whose write failed isn't.
    pub fn in_flight(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.in_flight)
    }

    /// A cut is in the store or owed to it, so the shared retry backoff must keep growing.
    pub fn cut_pending(&self) -> bool {
        matches!(
            self.pending,
            Some(Pending {
                wait: Wait::Cut { .. },
                ..
            })
        )
    }

    /// Whether a v1 clear is still unacknowledged. A new scene's first step waits while one is,
    /// and runs once none is.
    pub fn clears_pending(&mut self, count: usize) -> Output {
        let pending = count > 0;
        let mut out = Output::default();
        if self.clearing != pending {
            self.clearing = pending;
            self.pump(&mut out);
        }
        out
    }

    /// The thread a held scene belongs to, including a cleared or deleted scene still kept
    /// while its write is pending. `None` once the scene has left.
    pub fn thread(&self, scene: SceneId) -> Option<ThreadId> {
        self.scenes
            .iter()
            .find(|held| held.scene == scene)
            .map(|held| held.thread)
    }

    /// A live scene's labelled solid, for an ask: the scene's thread, its `alt` and the part's
    /// current text. `None` once the scene is cleared, deleted or gone, or the part isn't one.
    pub(crate) fn part(
        &self,
        scene: SceneId,
        element: kumochi_scene::Handle,
    ) -> Option<(ThreadId, &kumochi_scene::Text, kumochi_scene::Part<'_>)> {
        let held = &self.scenes[self.live(scene)?];
        let part = held.state.part(element)?;
        Some((held.thread, &held.state.header().alt, part))
    }

    /// Runs queued work until a write is in the store. Scenes that left go first, so their
    /// charges are released before anything is admitted.
    fn pump(&mut self, out: &mut Output) {
        while self.pending.is_none() {
            self.sweep();
            if self.clearing && self.queue.front().is_some_and(|job| self.begins_scene(job)) {
                return;
            }
            let Some(job) = self.queue.pop_front() else {
                return;
            };
            match job {
                Job::Record { run, call, record } => self.record(run, &call, *record, out),
                Job::Reject { run, call, error } => {
                    if let Some(at) = self.open_call(run, &call) {
                        self.fail(at, error, out);
                    }
                }
                Job::Finish { run, call, error } => self.finish(run, &call, error, out),
                Job::Cut { scene, cut, kind } => self.cut(scene, cut, kind, out),
                Job::Shown { scene, index } => self.shown(scene, index, out),
                Job::Unseen { run, named } => self.unseen(run, named, out),
                Job::ClearVisible => self.clear_visible(out),
            }
        }
    }

    /// Queues `job` behind the pending write. Past the ceiling the session is broken: dropping
    /// or merging work would change what commits, so the job goes with the session.
    fn enqueue(&mut self, job: Job, out: &mut Output) {
        if self.queue.len() < MAX_QUEUED_JOBS {
            self.queue.push_back(job);
            return;
        }
        let broken = Effect::Broken("more drawing work queued than the brain holds");
        if !out.effects.contains(&broken) {
            out.effects.push(broken);
        }
    }

    /// Sends `write` to the store; nothing else changes the scenes until it's answered.
    fn write(&mut self, wait: Wait, write: Write, out: &mut Output) {
        self.serial += 1;
        out.effects.push(Effect::Store(StoreOp::Drawing {
            serial: self.serial,
            write,
        }));
        self.pending = Some(Pending {
            serial: self.serial,
            in_flight: true,
            wait,
        });
    }

    /// The store answered the pending write.
    fn written(
        &mut self,
        serial: u64,
        reply: Result<Reply, ()>,
        out: &mut Output,
    ) -> Result<(), Stale> {
        let pending = self
            .pending
            .take_if(|pending| pending.in_flight && pending.serial == serial)
            .ok_or(Stale)?;
        match (pending.wait, reply) {
            (Wait::Lookup { run, call, begin }, Ok(Reply::Lookup(found))) => {
                self.looked_up(run, &call, *begin, found, out);
            }
            (Wait::Step(write), Ok(_)) if write.fenced => {
                if let Some(error) = write.interrupted {
                    self.finish(write.run, &write.call, Some(error), out);
                }
            }
            (Wait::Step(write), Ok(Reply::Step(id))) => self.stored(*write, id, out),
            (
                Wait::Cut {
                    scene,
                    revision,
                    cut,
                },
                Ok(Reply::Cut),
            ) => {
                self.cut_stored(scene, revision, cut, out);
            }
            // The scene left while its cut was in the store: its clear supersedes the cut.
            (Wait::Cut { scene, .. }, _) if self.live(scene).is_none() => {}
            (wait @ Wait::Cut { .. }, _) => {
                self.pending = Some(Pending {
                    serial,
                    in_flight: false,
                    wait,
                });
            }
            (Wait::Lookup { run, call, .. }, _) => self.store_failed(run, &call, out),
            (Wait::Step(write), _) => self.store_failed(write.run, &write.call, out),
        }
        let closed = self.closed;
        self.calls.retain(|call| call.run > closed || !call.done);
        self.runs.retain(|ledger| ledger.run > closed);
        Ok(())
    }

    fn live(&self, scene: SceneId) -> Option<usize> {
        self.scenes
            .iter()
            .position(|held| held.scene == scene && !held.gone)
    }
}

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rejected { .. } => "Rejected",
            Self::Call { .. } => "Call",
            Self::Record { .. } => "Record",
            Self::CallFinished { .. } => "CallFinished",
            Self::StopScenes { .. } => "StopScenes",
            Self::StepShown { .. } => "StepShown",
            Self::StepFailed { .. } => "StepFailed",
            Self::Clear { .. } => "Clear",
            Self::ClearVisible => "ClearVisible",
            Self::RunClosed { .. } => "RunClosed",
            Self::ThreadDeleted { .. } => "ThreadDeleted",
            Self::Written { .. } => "Written",
        })
    }
}

impl fmt::Debug for Drawing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Drawing")
            .field("scenes", &self.scenes.len())
            .field("calls", &self.calls.len())
            .field("queued", &self.queue.len())
            .field(
                "pending",
                &self.pending.as_ref().map(|pending| pending.serial),
            )
            .finish_non_exhaustive()
    }
}
