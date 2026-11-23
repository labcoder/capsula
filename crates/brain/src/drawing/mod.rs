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
mod digest;
pub mod io;
mod prepare;
mod ready;
mod scenes;
mod screen;
mod snapshot;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kumochi_protocol::message::{Content, RunId, SceneCuts, SceneFailure, SceneId, ThreadId};
use kumochi_scene::display::{PaintWork, WideMeasure};
use kumochi_scene::layout::LayoutState;
use kumochi_scene::theme::Theme;
use kumochi_scene::{Admitted, Begin, GlobalBudget, Record, SceneState, Step, StopCut};

use crate::config::tuning::DrawingTuning;
use crate::coordinator::{BuildKey, Effect, Stale, StoreOp};
use crate::engines::CallId;

pub use call::{CallEnd, CallError, Remaining};
pub(crate) use digest::{Digest, PrivateScene};
pub use io::{Reply, StoredScene, Target, Write};
pub use prepare::{PrepareJob, Prepared, Shared};
pub(crate) use screen::{SCREEN_MARKS, ScreenMarks, stored_item};
pub(crate) use snapshot::CHUNK_TARGET;
pub use snapshot::Snapshot;

use call::{Call, RunLedger};
use prepare::Paint;

/// The layout algorithm a new scene is begun with, persisted in its header.
pub const LAYOUT_VERSION: u32 = 2;

/// Jobs waiting behind the pending write: every record a run's calls may make, each call's
/// refusal and finish, and room for the shell's shown and ready steps, cuts and stops (guess).
const MAX_QUEUED_JOBS: usize = call::MAX_CALLS_PER_RUN * (call::MAX_CALL_RECORDS + 2) + 256;

/// Whether a run's lost drawing is told when the run can't repair it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loss {
    /// The run failed or finished on its own.
    Tell,
    /// The user ended the run, so the loss was theirs.
    Drop,
}

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
    /// A complete draw whose arguments already passed preflight.
    CheckedCall {
        /// The resolved thread.
        thread: ThreadId,
        /// The owning run.
        run: RunId,
        /// The host's internal call id.
        call: CallId,
        /// The typed records in tokenizer order.
        records: crate::dispatch::DrawRecords,
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
    /// A streamed call's arguments ended: `None` once they closed, `error` when they were malformed
    /// or truncated.
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
    /// The shell prepared a step and queued it to activate.
    StepReady {
        /// The run that committed it.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Its zero-based lifetime index.
        index: u32,
    },
    /// A ready-wait interval passed while a finished call waited for the shell.
    ReadyWait,
    /// The shell couldn't prepare a step and stopped its scene at `cut`.
    StepFailed {
        /// The run that committed it.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Its zero-based lifetime index.
        index: u32,
        /// Why, without content.
        reason: SceneFailure,
        /// The cut at the last activated revision.
        cut: StopCut,
    },
    /// The user cleared these scenes; ids the drawing system doesn't hold are ignored.
    Clear {
        /// The scenes.
        scenes: Vec<SceneId>,
    },
    /// A terminal whole-scene refusal, including a header rejected before its run was known.
    SceneFailed {
        /// The committed scene.
        scene: SceneId,
    },
    /// The shell skipped a screen scene because the screen may have changed since its picture.
    SceneSkipped {
        /// The committed scene.
        scene: SceneId,
    },
    /// Quit: every scene is cleared once the write in the store is answered.
    ClearVisible,
    /// The user turned off sharing the screen: screen scenes leave and nothing new is drawn on it.
    ScreenOff,
    /// The run ended: its later input, and any earlier run's, is stale. Its scenes stay.
    RunClosed {
        /// The run.
        run: RunId,
        /// Whether a drawing it lost is told.
        loss: Loss,
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
    /// The step `serial` was laid out with the wide measure.
    Prepared {
        /// The preparation.
        serial: u64,
        /// How it ended.
        outcome: Prepared,
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
    pub(crate) tuning: DrawingTuning,
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
    theme: Arc<Theme>,
    /// Unit tests lay steps out inline, answering at once, unless they defer to an effect.
    #[cfg(test)]
    pub(crate) defer_preparation: bool,
}

/// A visible scene.
struct Held {
    scene: SceneId,
    thread: ThreadId,
    layout_version: u32,
    state: SceneState,
    author: Author,
    /// The run that committed each lifetime revision; `None` for recovered steps.
    committers: Vec<Option<RunId>>,
    /// Revisions a failed cut already counted as refused, so a later one counts only newer ones.
    refused_through: usize,
    /// Cleared or deleted: no frame of it goes out, and it leaves once no write is pending.
    gone: bool,
    /// Its first step replaced earlier drawings and nothing of it was shown yet.
    replaced: bool,
    /// Its wide layout and charge; `None` once a preparation failed for any reason but paint
    /// work, leaving the shell's guard as the scene's only check.
    paint: Option<Paint>,
}

/// Which model may read a scene's metadata, from the protected material the requests of the runs
/// that drew it carried (a picture or a protected digest), whatever its surface.
#[derive(Clone, PartialEq, Eq)]
enum Author {
    /// Every run that drew it carried none.
    Public,
    /// The runs that carried any carried it for this recipient alone.
    Private(crate::context::PictureRecipient),
    /// Restored with a run that wasn't marked public, a screen scene with no recipient, or drawn
    /// for two recipients: no model gets it.
    Unknown,
}

impl Author {
    /// Folds another step's authorship into `self`.
    fn and(self, step: Self) -> Self {
        match (self, step) {
            (author, Self::Public) => author,
            (Self::Public, Self::Private(recipient)) => Self::Private(recipient),
            (Self::Private(held), Self::Private(recipient)) if held == recipient => {
                Self::Private(held)
            }
            _ => Self::Unknown,
        }
    }

    /// Whether this authorship came from protected material (`runs.protected = true`).
    const fn is_protected(&self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// Work that waits for the store to answer.
enum Job {
    ReplyText {
        key: BuildKey,
        text: Content,
    },
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
    Ready {
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
    Failed(SceneFailure),
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
    /// An admitted step being laid out before its write.
    Prepare {
        step: Box<StepWait>,
        write: Write,
        cancel: Arc<AtomicBool>,
    },
}

/// An admitted step in the store.
struct StepWait {
    run: RunId,
    call: CallId,
    thread: ThreadId,
    scene: Pend,
    step: Arc<Step>,
    /// Its thread was deleted: suppress the newcomer but settle committed replacement victims.
    fenced: bool,
    clear_requested: bool,
    /// The user ended its run first, so a replacement it begins reports no loss.
    loss_dropped: bool,
    interrupted: Option<CallError>,
    /// The step's wide layout and its resident bytes, which the scene keeps once the write
    /// commits; `None` writes it uncounted.
    laid: Option<(LayoutState, usize)>,
    /// Its run's authorship when the step was admitted, read once so the stored marker and the
    /// live stamp agree even when the run closes before the write is acknowledged; `revoke_private`
    /// may downgrade `Private` to `Unknown` while in flight.
    author: Author,
}

/// The scene a pending step goes into.
enum Pend {
    Held {
        scene: SceneId,
        admitted: Admitted,
    },
    New {
        admission: Box<kumochi_scene::ReplacementAdmission>,
        victims: Vec<SceneId>,
    },
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
            // A screen scene's boxes were never stored, so it can't be drawn again.
            let known = scene.header.layout_version == LAYOUT_VERSION
                && scene.header.begin.surface != kumochi_scene::record::Surface::Screen;
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
        let (stored, states): (Vec<_>, Vec<_>) = stored
            .into_iter()
            .zip(states)
            .filter(|(stored, state)| {
                if state.is_cleared() {
                    skipped.push(stored.scene);
                }
                !state.is_cleared()
            })
            .unzip();
        let theme = Arc::<Theme>::default();
        let (states, global, layouts) = Self::lay_out(&stored, states, global, &theme);
        let mut scenes = Vec::new();
        for ((stored, state), layout) in stored.into_iter().zip(states).zip(layouts) {
            scenes.push(Held {
                scene: stored.scene,
                thread: stored.thread,
                layout_version: stored.header.layout_version,
                committers: vec![None; usize::try_from(state.revision()).unwrap_or_default()],
                refused_through: 0,
                state,
                author: if stored.public {
                    Author::Public
                } else {
                    Author::Unknown
                },
                gone: false,
                replaced: false,
                paint: layout.map(Paint::at_rest),
            });
        }
        skipped.sort_unstable();
        let drawing = Self {
            global,
            scenes,
            theme,
            ..Self::default()
        };
        (drawing, skipped)
    }

    /// Lays the kept scenes out with the wide measure. Only a batch that lays out whole replaces
    /// the replayed states, since a layout belongs to the state it was prepared with.
    // shortcut: one scene that fails to lay out leaves every recovered scene uncounted until it's
    // cleared; recover the rest alone if a recovery failure shows up in use.
    fn lay_out(
        stored: &[StoredScene],
        states: Vec<SceneState>,
        global: GlobalBudget,
        theme: &Arc<Theme>,
    ) -> (Vec<SceneState>, GlobalBudget, Vec<Option<LayoutState>>) {
        let sources: Vec<_> = stored
            .iter()
            .map(|scene| (&scene.header.begin, scene.log.as_slice()))
            .collect();
        let (laid, laid_global) = kumochi_scene::layout::recover_all(
            &sources,
            prepare::SURFACE,
            theme,
            &mut WideMeasure,
            &AtomicBool::new(false),
            Some(PaintWork::LIMITS),
        );
        if let Ok(laid) = laid.into_iter().collect::<Result<Vec<_>, _>>() {
            let (states, layouts) = laid
                .into_iter()
                .map(|(state, layout)| (state, Some(layout)))
                .unzip();
            (states, laid_global, layouts)
        } else {
            let layouts = states.iter().map(|_| None).collect();
            (states, global, layouts)
        }
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
            } => self.complete_call(thread, run, call, call::CompleteArgs::Raw(&args), &mut out)?,
            Input::CheckedCall {
                thread,
                run,
                call,
                records,
            } => self.complete_call(
                thread,
                run,
                call,
                call::CompleteArgs::Checked(records),
                &mut out,
            )?,
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
            Input::StopScenes { run, cuts } => self.stop_scenes(run, &cuts, &mut out),
            Input::StepShown { run, scene, index } => {
                if self.committed_by(run, scene, index, &mut out) {
                    if let Some(at) = self.live(scene) {
                        self.scenes[at].replaced = false;
                    }
                    if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == run) {
                        ledger.loss = None;
                    }
                    self.enqueue(Job::Shown { scene, index }, &mut out);
                }
            }
            Input::StepReady { run, scene, index } => {
                if self.committed_by(run, scene, index, &mut out) {
                    self.enqueue(Job::Ready { scene, index }, &mut out);
                }
            }
            Input::ReadyWait => self.ready_wait(&mut out),
            Input::StepFailed {
                run,
                scene,
                index,
                reason,
                cut,
            } => {
                if self.committed_by(run, scene, index, &mut out) {
                    self.replacement_failed(scene, &mut out);
                    self.refused(scene, index, &scenes::failure(reason, &cut));
                    let job = Job::Cut {
                        scene,
                        cut,
                        kind: CutKind::Failed(reason),
                    };
                    self.enqueue(job, &mut out);
                }
            }
            Input::Clear { scenes } => self.clear(&scenes, &mut out),
            Input::SceneFailed { scene } => self.scene_failed(scene, &mut out),
            Input::SceneSkipped { scene } => self.scene_skipped(scene, &mut out),
            Input::ScreenOff => self.screen_off(&mut out),
            Input::ClearVisible => {
                self.clear_visible(&mut out);
                if !self.settled() {
                    self.enqueue(Job::ClearVisible, &mut out);
                }
            }
            Input::RunClosed { run, loss } => self.close(run, loss, &mut out),
            Input::ThreadDeleted { thread } => self.deleted(thread, &mut out),
            Input::Written { serial, reply } => self.written(serial, reply, &mut out)?,
            Input::Prepared { serial, outcome } => self.prepared(serial, outcome, &mut out)?,
        }
        self.pump(&mut out);
        self.release_left(&mut out);
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

    /// Whether `run` called `draw` and every step it committed was refused by the shell.
    pub fn drew_nothing(&self, run: RunId) -> bool {
        self.runs
            .iter()
            .any(|ledger| ledger.run == run && ledger.drew_nothing())
    }

    /// Work of `run` is in the store or queued behind it, so the run's outcome waits for it.
    pub fn pending_for(&self, run: RunId) -> bool {
        let writing = self
            .pending
            .as_ref()
            .is_some_and(|pending| match &pending.wait {
                Wait::Lookup { run: owner, .. } => *owner == run,
                Wait::Step(write) | Wait::Prepare { step: write, .. } => write.run == run,
                Wait::Cut { .. } => false,
            });
        writing
            || self.queue.iter().any(|job| {
                matches!(job, Job::ReplyText { key, .. } if key.run == run)
                    || matches!(job, Job::Record { run: owner, .. }
                    | Job::Reject { run: owner, .. }
                    | Job::Finish { run: owner, .. } if *owner == run)
            })
    }

    pub(crate) fn hold_reply_text(&mut self, key: BuildKey, text: &str) -> Output {
        let mut out = Output::default();
        if let Some(Job::ReplyText {
            key: owner,
            text: held,
        }) = self.queue.back_mut()
            && *owner == key
        {
            held.push_str(text);
        } else {
            self.enqueue(
                Job::ReplyText {
                    key,
                    text: text.into(),
                },
                &mut out,
            );
        }
        out
    }

    pub(crate) fn writing_call(&self, serial: u64) -> Option<&CallId> {
        match self.pending.as_ref() {
            Some(Pending {
                serial: owner,
                wait: Wait::Step(step),
                ..
            }) if *owner == serial => Some(&step.call),
            _ => None,
        }
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
        // The ask becomes stored turn text every later model reads, so only public scenes.
        if held.author != Author::Public {
            return None;
        }
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
                Job::ReplyText { key, text } => {
                    out.effects.push(Effect::DrawingReplyText { key, text });
                }
                Job::Record { run, call, record } => self.record(run, &call, *record, out),
                Job::Reject { run, call, error } => {
                    if let Some(at) = self.open_call(run, &call) {
                        self.fail(at, error, out);
                    }
                }
                Job::Finish { run, call, error } => self.finish(run, &call, error, out),
                Job::Cut { scene, cut, kind } => self.cut(scene, cut, kind, out),
                Job::Shown { scene, index } => self.shown(scene, index, out),
                Job::Ready { scene, index } => self.answered(scene, index, out),
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
            .take_if(|pending| {
                pending.in_flight
                    && pending.serial == serial
                    && !matches!(pending.wait, Wait::Prepare { .. })
            })
            .ok_or(Stale)?;
        match (pending.wait, reply) {
            (Wait::Lookup { run, call, begin }, Ok(Reply::Lookup(found))) => {
                self.looked_up(run, &call, *begin, found, out);
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
            (Wait::Prepare { .. }, _) => unreachable!("a preparation is never a write"),
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
            Self::CheckedCall { .. } => "CheckedCall",
            Self::Record { .. } => "Record",
            Self::CallFinished { .. } => "CallFinished",
            Self::StopScenes { .. } => "StopScenes",
            Self::StepShown { .. } => "StepShown",
            Self::StepReady { .. } => "StepReady",
            Self::ReadyWait => "ReadyWait",
            Self::StepFailed { .. } => "StepFailed",
            Self::Clear { .. } => "Clear",
            Self::SceneFailed { .. } => "SceneFailed",
            Self::SceneSkipped { .. } => "SceneSkipped",
            Self::ClearVisible => "ClearVisible",
            Self::ScreenOff => "ScreenOff",
            Self::RunClosed { .. } => "RunClosed",
            Self::ThreadDeleted { .. } => "ThreadDeleted",
            Self::Written { .. } => "Written",
            Self::Prepared { .. } => "Prepared",
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
