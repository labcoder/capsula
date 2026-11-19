//! The speaker: one connection's output device, fed by the link, with the played-content clock
//! behind it (docs/development/speaker.md). Nothing opens one in a normal launch:
//! [`crate::link::prepare`] attaches no [`Opener`].
//!
//! The link's data reader hands each speech frame through a capacity-1 channel and usually gets
//! the zeroed sample buffer back on another. Commands come on a bounded queue. The worker, which
//! a platform adapter owns, applies commands before data and sends its reports on the
//! connection's control queue.

mod clock;
mod engine;
#[cfg(test)]
pub(crate) mod fake;
#[expect(
    dead_code,
    reason = "the replacement queue stays dormant until the opener switch"
)]
mod queued;
mod tail;
#[cfg(test)]
#[path = "speaker_tests.rs"]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{ControlToBrain, OutputGeneration, RunId};
use zeroize::Zeroize;

pub use clock::{Fault, Observation, STALL};
pub use engine::{Data, Engine, Release, Staged, Taken};
pub use tail::{FAULT, RELEASED, Snapshot, Tail, UNKNOWN};

/// Frames a worker takes per pass, so the device is serviced between them.
const PACKETS: usize = 8;
/// Commands waiting for the worker, and the most it applies per pass.
const COMMANDS: usize = 16;
/// How long letting go of a speaker waits for its worker to finish [guess].
const TEARDOWN: Duration = Duration::from_secs(1);
/// How often a reader waiting on a busy worker checks that the speaker is still wanted.
const RETRY: Duration = Duration::from_millis(10);
/// A pass's wait once the device is gone.
const IDLE: Duration = Duration::from_millis(20);
/// The longest a retiring stream plays out what was written [guess]: the device's buffer is at
/// most 100 ms, and the clock's own stall deadline is 200 ms.
const DRAIN: Duration = Duration::from_millis(500);

/// A speech frame and its size on the pipe, which is credited once it's kept or dropped. Its
/// decoded samples are zeroed however it's let go.
pub struct Packet {
    /// The frame's bytes, for its credit.
    pub bytes: u32,
    /// The frame.
    pub data: Data,
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packet")
            .field("bytes", &self.bytes)
            .field("data", &self.data)
            .finish()
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        if let Data::Pcm(pcm) = &mut self.data {
            pcm.samples.zeroize();
        }
    }
}

/// Something the shell asks of its speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// The brain's revoke or a local Stop of `run` (see [`Engine::stop`]).
    Stop(RunId),
    /// See [`Engine::pause`].
    Pause(u32),
    /// See [`Engine::resume`].
    Resume(u32),
    /// The control pipe named a run (see [`Engine::name`]).
    Name(RunId),
    /// Talk-back off (see [`Engine::withdraw`]).
    Withdraw,
}

/// What a connection gives its speaker.
pub struct Outlet {
    /// The output generation this speaker announces.
    pub generation: OutputGeneration,
    /// The connection's control queue.
    pub control: async_channel::Sender<ControlToBrain>,
    /// Grants the brain data credit.
    pub grant: Box<dyn Fn(u32) + Send>,
    /// Where the generation's engine publishes what it owes.
    pub tail: Arc<Tail>,
    /// Wakes the output owner; a full channel already holds a wake.
    pub wake: async_channel::Sender<()>,
}

impl fmt::Debug for Outlet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outlet")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Opens a speaker for a connection; `None` leaves it without output.
#[derive(Clone)]
pub struct Opener(pub Arc<dyn Fn(Outlet) -> Option<Speaker> + Send + Sync>);

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Opener")
    }
}

/// The link's side of an open speaker. Dropping it ends the worker and waits up to
/// [`TEARDOWN`] for its thread, so it's dropped off the UI thread.
#[derive(Debug)]
pub struct Speaker {
    input: Option<Input>,
    commands: Option<SyncSender<Command>>,
    gone: Arc<AtomicBool>,
    accepted: Arc<AtomicBool>,
    wake: Arc<Mutex<Wake>>,
    worker: Option<JoinHandle<()>>,
}

/// What the worker's end wakes: nothing until the link arms it for an opened speaker.
#[derive(Default)]
struct Wake {
    end: Option<Box<dyn FnOnce() + Send>>,
    ended: bool,
}

impl fmt::Debug for Wake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wake")
            .field("armed", &self.end.is_some())
            .field("ended", &self.ended)
            .finish()
    }
}

/// The data reader's side: frames in, spent buffers back.
#[derive(Debug)]
pub struct Input {
    packets: SyncSender<Packet>,
    spent: Receiver<Vec<i16>>,
    gone: Arc<AtomicBool>,
}

impl Input {
    /// Hands the worker a frame, waiting while it holds the last one.
    ///
    /// # Errors
    /// The worker has gone or the link let go of the speaker, so the connection should end.
    pub fn send(&self, mut packet: Packet) -> Result<(), Packet> {
        loop {
            while self.spent.try_recv().is_ok() {}
            packet = match self.packets.try_send(packet) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(packet)) => return Err(packet),
                Err(TrySendError::Full(packet)) => packet,
            };
            if self.gone.load(Ordering::Acquire) {
                return Err(packet);
            }
            // shortcut: polls while the worker holds a frame; a wakeable wait if holds get long.
            thread::sleep(RETRY);
        }
    }
}

impl Speaker {
    /// The reader's side, once.
    pub fn input(&mut self) -> Option<Input> {
        self.input.take()
    }

    /// Hands the worker a command without waiting. False when its queue is full or it has
    /// gone; the caller ends the connection, since a revoke it can't deliver would leave
    /// speech playing.
    pub fn command(&self, command: Command) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.try_send(command).is_ok())
    }

    /// Another sender on the worker's command queue, for the output owner.
    pub(crate) fn commands(&self) -> Option<SyncSender<Command>> {
        self.commands.clone()
    }

    /// The thread that owns the worker; dropping the speaker joins it once it has finished.
    pub fn attach(&mut self, worker: JoinHandle<()>) {
        self.worker = Some(worker);
    }

    /// The output owner kept the speaker, fenced, for a current session: from now on its
    /// worker's reports go out.
    pub(crate) fn accept(&self) {
        self.accepted.store(true, Ordering::Release);
    }

    /// Calls `end` when the worker ends, or now if it already has. The link arms it only once
    /// the speaker opened, so a failed open never ends a session that goes on without output.
    pub fn arm(&self, end: Box<dyn FnOnce() + Send>) {
        let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        if wake.ended {
            drop(wake);
            end();
        } else {
            wake.end = Some(end);
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.gone.store(true, Ordering::Release);
        self.input = None;
        self.commands = None;
        let Some(worker) = self.worker.take() else {
            return;
        };
        let deadline = Instant::now() + TEARDOWN;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!("the speaker didn't stop in time; leaving its thread");
                return;
            }
            thread::sleep(RETRY);
        }
        if worker.join().is_err() {
            tracing::error!("the speaker thread panicked");
        }
    }
}

/// The worker's side: what an adapter's thread drives an [`Engine`] with.
pub struct Worker {
    packets: Receiver<Packet>,
    commands: Receiver<Command>,
    spent: SyncSender<Vec<i16>>,
    control: async_channel::Sender<ControlToBrain>,
    grant: Box<dyn Fn(u32) + Send>,
    generation: OutputGeneration,
    held: Option<Packet>,
    gone: Arc<AtomicBool>,
    accepted: Arc<AtomicBool>,
    wake: Arc<Mutex<Wake>>,
    tail: Arc<Tail>,
    owner: async_channel::Sender<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let end = {
            let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
            wake.ended = true;
            wake.end.take()
        };
        if let Some(end) = end {
            end();
        }
    }
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("generation", &self.generation)
            .field("held", &self.held.is_some())
            .finish_non_exhaustive()
    }
}

/// A speaker and its worker, joined by bounded channels.
pub fn pair(outlet: Outlet) -> (Speaker, Worker) {
    let (packets_in, packets) = mpsc::sync_channel(1);
    let (spent, spent_out) = mpsc::sync_channel(2);
    let (commands_in, commands) = mpsc::sync_channel(COMMANDS);
    let gone = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Mutex::new(Wake::default()));
    let speaker = Speaker {
        input: Some(Input {
            packets: packets_in,
            spent: spent_out,
            gone: Arc::clone(&gone),
        }),
        commands: Some(commands_in),
        gone: Arc::clone(&gone),
        accepted: Arc::clone(&accepted),
        wake: Arc::clone(&wake),
        worker: None,
    };
    let worker = Worker {
        packets,
        commands,
        spent,
        control: outlet.control,
        grant: outlet.grant,
        generation: outlet.generation,
        held: None,
        gone,
        accepted,
        wake,
        tail: outlet.tail,
        owner: outlet.wake,
    };
    (speaker, worker)
}

/// Runs `start` on the speaker's own thread with its worker and a channel for one answer: true
/// once its device runs. Only a true answer received before the wait of `within` times out makes
/// the speaker the caller's, still unaccepted: its reports, the announcement first, go out only
/// once the output owner accepts it. A no, a late yes or a thread that never started returns
/// `None`, and that worker publishes nothing; letting a refused speaker go takes up to
/// [`TEARDOWN`] more. The
/// thread wakes the output owner once more after its worker, engine and device are gone, on
/// return or unwinding, when nothing can write the tail again.
pub fn launch(
    outlet: Outlet,
    within: Duration,
    start: impl FnOnce(Worker, SyncSender<bool>) + Send + 'static,
) -> Option<Speaker> {
    let (ready, opened) = mpsc::sync_channel(1);
    let woken = Woken(outlet.wake.clone());
    let (mut speaker, worker) = pair(outlet);
    let thread = thread::Builder::new()
        .name("speaker".to_owned())
        .spawn(move || {
            let _woken = woken;
            start(worker, ready);
        })
        .ok()?;
    speaker.attach(thread);
    opened
        .recv_timeout(within)
        .unwrap_or(false)
        .then_some(speaker)
}

/// Wakes the output owner when dropped.
struct Woken(async_channel::Sender<()>);

impl Drop for Woken {
    fn drop(&mut self) {
        // A full channel already holds a wake.
        let _woken = self.0.try_send(());
    }
}

/// What a platform's render stream does for [`serve`].
pub trait Device {
    /// Waits for the device's next period, at most a pass.
    ///
    /// # Errors
    /// The device failed.
    fn wait(&self) -> Result<(), Fault>;
    /// Reads the device's clock and tops it up through [`Engine::stage`].
    ///
    /// # Errors
    /// The fault the device or the engine raised.
    fn service(&mut self, engine: &mut Engine) -> Result<(), Fault>;
    /// Stops the stream; false when the platform says it didn't.
    fn stop(&mut self) -> bool;
}

/// Serves a started device until the link lets go. A withdrawal or a fault retires the stream,
/// and the worker goes on without it, answering stops and crediting frames.
pub fn serve(mut worker: Worker, mut engine: Engine, device: impl Device) {
    let mut device = Some(device);
    loop {
        let waited = if let Some(device) = &device {
            device.wait()
        } else {
            thread::sleep(IDLE);
            Ok(())
        };
        let live = worker.ingest(&mut engine);
        if let Some(mut stream) = device.take() {
            if let Err(fault) = waited.and_then(|()| stream.service(&mut engine)) {
                engine.fail(fault);
            }
            if live && !engine.sealed() {
                device = Some(stream);
            } else {
                retire(stream, &mut engine);
            }
        }
        worker.flush(&mut engine);
        if !live {
            return;
        }
    }
}

/// Plays out what was written, as silence follows it, until the clock is quiet or faults or
/// [`DRAIN`] passes; then stops the stream and lets it go.
fn retire(mut device: impl Device, engine: &mut Engine) {
    let deadline = Instant::now() + DRAIN;
    while engine.fault().is_none() && !engine.drained() && Instant::now() < deadline {
        if let Err(fault) = device.wait().and_then(|()| device.service(engine)) {
            engine.fail(fault);
        }
    }
    if !device.stop() {
        tracing::warn!("the speaker's stream didn't stop cleanly");
    }
    drop(device);
    engine.released();
}

/// Answers that a device runs, then serves the worker with no device behind it until it ends.
#[cfg(test)]
fn idle(mut worker: Worker, ready: &SyncSender<bool>, rate: u32) {
    let _told = ready.send(true);
    let mut engine = Engine::new(
        rate,
        worker.generation(),
        10_000_000,
        worker.revocation(),
        worker.tail(),
    );
    while worker.ingest(&mut engine) {
        worker.flush(&mut engine);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    worker.flush(&mut engine);
}

/// A speaker whose device starts only once its caller has given up on it.
#[cfg(test)]
pub(crate) fn late(outlet: Outlet) -> Option<Speaker> {
    launch(outlet, Duration::from_millis(50), |worker, ready| {
        let given_up = worker.revocation();
        while !given_up.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        idle(worker, &ready, 24_000);
    })
}

impl Worker {
    /// The output generation the speaker announces.
    pub fn generation(&self) -> OutputGeneration {
        self.generation
    }

    /// Set once the link lets go of the speaker; the [`Engine`] stages only silence after it.
    pub fn revocation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.gone)
    }

    /// Where the [`Engine`] publishes this generation's tail.
    pub fn tail(&self) -> Arc<Tail> {
        Arc::clone(&self.tail)
    }

    /// Applies up to [`COMMANDS`] commands, then, once none is left, up to [`PACKETS`] frames.
    /// False once the link has let go, the brain sent speech that can't be true, or it stopped
    /// reading reports; the adapter then tears the device down.
    pub fn ingest(&mut self, engine: &mut Engine) -> bool {
        let mut drained = false;
        for _ in 0..COMMANDS {
            match self.commands.try_recv() {
                Ok(Command::Stop(run)) => engine.stop(run),
                Ok(Command::Pause(serial)) => engine.pause(serial),
                Ok(Command::Resume(serial)) => engine.resume(serial),
                Ok(Command::Name(run)) => engine.name(run),
                Ok(Command::Withdraw) => engine.withdraw(),
                Err(TryRecvError::Empty) => {
                    drained = true;
                    break;
                }
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        // Data waits while commands may still be queued, so it can't pass a fence.
        if drained && !self.take(engine) {
            return false;
        }
        engine.publish();
        !engine.backlogged()
    }

    fn take(&mut self, engine: &mut Engine) -> bool {
        for _ in 0..PACKETS {
            let Some(mut packet) = self.held.take().or_else(|| self.packets.try_recv().ok()) else {
                break;
            };
            match engine.data(&packet.data) {
                Ok(Taken::Held) => {
                    self.held = Some(packet);
                    break;
                }
                Ok(Taken::Kept | Taken::Stale) => {
                    (self.grant)(packet.bytes);
                    if let Data::Pcm(pcm) = &mut packet.data {
                        let mut samples = std::mem::take(&mut pcm.samples);
                        samples.zeroize();
                        // A full or closed return frees it here, already zeroed.
                        let _returned = self.spent.try_send(samples);
                    }
                }
                Err(_) => return false,
            }
        }
        true
    }

    /// Sends the engine's reports, in order, until the control queue is full; the rest wait for
    /// the next pass. Nothing goes out until the caller accepted the speaker (see [`launch`]). A
    /// closed queue drops them, since the connection has ended. Wakes the output owner when what
    /// a waiting claim depends on changed.
    pub fn flush(&mut self, engine: &mut Engine) {
        if engine.changed() {
            let _woken = self.owner.try_send(());
        }
        if !self.accepted.load(Ordering::Acquire) {
            return;
        }
        while let Some(report) = engine.reports.pop_front() {
            match self.control.try_send(report) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(report)) => {
                    engine.reports.push_front(report);
                    return;
                }
                Err(async_channel::TrySendError::Closed(_)) => {
                    engine.reports.clear();
                    return;
                }
            }
        }
    }
}

/// A speaker with no device behind it, for the link's tests: it keeps the queue and answers
/// stops, but no frame ever plays.
#[cfg(test)]
pub(crate) fn deviceless(outlet: Outlet, rate: u32) -> Speaker {
    launch(outlet, TEARDOWN, move |worker, ready| {
        idle(worker, &ready, rate);
    })
    .expect("a speaker with no device starts at once")
}
