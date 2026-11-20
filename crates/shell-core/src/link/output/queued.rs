use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::mpsc::TrySendError;
use std::sync::{OnceLock, Weak};
use std::thread::{self, Thread};
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::ControlToBrain;

use super::{Arc, Ordering, Output, OutputGeneration, OutputOwner, RunId, State, SyncSender};
use crate::link::Shared;
use crate::speaker::DRAIN;
use crate::speaker::queued::{Input, Packet, RATE, spawn_plain};

const LOCAL_QUIET: u8 = 1;
const RECEIPT_QUEUED: u8 = 2;
const RETIRING: u8 = 4;
const REPORT_REFUSED: u8 = 8;
const TAIL_UNAVAILABLE: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::link) enum RefusedTail {
    Quiet,
    Unavailable,
}

pub(in crate::link) struct Queued {
    closing: Arc<AtomicBool>,
    deadline: Instant,
    ack: Option<SyncSender<()>>,
    announced: bool,
    wake: Arc<OnceLock<Thread>>,
    stopped: RunId,
    early_end: Option<RunId>,
    pending: Option<u32>,
    active: Option<Active>,
}

struct Active {
    id: RunId,
    input: Option<Input>,
    proof: Arc<AtomicU8>,
}

impl Queued {
    pub(in crate::link) fn new(
        deadline: Instant,
        ack: SyncSender<()>,
        wake: Arc<OnceLock<Thread>>,
        closing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            closing,
            deadline,
            ack: Some(ack),
            announced: false,
            wake,
            stopped: RunId(0),
            early_end: None,
            pending: None,
            active: None,
        }
    }

    pub(super) fn owes(&self) -> bool {
        self.owes_proof(LOCAL_QUIET)
    }

    fn owes_proof(&self, bit: u8) -> bool {
        self.active
            .as_ref()
            .is_some_and(|run| run.proof.load(Ordering::Acquire) & bit == 0)
    }

    pub(super) fn close(&mut self) {
        self.ack = None;
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

    pub(super) fn stop(&mut self, run: RunId) -> bool {
        let Some(active) = self.active.as_ref().filter(|active| run <= active.id) else {
            return false;
        };
        if run != active.id {
            return true;
        }
        self.stopped = run;
        self.wake();
        active.input.as_ref().is_none_or(Input::stop)
    }

    fn wake(&self) {
        if let Some(reader) = self.wake.get() {
            reader.unpark();
        }
    }

    fn ready(&self) -> bool {
        self.announced && self.ack.is_none()
    }
}

impl State {
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
}

impl OutputOwner {
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
        self.try_lock()
            .ok()
            .as_mut()
            .and_then(|state| state.queue(session))
            .filter(|(_, owner)| {
                owner.ready() && !owner.owes_proof(RECEIPT_QUEUED) && owner.pending.is_none()
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

    pub(in crate::link) fn register_queued(
        &self,
        session: u64,
        out: &Sender<ControlToBrain>,
    ) -> bool {
        let mut state = self.lock();
        let Some((output, owner)) = state.queue(session) else {
            return false;
        };
        if !owner.announced {
            owner.announced = out
                .try_send(ControlToBrain::QueuedAudioOutput(RATE, output))
                .is_ok();
        }
        owner.announced
    }

    pub(in crate::link) fn acknowledge_queued(
        &self,
        session: u64,
        output: OutputGeneration,
    ) -> bool {
        let mut state = self.lock();
        let Some((_, owner)) = state.queue(session).filter(|(key, owner)| {
            *key == output && owner.announced && Instant::now() < owner.deadline
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

    pub(in crate::link) fn bind_queued(
        &self,
        session: u64,
        run: RunId,
        turn: kumochi_protocol::message::RunOrigin,
        out: &Sender<ControlToBrain>,
        mut credit: impl FnMut(u32) + Send + 'static,
        shared: Weak<Shared>,
    ) -> bool {
        let proof = Arc::new(AtomicU8::new(0));
        let (output, wake) = {
            let mut state = self.lock();
            let Some((output, owner)) = state
                .queue(session)
                .filter(|(_, q)| q.ready() && !q.owes_proof(RECEIPT_QUEUED))
            else {
                return false;
            };
            if !matches!(turn, kumochi_protocol::message::RunOrigin::Typed { turn } if owner.pending == Some(turn))
            {
                return false;
            }
            owner.pending = None;
            owner.retire();
            if owner.early_end == Some(run) {
                owner.stopped = run;
                owner.early_end = None;
            }
            owner.active = Some(Active {
                id: run,
                input: None,
                proof: Arc::clone(&proof),
            });
            (output, Arc::clone(&owner.wake))
        };
        let out = out.clone();
        let exit = Arc::clone(&proof);
        let tail = Arc::clone(&proof);
        let link_thread = thread::current();
        let reporter = Weak::clone(&shared);
        #[cfg(any(test, feature = "test-support"))]
        let fake = shared
            .upgrade()
            .and_then(|owner| owner.fake_playback.clone());
        let Ok(input) = spawn_plain(
            run,
            output,
            session,
            move |bytes| {
                credit(bytes);
                if let Some(reader) = wake.get() {
                    reader.unpark();
                }
            },
            (
                move |report| offer_queued_report(&reporter, &proof, session, run, &out, report),
                move |quiet| {
                    let bit = if quiet { LOCAL_QUIET } else { TAIL_UNAVAILABLE };
                    let _updated =
                        tail.fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                            (flags & REPORT_REFUSED != 0 && flags & TAIL_UNAVAILABLE == 0)
                                .then_some(flags | bit)
                        });
                    link_thread.unpark();
                },
            ),
            move |refused| {
                if (refused || exit.load(Ordering::Acquire) & RETIRING == 0)
                    && let Some(shared) = shared.upgrade()
                {
                    shared.output.end(session);
                }
            },
            #[cfg(any(test, feature = "test-support"))]
            fake,
        ) else {
            return false;
        };
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

    pub(in crate::link) fn queued_feed(
        &self,
        session: u64,
        run: RunId,
        output: OutputGeneration,
        packet: &mut Option<Packet>,
    ) -> Result<bool, ()> {
        let mut state = self.lock();
        let (_, owner) = state
            .queue(session)
            .filter(|(key, q)| *key == output && q.ready())
            .ok_or(())?;
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
            .queue(session)
            .is_some_and(|(_, owner)| owner.stop(run))
    }

    pub(in crate::link) fn end_queued_run(&self, session: u64, run: RunId) -> bool {
        let mut state = self.lock();
        let Some((_, owner)) = state.queue(session) else {
            return false;
        };
        let current = owner.active.as_ref().map_or(RunId(0), |active| active.id);
        if run == current {
            owner.retire();
            owner.stopped = run;
            owner.wake();
        } else if run > current && owner.early_end.replace(run).is_some() {
            return false;
        }
        true
    }
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
