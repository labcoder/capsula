//! The send lease: the coordinator orders the moment a body starts going out against every
//! delete of a source it carries (docs/development/engines.md, "The send lease").
//!
//! Right before a body is handed to the network the transport asks with `SendReady`. A grant
//! records a lease with the build's sources; a delete of one of them is then held, with no
//! writer op, until every lease carrying it settles. A lease that doesn't settle by its deadline
//! fails closed: its held deletes are refused, its run fails, and nothing is granted again until
//! the brain restarts. Leases belong to send attempts, not runs, so they outlive Stop. A run's
//! round and a background job (`job`) ask the same way, each under its own key.

use std::collections::BTreeSet;
use std::time::Duration;

use kumochi_protocol::message::{Deleted, ThreadId};

use super::availability::sign_in_needed;
use super::{BuildKey, Coordinator, Effect, JobKey, Stale};
use crate::store::Failure;

/// How long a granted send may go without settling [guess].
pub const SEND_DEADLINE: Duration = Duration::from_secs(15);
/// The longest a held send may go without settling: a Claude Code question's whole launch and
/// the grace for its teardown to confirm its job empty [guess].
pub const MAX_HOLD: Duration = Duration::from_secs(165);

/// What asks to send: a run's build, or a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendKey {
    /// A round of a run.
    Build(BuildKey),
    /// A background job.
    Job(JobKey),
}

/// One granted send attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseId {
    /// What it sends.
    pub key: SendKey,
    /// A serial across every grant, never reused.
    pub send: u64,
}

/// A granted send that hasn't settled, or one that failed closed and fences its sources until
/// the brain restarts.
#[derive(Debug)]
pub(super) struct Lease {
    id: LeaseId,
    sources: Vec<ThreadId>,
    failed: bool,
}

impl Coordinator {
    /// Threads some unsettled or failed lease carries.
    pub(super) fn leased(&self) -> BTreeSet<ThreadId> {
        self.leases
            .iter()
            .flat_map(|lease| lease.sources.iter().copied())
            .collect()
    }

    /// Threads a lease that failed closed carries.
    pub(super) fn fenced_for_good(&self) -> BTreeSet<ThreadId> {
        self.leases
            .iter()
            .filter(|lease| lease.failed)
            .flat_map(|lease| lease.sources.iter().copied())
            .collect()
    }

    /// The transport is about to send `key`'s body. It's granted only for the current run's
    /// admitted build that no delete has touched since; otherwise the body is dropped unsent,
    /// and a touched build is built again. Every ask gets an answer.
    pub(super) fn send_ready(
        &mut self,
        key: BuildKey,
        hold: Option<Duration>,
        effects: &mut Vec<Effect>,
    ) {
        let pending = self.pending_deletes();
        if !self.hold_valid() {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            if self.current_run() == Some(key.run) {
                self.fail(key.run, Failure::Engine, effects)
                    .expect("the run is current");
            }
            return;
        }
        let current = self
            .current
            .as_ref()
            .filter(|run| run.id == key.run)
            .and_then(|run| run.build.as_ref())
            .filter(|build| build.key() == key && build.admitted());
        let Some(build) = current else {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            return;
        };
        let refused = self
            .current
            .as_ref()
            .and_then(|run| run.connection)
            .map_or_else(
                || Some(sign_in_needed()),
                |connection| self.refusal(connection),
            );
        if let Some(reason) = refused {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.fail_with(key.run, Failure::Engine, reason, effects)
                .expect("the run is current");
            return;
        }
        if self.transport_failed {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.fail(key.run, Failure::Engine, effects)
                .expect("the run is current");
            return;
        }
        let voided = build.voided(build.sources());
        if !voided.is_empty() {
            effects.push(Effect::CancelEngine { run: key.run });
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.rebuild(key, &voided, &pending, effects);
            return;
        }
        let sources = build.sources().to_vec();
        self.grant(SendKey::Build(key), sources, hold, effects);
    }

    /// Records a lease for `key` carrying `sources`, grants it and arms its deadline: `hold`,
    /// capped at [`MAX_HOLD`], or [`SEND_DEADLINE`].
    pub(super) fn grant(
        &mut self,
        key: SendKey,
        sources: Vec<ThreadId>,
        hold: Option<Duration>,
        effects: &mut Vec<Effect>,
    ) {
        self.sends += 1;
        let id = LeaseId {
            key,
            send: self.sends,
        };
        self.leases.push(Lease {
            id,
            sources,
            failed: false,
        });
        effects.push(Effect::Grant(id));
        effects.push(Effect::ArmSendDeadline {
            lease: id,
            after: hold.map_or(SEND_DEADLINE, |hold| hold.min(MAX_HOLD)),
        });
    }

    /// A granted send stopped: its lease retires, and each held delete no other lease carries
    /// goes to the store. A failed lease's late settlement changes nothing.
    pub(super) fn send_settled(
        &mut self,
        lease: LeaseId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let at = self
            .leases
            .iter()
            .position(|held| held.id == lease)
            .ok_or(Stale)?;
        if !self.leases[at].failed {
            self.leases.remove(at);
            let leased = self.leased();
            self.release_deletes(&leased, effects);
        }
        Ok(())
    }

    /// A lease ran past its deadline. A delete is never released because time ran out, so the
    /// lease fails closed instead.
    pub(super) fn send_deadline(
        &mut self,
        lease: LeaseId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let held = self
            .leases
            .iter_mut()
            .find(|held| held.id == lease && !held.failed)
            .ok_or(Stale)?;
        held.failed = true;
        let sources = held.sources.clone();
        self.transport_failed = true;
        match lease.key {
            SendKey::Build(key) if self.current_run() == Some(key.run) => {
                self.fail(key.run, Failure::Engine, effects)?;
            }
            SendKey::Build(key) => effects.push(Effect::CancelEngine { run: key.run }),
            SendKey::Job(key) => self.cancel_job_if(key, effects),
        }
        for thread in self.held_deletes() {
            if sources.contains(&thread) {
                self.refused_delete = true;
                self.deleted(thread, Deleted::NotHidden, None, effects)?;
            }
        }
        Ok(())
    }
}
