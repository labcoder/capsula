//! Admitting a round's context. Every round's request is built from the store after every op
//! sent to it before, and a build that rendered a thread with a delete pending or accepted since
//! it began is thrown away, so text read before a delete is never sent after it commits
//! (docs/development/engines.md, "Builds and deletes").

use std::collections::BTreeSet;

use kumochi_protocol::message::{ConnectionKind, RunId, ThreadId};

use super::inline::Inline;
use super::{Coordinator, Effect, SendKey, Stale, StoreOp};
use crate::context::{Built, ConnectionTag, Origin, RunSnapshot};
use crate::engines::anthropic::live::Motion;
use crate::engines::{DRAW, RunRequest, ToolName};
use crate::store::Failure;

/// One build attempt of one round of a run. A rebuild gets a new attempt, so a late result of
/// an older one is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildKey {
    /// The run.
    pub run: RunId,
    /// The model round, from 1.
    pub round: u32,
    /// A serial across every build, never reused.
    pub attempt: u64,
}

/// A build for the page reader, after every op sent to the store before it.
/// Equality compares request data, not its execution-time cancellation capability.
#[derive(Clone, Debug)]
pub struct BuildContext {
    pub(crate) control: crate::recall::control::Control,
    /// The pipeline-assigned identity of the context being built.
    pub key: BuildKey,
    pub(crate) thread: ThreadId,
    pub(crate) origin: Origin,
    /// What the run read when it started. The runtime fills it in for the run's first build from
    /// the settings, and that build reads the thread's model choice into it.
    pub(crate) snapshot: Option<RunSnapshot>,
    /// The run's first build.
    pub(crate) first: bool,
    /// The tag each kind had when the run's turn arrived; the first build takes its choice's.
    pub(crate) arrived: super::Arrived,
    /// The thread's drawings on screen when the build began, headers and objects together.
    pub(crate) drawings: crate::drawing::Digest,
}

impl PartialEq for BuildContext {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.thread == other.thread
            && self.origin == other.origin
            && self.snapshot == other.snapshot
            && self.first == other.first
            && self.arrived == other.arrived
            && self.drawings == other.drawings
    }
}
impl Eq for BuildContext {}

/// A finished build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextBuilt {
    pub(crate) key: BuildKey,
    /// What the build ran with; `None` when it never reached the store.
    pub(crate) snapshot: Option<RunSnapshot>,
    pub(crate) result: Result<Built, Unbuilt>,
}

#[cfg(feature = "test-support")]
impl ContextBuilt {
    /// A clean result for `build` that rendered only its own thread, for tests in other crates.
    pub fn own_thread(build: &BuildContext) -> Self {
        Self {
            key: build.key,
            snapshot: None,
            result: Ok(Built {
                request: crate::context::ModelRequest {
                    choice: None,
                    instructions: String::new(),
                    input: Vec::new(),
                    tools: Vec::new(),
                },
                sources: vec![build.thread],
            }),
        }
    }
}

/// Why a build made no request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unbuilt {
    /// Over the request's bound. `room` is what the bound leaves for the thread's summary after
    /// the current turn, when the turn fits without it.
    TooLarge {
        /// Bytes left for a summary; `None` when the current turn alone doesn't fit.
        room: Option<usize>,
    },
    /// The read failed, or the stored thread couldn't be built.
    Failed,
}

/// The current run's build.
#[derive(Debug)]
pub(super) struct Build {
    pub(super) control: crate::recall::control::CancelOnDrop,
    key: BuildKey,
    /// Threads whose delete was pending when the build began or was accepted since. Kept
    /// through the deletes' answers, so one answered before the result arrives still voids it.
    exposed: BTreeSet<ThreadId>,
    /// A voided build's rebuild waits until these deletes are answered.
    blocked: BTreeSet<ThreadId>,
    admitted: bool,
    pub(super) provider_failed: bool,
    /// The admitted request offered the draw tool, or the dev fixture answers (D13-H2).
    offers_draw: bool,
    /// The dispatcher's tools the admitted request offered.
    offered: Vec<ToolName>,
    /// The admitted request's sources, checked again before each send.
    sources: Vec<ThreadId>,
    /// An inline compaction's progress; `None` for a normal build.
    pub(super) inline: Option<Inline>,
}

impl Build {
    pub(super) fn new(
        key: BuildKey,
        exposed: BTreeSet<ThreadId>,
        inline: Option<Inline>,
        control: crate::recall::control::Control,
    ) -> Self {
        Self {
            control: crate::recall::control::CancelOnDrop(control),
            key,
            exposed,
            blocked: BTreeSet::new(),
            admitted: false,
            provider_failed: false,
            offers_draw: false,
            offered: Vec::new(),
            sources: Vec::new(),
            inline,
        }
    }

    /// Admits the request that carries `sources`.
    pub(super) fn admit(&mut self, sources: Vec<ThreadId>) {
        self.admitted = true;
        self.sources = sources;
    }

    pub(super) fn admitted(&self) -> bool {
        self.admitted
    }

    pub(super) fn offers_draw(&self) -> bool {
        self.offers_draw
    }

    pub(super) fn offered(&self) -> &[ToolName] {
        &self.offered
    }

    pub(super) fn key(&self) -> BuildKey {
        self.key
    }

    pub(super) fn sources(&self) -> &[ThreadId] {
        &self.sources
    }

    /// The `sources` a delete reached. Every pending delete is exposed, since exposure starts
    /// from the deletes pending when the build began and adds each one accepted after.
    pub(super) fn voided(&self, sources: &[ThreadId]) -> BTreeSet<ThreadId> {
        sources
            .iter()
            .copied()
            .filter(|source| self.exposed.contains(source))
            .collect()
    }
}

impl Coordinator {
    pub(crate) fn key_motion(&self, request: &RunRequest) -> Option<(ConnectionTag, Motion)> {
        let (route, motion) = match request.key {
            SendKey::Build(key) => {
                let run = self.current.as_ref()?;
                let build = run.build.as_ref()?;
                if run.id != request.run
                    || run.thread != request.thread
                    || build.key != key
                    || !build.admitted
                {
                    return None;
                }
                let motion = if build.inline.is_some() {
                    Motion::Standalone
                } else if key.round == 1 {
                    Motion::First
                } else {
                    Motion::Continue
                };
                (run.route?, motion)
            }
            SendKey::Job(key) => (self.key_job_route(key, request)?, Motion::Standalone),
        };
        (route.kind == ConnectionKind::AnthropicKey && self.refusal(route).is_none())
            .then_some((route.tag, motion))
    }

    /// Starts a new build attempt of the current run's `round`.
    pub(super) fn build(&mut self, run: RunId, round: u32, effects: &mut Vec<Effect>) {
        self.attempts += 1;
        let key = BuildKey {
            run,
            round,
            attempt: self.attempts,
        };
        let exposed = self.pending_deletes();
        let main = self.main_thread;
        self.revoke_mcp(run, effects);
        let Ok(current) = self.current(run) else {
            return;
        };
        let control = current.control.0.child();
        current.build = Some(Build::new(key, exposed, None, control.clone()));
        let (thread, snapshot, arrived) = (
            current.thread,
            current.effective_snapshot(),
            current.arrived,
        );
        effects.push(Effect::Store(StoreOp::BuildContext(BuildContext {
            control,
            key,
            thread,
            origin: origin(thread, main),
            first: snapshot.is_none(),
            arrived,
            snapshot,
            drawings: self.drawing.digest(thread),
        })));
    }

    /// A delete was accepted: the current build may have read its thread.
    pub(super) fn expose(&mut self, thread: ThreadId) {
        self.expose_job(thread);
        if let Some(build) = self.current.as_mut().and_then(|run| run.build.as_mut()) {
            build.exposed.insert(thread);
        }
    }

    /// A delete was answered: a build waiting on deletes starts again once none it waits on is
    /// pending.
    pub(super) fn unblock(&mut self, effects: &mut Vec<Effect>) {
        let pending = self.pending_deletes();
        let ready = self.current.as_ref().and_then(|run| {
            let build = run.build.as_ref()?;
            let ready = !build.blocked.is_empty() && build.blocked.is_disjoint(&pending);
            ready.then_some(build.key)
        });
        if let Some(key) = ready {
            self.build(key.run, key.round, effects);
        }
    }

    /// A build finished. Only the current attempt counts. A request that rendered a thread
    /// exposed to a delete is dropped and built again, once the delete is answered when it's
    /// still pending; otherwise the engine starts with it.
    pub(super) fn context_built(
        &mut self,
        done: ContextBuilt,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let ContextBuilt {
            key,
            snapshot,
            result,
        } = done;
        let current = self.current(key.run)?;
        if !current.build.as_ref().is_some_and(|build| {
            build.key == key
                && !build.admitted
                && build.blocked.is_empty()
                && build.inline.is_none()
        }) {
            return Err(Stale);
        }
        let first = current.snapshot.is_none() || (current.hold.is_some() && key.round == 1);
        if let Some(snapshot) = snapshot {
            current.snapshot.get_or_insert(snapshot);
        }
        let made = match result {
            Ok(made) => Ok(made),
            Err(Unbuilt::TooLarge { room: Some(room) }) => Err(room),
            Err(Unbuilt::TooLarge { room: None }) => {
                return self.fail(key.run, Failure::Budget, effects);
            }
            Err(Unbuilt::Failed) => return self.fail(key.run, Failure::Store, effects),
        };
        let choice = current
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.choice.clone());
        let (bound, arrived, pinned) = (current.route, current.arrived, current.effective_effort);
        let route = match bound.map_or_else(|| self.route_for(arrived, choice.as_ref()), Ok) {
            Ok(route) => route,
            Err(_) if self.waits_for_claude_code(key.run, choice.as_ref(), effects) => {
                return Ok(());
            }
            Err(reason) => return self.fail_with(key.run, Failure::Engine, reason, effects),
        };
        let refused = self.refusal(route).or_else(|| {
            first
                .then(|| self.unlisted(route.kind, choice.as_ref()))
                .flatten()
        });
        if let Some(reason) = refused {
            return self.fail_with(key.run, Failure::Engine, reason, effects);
        }
        let effective =
            if bound.is_none() && route.kind == ConnectionKind::ChatGpt && !self.trusted_draw() {
                let Some(effort) = self.resolve_chatgpt_effort(route, choice.as_ref()) else {
                    return self.fail_with(
                        key.run,
                        Failure::Engine,
                        super::availability::model_unavailable(),
                        effects,
                    );
                };
                Some(effort)
            } else {
                pinned
            };
        let current = self.current(key.run)?;
        current.effective_effort = effective;
        current.route = Some(route);
        current.finding = None;
        let made = match made {
            Ok(made) => made,
            Err(room) => return self.compact_inline(key, room, effects),
        };
        self.admit_built(key, made, effects)
    }

    fn admit_built(
        &mut self,
        key: BuildKey,
        made: Built,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let pending = self.pending_deletes();
        let main = self.main_thread;
        let offers_draw =
            made.request.tools.iter().any(|tool| tool.name == DRAW) || self.trusted_draw();
        let current = self.current(key.run)?;
        let thread = current.thread;
        let mut request = made.request;
        if let (Some(choice), Some(effort)) = (&mut request.choice, current.effective_effort) {
            choice.effort = effort;
        }
        // shortcut: the screen's developer line isn't counted in BOUND, so a request built within
        // about 60 bytes of it is refused at send (failing safe); count it if that's ever hit.
        if current
            .screen
            .as_ref()
            .is_some_and(|screen| request.add_screen(screen).is_err())
        {
            let reason = kumochi_protocol::message::ErrorKey::ScreenUnavailable.into();
            return self.fail_with(key.run, Failure::Engine, reason, effects);
        }
        let current = self.current(key.run)?;
        // A key run's later round goes on from the request already sent, so it ends rather than
        // send again a source that has gone since, such as a deleted task.
        if key.round > 1
            && current
                .route
                .is_some_and(|route| route.kind == ConnectionKind::AnthropicKey)
            && current
                .sent
                .iter()
                .any(|source| !made.sources.contains(source))
        {
            return self.fail(key.run, Failure::Engine, effects);
        }
        let build = current.build.as_mut().expect("the build was just matched");
        build.offers_draw = offers_draw;
        build.offered = request
            .tools
            .iter()
            .filter_map(|tool| crate::dispatch::named(tool.name))
            .collect();
        let voided = build.voided(&made.sources);
        if !voided.is_empty() {
            self.rebuild(key, &voided, &pending, effects);
            return Ok(());
        }
        build.admit(made.sources.clone());
        if key.round == 1 {
            current.sent.clear();
        }
        current.sent.extend(made.sources);
        current.compacted = None;
        current.tell_screen_unless_image(effects);
        effects.push(Effect::StartEngine {
            request: RunRequest {
                key: SendKey::Build(key),
                run: key.run,
                thread,
                origin: origin(thread, main),
                request,
            },
        });
        Ok(())
    }

    /// Drops the current build, whose request a delete reached. Its rebuild waits for the
    /// answers of the deletes still pending among them, so its read sees them; with none, it
    /// starts now.
    pub(super) fn rebuild(
        &mut self,
        key: BuildKey,
        voided: &BTreeSet<ThreadId>,
        pending: &BTreeSet<ThreadId>,
        effects: &mut Vec<Effect>,
    ) {
        self.revoke_mcp(key.run, effects);
        self.close_provider_round(key.run, effects);
        let Some(current) = self.current.as_mut() else {
            return;
        };
        current.tools.drop_round();
        current.finishing = false;
        let Some(build) = current.build.as_mut() else {
            return;
        };
        build.admitted = false;
        build.control.0.cancel();
        build.blocked = voided & pending;
        if build.blocked.is_empty() {
            self.build(key.run, key.round, effects);
        }
    }
}

/// A run's origin comes from its thread, never from anything a model produced.
pub(super) fn origin(thread: ThreadId, main: ThreadId) -> Origin {
    if thread == main {
        Origin::Kumo
    } else {
        Origin::Task(thread)
    }
}
