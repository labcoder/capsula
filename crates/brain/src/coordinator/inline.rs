//! Inline compaction (B5): a run whose current turn fits without its thread's summary but not
//! with it rewrites the summary shorter, under the run, before it builds again
//! (docs/development/engines.md, "Compaction").
//!
//! It's an ordinary build attempt of the run's round with a fresh key: a read that returns its
//! sources, admission against deletes, a send lease under [`SendKey::Build`], then a summary
//! write owned by that key and a fresh normal build. The run's deadline and Stop cover it, and
//! after either, every late answer of it is stale. A run compacts at most once until a build is
//! admitted, so a delete's rebuild can't start a second one.

use std::fmt;

use kumochi_protocol::message::EventId;

use super::admission::{Build, BuildKey, origin};
use super::{Coordinator, Effect, JobAsk, SendKey, Stale, StoreOp, SummaryText, Unbuilt};
use crate::context::RunSnapshot;
use crate::context::compact::fit_summary;
use crate::engines::{Finish, RunRequest, TurnEvent};
use crate::store::{Failure, Generation};

/// Inline compaction's read, for the page reader.
/// Equality compares request data, not its execution-time cancellation capability.
#[derive(Clone, Debug)]
pub struct CompactContext {
    pub(crate) control: crate::recall::control::Control,
    pub(crate) key: BuildKey,
    pub(crate) thread: kumochi_protocol::message::ThreadId,
    /// What the run read when its first build began.
    pub(crate) snapshot: Option<RunSnapshot>,
    /// The bytes the bound leaves for the summary after the current turn.
    pub(crate) room: usize,
}

impl PartialEq for CompactContext {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.thread == other.thread
            && self.snapshot == other.snapshot
            && self.room == other.room
    }
}
impl Eq for CompactContext {}

/// Inline compaction's read, finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactBuilt {
    pub(crate) key: BuildKey,
    /// The request and the escaped bytes its summary's text may take; `Ok(None)` when there's no
    /// summary to rewrite or no room for one.
    pub(crate) result: Result<Option<(JobAsk, usize)>, Unbuilt>,
}

/// Where an inline compaction is.
pub(super) enum Inline {
    Reading,
    Sending {
        generation: Generation,
        covers: (EventId, EventId),
        allowance: usize,
        model: Option<String>,
        text: String,
    },
    Writing,
}

impl fmt::Debug for Inline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reading => f.write_str("Reading"),
            Self::Sending {
                generation,
                covers,
                allowance,
                model,
                text,
            } => f
                .debug_struct("Sending")
                .field("generation", generation)
                .field("covers", covers)
                .field("allowance", allowance)
                .field("model", model)
                .field("text_bytes", &text.len())
                .finish(),
            Self::Writing => f.write_str("Writing"),
        }
    }
}

impl Coordinator {
    /// The current run's build `key` was too large with its summary but leaves `room` without
    /// it: the run compacts inline, unless it already did since its last admitted build.
    pub(super) fn compact_inline(
        &mut self,
        key: BuildKey,
        room: usize,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        self.attempts += 1;
        let attempt = self.attempts;
        let exposed = self.pending_deletes();
        let current = self.current(key.run)?;
        if current.compacted == Some(key.round) {
            return self.fail(key.run, Failure::Budget, effects);
        }
        current.compacted = Some(key.round);
        let key = BuildKey { attempt, ..key };
        let control = current.control.0.child();
        current.build = Some(Build::new(
            key,
            exposed,
            Some(Inline::Reading),
            control.clone(),
        ));
        effects.push(Effect::Store(StoreOp::CompactContext(CompactContext {
            control,
            key,
            thread: current.thread,
            snapshot: current.effective_snapshot(),
            room,
        })));
        Ok(())
    }

    /// Inline compaction's read finished. A request that read a thread a delete reached is
    /// dropped and built again as a normal build; otherwise it's admitted and sent. With nothing
    /// to rewrite, the run fails on its budget.
    pub(super) fn compact_built(
        &mut self,
        done: CompactBuilt,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let CompactBuilt { key, result } = done;
        let pending = self.pending_deletes();
        let main = self.main_thread;
        let current = self.current(key.run)?;
        let thread = current.thread;
        let build = current
            .build
            .as_mut()
            .filter(|build| build.key() == key && matches!(build.inline, Some(Inline::Reading)))
            .ok_or(Stale)?;
        let (ask, allowance) = match result {
            Ok(Some(found)) => found,
            Ok(None) | Err(Unbuilt::TooLarge { .. }) => {
                return self.fail(key.run, Failure::Budget, effects);
            }
            Err(Unbuilt::Failed) => return self.fail(key.run, Failure::Store, effects),
        };
        let Some(covers) = ask.covers else {
            return self.fail(key.run, Failure::Store, effects);
        };
        let voided = build.voided(&ask.built.sources);
        if !voided.is_empty() {
            self.rebuild(key, &voided, &pending, effects);
            return Ok(());
        }
        build.admit(ask.built.sources);
        build.inline = Some(Inline::Sending {
            generation: ask.generation,
            covers,
            allowance,
            model: ask
                .built
                .request
                .choice
                .as_ref()
                .map(|choice| choice.model.as_str().to_owned()),
            text: String::new(),
        });
        effects.push(Effect::StartEngine {
            request: RunRequest {
                key: SendKey::Build(key),
                run: key.run,
                thread,
                origin: origin(thread, main),
                request: ask.built.request,
            },
        });
        Ok(())
    }

    /// An event of the current run's inline compaction round: its text is collected, never
    /// shown, and a completed round writes the summary. Usage is ignored; anything else fails the
    /// run.
    pub(super) fn compact_event(
        &mut self,
        run: kumochi_protocol::message::RunId,
        event: TurnEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let current = self.current(run)?;
        let thread = current.thread;
        let Some(build) = current.build.as_mut() else {
            return Err(Stale);
        };
        let key = build.key();
        let Some(Inline::Sending {
            generation,
            covers,
            allowance,
            model,
            text,
        }) = &mut build.inline
        else {
            return Err(Stale);
        };
        match event {
            TurnEvent::TextDelta(delta)
                if text.len() + delta.len() <= crate::jobs::MAX_SUMMARY_OUTPUT =>
            {
                text.push_str(&delta);
            }
            TurnEvent::OutputItem { .. } | TurnEvent::KeyCall { .. } | TurnEvent::Usage(_) => {}
            TurnEvent::Finished(Finish::Completed) => {
                let Some(summary) = fit_summary(text, *allowance) else {
                    return self.fail(run, Failure::Budget, effects);
                };
                let write = StoreOp::StoreSummary {
                    owner: SendKey::Build(key),
                    thread,
                    generation: *generation,
                    covers: *covers,
                    text: SummaryText(summary),
                    model: model.take(),
                };
                build.inline = Some(Inline::Writing);
                effects.push(Effect::Store(write));
            }
            TurnEvent::TextDelta(_) => return self.fail(run, Failure::Budget, effects),
            _ => return self.fail(run, Failure::Engine, effects),
        }
        Ok(())
    }

    /// The summary build `key` wrote is stored, or wasn't: the run builds again normally. Only
    /// the current run's own inline write counts.
    pub(super) fn inline_written(
        &mut self,
        key: BuildKey,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let current = self.current(key.run)?;
        if !current.build.as_ref().is_some_and(|build| {
            build.key() == key && matches!(build.inline, Some(Inline::Writing))
        }) {
            return Err(Stale);
        }
        self.build(key.run, key.round, effects);
        Ok(())
    }
}
