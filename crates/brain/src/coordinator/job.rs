//! The background job slot: one model request at a time outside any run, which a run ending
//! `Done` starts and any new run cancels (docs/development/engines.md, "Background jobs").
//!
//! A task's run starts a title job, and when that ends on its own a compaction job for the same
//! thread; Kumo's run starts compaction only. A job reads like a build: on the page reader,
//! after every op sent before it, with its own exposure to deletes, so a delete of what it read
//! voids it. It asks to send under its own [`SendKey::Job`], so a delete of what it read is held
//! while its body goes out, and its write is conditional on the deletion generation it read. It
//! answers on the route of the run that started it: a `ChatGPT` job only while an account is
//! signed in, which a sign-out cancels before anything else, and another connection's only while
//! that connection is ready under the run's sign-in. A job is never resumed or retried on its
//! own: the next trigger starts a new one.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use kumochi_protocol::message::{ConnectionKind, EventId, RunId, ThreadId, ThreadTitle};

use super::admission::origin;
use super::{BuildKey, Coordinator, Effect, SendKey, Stale, StoreOp, SummaryText, Unbuilt};
use crate::context::compact::COMPACT_AT;
use crate::context::{Built, Origin, Route, RunSnapshot};
use crate::engines::RunRequest;
use crate::store::{Generation, TitleSource};

/// One background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobKey {
    /// The thread it's for.
    pub thread: ThreadId,
    /// A serial from the build attempts' counter, so no key repeats.
    pub serial: u64,
}

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Titles a task from its first ask.
    Title,
    /// Summarizes a thread's oldest runs.
    Compact,
}

/// A job's read, for the page reader.
/// Equality compares request data, not its execution-time cancellation capability.
#[derive(Clone, Debug)]
pub struct ReadJob {
    pub(crate) control: crate::recall::control::Control,
    pub(crate) key: JobKey,
    pub(crate) kind: JobKind,
    pub(crate) origin: Origin,
    /// The names and default model now; the runtime fills it in, and the read takes the
    /// thread's own model choice into it.
    pub(crate) snapshot: Option<RunSnapshot>,
    /// The uncovered backlog, in estimated tokens, that compaction waits for; the runtime
    /// fills it in.
    pub(crate) compact_at: u64,
}

impl PartialEq for ReadJob {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.kind == other.kind
            && self.origin == other.origin
            && self.snapshot == other.snapshot
            && self.compact_at == other.compact_at
    }
}
impl Eq for ReadJob {}

/// A job's read, finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobBuilt {
    pub(crate) key: JobKey,
    /// The request; `Ok(None)` when there's nothing to do: the task's title isn't its first
    /// words, or the thread has nothing to compact.
    pub(crate) result: Result<Option<JobAsk>, Unbuilt>,
}

/// What a job sends, and the deletion generation it read the thread in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobAsk {
    pub(crate) generation: Generation,
    pub(crate) built: Built,
    /// The events a compaction's summary covers; `None` for a title.
    pub(crate) covers: Option<(EventId, EventId)>,
}

/// What a job's request came to, cleaned.
#[derive(Clone, PartialEq, Eq)]
pub enum JobOutput {
    /// A title.
    Title(ThreadTitle),
    /// A summary.
    Summary(String),
}

impl fmt::Debug for JobOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Title(title) => f.debug_tuple("Title").field(&title.as_str().len()).finish(),
            Self::Summary(text) => f.debug_tuple("Summary").field(&text.len()).finish(),
        }
    }
}

/// The job in the slot.
#[derive(Debug)]
pub(super) struct Job {
    control: crate::recall::control::CancelOnDrop,
    pub(super) key: JobKey,
    kind: JobKind,
    /// The run whose end started it.
    run: RunId,
    /// That run's route, which the job answers on.
    pub(super) route: Route,
    /// Threads whose delete was pending when it began or was accepted since.
    exposed: BTreeSet<ThreadId>,
    stage: Stage,
}

#[derive(Debug)]
enum Stage {
    Reading,
    Sending {
        generation: Generation,
        sources: Vec<ThreadId>,
        covers: Option<(EventId, EventId)>,
        model: Option<String>,
    },
    Writing,
}

impl Job {
    fn voided(&self, sources: &[ThreadId]) -> bool {
        sources.iter().any(|source| self.exposed.contains(source))
    }
}

impl Coordinator {
    fn current_claude_build(&mut self, key: BuildKey) -> Option<&mut super::admission::Build> {
        let run = self.current.as_mut().filter(|run| {
            run.started()
                && !run.finishing
                && run
                    .route
                    .is_some_and(|route| route.kind == ConnectionKind::ClaudeCode)
        })?;
        run.build
            .as_mut()
            .filter(|build| build.key() == key && build.admitted())
    }

    pub(crate) fn claude_provider_observed(&mut self, key: BuildKey) {
        if let Some(build) = self.current_claude_build(key) {
            build.provider_failed = true;
        }
    }

    pub(crate) fn claude_recheck_control(
        &mut self,
        key: SendKey,
    ) -> Option<crate::recall::control::Control> {
        match key {
            SendKey::Build(key) => self
                .current_claude_build(key)
                .map(|build| build.control.0.clone()),
            SendKey::Job(key) => {
                let job = self.job.as_ref()?;
                (job.key == key
                    && matches!(job.stage, Stage::Sending { .. })
                    && job.route.kind == ConnectionKind::ClaudeCode)
                    .then(|| job.control.0.clone())
            }
        }
    }

    pub(super) fn key_job_route(&self, key: JobKey, request: &RunRequest) -> Option<Route> {
        self.job
            .as_ref()
            .filter(|job| {
                job.key == key
                    && job.run == request.run
                    && key.thread == request.thread
                    && matches!(job.stage, Stage::Sending { .. })
            })
            .map(|job| job.route)
    }

    /// `run` in `thread` ended `Done` on `route`: a task's title job starts, or Kumo's compaction.
    pub(super) fn after_run(
        &mut self,
        thread: ThreadId,
        run: RunId,
        route: Option<Route>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(route) = route else {
            return;
        };
        let kind = if thread == self.main_thread {
            JobKind::Compact
        } else {
            JobKind::Title
        };
        self.start_job(kind, thread, run, route, effects);
    }

    /// Starts `kind` for `thread` on `route` in the slot, unless a run is current, the route can't
    /// answer (for `ChatGPT`, no account is signed in) or sends failed closed. Its read stops it
    /// when there's nothing to do.
    fn start_job(
        &mut self,
        kind: JobKind,
        thread: ThreadId,
        run: RunId,
        route: Route,
        effects: &mut Vec<Effect>,
    ) {
        let ready = match route.kind {
            ConnectionKind::ChatGpt => self.account.is_some(),
            ConnectionKind::AnthropicKey | ConnectionKind::ClaudeCode => {
                self.refusal(route).is_none()
            }
        };
        if !ready || self.current.is_some() || self.transport_failed {
            return;
        }
        self.cancel_job(effects);
        self.attempts += 1;
        let key = JobKey {
            thread,
            serial: self.attempts,
        };
        let control =
            crate::recall::control::Control::new(std::time::Instant::now() + super::WALL_CLOCK);
        self.job = Some(Job {
            control: crate::recall::control::CancelOnDrop(control.clone()),
            key,
            kind,
            run,
            route,
            exposed: self.pending_deletes(),
            stage: Stage::Reading,
        });
        effects.push(Effect::Store(StoreOp::ReadJob(ReadJob {
            control,
            key,
            kind,
            origin: origin(thread, self.main_thread),
            snapshot: None,
            compact_at: COMPACT_AT,
        })));
    }

    /// The job in the slot wrote its output or found nothing to do: a title job hands over to its
    /// thread's compaction, unless a delete reached that thread.
    fn job_ended(&mut self, effects: &mut Vec<Effect>) {
        let Some(job) = self.job.take() else {
            return;
        };
        if job.kind == JobKind::Title && !job.exposed.contains(&job.key.thread) {
            self.start_job(
                JobKind::Compact,
                job.key.thread,
                job.run,
                job.route,
                effects,
            );
        }
    }

    /// Empties the slot; the runtime drops the job's request, whose lease settles on its own.
    pub(super) fn cancel_job(&mut self, effects: &mut Vec<Effect>) {
        if let Some(job) = self.job.take() {
            effects.push(Effect::CancelJob(job.key));
        }
    }

    /// Empties the slot if its job answers on `kind`.
    pub(super) fn cancel_job_on(&mut self, kind: ConnectionKind, effects: &mut Vec<Effect>) {
        if self.job.as_ref().is_some_and(|job| job.route.kind == kind) {
            self.cancel_job(effects);
        }
    }

    /// Empties the slot if `key` holds it.
    pub(super) fn cancel_job_if(&mut self, key: JobKey, effects: &mut Vec<Effect>) {
        if self.job.as_ref().is_some_and(|job| job.key == key) {
            self.cancel_job(effects);
        }
    }

    /// A delete was accepted: the job may have read its thread.
    pub(super) fn expose_job(&mut self, thread: ThreadId) {
        if let Some(job) = &mut self.job {
            job.exposed.insert(thread);
        }
    }

    /// The job's read finished: its request starts, unless there's nothing to do, the read
    /// failed, the read's choice names another connection than the job's route or a delete
    /// reached what it read.
    pub(super) fn job_built(
        &mut self,
        done: JobBuilt,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let job = self
            .job
            .as_ref()
            .filter(|job| job.key == done.key && matches!(job.stage, Stage::Reading))
            .ok_or(Stale)?;
        let Ok(Some(mut ask)) = done.result else {
            self.job_ended(effects);
            return Ok(());
        };
        let elsewhere = ask
            .built
            .request
            .choice
            .as_ref()
            .map_or(ConnectionKind::ChatGpt, |choice| choice.connection)
            != job.route.kind;
        if elsewhere || job.voided(&ask.built.sources) {
            self.job_ended(effects);
            return Ok(());
        }
        let route = job.route;
        if route.kind == ConnectionKind::ChatGpt && !self.trusted_draw() {
            let Some(effort) =
                self.resolve_chatgpt_effort(route, ask.built.request.choice.as_ref())
            else {
                self.job_ended(effects);
                return Ok(());
            };
            if let Some(choice) = &mut ask.built.request.choice {
                choice.effort = effort;
            }
        }
        let job = self.job.as_mut().expect("the reading job was just matched");
        let (key, kind, run) = (job.key, job.kind, job.run);
        let origin = origin(key.thread, self.main_thread);
        job.stage = Stage::Sending {
            generation: ask.generation,
            sources: ask.built.sources,
            covers: ask.covers,
            model: ask
                .built
                .request
                .choice
                .as_ref()
                .map(|choice| choice.model.as_str().to_owned()),
        };
        effects.push(Effect::StartJob {
            job: key,
            kind,
            request: RunRequest {
                key: SendKey::Job(key),
                run,
                thread: key.thread,
                origin,
                request: ask.built.request,
            },
        });
        Ok(())
    }

    /// The job's request is about to send. It's granted only for the job in the slot, with
    /// nothing it read touched by a delete and its route still able to answer, as a run's send is;
    /// otherwise its body is dropped unsent and the job ends.
    pub(super) fn job_ready(
        &mut self,
        key: JobKey,
        hold: Option<Duration>,
        effects: &mut Vec<Effect>,
    ) {
        let sources = self
            .job
            .as_ref()
            .filter(|job| job.key == key)
            .filter(|job| self.refusal(job.route).is_none())
            .and_then(|job| match &job.stage {
                Stage::Sending { sources, model, .. }
                    if !job.voided(sources) && self.offered(job.route, model.as_deref()) =>
                {
                    Some(sources.clone())
                }
                _ => None,
            });
        match sources {
            Some(sources) if !self.transport_failed => {
                self.grant(SendKey::Job(key), sources, hold, None, effects);
            }
            _ => {
                effects.push(Effect::Rebuild(SendKey::Job(key)));
                self.cancel_job_if(key, effects);
            }
        }
    }

    /// The job's request ended with `output`, or failed. A title or summary is written,
    /// conditional on the generation the job read, unless a delete reached what it read.
    pub(super) fn job_done(
        &mut self,
        key: JobKey,
        output: Option<JobOutput>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let job = self
            .job
            .as_mut()
            .filter(|job| job.key == key)
            .ok_or(Stale)?;
        let voided = match &job.stage {
            Stage::Sending { sources, .. } => job.voided(sources),
            Stage::Reading | Stage::Writing => return Err(Stale),
        };
        let Stage::Sending {
            generation,
            covers,
            model,
            ..
        } = &mut job.stage
        else {
            return Err(Stale);
        };
        let write = match (output, *covers) {
            _ if voided => None,
            (Some(JobOutput::Title(title)), None) => Some(StoreOp::SetTitle {
                thread: key.thread,
                title,
                source: TitleSource::Model(*generation),
                job: Some(key),
            }),
            (Some(JobOutput::Summary(text)), Some(covers)) => Some(StoreOp::StoreSummary {
                owner: SendKey::Job(key),
                thread: key.thread,
                generation: *generation,
                covers,
                text: SummaryText(text),
                model: model.take(),
            }),
            _ => None,
        };
        let Some(write) = write else {
            // A request went out and wrote nothing, so nothing follows it (OSC-213): after a
            // usage-limit answer, no compaction request goes out until the next `Done`.
            self.job = None;
            return Ok(());
        };
        job.stage = Stage::Writing;
        effects.push(Effect::Store(write));
        Ok(())
    }

    /// A title or summary write by `job` finished: only the slot's own write ends the job.
    pub(super) fn job_written(&mut self, job: Option<JobKey>, effects: &mut Vec<Effect>) {
        if job.is_some_and(|key| {
            self.job
                .as_ref()
                .is_some_and(|held| held.key == key && matches!(held.stage, Stage::Writing))
        }) {
            self.job_ended(effects);
        }
    }

    /// An account is signed in as `generation`; one at or under a sign-out already accepted
    /// changes nothing. Another account's `ChatGPT` job ends.
    pub(super) fn signed_in(
        &mut self,
        generation: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if self.signed_out.is_some_and(|out| generation <= out) {
            return Err(Stale);
        }
        if self.account != Some(generation) {
            self.cancel_job_on(ConnectionKind::ChatGpt, effects);
        }
        self.account = Some(generation);
        Ok(())
    }

    /// The account signed in as `generation` signed out: no `ChatGPT` job runs or writes from
    /// here, and none starts until an account signs in. An older generation's sign-out changes
    /// nothing.
    pub(super) fn signed_out(
        &mut self,
        generation: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if self.account != Some(generation) {
            return Err(Stale);
        }
        self.signed_out = Some(generation);
        self.account = None;
        self.cancel_job_on(ConnectionKind::ChatGpt, effects);
        Ok(())
    }
}
