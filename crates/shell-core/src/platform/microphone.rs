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

#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only native capture and PCM tests use this module"
    )
)]
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

#[cfg(windows)]
pub(super) use pcm::{Format, Packetizer, Render, Sample};

/// The capture pump's sleep while the endpoint is empty, one wire chunk long; the reaper and
/// the UI's hold watcher reuse it as their check interval.
pub const POLL: Duration = Duration::from_millis(20);
/// One wire chunk, as the packetizer cuts them.
#[cfg_attr(
    all(not(windows), not(any(test, feature = "test-support"))),
    expect(
        dead_code,
        reason = "only native or injected capture sizes a PCM queue"
    )
)]
const CHUNK: Duration = Duration::from_millis(20);
/// A whole capture buffer drained in one pass, plus three chunks for the forwarder to lag.
#[expect(
    clippy::as_conversions,
    reason = "a chunk count of a 100 ms buffer fits usize"
)]
#[cfg_attr(
    all(not(windows), not(any(test, feature = "test-support"))),
    expect(
        dead_code,
        reason = "only native or injected capture creates a PCM queue"
    )
)]
const PCM_QUEUE: usize = (CAPTURE_BUFFER.as_millis() / CHUNK.as_millis()) as usize + 3;

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
pub enum MicError {
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
    #[cfg(not(windows))]
    #[error("native microphone capture is unsupported on this platform")]
    Unsupported,
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
#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only native capture and packet tests inspect device status"
    )
)]
pub(super) struct PacketStatus {
    pub(super) frames: u32,
    /// The device's stream position of the packet's first frame, in frames.
    pub(super) position: u64,
    pub(super) discontinuity: bool,
    pub(super) timestamp_error: bool,
}

/// The endpoint buffer the adapter asks for: five polls, so the pump can stall up to about
/// 100 ms before the device overruns and drops audio.
#[cfg_attr(
    all(not(windows), not(any(test, feature = "test-support"))),
    expect(
        dead_code,
        reason = "only native or injected capture sizes a PCM queue"
    )
)]
pub(super) const CAPTURE_BUFFER: Duration = Duration::from_millis(100);

/// A hold fails when more than one frame in this many of its span, from the first kept frame
/// to the last, never reached it: sparse losses pass and a mostly lost hold doesn't.
#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "native capture and packet tests enforce this loss bound"
    )
)]
const MISSING_SHARE_DIVISOR: u64 = 4;

#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only native capture and packet tests track packet boundaries"
    )
)]
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
    #[cfg(windows)]
    queue_high_water: usize,
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
            #[cfg(windows)]
            queue_high_water: 0,
            discarded_before_listening: 0,
        }
    }
}

#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only native capture and packet tests track packet boundaries"
    )
)]
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
        // shortcut: a position that restarts, goes backwards or always reads 0 counts nothing
        // missing, so the hold passes; count those steps in the summary if a desk log shows them.
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

    #[cfg(windows)]
    pub(super) fn queued(&mut self, high_water: usize) {
        self.queue_high_water = self.queue_high_water.max(high_water);
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

    #[cfg(windows)]
    pub(super) fn log(&self) {
        tracing::info!(
            discontinuities = self.discontinuities,
            first_discontinuity_packet = self.first_discontinuity_packet,
            first_discontinuity_ms = self.first_discontinuity_ms,
            kept_packets = self.kept_packets,
            kept_frames = self.kept_frames,
            missing_frames = self.missing_frames,
            queue_high_water = self.queue_high_water,
            discarded_before_listening = self.discarded_before_listening,
            "held microphone capture summary"
        );
    }
}

/// Native input closes this atomic fence without borrowing GPUI.
#[derive(Clone, Debug, Default)]
pub struct CollectionGate(Arc<AtomicU64>);

impl CollectionGate {
    pub fn latest(&self) -> u64 {
        self.0.load(Ordering::Acquire) >> 3
    }

    /// Arms a strictly newer nonzero gesture only after the prior join receipt.
    pub fn arm(&self, gesture: GestureId) -> Result<Generation, MicError> {
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
    pub fn release(&self, gesture: GestureId) {
        transition(&self.0, gesture, RELEASED);
    }

    /// Cancellation upgrades normal release and is terminal for this generation.
    pub fn cancel(&self, gesture: GestureId) {
        transition(&self.0, gesture, CANCELLED);
    }

    /// Reset/input-session loss needs no deferred gesture identifier lookup.
    pub fn cancel_current(&self) {
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
pub struct Generation {
    gate: Arc<AtomicU64>,
    gesture: GestureId,
    at: Instant,
    startup: Arc<OnceLock<Instant>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Preparing,
    Collecting,
    Release,
}

impl Generation {
    /// Retires denied admission only if no source start has claimed this generation.
    pub fn discard_unstarted(&self) -> Result<(), MicError> {
        self.startup
            .set(Instant::now())
            .map_err(|_| MicError::Stale)?;
        self.cancel();
        transition(&self.gate, self.gesture, RETIRED);
        Ok(())
    }

    pub fn gesture(&self) -> GestureId {
        self.gesture
    }

    pub fn hold_deadline(&self) -> Instant {
        self.at + HOLD_LIMIT
    }

    pub fn retired(&self) -> bool {
        self.gate.load(Ordering::Acquire) == (u64::from(self.gesture.0) << 3 | RETIRED)
    }

    /// Retired, or a newer gesture armed, which needs this one retired first.
    pub fn finished(&self) -> bool {
        let state = self.gate.load(Ordering::Acquire);
        state >> 3 > u64::from(self.gesture.0) || self.retired()
    }

    /// Claims this generation's single start (`Armed → Opening`), for [`MicOwner::start_reserved`].
    /// A refused claim retires it.
    pub fn reserve(&self) -> Result<Reserved, MicError> {
        self.startup
            .set(Instant::now())
            .map_err(|_| MicError::Stale)?;
        if let Err(error) = self.advance(ARMED, OPENING) {
            self.cancel();
            transition(&self.gate, self.gesture, RETIRED);
            return Err(error);
        }
        Ok(Reserved(Some(self.clone())))
    }

    fn advance(&self, from: u64, to: u64) -> Result<(), MicError> {
        let key = u64::from(self.gesture.0) << 3;
        self.gate
            .compare_exchange(key | from, key | to, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| MicError::Stale)
    }

    pub(crate) fn cancel(&self) {
        transition(&self.gate, self.gesture, CANCELLED);
    }

    #[cfg_attr(
        all(not(windows), not(test)),
        expect(
            dead_code,
            reason = "native capture and its tests poll collection authority"
        )
    )]
    pub(super) fn collecting(&self, session: &AtomicBool) -> bool {
        self.mode(session, Instant::now()) == Ok(Mode::Collecting)
    }

    pub fn mode(&self, session: &AtomicBool, now: Instant) -> Result<Mode, MicError> {
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

    #[cfg_attr(
        all(not(windows), not(any(test, feature = "test-support"))),
        expect(
            dead_code,
            reason = "native capture and fixture sources publish readiness"
        )
    )]
    pub(crate) fn running(&self) -> Result<(), MicError> {
        self.advance(OPENING, RUNNING)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn running_for_tests(&self) -> Result<(), MicError> {
        self.running()
    }
}
/// A generation's claimed single start. Dropped unstarted, it retires the generation, since no
/// worker ever ran.
#[derive(Debug)]
pub struct Reserved(Option<Generation>);

impl Drop for Reserved {
    fn drop(&mut self) {
        if let Some(generation) = self.0.take() {
            generation.cancel();
            transition(&generation.gate, generation.gesture, RETIRED);
        }
    }
}

/// Running is preparation evidence, never permission to copy samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Running {
    pub(crate) gesture: GestureId,
    pub(crate) rate: u32,
}

#[cfg(any(test, feature = "test-support"))]
impl Running {
    pub fn for_tests(gesture: GestureId, rate: u32) -> Self {
        Self { gesture, rate }
    }
}

/// Finalization is reported only after the worker's native teardown and join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finalization {
    Released { packets: u32 },
    Discarded(MicError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    #[cfg(any(test, feature = "test-support"))]
    Release,
    Cancel,
}

/// This receipt, unlike a stop request, releases the device generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopReceipt {
    pub(crate) gesture: GestureId,
    pub finalization: Finalization,
}

/// One source worker. Dropping requests cancellation but is not a join receipt.
pub struct MicOwner {
    generation: Generation,
    session: Arc<AtomicBool>,
    wake: Arc<OnceLock<thread::Thread>>,
    completion: Receiver<Result<Finalization, MicError>>,
    reported: bool,
    pub running: Receiver<Running>,
    pub pcm: Receiver<MicPcm>,
}

#[cfg_attr(
    all(not(windows), not(any(test, feature = "test-support"))),
    expect(dead_code, reason = "only native or injected capture creates a worker")
)]
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
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the generation keeps its one start with the caller that used it"
    )]
    pub fn start(generation: Generation, session: Arc<AtomicBool>) -> Result<Self, MicError> {
        Self::start_reserved(generation.reserve()?, session)
    }

    /// Starts a generation whose single start was claimed (see [`Generation::reserve`]).
    #[cfg(windows)]
    pub fn start_reserved(
        mut reserved: Reserved,
        session: Arc<AtomicBool>,
    ) -> Result<Self, MicError> {
        let generation = reserved.0.take().ok_or(MicError::Stale)?;
        if cfg!(any(test, feature = "test-support")) {
            return Self::spawn(generation, session, |_, _, _, _| {
                Ok(Finalization::Discarded(MicError::Unavailable))
            });
        }
        Self::spawn(generation, session, super::windows::microphone::capture)
    }

    #[cfg(not(windows))]
    pub fn start_reserved(_: Reserved, _: Arc<AtomicBool>) -> Result<Self, MicError> {
        Err(MicError::Unsupported)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn start_with_for_tests(
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
        Self::start_with(generation, session, capture)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn start_reserved_with_for_tests(
        mut reserved: Reserved,
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
        let generation = reserved.0.take().ok_or(MicError::Stale)?;
        Self::spawn(generation, session, capture)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the generation keeps its one start with the caller that used it"
    )]
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
        let mut reserved = generation.reserve()?;
        let generation = reserved.0.take().ok_or(MicError::Stale)?;
        Self::spawn(generation, session, capture)
    }

    #[cfg_attr(
        all(not(windows), not(any(test, feature = "test-support"))),
        expect(dead_code, reason = "only native or injected capture starts a worker")
    )]
    fn spawn(
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

    /// Whether this worker runs `generation` itself, not another with the same gesture number.
    pub(crate) fn runs(&self, generation: &Generation) -> bool {
        Arc::ptr_eq(&self.generation.startup, &generation.startup)
    }

    /// Whether this worker's generation still runs uncollected: no key-up, cancel or end yet.
    pub(crate) fn still_running(&self) -> bool {
        self.generation.mode(&self.session, Instant::now()) == Ok(Mode::Preparing)
    }

    /// Call only after accepting this running receipt and publishing matching UI state.
    pub fn accept_running(&self, running: Running) -> Result<(), MicError> {
        if running.gesture != self.generation.gesture {
            return Err(MicError::Stale);
        }
        self.generation.mode(&self.session, Instant::now())?;
        self.generation.advance(RUNNING, COLLECTING)
    }

    /// The native edge should already have closed the gate; this is idempotent.
    pub fn stop(&self, reason: StopReason) {
        match reason {
            #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only this crate's tests poll for the receipt")
    )]
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

    pub async fn receipt(&mut self) -> Result<StopReceipt, MicError> {
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
