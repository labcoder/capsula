//! The brain's systems around the coordinator: the store thread, the fixture engine, timers,
//! and one shell connection's readers and writers.
//!
//! Each pipe has its own reader and writer, polled together in one task, so a reader never
//! waits behind a write (CAPS-8 finding 1). Esc reaches the coordinator even while the data
//! writer is blocked on a shell that stopped reading.

mod data;
pub(crate) mod fixture;
mod store_thread;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use capsula_protocol::frame::{self, Pipe};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain, DataToShell, RunId, ThreadId,
};
use capsula_protocol::tap::Direction;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;

use crate::coordinator::{Coordinator, Effect, Input};
use crate::ipc;
use crate::store::Store;
use crate::tap::Tap;
use data::DataQueue;
use fixture::Fixture;
use store_thread::StoreThread;

/// Control frames queued for the shell before it counts as hung (architecture v1 §3.2) [guess].
/// shortcut: no slots reserved for urgent frames and no coalescing of `RunState`; the brain sends
/// only `RunState` and `Ping` today, so the queue can't fill short of a hung shell. Add both
/// with the first urgent brain-to-shell message.
const CONTROL_QUEUE: usize = 256;
/// The watchdog's ping interval while a run is active [guess].
const PING_EVERY: Duration = Duration::from_millis(500);
/// How long a ping may go unanswered [guess].
const PONG_WITHIN: Duration = Duration::from_secs(2);

/// How a shell connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The shell sent `Quitting`.
    Quit,
    /// The shell went away or stopped answering.
    Lost,
    /// The shell broke the protocol after the handshake.
    Broken(&'static str),
}

/// What a reader hands the core loop.
enum Frame {
    Control(ControlToBrain),
    Data(DataToBrain),
}

/// The coordinator and the systems that outlive one shell connection.
#[derive(Debug)]
pub(crate) struct Brain {
    coordinator: Coordinator,
    store: Option<StoreThread>,
    fixture: Arc<Fixture>,
    inputs_tx: mpsc::UnboundedSender<Input>,
    inputs: mpsc::UnboundedReceiver<Input>,
    engine: Option<(RunId, oneshot::Sender<()>)>,
    deadline: Option<(RunId, Instant)>,
}

impl Brain {
    /// Starts the store thread. The store must already be recovered, and `high_water` is its
    /// highest thread and run ids.
    pub(crate) fn new(
        store: Store,
        high_water: (ThreadId, RunId),
        fixture: Fixture,
    ) -> std::io::Result<Self> {
        let (last_thread, last_run) = high_water;
        let (inputs_tx, inputs) = mpsc::unbounded_channel();
        let store = StoreThread::spawn(store, inputs_tx.clone())?;
        Ok(Self {
            coordinator: Coordinator::new(last_thread, last_run),
            store: Some(store),
            fixture: Arc::new(fixture),
            inputs_tx,
            inputs,
            engine: None,
            deadline: None,
        })
    }

    /// Lets queued store writes finish.
    pub(crate) fn close(mut self) {
        if let Some(store) = self.store.take() {
            store.close();
        }
    }

    /// Serves one authenticated shell until it quits, leaves or stops answering. Whatever run
    /// was active ends as `interrupted(shell_lost)`.
    pub(crate) async fn serve<C, D>(&mut self, control: C, data: D, tap: &mut Tap) -> Ended
    where
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut control_in, mut control_out) = tokio::io::split(control);
        let (mut data_in, mut data_out) = tokio::io::split(data);
        let (frames_tx, mut frames) = mpsc::channel::<Frame>(64);
        let (out_tx, mut out) = mpsc::channel::<Vec<u8>>(CONTROL_QUEUE);
        let queue = RefCell::new(DataQueue::new(window()));
        let ready = Notify::new();

        let control_reader = read_control(&mut control_in, &frames_tx);
        let data_reader = read_data(&mut data_in, &frames_tx);
        let control_writer = write_control(&mut control_out, &mut out);
        let data_writer = write_data(&mut data_out, &queue, &ready);
        let link = Link {
            out: &out_tx,
            queue: &queue,
            ready: &ready,
            tap: &mut *tap,
        };
        let core = self.core(&mut frames, link);
        let ended = tokio::select! {
            ended = control_reader => ended,
            ended = data_reader => ended,
            ended = control_writer => ended,
            ended = data_writer => ended,
            ended = core => ended,
        };
        if let Ok(effects) = self.coordinator.handle(Input::ShellLost) {
            // No shell to hear about it: only the store and the engine act, and nothing is full.
            let offline = self.apply(effects, None);
            debug_assert!(offline.is_ok(), "applying without a shell can't fail");
        }
        self.deadline = None;
        ended
    }

    /// The core loop: frames from the shell, completions from the systems, the deadline and
    /// the watchdog, each turned into coordinator inputs and their effects applied.
    async fn core(&mut self, frames: &mut mpsc::Receiver<Frame>, mut link: Link<'_, '_>) -> Ended {
        let mut ticks = tokio::time::interval(PING_EVERY);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ping_sent: Option<Instant> = None;
        loop {
            let running = self.coordinator.current_run();
            let deadline = self.deadline;
            let input = tokio::select! {
                biased;
                Some(frame) = frames.recv() => match frame {
                    Frame::Control(message) => {
                        link.tap.record(Pipe::Control, Direction::ToBrain, &message);
                        match message {
                            ControlToBrain::Stop { run, revealed } => {
                                link.queue.borrow_mut().revoke(run);
                                Input::Stop { run, revealed }
                            }
                            ControlToBrain::Pong => {
                                ping_sent = None;
                                continue;
                            }
                            // The readers end the connection on these.
                            ControlToBrain::Hello(_) | ControlToBrain::Quitting => continue,
                        }
                    }
                    Frame::Data(DataToBrain::TextSubmit { thread, text }) => {
                        if let Some(replaced) = running {
                            link.queue.borrow_mut().revoke(replaced);
                        }
                        Input::Turn { thread, text }
                    }
                    Frame::Data(DataToBrain::Credit { bytes }) => {
                        link.queue.borrow_mut().grant(bytes);
                        link.ready.notify_one();
                        continue;
                    }
                    Frame::Data(DataToBrain::Hello(_)) => continue,
                },
                Some(input) = self.inputs.recv() => input,
                () = sleep_until(deadline) => {
                    self.deadline = None;
                    match deadline {
                        Some((run, _)) => Input::Deadline { run },
                        None => continue,
                    }
                }
                _ = ticks.tick(), if running.is_some() => {
                    let now = Instant::now();
                    if ping_sent.is_some_and(|sent| now.duration_since(sent) > PONG_WITHIN) {
                        tracing::warn!("the shell stopped answering pings");
                        return Ended::Lost;
                    }
                    if ping_sent.is_none() {
                        ping_sent = Some(now);
                        if link.control(&ControlToShell::Ping).is_err() {
                            return Ended::Lost;
                        }
                    }
                    match running {
                        Some(run) if link.queue.borrow().stalled(now.into_std()) => Input::Stalled { run },
                        _ => continue,
                    }
                }
            };
            let Ok(effects) = self.coordinator.handle(input) else {
                continue;
            };
            if self.coordinator.current_run().is_none() {
                ping_sent = None;
                self.deadline = None;
            }
            if self.apply(effects, Some(&mut link)).is_err() {
                tracing::warn!("the shell's control queue is full");
                return Ended::Lost;
            }
        }
    }

    /// Applies effects in order. Frames go to `link`, or nowhere without a shell. Fails when
    /// the shell's control queue is full.
    fn apply(
        &mut self,
        effects: Vec<Effect>,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        for effect in effects {
            match effect {
                Effect::Store(op) => {
                    let run = op.run();
                    if !self.store.as_ref().is_some_and(|store| store.send(op)) {
                        tracing::error!("the store thread is gone");
                        let failed = self.inputs_tx.send(Input::StoreFailed { run });
                        debug_assert!(failed.is_ok(), "the brain holds its own receiver");
                    }
                }
                Effect::Control(message) => {
                    if let Some(link) = link.as_deref_mut() {
                        link.control(&message)?;
                    }
                }
                Effect::Data(message) => {
                    if let Some(link) = link.as_deref_mut() {
                        link.data(&message);
                    }
                }
                Effect::StartEngine { run, text: _ } => {
                    let cancel =
                        fixture::start(Arc::clone(&self.fixture), run, self.inputs_tx.clone());
                    self.engine = Some((run, cancel));
                }
                Effect::CancelEngine { run } => {
                    if let Some((engine_run, cancel)) = self.engine.take() {
                        if engine_run == run {
                            // An engine that already finished has dropped its receiver.
                            let _finished = cancel.send(());
                        } else {
                            self.engine = Some((engine_run, cancel));
                        }
                    }
                }
                Effect::ArmDeadline { run, after } => {
                    self.deadline = Some((run, Instant::now() + after));
                }
            }
        }
        Ok(())
    }
}

async fn read_control(pipe: &mut (impl AsyncRead + Unpin), frames: &mpsc::Sender<Frame>) -> Ended {
    loop {
        match ipc::read::<ControlToBrain>(pipe, Pipe::Control).await {
            Ok(Some(ControlToBrain::Hello(_))) => return Ended::Broken("Hello sent twice"),
            Ok(Some(ControlToBrain::Quitting)) => return Ended::Quit,
            Ok(Some(message)) => {
                if frames.send(Frame::Control(message)).await.is_err() {
                    return Ended::Lost;
                }
            }
            Ok(None) => return Ended::Lost,
            Err(error) => {
                tracing::warn!(%error, "control pipe failed");
                return Ended::Lost;
            }
        }
    }
}

async fn read_data(pipe: &mut (impl AsyncRead + Unpin), frames: &mpsc::Sender<Frame>) -> Ended {
    loop {
        match ipc::read::<DataToBrain>(pipe, Pipe::Data).await {
            Ok(Some(DataToBrain::Hello(_))) => return Ended::Broken("Hello sent twice"),
            Ok(Some(message)) => {
                if frames.send(Frame::Data(message)).await.is_err() {
                    return Ended::Lost;
                }
            }
            Ok(None) => return Ended::Lost,
            Err(error) => {
                tracing::warn!(%error, "data pipe failed");
                return Ended::Lost;
            }
        }
    }
}

async fn write_control(
    pipe: &mut (impl AsyncWrite + Unpin),
    out: &mut mpsc::Receiver<Vec<u8>>,
) -> Ended {
    while let Some(frame) = out.recv().await {
        if let Err(error) = pipe.write_all(&frame).await {
            tracing::warn!(%error, "control pipe failed");
            return Ended::Lost;
        }
    }
    Ended::Lost
}

async fn write_data(
    pipe: &mut (impl AsyncWrite + Unpin),
    queue: &RefCell<DataQueue>,
    ready: &Notify,
) -> Ended {
    loop {
        let next = queue.borrow_mut().pop(Instant::now().into_std());
        let Some(frame) = next else {
            ready.notified().await;
            continue;
        };
        if let Err(error) = pipe.write_all(&frame).await {
            tracing::warn!(%error, "data pipe failed");
            return Ended::Lost;
        }
        queue.borrow_mut().wrote();
    }
}

/// The shell's control queue is full, so the shell counts as hung.
#[derive(Debug)]
struct Full;

/// One connection's outgoing side.
struct Link<'a, 't> {
    out: &'a mpsc::Sender<Vec<u8>>,
    queue: &'a RefCell<DataQueue>,
    ready: &'a Notify,
    tap: &'t mut Tap,
}

impl Link<'_, '_> {
    fn control(&mut self, message: &ControlToShell) -> Result<(), Full> {
        self.tap.record(Pipe::Control, Direction::ToShell, message);
        match frame::encode(message, Pipe::Control) {
            Ok(frame) => self.out.try_send(frame).map_err(|_| Full),
            Err(error) => {
                tracing::error!(%error, "a control message didn't encode");
                Ok(())
            }
        }
    }

    /// Data frames carry content, so the tap never records them.
    fn data(&mut self, message: &DataToShell) {
        let run = match message {
            DataToShell::AssistantText { run, .. }
            | DataToShell::StepCommit { run, .. }
            | DataToShell::RunEnd { run } => *run,
        };
        match frame::encode(message, Pipe::Data) {
            Ok(frame) => {
                self.queue.borrow_mut().push(run, frame);
                self.ready.notify_one();
            }
            Err(error) => tracing::error!(%error, "a data message didn't encode"),
        }
    }
}

fn window() -> usize {
    usize::try_from(DATA_WINDOW).expect("the window fits in usize")
}

async fn sleep_until(deadline: Option<(RunId, Instant)>) {
    match deadline {
        Some((_, at)) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
