//! Sign-out (design §3 rule 5). Use stops at admission; then cleanup runs as an exclusive phase
//! whose steps are each safe to repeat. The marker's phase says where a restart or a Retry
//! enters: Prepare, Revoke and the checkpoint while `Revoking`, Delete and Finish once
//! `Deleting`. Only the checkpoint decides the notice, before anything is deleted.

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, Cleanup, ErrorKey};

use super::lane::Lane;
use super::sweep::Pass;
use super::{AccountState, Done, Effect, Job, Loaded, Queued, error};
use crate::connections::error::VaultError;
use crate::connections::marker::{Disposition, MarkerUpdate, Phase, SignOut};
use crate::connections::records::Revocable;

/// A sign-out, from admission or a startup load until Finish.
#[derive(Debug)]
pub(super) struct Leaving {
    step: Step,
    /// The step was refused; it waits for Retry or the next start.
    stuck: Option<Cleanup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Waiting for a load: the marker couldn't be read, so whether a sign-out is pending is
    /// unknown, or Finish ended and the registration Keep left isn't back in memory yet.
    Read,
    /// The promotion write.
    Promote(Disposition),
    /// Prepare starts once the lane's request and any revocation pass have ended.
    Settle,
    Prepare,
    Revoke,
    Checkpoint,
    /// Delete, then Finish.
    Delete,
}

impl AccountState {
    /// Admission: the attempt is cancelled and the account dropped from memory, then the
    /// promotion write makes the sign-out durable. A dispatched save of the current attempt
    /// still lands, as a disk fact Prepare adopts; the promotion queues behind it.
    pub(super) fn sign_out(&mut self, effects: &mut Vec<Effect>) {
        if !self.saving_current()
            && self
                .loaded
                .account
                .as_ref()
                .is_none_or(|account| account.expose().tokens.is_none())
        {
            return;
        }
        self.attempt += 1;
        self.admitting = None;
        self.deferred = None;
        self.moved(effects);
        if self.live.take().is_some() {
            effects.push(Effect::Close);
        }
        self.loaded.account = None;
        self.loaded.notice = true;
        // shortcut: nothing reports `PlanState::Unavailable` until PR 10 reads the catalog; then
        // that account's sign-out erases its registration (pick 2).
        let disposition = Disposition::Keep;
        self.leaving = Some(Leaving {
            step: Step::Promote(disposition),
            stuck: None,
        });
        self.step(Job::Marker(MarkerUpdate::Promote(disposition)));
    }

    /// A sign-out found at startup resumes at its persisted phase.
    pub(super) fn resume(&mut self, sign_out: SignOut) {
        let deleting = sign_out.phase == Phase::Deleting;
        self.leaving = Some(Leaving {
            step: if deleting { Step::Delete } else { Step::Settle },
            stuck: None,
        });
        if deleting {
            self.step(Job::Finish);
        }
    }

    /// A refused marker read keeps use fenced without deciding anything; Retry reads it again.
    pub(super) fn unread(&mut self, failure: &VaultError) {
        tracing::warn!(%failure, "the sign-out marker can't be read; use stays fenced");
        self.leaving = Some(Leaving {
            step: Step::Read,
            stuck: Some(Cleanup::Refused),
        });
        self.loaded = Loaded {
            notice: true,
            ..Loaded::default()
        };
    }

    /// The load a `Read` step waits on failed before reading anything; Retry runs it again.
    pub(super) fn read_refused(&mut self) {
        if let Some(leaving) = &mut self.leaving
            && leaving.step == Step::Read
        {
            leaving.stuck = Some(Cleanup::Refused);
        }
    }

    /// A sign-out is past admission and no load owns its fence: only a `Read` step waits on one.
    pub(super) fn admitted(&self) -> bool {
        self.leaving
            .as_ref()
            .is_some_and(|leaving| leaving.step != Step::Read)
    }

    /// The local cleanup's progress, while a sign-out is pending.
    pub(super) fn cleanup(&self) -> Option<Cleanup> {
        let leaving = self.leaving.as_ref()?;
        Some(leaving.stuck.unwrap_or(if self.held() {
            Cleanup::Refused
        } else {
            Cleanup::Running
        }))
    }

    /// Reruns the refused step's phase from its first step; a set held by a refused list save
    /// is saved again first.
    pub(super) fn retry_sign_out(&mut self) {
        if self.held() {
            return self.retry();
        }
        let Some(leaving) = &mut self.leaving else {
            return;
        };
        if leaving.stuck.take().is_none() {
            return;
        }
        match leaving.step {
            Step::Read => self.reload(),
            Step::Promote(disposition) => {
                self.step(Job::Marker(MarkerUpdate::Promote(disposition)));
            }
            Step::Delete => self.step(Job::Finish),
            Step::Settle | Step::Prepare | Step::Revoke | Step::Checkpoint => {}
        }
    }

    /// Starts Prepare once nothing outstanding can still change the list.
    pub(super) fn settle(&mut self) {
        if let Some(leaving) = &mut self.leaving
            && leaving.step == Step::Settle
            && leaving.stuck.is_none()
            && matches!(self.lane, Lane::Free)
            && self.pass == Pass::Idle
        {
            leaving.step = Step::Prepare;
            self.step(Job::Prepare);
        }
    }

    /// The sign-out's own revocation run ended.
    pub(super) fn revoked_for_sign_out(&mut self, confirmed: Vec<Secret<Revocable>>) {
        let Some(leaving) = self
            .leaving
            .as_mut()
            .filter(|leaving| leaving.step == Step::Revoke)
        else {
            debug_assert!(false, "a revocation run ended with nothing revoking");
            return;
        };
        leaving.step = Step::Checkpoint;
        self.step(Job::Checkpoint(confirmed));
    }

    pub(super) fn left(&mut self, done: Done, effects: &mut Vec<Effect>) {
        let Some(leaving) = &mut self.leaving else {
            debug_assert!(false, "a sign-out step ended with no sign-out");
            return;
        };
        let refused = match done {
            Done::Marked(Ok(())) => {
                leaving.step = Step::Settle;
                return;
            }
            Done::Prepared(Ok(entries)) if entries.is_empty() => {
                leaving.step = Step::Checkpoint;
                return self.step(Job::Checkpoint(Vec::new()));
            }
            Done::Prepared(Ok(entries)) => {
                leaving.step = Step::Revoke;
                return effects.push(Effect::Revoke(entries));
            }
            Done::Checkpointed(Ok(notice)) => {
                leaving.step = Step::Delete;
                self.loaded.notice = notice;
                return self.step(Job::Finish);
            }
            // Use stays fenced until the reload brings back the registration Keep left, which
            // the next sign-in reuses.
            Done::SignedOut(Ok(())) => {
                leaving.step = Step::Read;
                return self.reload();
            }
            Done::Marked(Err(failure)) => {
                effects.push(error(ErrorKey::SignOutNotDurable, Action::Retry));
                (failure, leaving.step, Cleanup::NotDurable)
            }
            Done::Prepared(Err(failure)) | Done::Checkpointed(Err(failure)) => {
                (failure, Step::Settle, Cleanup::Refused)
            }
            Done::SignedOut(Err(failure)) => (failure, Step::Delete, Cleanup::Refused),
            done => {
                debug_assert!(false, "{done:?} doesn't answer a sign-out step");
                return;
            }
        };
        let (failure, step, cleanup) = refused;
        tracing::warn!(%failure, "a sign-out step was refused");
        leaving.step = step;
        leaving.stuck = Some(cleanup);
    }

    fn step(&mut self, job: Job) {
        self.jobs.push_back(Queued::Leaving(job));
    }
}
