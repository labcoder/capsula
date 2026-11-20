//! The send lease: the coordinator orders the moment a body starts going out against every
//! delete of a source it carries (docs/development/engines.md, "The send lease").
//!
//! Right before a body is handed to the network the transport asks with `SendReady`. A grant
//! records a lease with the build's sources; a delete of one of them is then held, with no
//! writer op, until every lease carrying it settles. A lease that doesn't settle by its deadline
//! fails closed: its held deletes are refused, its run fails, and nothing is granted again until
//! the brain restarts, unless it was a held send: a Claude Code process on its own pipes, not the
//! shared transport. Leases belong to send attempts, not runs, so they outlive Stop. A run's
//! round and a background job (`job`) ask the same way, each under its own key.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::message::{ConnectionKind, Deleted, HeldScreen, RunId, ThreadId};

use super::availability::sign_in_needed;
use super::{BuildKey, Coordinator, Effect, HoldKey, JobKey, Stale, StoreOp};
use crate::context::Screen;
use crate::store::Failure;

/// How long a granted send may go without settling [guess].
pub const SEND_DEADLINE: Duration = Duration::from_secs(15);

/// A Claude route's revocable grant for the picture a send carries. Only the coordinator makes
/// one, so a lease without it can't claim a picture.
#[derive(Clone, Debug)]
pub(crate) struct PictureGrant(Arc<AtomicBool>);

impl PictureGrant {
    /// Whether the send may still write its picture.
    pub(crate) fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Ends the grant: a transport that hasn't written yet writes nothing.
    pub(crate) fn close(&self) {
        self.0.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn open_for_tests() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

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
    /// Asked with a hold, so its deadline leaves the shared transport granting.
    held_send: bool,
    /// The hold's picture this send carries, until it settles.
    screen: Option<Carried>,
    /// Open while the picture this send carries on a Claude route may still go; the transport
    /// checks it right before its first write.
    picture: Option<Arc<AtomicBool>>,
}

/// A granted send carrying a hold's picture, which its turn already records as sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Carried {
    pub(super) run: RunId,
    pub(super) hold: HoldKey,
    pub(super) outcome: HeldScreen,
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

    /// The picture grant of `lease`, which its transport checks right before its first write.
    pub(crate) fn picture_grant(&self, lease: LeaseId) -> Option<PictureGrant> {
        self.leases
            .iter()
            .find(|held| held.id == lease)?
            .picture
            .clone()
            .map(PictureGrant)
    }

    /// Closes each picture grant whose run may no longer send its picture, so a transport that
    /// hasn't written yet writes nothing.
    pub(super) fn revoke_pictures(&self) {
        for lease in &self.leases {
            let (Some(open), SendKey::Build(key)) = (&lease.picture, lease.id.key) else {
                continue;
            };
            if !self
                .ending
                .iter()
                .chain(&self.current)
                .find(|run| run.id == key.run)
                .is_some_and(|run| self.run_pictured(run))
            {
                open.store(false, Ordering::Release);
            }
        }
    }

    /// The current run's picture may no longer go: its build goes again, without the picture
    /// when no send may have written it, and otherwise its admission fails closed.
    fn rebuild_without_picture(
        &mut self,
        key: BuildKey,
        pending: &BTreeSet<ThreadId>,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(run) = self.current.as_mut() {
            run.picture_lost(effects);
        }
        effects.push(Effect::CancelEngine { run: key.run });
        self.rebuild(key, &BTreeSet::new(), pending, effects);
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
            .and_then(|run| run.route)
            .map_or_else(|| Some(sign_in_needed()), |route| self.refusal(route))
            .or_else(|| {
                // A request that started before the switch went off still holds its own copy.
                self.current
                    .as_ref()
                    .filter(|run| matches!(run.screen, Some(crate::context::Screen::Withdrawn)))
                    .map(|_| kumochi_protocol::message::ErrorKey::ScreenUnavailable.into())
            });
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
        // Only a normal build adds the hold's screen; an inline compaction's request is text.
        let carries_screen = build.inline.is_none();
        let picture = self.current.as_ref().filter(|run| {
            carries_screen && matches!(run.screen, Some(Screen::Image(_) | Screen::Picture(..)))
        });
        let revocable = picture
            .and_then(|run| run.route)
            .is_some_and(|route| route.kind != ConnectionKind::ChatGpt);
        if picture.is_some_and(|run| !self.run_pictured(run)) {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.rebuild_without_picture(key, &pending, effects);
            return;
        }
        if let Some(run) = self.current.as_mut()
            && run
                .route
                .is_some_and(|route| route.kind == ConnectionKind::AnthropicKey)
        {
            run.key_granted = true;
        }
        let screen = self
            .current
            .as_mut()
            .filter(|_| carries_screen)
            .and_then(super::Run::carried);
        self.grant(SendKey::Build(key), sources, hold, screen, effects);
        if revocable && let Some(lease) = self.leases.last_mut() {
            lease.picture = Some(Arc::new(AtomicBool::new(true)));
        }
    }

    /// Records a lease for `key` carrying `sources` and `screen`, grants it and arms its deadline:
    /// `hold`, capped at [`MAX_HOLD`], or [`SEND_DEADLINE`].
    pub(super) fn grant(
        &mut self,
        key: SendKey,
        sources: Vec<ThreadId>,
        hold: Option<Duration>,
        screen: Option<Carried>,
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
            held_send: hold.is_some(),
            screen,
            picture: None,
        });
        effects.push(Effect::Grant(id));
        effects.push(Effect::ArmSendDeadline {
            lease: id,
            after: hold.map_or(SEND_DEADLINE, |hold| hold.min(MAX_HOLD)),
        });
    }

    /// A granted send stopped: the picture it carried is settled, its lease retires, and each held
    /// delete no other lease carries goes to the store. A failed lease's late settlement settles
    /// only the picture.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the transport proved no write was attempted"
    )]
    pub(super) fn send_settled(
        &mut self,
        lease: LeaseId,
        unsent: bool,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let at = self
            .leases
            .iter()
            .position(|held| held.id == lease)
            .ok_or(Stale)?;
        if let Some(carried) = self.leases[at].screen.take() {
            self.screen_settled(carried, unsent, effects);
        }
        let revoked = unsent
            && self.leases[at]
                .picture
                .as_ref()
                .is_some_and(|open| !open.load(Ordering::Acquire));
        if revoked
            && let SendKey::Build(key) = lease.key
            && self.current.as_ref().is_some_and(|run| {
                run.id == key.run && run.build.as_ref().is_some_and(|build| build.key() == key)
            })
        {
            let pending = self.pending_deletes();
            self.rebuild_without_picture(key, &pending, effects);
        }
        if !self.leases[at].failed {
            self.leases.remove(at);
            let leased = self.leased();
            self.release_deletes(&leased, effects);
        }
        Ok(())
    }

    /// A send that may have written any of its request tells the shell, once, and marks the turn's
    /// pending picture sent. One that wrote nothing leaves its run as if never granted: a
    /// current run clears the mark at its end, or a later grant carries the picture again, and
    /// any other run's mark is cleared now.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the transport proved no write was attempted"
    )]
    fn screen_settled(&mut self, carried: Carried, unsent: bool, effects: &mut Vec<Effect>) {
        if !unsent {
            effects.push(Effect::HoldScreen {
                hold: carried.hold,
                outcome: carried.outcome,
            });
            effects.push(Effect::Store(StoreOp::SentScreen { run: carried.run }));
            return;
        }
        if let Some(run) = self.current.as_mut().filter(|run| {
            run.id == carried.run
                && run
                    .screen
                    .as_ref()
                    .is_some_and(|screen| screen.held() == carried.outcome)
        }) {
            run.untold = Some(carried.hold.gesture);
            return;
        }
        let withdrawn = self
            .find(carried.run)
            .is_ok_and(|run| matches!(run.screen, Some(crate::context::Screen::Withdrawn)));
        let run = carried.run;
        effects.push(Effect::Store(if withdrawn {
            StoreOp::WithdrawScreen { run }
        } else {
            StoreOp::ForgetScreen { run }
        }));
    }

    /// A lease ran past its deadline. A delete is never released because time ran out, so the
    /// lease fails closed instead. Only a lease that wasn't held stops every later grant.
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
        self.transport_failed |= !held.held_send;
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
