//! The background job slot: one model request at a time outside any run, which a task's run
//! ending `Done` starts and any new run cancels (docs/development/engines.md, "Background jobs").
//!
//! A job reads like a build: on the page reader, after every op sent before it, with its own
//! exposure to deletes, so a delete of what it read voids it. It asks to send under its own
//! [`SendKey::Job`], so a delete of its task is held while its body goes out, and its write is
//! conditional on the deletion generation it read. It runs only while an account is signed in,
//! and a sign-out cancels it before anything else. A job is never resumed or retried on its own:
//! the next trigger starts a new one.

use std::collections::BTreeSet;

use kumochi_protocol::message::{RunId, ThreadId, ThreadTitle};

use super::{Coordinator, Effect, SendKey, Stale, StoreOp, Unbuilt};
use crate::context::{Built, Origin, RunSnapshot};
use crate::engines::RunRequest;
use crate::store::{Generation, TitleSource};

/// One background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobKey {
    /// The task it's for.
    pub thread: ThreadId,
    /// A serial from the build attempts' counter, so no key repeats.
    pub serial: u64,
}

/// A title job's read, for the page reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadTitle {
    pub(crate) key: JobKey,
    /// The names and default model now; the runtime fills it in, and the read takes the task's
    /// own model choice into it.
    pub(crate) snapshot: Option<RunSnapshot>,
}

/// A title job's read, finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobBuilt {
    pub(crate) key: JobKey,
    /// The request; `Ok(None)` when the task's title isn't its first words any more.
    pub(crate) result: Result<Option<TitleAsk>, Unbuilt>,
}

/// What a title job sends, and the deletion generation it read the task in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TitleAsk {
    pub(crate) generation: Generation,
    pub(crate) built: Built,
}

/// The job in the slot.
#[derive(Debug)]
pub(super) struct Job {
    key: JobKey,
    /// The run whose end started it.
    run: RunId,
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
    },
    Writing,
}

impl Job {
    fn voided(&self, sources: &[ThreadId]) -> bool {
        sources.iter().any(|source| self.exposed.contains(source))
    }
}

impl Coordinator {
    /// `run` in `thread` ended `Done`: a task's title job starts, unless a run is current, no
    /// account is signed in or sends failed closed. The read stops it when the title isn't
    /// first words, so a renamed or model-titled task never sends.
    pub(super) fn title_job(&mut self, thread: ThreadId, run: RunId, effects: &mut Vec<Effect>) {
        if self.account.is_none()
            || thread == self.main_thread
            || self.current.is_some()
            || self.transport_failed
        {
            return;
        }
        self.cancel_job(effects);
        self.attempts += 1;
        let key = JobKey {
            thread,
            serial: self.attempts,
        };
        self.job = Some(Job {
            key,
            run,
            exposed: self.pending_deletes(),
            stage: Stage::Reading,
        });
        effects.push(Effect::Store(StoreOp::ReadTitle(ReadTitle {
            key,
            snapshot: None,
        })));
    }

    /// Empties the slot; the runtime drops the job's request, whose lease settles on its own.
    pub(super) fn cancel_job(&mut self, effects: &mut Vec<Effect>) {
        if let Some(job) = self.job.take() {
            effects.push(Effect::CancelJob(job.key));
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

    /// The job's read finished: its request starts, unless the title isn't first words, the
    /// read failed or a delete reached what it read.
    pub(super) fn job_built(
        &mut self,
        done: JobBuilt,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let job = self
            .job
            .as_mut()
            .filter(|job| job.key == done.key && matches!(job.stage, Stage::Reading))
            .ok_or(Stale)?;
        let Ok(Some(ask)) = done.result else {
            self.job = None;
            return Ok(());
        };
        if job.voided(&ask.built.sources) {
            self.job = None;
            return Ok(());
        }
        let (key, run) = (job.key, job.run);
        job.stage = Stage::Sending {
            generation: ask.generation,
            sources: ask.built.sources,
        };
        effects.push(Effect::StartJob {
            job: key,
            request: RunRequest {
                key: SendKey::Job(key),
                run,
                thread: key.thread,
                origin: Origin::Task(key.thread),
                request: ask.built.request,
            },
        });
        Ok(())
    }

    /// The job's request is about to send. It's granted only for the job in the slot, with
    /// nothing it read touched by a delete; otherwise its body is dropped unsent and the job ends.
    /// A job in the slot always runs under the signed-in account: an account change empties it.
    pub(super) fn job_ready(&mut self, key: JobKey, effects: &mut Vec<Effect>) {
        let sources = self
            .job
            .as_ref()
            .filter(|job| job.key == key)
            .and_then(|job| match &job.stage {
                Stage::Sending { sources, .. } if !job.voided(sources) => Some(sources.clone()),
                _ => None,
            });
        match sources {
            Some(sources) if !self.transport_failed => {
                self.grant(SendKey::Job(key), sources, effects);
            }
            _ => {
                effects.push(Effect::Rebuild(SendKey::Job(key)));
                self.cancel_job_if(key, effects);
            }
        }
    }

    /// The job's request ended with `title`, or failed. The title is written, conditional on
    /// the generation the job read, unless a delete reached the task meanwhile.
    pub(super) fn job_done(
        &mut self,
        key: JobKey,
        title: Option<ThreadTitle>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let job = self
            .job
            .as_mut()
            .filter(|job| job.key == key)
            .ok_or(Stale)?;
        let Stage::Sending {
            generation,
            sources,
        } = &job.stage
        else {
            return Err(Stale);
        };
        let generation = *generation;
        let (Some(title), false) = (title, job.voided(sources)) else {
            self.job = None;
            return Ok(());
        };
        job.stage = Stage::Writing;
        effects.push(Effect::Store(StoreOp::SetTitle {
            thread: key.thread,
            title,
            source: TitleSource::Model(generation),
            job: Some(key),
        }));
        Ok(())
    }

    /// A title write by `job` finished: only the slot's own write empties the slot.
    pub(super) fn title_written(&mut self, job: Option<JobKey>) {
        if job.is_some_and(|key| {
            self.job
                .as_ref()
                .is_some_and(|held| held.key == key && matches!(held.stage, Stage::Writing))
        }) {
            self.job = None;
        }
    }

    /// An account is signed in as `generation`; one at or under a sign-out already accepted
    /// changes nothing. Another account's job ends.
    pub(super) fn signed_in(
        &mut self,
        generation: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if self.signed_out.is_some_and(|out| generation <= out) {
            return Err(Stale);
        }
        if self.account != Some(generation) {
            self.cancel_job(effects);
        }
        self.account = Some(generation);
        Ok(())
    }

    /// The account signed in as `generation` signed out: no job runs or writes from here, and
    /// none starts until an account signs in. An older generation's sign-out changes nothing.
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
        self.cancel_job(effects);
        Ok(())
    }
}
