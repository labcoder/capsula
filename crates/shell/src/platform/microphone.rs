//! One microphone generation's collection authority, independent of GPUI delivery.
//!
//! Only an admitted, running generation whose UI receipt was accepted may collect.
//! Native release/reset and link loss revoke it before deferred UI work; deadlines
//! never prove physical release. A later adapter must preserve input-only operation,
//! stable format/device identity, bounded storage and off-UI confirmed teardown.
//!
//! CPAL 0.18.2 was rejected because WASAPI's input xrun path
//! (`src/host/wasapi/stream.rs:825`) calls `try_emit_error`, which acquires a
//! mutex with `try_lock` at `src/host/error_emit.rs:29`. That is nonblocking,
//! but still violates the no-lock contract, including borrowed error paths.
//! `CoreAudio` input calls the same helper at `src/host/coreaudio/macos/device.rs:781`.

mod pcm;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::Receiver;
use kumochi_protocol::message::MicPcm;
use kumochi_protocol::message::{GestureId, HOLD_START_TIMEOUT_MS};
use zeroize::Zeroize;

pub(super) use pcm::{Format, Packetizer, Sample};

pub(crate) const POLL: Duration = Duration::from_millis(20);
const PCM_QUEUE: usize = 4;

/// The existing 60-second hold limit, independent of sample delivery.
const HOLD_LIMIT: Duration = Duration::from_secs(60);
const ARMED: u64 = 1;
const OPENING: u64 = 2;
const RUNNING: u64 = 3;
const COLLECTING: u64 = 4;
const RELEASED: u64 = 5;
const CANCELLED: u64 = 6;
const RETIRED: u64 = 7;

/// Fixed diagnostics only; foreign errors and driver identifiers never escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MicError {
    #[error("microphone generation is stale")]
    Stale,
    #[error("microphone was cancelled")]
    Cancelled,
    #[error("microphone session ended")]
    SessionLost,
    #[error("microphone hold limit reached")]
    Deadline,
    #[error("microphone is unavailable")]
    Unavailable,
    #[error("microphone format is unsupported")]
    Format,
    #[error("microphone data is discontinuous")]
    Discontinuity,
    #[error("microphone timestamp is unreliable")]
    Timestamp,
    #[error("microphone capacity exceeded")]
    Overflow,
    #[error("microphone shutdown is unconfirmed")]
    Unconfirmed,
}

#[derive(Clone, Copy)]
pub(super) struct PacketStatus {
    pub(super) frames: u32,
    /// The device's stream position of the packet's first frame, in frames.
    pub(super) position: u64,
    pub(super) discontinuity: bool,
    pub(super) timestamp_error: bool,
}

pub(super) const CAPTURE_BUFFER: Duration = Duration::from_millis(100);

/// A hold fails when more than one frame in this many of its span, from the first kept frame
/// to the last, never reached it: sparse losses pass and a mostly lost hold doesn't.
const MISSING_SHARE_DIVISOR: u64 = 4;

pub(super) struct PacketBoundary {
    startup: bool,
    listening: bool,
    failed: Option<MicError>,
    started: Option<Instant>,
    discontinuities: u32,
    first_discontinuity_packet: u32,
    first_discontinuity_ms: u64,
    kept_packets: u32,
    kept_frames: u64,
    missing_frames: u64,
    next_position: Option<u64>,
    discarded_before_listening: u32,
}

impl Default for PacketBoundary {
    fn default() -> Self {
        Self {
            startup: true,
            listening: false,
            failed: None,
            started: None,
            discontinuities: 0,
            first_discontinuity_packet: 0,
            first_discontinuity_ms: 0,
            kept_packets: 0,
            kept_frames: 0,
            missing_frames: 0,
            next_position: None,
            discarded_before_listening: 0,
        }
    }
}

impl PacketBoundary {
    pub(super) fn started(&mut self, at: Instant) {
        self.started = Some(at);
    }

    pub(super) fn observe(&mut self, mode: Mode) {
        if mode == Mode::Collecting {
            self.listening = true;
        }
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "collection authority and the drained queue fence one startup packet"
    )]
    pub(super) fn packet(
        &mut self,
        live: bool,
        drained: bool,
        packet: PacketStatus,
    ) -> Result<bool, MicError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if packet.timestamp_error {
            self.failed = Some(MicError::Timestamp);
            return Err(MicError::Timestamp);
        }
        self.listening |= live || drained;
        if !self.listening && packet.frames != 0 {
            self.discarded_before_listening = self.discarded_before_listening.saturating_add(1);
        }
        if !live || !drained || packet.frames == 0 {
            return Ok(false);
        }
        if self.startup {
            self.startup = false;
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) fn kept(&mut self, packet: PacketStatus, at: Instant) {
        self.kept_packets = self.kept_packets.saturating_add(1);
        self.kept_frames = self.kept_frames.saturating_add(u64::from(packet.frames));
        if let Some(next) = self.next_position {
            self.missing_frames = self
                .missing_frames
                .saturating_add(packet.position.saturating_sub(next));
        }
        self.next_position = Some(packet.position.saturating_add(u64::from(packet.frames)));
        if packet.discontinuity {
            self.discontinuities = self.discontinuities.saturating_add(1);
            if self.discontinuities == 1 {
                self.first_discontinuity_packet = self.kept_packets;
                self.first_discontinuity_ms = self.started.map_or(0, |start| {
                    u64::try_from(at.saturating_duration_since(start).as_millis())
                        .unwrap_or(u64::MAX)
                });
            }
        }
    }

    pub(super) fn finish(&mut self, source: Result<(), MicError>) -> Result<(), MicError> {
        if self.failed.is_none() {
            self.failed = source.err();
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.missing_frames.saturating_mul(MISSING_SHARE_DIVISOR)
            > self.kept_frames.saturating_add(self.missing_frames)
        {
            self.failed = Some(MicError::Discontinuity);
            return Err(MicError::Discontinuity);
        }
        Ok(())
    }

    pub(super) fn log(&self) {
        tracing::info!(
            discontinuities = self.discontinuities,
            first_discontinuity_packet = self.first_discontinuity_packet,
            first_discontinuity_ms = self.first_discontinuity_ms,
            kept_packets = self.kept_packets,
            kept_frames = self.kept_frames,
            missing_frames = self.missing_frames,
            discarded_before_listening = self.discarded_before_listening,
            "held microphone capture summary"
        );
    }
}

/// Native input closes this atomic fence without borrowing GPUI.
#[derive(Clone, Debug, Default)]
pub(crate) struct CollectionGate(Arc<AtomicU64>);

impl CollectionGate {
    pub(crate) fn latest(&self) -> u64 {
        self.0.load(Ordering::Acquire) >> 3
    }

    /// Arms a strictly newer nonzero gesture only after the prior join receipt.
    pub(crate) fn arm(&self, gesture: GestureId) -> Result<Generation, MicError> {
        self.arm_at(gesture, Instant::now())
    }

    fn arm_at(&self, gesture: GestureId, at: Instant) -> Result<Generation, MicError> {
        let old = self.0.load(Ordering::Acquire);
        if gesture.0 == 0 || u64::from(gesture.0) <= old >> 3 || old & 7 != 0 && old & 7 != RETIRED
        {
            return Err(MicError::Stale);
        }
        let key = u64::from(gesture.0) << 3;
        self.0
            .compare_exchange(old, key | ARMED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MicError::Stale)?;
        Ok(Generation {
            gate: Arc::clone(&self.0),
            gesture,
            at,
            startup: Arc::new(OnceLock::new()),
        })
    }

    /// Normal release can never downgrade an earlier cancellation.
    pub(crate) fn release(&self, gesture: GestureId) {
        transition(&self.0, gesture, RELEASED);
    }

    /// Cancellation upgrades normal release and is terminal for this generation.
    pub(crate) fn cancel(&self, gesture: GestureId) {
        transition(&self.0, gesture, CANCELLED);
    }

    /// Reset/input-session loss needs no deferred gesture identifier lookup.
    pub(crate) fn cancel_current(&self) {
        let current = self.0.load(Ordering::Acquire);
        if let Some(id) = u32::try_from(current >> 3).ok().filter(|id| *id != 0) {
            self.cancel(GestureId(id));
        }
    }
}

fn transition(gate: &AtomicU64, gesture: GestureId, target: u64) {
    let key = u64::from(gesture.0) << 3;
    let _unchanged = gate.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        (old & 7 != 0 && old >> 3 == key >> 3 && old & 7 < target).then_some(key | target)
    });
}

/// A checked, single-start generation; cloning it cannot grant a second open.
#[derive(Clone, Debug)]
pub(crate) struct Generation {
    gate: Arc<AtomicU64>,
    gesture: GestureId,
    at: Instant,
    startup: Arc<OnceLock<Instant>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Preparing,
    Collecting,
    Release,
}

impl Generation {
    /// Retires denied admission only if no source start has claimed this generation.
    pub(crate) fn discard_unstarted(&self) -> Result<(), MicError> {
        self.startup
            .set(Instant::now())
            .map_err(|_| MicError::Stale)?;
        self.cancel();
        transition(&self.gate, self.gesture, RETIRED);
        Ok(())
    }

    pub(crate) fn gesture(&self) -> GestureId {
        self.gesture
    }

    pub(crate) fn retired(&self) -> bool {
        self.gate.load(Ordering::Acquire) == (u64::from(self.gesture.0) << 3 | RETIRED)
    }

    fn advance(&self, from: u64, to: u64) -> Result<(), MicError> {
        let key = u64::from(self.gesture.0) << 3;
        self.gate
            .compare_exchange(key | from, key | to, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| MicError::Stale)
    }

    pub(super) fn cancel(&self) {
        transition(&self.gate, self.gesture, CANCELLED);
    }

    pub(super) fn collecting(&self, session: &AtomicBool) -> bool {
        self.mode(session, Instant::now()) == Ok(Mode::Collecting)
    }

    pub(crate) fn mode(&self, session: &AtomicBool, now: Instant) -> Result<Mode, MicError> {
        let state = self.gate.load(Ordering::Acquire);
        if state >> 3 != u64::from(self.gesture.0) {
            return Err(MicError::Stale);
        }
        let error = if !session.load(Ordering::Acquire) {
            Some(MicError::SessionLost)
        } else if (state & 7 < COLLECTING
            && self.startup.get().is_some_and(|start| {
                now.saturating_duration_since(*start)
                    >= Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS))
            }))
            || (state & 7 < RELEASED && now.saturating_duration_since(self.at) >= HOLD_LIMIT)
        {
            Some(MicError::Deadline)
        } else {
            None
        };
        if let Some(error) = error {
            self.cancel();
            return Err(error);
        }
        match state & 7 {
            ARMED..=RUNNING => Ok(Mode::Preparing),
            COLLECTING => Ok(Mode::Collecting),
            RELEASED => Ok(Mode::Release),
            _ => Err(MicError::Cancelled),
        }
    }

    pub(crate) fn running(&self) -> Result<(), MicError> {
        self.advance(OPENING, RUNNING)
    }
}
/// Running is preparation evidence, never permission to copy samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Running {
    pub(crate) gesture: GestureId,
    pub(crate) rate: u32,
}

/// Finalization is reported only after the worker's native teardown and join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finalization {
    Released { packets: u32 },
    Discarded(MicError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    #[cfg(test)]
    Release,
    Cancel,
}

/// This receipt, unlike a stop request, releases the device generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StopReceipt {
    pub(crate) gesture: GestureId,
    pub(crate) finalization: Finalization,
}

/// One source worker. Dropping requests cancellation but is not a join receipt.
pub(crate) struct MicOwner {
    generation: Generation,
    session: Arc<AtomicBool>,
    wake: Arc<OnceLock<thread::Thread>>,
    completion: Receiver<Result<Finalization, MicError>>,
    reported: bool,
    pub(crate) running: Receiver<Running>,
    pub(crate) pcm: Receiver<MicPcm>,
}

struct WorkerCleanup {
    generation: Generation,
    discard: Receiver<MicPcm>,
    keep: bool,
}
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        if !self.keep {
            self.generation.cancel();
            while let Ok(mut packet) = self.discard.try_recv() {
                packet.samples.zeroize();
            }
        }
    }
}

impl fmt::Debug for MicOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MicOwner")
            .field("gesture", &self.generation.gesture)
            .field("queued_packets", &self.pcm.len())
            .finish_non_exhaustive()
    }
}

impl MicOwner {
    /// Caller must have Brain admission and must forward PCM on a separate worker.
    /// No device is touched on this calling thread.
    pub(crate) fn start(
        generation: Generation,
        session: Arc<AtomicBool>,
    ) -> Result<Self, MicError> {
        if cfg!(test) {
            return Self::start_with(generation, session, |_, _, _, _| {
                Ok(Finalization::Discarded(MicError::Unavailable))
            });
        }
        Self::start_with(generation, session, super::windows::microphone::capture)
    }

    pub(crate) fn start_with(
        generation: Generation,
        session: Arc<AtomicBool>,
        capture: impl FnOnce(
            &Generation,
            &AtomicBool,
            &async_channel::Sender<Running>,
            &async_channel::Sender<MicPcm>,
        ) -> Result<Finalization, MicError>
        + Send
        + 'static,
    ) -> Result<Self, MicError> {
        if generation.startup.set(Instant::now()).is_err() {
            return Err(MicError::Stale);
        }
        if let Err(error) = generation.advance(ARMED, OPENING) {
            generation.cancel();
            transition(&generation.gate, generation.gesture, RETIRED);
            return Err(error);
        }
        let (running_tx, running) = async_channel::bounded(1);
        let (pcm_tx, pcm) = async_channel::bounded(PCM_QUEUE);
        let token = generation.clone();
        let permit = Arc::clone(&session);
        let discard = pcm.clone();
        let wake = Arc::new(OnceLock::new());
        let source_wake = Arc::clone(&wake);
        let (completed, completion) = async_channel::bounded(1);
        let watch = generation.clone();
        let watch_permit = Arc::clone(&session);
        let reaper = thread::Builder::new()
            .spawn(move || {
                let worker = thread::Builder::new().spawn(move || {
                    let mut cleanup = WorkerCleanup {
                        generation: token.clone(),
                        discard,
                        keep: false,
                    };
                    if source_wake.set(thread::current()).is_err() {
                        return Err(MicError::Unconfirmed);
                    }
                    let result = capture(&token, &permit, &running_tx, &pcm_tx);
                    cleanup.keep = matches!(result, Ok(Finalization::Released { .. }));
                    result
                });
                let result = if let Ok(worker) = worker {
                    while !worker.is_finished() {
                        if watch.mode(&watch_permit, Instant::now()).is_err() {
                            worker.thread().unpark();
                        }
                        thread::park_timeout(POLL);
                    }
                    worker.join().unwrap_or(Err(MicError::Unconfirmed))
                } else {
                    watch.cancel();
                    Ok(Finalization::Discarded(MicError::Unavailable))
                };
                if completed.try_send(result).is_err() {
                    watch.cancel();
                }
            })
            .map_err(|_| {
                generation.cancel();
                transition(&generation.gate, generation.gesture, RETIRED);
                MicError::Unavailable
            })?;
        drop(reaper);
        Ok(Self {
            generation,
            session,
            wake,
            completion,
            reported: false,
            running,
            pcm,
        })
    }

    /// Call only after accepting this running receipt and publishing matching UI state.
    pub(crate) fn accept_running(&self, running: Running) -> Result<(), MicError> {
        if running.gesture != self.generation.gesture {
            return Err(MicError::Stale);
        }
        self.generation.mode(&self.session, Instant::now())?;
        self.generation.advance(RUNNING, COLLECTING)
    }

    /// The native edge should already have closed the gate; this is idempotent.
    pub(crate) fn stop(&self, reason: StopReason) {
        match reason {
            #[cfg(test)]
            StopReason::Release => {
                transition(&self.generation.gate, self.generation.gesture, RELEASED);
            }
            StopReason::Cancel => self.generation.cancel(),
        }
        if let Some(worker) = self.wake.get() {
            worker.unpark();
        }
    }

    /// Nonblocking polling; an error leaves the generation unavailable, not retired.
    #[cfg(test)]
    pub(crate) fn try_receipt(&mut self) -> Option<Result<StopReceipt, MicError>> {
        if self.reported {
            return None;
        }
        match self.completion.try_recv() {
            Ok(result) => {
                self.reported = true;
                Some(self.joined(&Ok(result)))
            }
            Err(async_channel::TryRecvError::Empty) => None,
            Err(async_channel::TryRecvError::Closed) => {
                self.reported = true;
                self.generation.cancel();
                Some(Err(MicError::Unconfirmed))
            }
        }
    }

    pub(crate) async fn receipt(&mut self) -> Result<StopReceipt, MicError> {
        if self.reported {
            return Err(MicError::Stale);
        }
        self.reported = true;
        let result = self
            .completion
            .recv()
            .await
            .unwrap_or(Err(MicError::Unconfirmed));
        self.joined(&Ok(result))
    }

    fn joined(
        &self,
        result: &thread::Result<Result<Finalization, MicError>>,
    ) -> Result<StopReceipt, MicError> {
        let Ok(Ok(finalization)) = result else {
            self.generation.cancel();
            return Err(MicError::Unconfirmed);
        };
        let mut finalization = *finalization;
        if matches!(finalization, Finalization::Released { .. }) {
            match self.generation.mode(&self.session, Instant::now()) {
                Ok(Mode::Release) => {}
                Err(error) => finalization = Finalization::Discarded(error),
                _ => finalization = Finalization::Discarded(MicError::Cancelled),
            }
        }
        if matches!(finalization, Finalization::Discarded(_)) {
            while let Ok(mut packet) = self.pcm.try_recv() {
                packet.samples.zeroize();
            }
        }
        transition(&self.generation.gate, self.generation.gesture, RETIRED);
        Ok(StopReceipt {
            gesture: self.generation.gesture,
            finalization,
        })
    }
}

impl Drop for MicOwner {
    fn drop(&mut self) {
        self.stop(StopReason::Cancel);
        while let Ok(mut packet) = self.pcm.try_recv() {
            packet.samples.zeroize();
        }
    }
}
