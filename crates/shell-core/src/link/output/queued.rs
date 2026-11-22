use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::mpsc::TrySendError;
use std::sync::{OnceLock, Weak};
use std::thread::{self, Thread};
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::{ControlToBrain, RunOrigin, SegmentId, SettingsView};

use super::{
    Arc, Denied, Ordering, Output, OutputGeneration, OutputOwner, RunId, State, SyncSender,
};
use crate::link::{Attendance, Shared, narrated::Narrated, queued::MetadataFrame};
use crate::speaker::DRAIN;
use crate::speaker::conductor::Assignment;
use crate::speaker::queued::{Command, Input, Packet, RATE, Speech, spawn, spawn_plain};

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;

const LOCAL_QUIET: u8 = 1;
const RECEIPT_QUEUED: u8 = 2;
const RETIRING: u8 = 4;
const REPORT_REFUSED: u8 = 8;
const TAIL_UNAVAILABLE: u8 = 16;
const DATA_ENDED: u8 = 32;
const CONTROL_ENDED: u8 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::link) enum RefusedTail {
    Quiet,
    Unavailable,
}

pub(in crate::link) enum NarratedReservation {
    Waiting,
    Reserved,
    Drained,
}

#[derive(Clone, Copy)]
pub(super) enum StopOrigin {
    Local,
    Brain,
}

pub(in crate::link) struct Queued {
    closing: Arc<AtomicBool>,
    deadline: Instant,
    ack: Option<SyncSender<()>>,
    talk_back: Option<bool>,
    switching: Option<bool>,
    show_screen: Option<bool>,
    wake: Arc<OnceLock<Thread>>,
    stopped: RunId,
    narrated: Option<Arc<Narrated>>,
    predecessor: Option<(RunId, Delivery)>,
    early_end: Option<RunId>,
    pending: Option<u32>,
    active: Option<Active>,
}

enum Delivery {
    Unpromised,
    Promised(Assignment),
    Announced,
}

enum DeliveryTarget<'a> {
    Waiting,
    Live(&'a mut Delivery, Option<&'a Input>),
    Draining(&'a mut Delivery),
}

struct Active {
    id: RunId,
    input: Option<Input>,
    proof: Arc<AtomicU8>,
    delivery: Option<Delivery>,
    locally_stopped: bool,
    speech: Speech,
}

impl Queued {
    pub(in crate::link) fn new(
        deadline: Instant,
        ack: SyncSender<()>,
        wake: Arc<OnceLock<Thread>>,
        closing: Arc<AtomicBool>,
        narrated: Option<Arc<Narrated>>,
    ) -> Self {
        Self {
            closing,
            deadline,
            ack: Some(ack),
            talk_back: None,
            switching: None,
            show_screen: None,
            wake,
            stopped: RunId(0),
            narrated,
            predecessor: None,
            early_end: None,
            pending: None,
            active: None,
        }
    }

    pub(super) fn owes(&self) -> bool {
        self.owes_proof(LOCAL_QUIET)
    }

    fn owes_proof(&self, bit: u8) -> bool {
        self.talk_back == Some(true)
            && self
                .active
                .as_ref()
                .is_some_and(|run| run.proof.load(Ordering::Acquire) & bit == 0)
    }

    pub(super) fn close(&mut self) {
        self.ack = None;
        self.predecessor = None;
        if let Some(active) = &mut self.active {
            active.delivery = None;
        }
        self.retire();
    }

    fn retire(&mut self) {
        if let Some(run) = &mut self.active {
            run.proof.fetch_or(RETIRING, Ordering::AcqRel);
            drop(run.input.take());
        }
    }

    pub(super) fn refuse(&self) {
        self.closing.store(true, Ordering::Release);
        self.wake();
    }

    pub(super) fn stop(&mut self, run: RunId, origin: StopOrigin) -> bool {
        let Some(active) = self.active.as_mut().filter(|active| run <= active.id) else {
            return false;
        };
        if run != active.id {
            return true;
        }
        self.stopped = run;
        let accepted = active.input.as_ref().is_none_or(Input::stop);
        if accepted
            && matches!(origin, StopOrigin::Local)
            && let Some(bridge) = &self.narrated
        {
            active.locally_stopped = true;
            bridge.stop(run);
        }
        self.wake();
        accepted
    }

    fn wake(&self) {
        if let Some(reader) = self.wake.get() {
            reader.unpark();
        }
    }

    fn ready(&self) -> bool {
        self.talk_back.is_some() && self.ack.is_none() && self.switching.is_none()
    }

    /// A speaking pair also needs the previous run's delivered quiet receipt and nothing reserved.
    fn hold_ready(&self) -> bool {
        self.ready()
            && self.show_screen.is_some()
            && !self.closing.load(Ordering::Acquire)
            && (self.talk_back == Some(false)
                || self.narrated.is_none()
                    && self.pending.is_none()
                    && self.predecessor.is_none()
                    && !self.owes_proof(RECEIPT_QUEUED))
    }

    fn delivery(&mut self, run: RunId) -> Result<DeliveryTarget<'_>, ()> {
        if let Some((_, delivery)) = self.predecessor.as_mut().filter(|(id, _)| *id == run) {
            return Ok(DeliveryTarget::Draining(delivery));
        }
        let current = self.active.as_ref().map_or(RunId(0), |active| active.id);
        if run > current && self.pending.is_some() && self.early_end != Some(run) {
            return Ok(DeliveryTarget::Waiting);
        }
        let active = self
            .active
            .as_mut()
            .filter(|active| active.id == run)
            .ok_or(())?;
        let delivery = active.delivery.as_mut().ok_or(())?;
        if run <= self.stopped {
            Ok(DeliveryTarget::Draining(delivery))
        } else {
            Ok(DeliveryTarget::Live(delivery, active.input.as_ref()))
        }
    }
}

impl State {
    pub(super) fn queued_hold(&self) -> Option<OutputGeneration> {
        if let Output::Queued(output) = self.output
            && self.session.is_some()
            && !self.off
            && self.queued.as_ref().is_some_and(Queued::hold_ready)
        {
            return Some(output);
        }
        None
    }

    fn queue(&mut self, session: u64) -> Option<(OutputGeneration, &mut Queued)> {
        let Output::Queued(output) = self.output else {
            return None;
        };
        if self.session != Some(session) || self.off {
            return None;
        }
        self.queued
            .as_mut()
            .filter(|owner| !owner.closing.load(Ordering::Acquire))
            .map(|owner| (output, owner))
    }

    fn speech_queue(&mut self, session: u64) -> Option<(OutputGeneration, &mut Queued)> {
        self.queue(session)
            .filter(|(_, owner)| owner.talk_back == Some(true) && owner.ready())
    }
}

impl OutputOwner {
    /// The run `session`'s native route is presenting, and when that evidence runs out.
    ///
    /// # Errors
    /// `Busy` on contention.
    pub fn speech(&self, session: u64) -> Result<Option<(RunId, Instant)>, Denied> {
        // The plain lock: refusals a test scripts for the owner's calls aren't spent on this read.
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(state)) => state.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(Denied::Busy),
        };
        Ok(state.queue(session).and_then(|(_, owner)| {
            let active = owner.active.as_ref()?;
            Some((active.id, active.speech.until()?))
        }))
    }

    /// Stops `run`'s audio on `session` locally; nothing it finds refuses the pair.
    ///
    /// # Errors
    /// `Busy` on contention.
    pub fn stop_locally(&self, session: u64, run: RunId) -> Result<(), Denied> {
        if let Some((_, owner)) = self.try_lock()?.queue(session) {
            owner.stop(run, StopOrigin::Local);
        }
        Ok(())
    }

    pub(in crate::link) fn wait_for_refused_tail(&self) -> Option<RefusedTail> {
        let proof = {
            let state = self.lock();
            let active = state.queued.as_ref()?.active.as_ref()?;
            if active.proof.load(Ordering::Acquire) & REPORT_REFUSED == 0 {
                return None;
            }
            Arc::clone(&active.proof)
        };
        let deadline = Instant::now() + DRAIN;
        loop {
            let flags = proof.load(Ordering::Acquire);
            if flags & LOCAL_QUIET != 0 {
                return Some(RefusedTail::Quiet);
            }
            if flags & TAIL_UNAVAILABLE != 0 {
                return Some(RefusedTail::Unavailable);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                if proof
                    .compare_exchange(
                        flags,
                        flags | TAIL_UNAVAILABLE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Some(RefusedTail::Unavailable);
                }
            } else {
                thread::park_timeout(left);
            }
        }
    }

    pub(in crate::link) fn queued_turn(
        &self,
        session: u64,
        turn: Option<u32>,
        send: impl FnOnce() -> bool,
    ) -> bool {
        let mut state = self.lock();
        let capturing = state.inhibit.is_some();
        state
            .queue(session)
            .filter(move |(_, owner)| !capturing || owner.talk_back != Some(true))
            .filter(|(_, owner)| {
                owner.ready()
                    && !owner.owes_proof(RECEIPT_QUEUED)
                    && owner.pending.is_none()
                    && owner.predecessor.is_none()
            })
            .is_some_and(|(_, owner)| {
                if !send() {
                    return false;
                }
                owner.pending = turn;
                true
            })
    }

    pub(in crate::link) fn queued_refused(&self, session: u64, turn: u32) {
        if let Some((_, owner)) = self.lock().queue(session) {
            let _pending = owner.pending.take_if(|pending| *pending == turn);
        }
    }

    pub(in crate::link) fn try_queued_hold_ready(&self, session: u64) -> bool {
        self.try_lock()
            .is_ok_and(|state| state.session == Some(session) && state.queued_hold().is_some())
    }

    pub(in crate::link) fn queued_hold_ready(&self, session: u64) -> bool {
        let state = self.lock();
        state.session == Some(session) && state.queued_hold().is_some()
    }

    pub(in crate::link) fn can_reserve_hold(&self, session: u64) -> bool {
        self.lock()
            .queue(session)
            .is_some_and(|(_, owner)| owner.hold_ready() && owner.pending.is_none())
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the wire preference is a boolean"
    )]
    pub(in crate::link) fn switch_talk_back(
        &self,
        session: u64,
        on: bool,
        send: impl FnOnce() -> bool,
    ) -> Result<(), Denied> {
        let mut state = self.lock();
        let inhibited = state.inhibit.is_some();
        let Some((_, owner)) = state.queue(session) else {
            return Err(Denied::Stale);
        };
        let ended = DATA_ENDED | CONTROL_ENDED;
        if inhibited
            || !owner.ready()
            || owner.pending.is_some()
            || owner.predecessor.is_some()
            || owner.owes_proof(RECEIPT_QUEUED)
            || owner
                .active
                .as_ref()
                .is_some_and(|run| run.proof.load(Ordering::Acquire) & ended != ended)
        {
            return Err(Denied::Busy);
        }
        if owner.talk_back == Some(on) || !send() {
            return Err(Denied::Lost);
        }
        owner.switching = Some(on);
        Ok(())
    }

    /// Whether the pair's registered Show Screen differs from the one `settings` carries.
    pub(in crate::link) fn show_screen_changes(
        &self,
        session: u64,
        settings: &SettingsView,
    ) -> bool {
        self.lock().queue(session).is_some_and(|(_, owner)| {
            owner
                .show_screen
                .is_some_and(|known| known != settings.show_screen)
        })
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the wire bit attributes an answer"
    )]
    pub(in crate::link) fn register_queued(
        &self,
        session: u64,
        settings: &SettingsView,
        answer: bool,
        out: &Sender<ControlToBrain>,
    ) -> Result<(), Attendance> {
        let mut state = self.lock();
        let Some((output, owner)) = state.queue(session) else {
            return Err(Attendance::Lost);
        };
        if owner
            .talk_back
            .is_some_and(|registered| registered != settings.talk_back)
        {
            owner.show_screen = None;
            return Err(if answer && owner.switching == Some(settings.talk_back) {
                Attendance::TalkBackChanged
            } else {
                Attendance::Lost
            });
        }
        if answer {
            owner.switching = None;
        }
        owner.show_screen = Some(settings.show_screen);
        if owner.talk_back.is_some() {
            return Ok(());
        }
        if out
            .try_send(ControlToBrain::QueuedAudioOutput(RATE, output))
            .is_err()
        {
            return Err(Attendance::Lost);
        }
        owner.talk_back = Some(settings.talk_back);
        Ok(())
    }

    pub(in crate::link) fn acknowledge_queued(
        &self,
        session: u64,
        output: OutputGeneration,
    ) -> bool {
        let mut state = self.lock();
        let Some((_, owner)) = state.queue(session).filter(|(key, owner)| {
            *key == output && owner.talk_back.is_some() && Instant::now() < owner.deadline
        }) else {
            return false;
        };
        if owner
            .ack
            .as_ref()
            .is_none_or(|ack| ack.try_send(()).is_err())
        {
            return false;
        }
        owner.ack = None;
        true
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one transition binds either kind of run before deciding whether to start playback"
    )]
    pub(in crate::link) fn bind_queued(
        &self,
        session: u64,
        run: RunId,
        turn: RunOrigin,
        out: &Sender<ControlToBrain>,
        mut credit: impl FnMut(u32) + Send + 'static,
        shared: Weak<Shared>,
    ) -> bool {
        let proof = Arc::new(AtomicU8::new(0));
        let speech = Speech::new(self.changed.clone());
        let (output, wake, narrated) = {
            let mut state = self.lock();
            let Some((output, owner)) = state.queue(session).filter(|(_, q)| {
                q.ready() && !q.owes_proof(RECEIPT_QUEUED) && q.predecessor.is_none()
            }) else {
                return false;
            };
            let offered = match turn {
                RunOrigin::Typed { turn } => owner.pending == Some(turn),
                RunOrigin::Hold { .. } => {
                    owner.pending.is_none()
                        && (owner.talk_back == Some(false) || owner.narrated.is_none())
                }
            };
            if !offered {
                return false;
            }
            owner.pending = None;
            owner.retire();
            if let Some(old) = owner.active.take().filter(|old| old.locally_stopped)
                && let Some(delivery) = old.delivery
            {
                owner.predecessor = Some((old.id, delivery));
            }
            let ended = owner.early_end == Some(run);
            if ended {
                owner.stopped = run;
                owner.early_end = None;
                proof.fetch_or(DATA_ENDED, Ordering::Relaxed);
            }
            let silent = owner.talk_back == Some(false);
            owner.active = Some(Active {
                id: run,
                input: None,
                proof: Arc::clone(&proof),
                delivery: (!silent && owner.narrated.is_some() && !ended)
                    .then_some(Delivery::Unpromised),
                locally_stopped: false,
                speech: speech.clone(),
            });
            if silent {
                owner.wake();
                return true;
            }
            (output, Arc::clone(&owner.wake), owner.narrated.clone())
        };
        let out = out.clone();
        let exit = Arc::clone(&proof);
        let tail = Arc::clone(&proof);
        let link_thread = thread::current();
        let reporter = Weak::clone(&shared);
        #[cfg(any(test, feature = "test-support"))]
        let fake = fake_queued_output(&shared);
        let fault = Arc::clone(&proof);
        let grant = move |bytes| {
            credit(bytes);
            if let Some(reader) = wake.get() {
                reader.unpark();
            }
        };
        let reports = (
            move |report| offer_queued_report(&reporter, &proof, session, run, &out, report),
            move |quiet| {
                let bit = if quiet { LOCAL_QUIET } else { TAIL_UNAVAILABLE };
                let _updated = tail.fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                    (flags & REPORT_REFUSED != 0 && flags & TAIL_UNAVAILABLE == 0)
                        .then_some(flags | bit)
                });
                link_thread.unpark();
            },
            speech,
        );
        let exit = move |refused| {
            if (refused || exit.load(Ordering::Acquire) & RETIRING == 0)
                && let Some(shared) = shared.upgrade()
            {
                shared.output.end(session);
            }
        };
        let result = if let Some(mode) = narrated {
            spawn(
                run,
                output,
                session,
                mode.origin,
                mode.worker(
                    run,
                    move || {
                        fault.fetch_or(REPORT_REFUSED, Ordering::AcqRel);
                    },
                    exit,
                ),
                grant,
                reports,
                #[cfg(any(test, feature = "test-support"))]
                fake,
            )
        } else {
            spawn_plain(
                run,
                output,
                session,
                grant,
                reports,
                exit,
                #[cfg(any(test, feature = "test-support"))]
                fake,
            )
        };
        result.is_ok_and(|input| self.install_queued_input(session, run, input))
    }

    fn install_queued_input(&self, session: u64, run: RunId, input: Input) -> bool {
        let mut state = self.lock();
        let Some((_, owner)) = state.queue(session) else {
            return false;
        };
        let stopped = run <= owner.stopped;
        let Some(active) = owner.active.as_mut().filter(|active| active.id == run) else {
            return false;
        };
        if stopped && !input.stop() {
            return false;
        }
        active.input = Some(input);
        owner.wake();
        true
    }

    pub(in crate::link) fn narrated_metadata(
        &self,
        assignment: Assignment,
        frame: MetadataFrame,
    ) -> Result<NarratedReservation, ()> {
        let mut state = self.lock();
        let (_, owner) = state
            .speech_queue(assignment.session)
            .filter(|(output, _)| *output == assignment.output)
            .ok_or(())?;
        let (delivery, input, outcome) = match owner.delivery(assignment.run)? {
            DeliveryTarget::Waiting => return Ok(NarratedReservation::Waiting),
            DeliveryTarget::Live(delivery, input) => {
                (delivery, input, NarratedReservation::Reserved)
            }
            DeliveryTarget::Draining(delivery) => (delivery, None, NarratedReservation::Drained),
        };
        match (&*delivery, frame) {
            (Delivery::Unpromised, MetadataFrame::Commit) => {
                *delivery = Delivery::Promised(assignment);
            }
            (Delivery::Promised(expected), MetadataFrame::Announcement)
                if *expected == assignment =>
            {
                if matches!(outcome, NarratedReservation::Reserved) {
                    let Some(input) = input else {
                        return Ok(NarratedReservation::Waiting);
                    };
                    input.narrate(assignment).map_err(|_| ())?;
                }
                *delivery = Delivery::Announced;
            }
            _ => return Err(()),
        }
        Ok(outcome)
    }

    pub(in crate::link) fn wait_narrated_gate(&self) {
        drop(self.state.lock());
    }

    pub(in crate::link) fn narrated_gate(
        &self,
        command: Command,
        before: impl FnOnce() -> bool,
    ) -> Result<(), super::Denied> {
        use super::Denied;

        let a = match command {
            Command::Allow(binding) | Command::Revoke(binding) => binding.assignment,
            Command::RevokeSource(assignment) => assignment,
            _ => return Err(Denied::Stale),
        };
        let mut state = self.try_lock()?;
        let (_, owner) = state
            .queue(a.session)
            .filter(|(output, owner)| *output == a.output && owner.ready())
            .ok_or(Denied::Stale)?;
        if !matches!(command, Command::Allow(_)) && a.run <= owner.stopped {
            return Ok(());
        }
        owner
            .active
            .as_ref()
            .filter(|active| active.id == a.run && a.run > owner.stopped)
            .and_then(|active| active.input.as_ref())
            .is_some_and(|input| {
                before()
                    && match command {
                        Command::Allow(binding) => input.allow(binding),
                        Command::Revoke(binding) => input.revoke(binding),
                        Command::RevokeSource(assignment) => input.revoke_source(assignment),
                        _ => false,
                    }
            })
            .then_some(())
            .ok_or(Denied::Lost)
    }

    pub(in crate::link) fn queued_feed(
        &self,
        session: u64,
        run: RunId,
        output: OutputGeneration,
        segment: SegmentId,
        packet: &mut Option<Packet>,
    ) -> Result<bool, ()> {
        let mut state = self.lock();
        let (_, owner) = state
            .speech_queue(session)
            .filter(|(key, _)| *key == output)
            .ok_or(())?;
        if owner.narrated.is_some() {
            let delivery = owner
                .predecessor
                .as_ref()
                .filter(|(id, _)| *id == run)
                .map(|(_, delivery)| delivery)
                .or_else(|| {
                    owner
                        .active
                        .as_ref()
                        .filter(|active| active.id == run)?
                        .delivery
                        .as_ref()
                });
            if matches!(delivery, Some(Delivery::Promised(a)) if a.segment == segment) {
                return Err(());
            }
        }
        let Some(active) = &owner.active else {
            return Ok(false);
        };
        if run > active.id {
            return Ok(false);
        }
        if run < active.id || run <= owner.stopped {
            return Ok(true);
        }
        let Some(input) = &active.input else {
            return Ok(false);
        };
        if let Some(pcm) = packet.take() {
            match input.submit(pcm) {
                Ok(()) => {}
                Err(TrySendError::Full(pcm)) => {
                    *packet = Some(pcm);
                    return Ok(false);
                }
                Err(TrySendError::Disconnected(_)) => return Err(()),
            }
        }
        Ok(true)
    }

    pub(in crate::link) fn revoke_queued(&self, session: u64, run: RunId) -> bool {
        self.lock()
            .speech_queue(session)
            .is_some_and(|(_, owner)| owner.stop(run, StopOrigin::Brain))
    }

    pub(in crate::link) fn queued_terminal(&self, session: u64, run: RunId) {
        if let Some(active) = self
            .lock()
            .queue(session)
            .and_then(|(_, owner)| owner.active.as_ref())
            && active.id == run
        {
            active.proof.fetch_or(CONTROL_ENDED, Ordering::Release);
        }
    }

    pub(in crate::link) fn end_queued_run(&self, session: u64, run: RunId) -> bool {
        let mut state = self.lock();
        let Some((_, owner)) = state.queue(session) else {
            return false;
        };
        if owner.predecessor.take_if(|(id, _)| *id == run).is_some() {
            return true;
        }
        let current = owner.active.as_ref().map_or(RunId(0), |active| active.id);
        if let Some(active) = owner.active.as_mut().filter(|active| active.id == run) {
            active.proof.fetch_or(DATA_ENDED, Ordering::Release);
            active.delivery = None;
            owner.retire();
            owner.stopped = run;
            owner.wake();
        } else if run > current && owner.early_end.replace(run).is_some() {
            return false;
        }
        true
    }
}

#[cfg(any(test, feature = "test-support"))]
fn fake_queued_output(shared: &Weak<Shared>) -> Option<crate::speaker::queued::FakeOutput> {
    let owner = shared.upgrade()?;
    Some(crate::speaker::queued::FakeOutput {
        sender: owner.fake_playback.clone()?,
        prime: owner.prime_fake_playback.swap(false, Ordering::AcqRel),
    })
}

fn offer_queued_report(
    reporter: &Weak<Shared>,
    proof: &Arc<AtomicU8>,
    session: u64,
    run: RunId,
    out: &Sender<ControlToBrain>,
    report: ControlToBrain,
) -> bool {
    let quiet = matches!(
        &report,
        ControlToBrain::SpeechStopped { run: stopped, .. } if *stopped == run
    );
    if quiet {
        proof.fetch_or(LOCAL_QUIET, Ordering::AcqRel);
    }
    let Some(shared) = reporter.upgrade() else {
        return false;
    };
    let mut state = shared.output.lock();
    let Some((_, owner)) = state.queue(session).filter(|(_, owner)| {
        owner
            .active
            .as_ref()
            .is_some_and(|active| active.id == run && Arc::ptr_eq(&active.proof, proof))
    }) else {
        return true;
    };
    #[cfg(any(test, feature = "test-support"))]
    #[cfg(windows)]
    let retirement = match &report {
        ControlToBrain::SpeechRetired(receipt) => Some(*receipt),
        _ => None,
    };
    if out.try_send(report).is_err() {
        #[cfg(any(test, feature = "test-support"))]
        #[cfg(windows)]
        if let Some(receipt) = retirement {
            *shared
                .refused_retirement
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(receipt);
        }
        if !quiet {
            proof.fetch_or(REPORT_REFUSED, Ordering::AcqRel);
        }
        owner.refuse();
        return false;
    }
    if quiet {
        proof.fetch_or(RECEIPT_QUEUED, Ordering::AcqRel);
    }
    true
}
