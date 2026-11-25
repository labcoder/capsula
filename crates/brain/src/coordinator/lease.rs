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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kumochi_protocol::message::{ConnectionKind, Deleted, HeldScreen, RunId, ThreadId};

use super::availability::sign_in_needed;
use super::{BuildKey, Coordinator, Effect, HoldKey, JobKey, Stale, StoreOp};
use crate::store::Failure;

/// How long a granted send may go without settling [guess].
pub const SEND_DEADLINE: Duration = Duration::from_secs(15);

/// A Claude route's revocable grant for the picture a send carries. Only the coordinator makes
/// one, so a lease without it can't claim a picture.
#[derive(Clone, Debug)]
pub(crate) struct PictureGrant(Arc<AtomicU8>);

impl PictureGrant {
    const CLOSED: u8 = 1;
    const WRITING: u8 = 2;

    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }

    /// Whether the grant is still open, including during an admitted write.
    pub(crate) fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire) & Self::CLOSED == 0
    }

    /// Admits at most one pipe write. If close wins the race, this can't admit it.
    pub(crate) fn reserve_write(&self) -> Option<PictureWrite> {
        self.0
            .compare_exchange(0, Self::WRITING, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| PictureWrite(Arc::downgrade(&self.0)))
    }

    /// Whether a handle besides this one is left: once a send has settled, its lease's and
    /// the coordinator's are gone, and the last one left guards nothing.
    pub(crate) fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }

    /// Fences new writes. A write already admitted may still begin or finish.
    pub(crate) fn close(&self) {
        self.0.fetch_or(Self::CLOSED, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn open_for_tests() -> Self {
        Self::new()
    }
}

/// One admitted pipe write. Dropping this permit doesn't reopen a closed grant.
#[derive(Debug)]
pub(crate) struct PictureWrite(Weak<AtomicU8>);

impl Drop for PictureWrite {
    fn drop(&mut self) {
        if let Some(state) = self.0.upgrade() {
            state.fetch_and(!PictureGrant::WRITING, Ordering::AcqRel);
        }
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

/// The protected digest a Key run's first request carried, which later true continuations resend
/// verbatim in their held prefix.
#[derive(Clone, Debug)]
pub(super) struct KeyDigest {
    pub(super) recipient: crate::context::PictureRecipient,
    pub(super) scenes: Vec<crate::drawing::PrivateScene>,
    /// Latched once authority, image fact, or screen sharing was lost after round 1 was granted.
    pub(super) revoked: bool,
}

/// Which protected digest a granted lease carries.
#[derive(Debug)]
enum DigestGate {
    /// A fresh build carrying `recipient`'s digest from `Build.drawings`.
    Build(crate::context::PictureRecipient),
    /// A true Key continuation (`key.round > 1`, non-inline) resending round 1's held prefix.
    Continuation(crate::context::PictureRecipient),
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
    /// The revocable grant for the picture this send carries on a Claude route.
    picture: Option<PictureGrant>,
    /// The protected digest this send carries, which its grant also gates.
    digest: Option<DigestGate>,
}

/// A granted send carrying a hold's picture, which its turn already records as sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Carried {
    pub(super) run: RunId,
    pub(super) hold: HoldKey,
    pub(super) outcome: HeldScreen,
    /// The held turn's thread, so a settlement that changes its row tells the shell even after
    /// the run has gone.
    pub(super) thread: ThreadId,
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

    /// The picture grant of `lease`, shared with its transport.
    pub(crate) fn picture_grant(&self, lease: LeaseId) -> Option<PictureGrant> {
        self.leases
            .iter()
            .find(|held| held.id == lease)?
            .picture
            .clone()
    }

    /// Fences new write admissions for each picture grant whose run may no longer send it: its
    /// picture was withdrawn or lost, its recipient no longer takes it, or the drawings its digest
    /// lists no longer read as its build read them. Each grant's first
    /// close keeps that cause on its run for the send's terminal (`Self::round_reason`).
    pub(super) fn revoke_pictures(&mut self) {
        let mut first = Vec::new();
        for lease in &self.leases {
            let (Some(grant), SendKey::Build(key)) = (&lease.picture, lease.id.key) else {
                continue;
            };
            let run = self
                .ending
                .iter()
                .chain(&self.current)
                .find(|run| run.id == key.run);
            let cause = match run {
                Some(run) if matches!(run.screen, Some(crate::context::Screen::Withdrawn)) => {
                    Some(kumochi_protocol::message::ErrorKey::ScreenUnavailable.into())
                }
                Some(run)
                    if run.screen_lost.is_none()
                        && self.run_pictured(run)
                        && lease
                            .digest
                            .as_ref()
                            .is_none_or(|gate| self.digest_gate_current(run, key, gate)) =>
                {
                    continue;
                }
                Some(run) => Some(run.picture_stopped()),
                None => None,
            };
            if let Some(cause) = cause.filter(|_| grant.is_open()) {
                first.push((key, cause));
            }
            grant.close();
        }
        for (key, cause) in first {
            if let Some(run) = self
                .ending
                .iter_mut()
                .chain(&mut self.current)
                .find(|run| run.id == key.run)
            {
                run.grant_closed = Some((key, cause));
            }
        }
    }

    /// Closes every open picture grant for `run`, stopping any unstarted write at once.
    pub(super) fn close_grants(&self, run: RunId) {
        for lease in &self.leases {
            if let (Some(grant), SendKey::Build(key)) = (&lease.picture, lease.id.key)
                && key.run == run
            {
                grant.close();
            }
        }
    }

    /// Closes every open digest-carrying grant for `run`, leaving unrelated runs and picture-only
    /// grants untouched.
    pub(super) fn close_digest_grants(&self, run: RunId) {
        for lease in &self.leases {
            if lease.digest.is_some()
                && let (Some(grant), SendKey::Build(key)) = (&lease.picture, lease.id.key)
                && key.run == run
            {
                grant.close();
            }
        }
    }

    /// Whether a granted lease's protected digest is still valid.
    fn digest_gate_current(&self, run: &super::Run, key: BuildKey, gate: &DigestGate) -> bool {
        match gate {
            DigestGate::Build(recipient) => self.build_digest_current(run, key, recipient),
            DigestGate::Continuation(recipient) => self.key_digest_current(run, recipient),
        }
    }

    /// Whether `recipient`'s digest in `run`'s fresh build `key` still reads as the thread's
    /// drawings read now and every private scene it captured is still live and unchanged.
    fn build_digest_current(
        &self,
        run: &super::Run,
        key: BuildKey,
        recipient: &crate::context::PictureRecipient,
    ) -> bool {
        run.build
            .as_ref()
            .filter(|build| build.key() == key && build.inline.is_none())
            .is_some_and(|build| {
                let built = build.drawings.line_for(recipient);
                built.is_some()
                    && built == self.drawing.digest(run.thread).line_for(recipient)
                    && build.private_scenes_for(recipient).is_some_and(|scenes| {
                        self.drawing
                            .private_scenes_current(run.thread, recipient, scenes)
                    })
            })
    }

    /// Whether the protected digest carried in `run`'s first Key request is still live, unchanged,
    /// and never lost authority since round 1 was granted.
    fn key_digest_current(
        &self,
        run: &super::Run,
        recipient: &crate::context::PictureRecipient,
    ) -> bool {
        run.key_digest.as_ref().is_some_and(|carried| {
            !carried.revoked
                && carried.recipient == *recipient
                && self.pictured(carried.recipient.route, Some(&carried.recipient.model))
                && self.drawing.private_scenes_current(
                    run.thread,
                    &carried.recipient,
                    &carried.scenes,
                )
        })
    }

    /// Why a round ended, read the same way for every build kind: a send its own lease stopped
    /// ends by why that send's grant first closed, and anything else by the engine's reason.
    pub(super) fn round_reason(
        &self,
        key: BuildKey,
        finish: crate::engines::Finish,
    ) -> Option<kumochi_protocol::message::UserError> {
        let run = self.current.as_ref().filter(|run| run.id == key.run);
        let withheld = matches!(
            finish,
            crate::engines::Finish::Failed(
                crate::engines::EngineFailure::Stream(
                    crate::connections::model::error::Failure::Withheld
                ) | crate::engines::EngineFailure::Anthropic(
                    crate::connections::anthropic::error::Failure::Withheld
                )
            )
        );
        if withheld
            && let Some((_, cause)) = run
                .and_then(|run| run.grant_closed.as_ref())
                .filter(|(closed, _)| *closed == key)
        {
            return Some(cause.clone());
        }
        self.engine_reason(run.and_then(|run| run.route), finish)
    }

    /// The current run's hold's picture is lost for good when its recipient no longer takes
    /// pictures, or, with `kind`, answers on that kind: no later build carries it. A typed Key
    /// run's carried digest latches the same loss for any later continuation.
    pub(crate) fn lose_pictures(&mut self, kind: Option<ConnectionKind>) {
        let revoke_key = self
            .current
            .as_ref()
            .and_then(|run| run.key_digest.as_ref())
            .is_some_and(|carried| {
                kind.is_none_or(|kind| carried.recipient.route.kind == kind)
                    && !self.pictured(carried.recipient.route, Some(&carried.recipient.model))
            });
        if revoke_key
            && let Some(carried) = self
                .current
                .as_mut()
                .and_then(|run| run.key_digest.as_mut())
        {
            carried.revoked = true;
        }
        let Some(choice) = self
            .current
            .as_ref()
            .and_then(|run| run.hold.as_ref())
            .and_then(|hold| hold.defaults.choice.as_ref())
        else {
            return;
        };
        let lost = kind.map_or_else(
            || {
                (!self.takes_pictures(choice.connection, &choice.model))
                    .then_some(super::Lost::Fact)
            },
            |kind| (choice.connection == kind).then_some(super::Lost::Generation),
        );
        if let Some(lost) = lost
            && let Some(run) = self.current.as_mut()
        {
            run.screen_lost.get_or_insert(lost);
        }
    }

    /// Why the current run ends now: a send may have written its picture, which may no longer go,
    /// so no later request of the run may start.
    pub(super) fn written_picture_lost(&self) -> Option<kumochi_protocol::message::UserError> {
        let run = self.current.as_ref()?;
        (run.exposed() && (run.screen_lost.is_some() || !self.run_pictured(run)))
            .then(|| run.picture_stopped())
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
    #[expect(
        clippy::too_many_lines,
        reason = "one admission orders deletes, digests, pictures and the grant"
    )]
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
            .and_then(|run| Some((run, run.build.as_ref()?)))
            .filter(|(_, build)| build.key() == key && build.admitted());
        let Some((run, build)) = current else {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            return;
        };
        let refused = run
            .route
            .map_or_else(
                || Some(sign_in_needed()),
                |route| {
                    self.refusal(route).or_else(|| {
                        // A recovered route sends only a body read to its exact cutoff.
                        (build.recovered_context != self.required_recovery(route))
                            .then(sign_in_needed)
                    })
                },
            )
            .or_else(|| {
                // A request that started before the switch went off still holds its own copy.
                matches!(run.screen, Some(crate::context::Screen::Withdrawn))
                    .then(|| kumochi_protocol::message::ErrorKey::ScreenUnavailable.into())
            })
            .or_else(|| {
                // A same-key catalog refresh can drop the model after the build was admitted.
                let model = run
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.choice.as_ref())
                    .map(|choice| choice.model.as_str());
                (!self.offered(run.route?, model)).then(super::availability::model_unavailable)
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
        let inline = build.inline.is_some();
        let key_continuation = key.round > 1
            && !inline
            && run
                .route
                .is_some_and(|route| route.kind == ConnectionKind::AnthropicKey);
        let digest = (!inline)
            .then(|| {
                build
                    .screen_digests
                    .iter()
                    .find(|recipient| {
                        run.route == Some(recipient.route)
                            && run
                                .snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.choice.as_ref())
                                .is_some_and(|choice| choice.model == recipient.model)
                    })
                    .cloned()
            })
            .flatten();
        if key_continuation {
            if let Some(prior) = run.key_digest.as_ref()
                && !self.key_digest_current(run, &prior.recipient)
            {
                effects.push(Effect::Rebuild(SendKey::Build(key)));
                self.fail(key.run, Failure::Engine, effects)
                    .expect("the run is current");
                return;
            }
        } else if let Some(recipient) = digest.as_ref()
            && (!self.pictured(recipient.route, Some(&recipient.model))
                || !self.drawing.visible_to(run.thread, recipient)
                || !self.build_digest_current(run, key, recipient))
        {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.rebuild(key, &BTreeSet::new(), &pending, effects);
            return;
        }
        // The request handed to the engine is judged, never the run's account of its screen.
        let pictured = build.pictured();
        if let Some(reason) = self.written_picture_lost() {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.fail_with(key.run, Failure::Engine, reason, effects)
                .expect("the run is current");
            return;
        }
        if pictured && (run.screen_lost.is_some() || !self.run_pictured(run)) {
            effects.push(Effect::Rebuild(SendKey::Build(key)));
            self.rebuild_without_picture(key, &pending, effects);
            return;
        }
        // A later Key continuation resends its first request, so only that round's digest went
        // out; a standalone inline rewrite sends a fresh body without the prior prefix.
        let carried = if key_continuation {
            run.key_digest
                .as_ref()
                .map(|prior| DigestGate::Continuation(prior.recipient.clone()))
        } else {
            digest.clone().map(DigestGate::Build)
        };
        let protected = pictured || carried.is_some();
        // shortcut: Claude Code's text writes take no grant, so its digest-only sends stay
        // ungated; gate them before Claude Code pictures turn on.
        let gated = carried.filter(|_| {
            run.route
                .is_some_and(|route| route.kind != ConnectionKind::ClaudeCode)
        });
        // A `ChatGPT` run that may have written its picture keeps every later send, such as an
        // inline rewrite, under a grant too, though that send carries no picture of its own.
        let revocable = pictured
            || gated.is_some()
            || (run.exposed()
                && run
                    .route
                    .is_some_and(|route| route.kind == ConnectionKind::ChatGpt));
        let first_key_digest = (key.round == 1 && !inline)
            .then(|| {
                digest.as_ref().map(|recipient| KeyDigest {
                    recipient: recipient.clone(),
                    scenes: build.private_scenes_for(recipient).map_or_else(
                        || self.drawing.private_scenes(run.thread, recipient),
                        <[_]>::to_vec,
                    ),
                    revoked: false,
                })
            })
            .flatten();
        if let Some(run) = self.current.as_mut()
            && let Some(route) = run.route
        {
            if route.kind == ConnectionKind::AnthropicKey && !inline {
                run.key_granted = true;
                if key.round == 1 {
                    run.key_digest = first_key_digest;
                }
            }
            if protected {
                run.picture_recipient = Some(crate::context::PictureRecipient {
                    route,
                    model: run
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.choice.as_ref())
                        .expect("a protected request has a resolved model")
                        .model
                        .clone(),
                });
            }
        }
        let screen = self
            .current
            .as_mut()
            .filter(|_| pictured)
            .and_then(super::Run::carried);
        self.grant(SendKey::Build(key), sources, hold, screen, effects);
        if revocable && let Some(lease) = self.leases.last_mut() {
            lease.picture = Some(PictureGrant::new());
            lease.digest = gated;
        }
    }

    /// Records a lease for `key` carrying `sources` and `screen`, grants it and arms its deadline:
    /// `hold`, capped at [`MAX_HOLD`], or the configured send lease.
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
            digest: None,
        });
        effects.push(Effect::Grant(id));
        effects.push(Effect::ArmSendDeadline {
            lease: id,
            after: hold.map_or(self.tuning.send_lease, |hold| hold.min(MAX_HOLD)),
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
                .is_some_and(|grant| !grant.is_open());
        if unsent
            && let SendKey::Build(key) = lease.key
            && key.round == 1
            && let Some(run) = self.current.as_mut().filter(|run| {
                run.id == key.run
                    && run
                        .build
                        .as_ref()
                        .is_some_and(|build| build.key() == key && build.inline.is_none())
            })
        {
            run.key_granted = false;
            run.key_digest = None;
            run.picture_recipient = None;
            self.drawing.reset_unsent_author(key.run);
        }
        if revoked
            && let SendKey::Build(key) = lease.key
            && self.current.as_ref().is_some_and(|run| {
                run.id == key.run && run.build.as_ref().is_some_and(|build| build.key() == key)
            })
        {
            let carried = matches!(self.leases[at].digest, Some(DigestGate::Continuation(_)));
            if let Some(reason) = self.written_picture_lost() {
                self.fail_with(key.run, Failure::Engine, reason, effects)
                    .expect("the run is current");
            } else if carried {
                let current = self.current.as_ref().expect("the run is current");
                let reason = current
                    .grant_closed
                    .as_ref()
                    .filter(|(closed, _)| *closed == key)
                    .map_or_else(|| current.picture_stopped(), |(_, reason)| reason.clone());
                self.fail_with(key.run, Failure::Engine, reason, effects)
                    .expect("the run is current");
            } else {
                let picture_lost = self.current.as_ref().is_some_and(|run| {
                    run.screen_lost.is_some()
                        || matches!(run.screen, Some(crate::context::Screen::Withdrawn))
                        || !self.run_pictured(run)
                });
                let pending = self.pending_deletes();
                if picture_lost {
                    self.rebuild_without_picture(key, &pending, effects);
                } else {
                    effects.push(Effect::CancelEngine { run: key.run });
                    self.rebuild(key, &BTreeSet::new(), &pending, effects);
                }
            }
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
    /// any other run's mark is cleared now. Either change to the turn's row refreshes its thread.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the transport proved no write was attempted"
    )]
    fn screen_settled(&mut self, carried: Carried, unsent: bool, effects: &mut Vec<Effect>) {
        if unsent
            && let Some(run) = self.current.as_mut().filter(|run| {
                run.id == carried.run
                    && run
                        .screen
                        .as_ref()
                        .is_some_and(|screen| screen.held() == carried.outcome)
            })
        {
            run.untold = Some(carried.hold.gesture);
            return;
        }
        let run = carried.run;
        if unsent {
            let withdrawn = self
                .find(run)
                .is_ok_and(|run| matches!(run.screen, Some(crate::context::Screen::Withdrawn)));
            effects.push(Effect::Store(if withdrawn {
                StoreOp::WithdrawScreen { run }
            } else {
                StoreOp::ForgetScreen { run }
            }));
        } else {
            effects.push(Effect::HoldScreen {
                hold: carried.hold,
                outcome: carried.outcome,
            });
            effects.push(Effect::Store(StoreOp::SentScreen { run }));
        }
        if !self.pending_deletes().contains(&carried.thread) {
            effects.push(Effect::EventsChanged {
                hold: carried.hold,
                thread: carried.thread,
            });
        }
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
