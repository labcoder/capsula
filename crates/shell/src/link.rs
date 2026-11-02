//! The shell's connection to the brain. Connecting and watching the control pipe run on the link
//! thread, because pipe reads block. Once connected, each pipe gets its own writer thread, fed by
//! channels the UI thread sends on without blocking, and the data pipe gets its own reader
//! thread, so no read waits behind a write and the UI never waits on the brain. Everything the
//! brain sends reaches the UI thread as [`LinkEvent`]s on one channel.
//!
//! Connecting starts the brain when no brain pipe answers, authenticates with the capability
//! from Credential Manager, and checks that the pipe's server is the brain beside the shell.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::BRAIN_EXE;
use capsula_protocol::frame::{self, FrameError, Pipe};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Hello, Incarnation,
    MAX_PENDING_TURNS, Phase, RunId, ThreadId,
};

use crate::credential::{self, CredentialError};
use crate::platform::{self, PipeClient};

/// How long one connection attempt may take, starting the brain included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between tries while the brain starts or rotates its capability.
const RETRY_DELAY: Duration = Duration::from_millis(50);
/// How long each handshake read or write may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// `ERROR_PIPE_BUSY`: the pipe exists but another client holds it.
const PIPE_BUSY: i32 = 231;
/// How long Quit waits for the brain to take `Quitting` and close the pipe.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);
/// A second brain loss within this long needs the user's Retry [guess, v1 §2.3].
const REPEAT_WINDOW: Duration = Duration::from_secs(60);
/// Control messages waiting for the brain to read them before a send fails [guess]. The shell
/// sends a handful at a time: Stop, `ClearDrawing`, `Pong` and `Quitting`.
const CONTROL_QUEUE: usize = 64;
/// Data messages waiting for the brain to read them: the turns the shell may leave unanswered,
/// and room for credit.
const DATA_QUEUE: usize = MAX_PENDING_TURNS + 8;
/// What the data reader buffers, so a small frame doesn't cost a read of its own [guess].
const READ_BUFFER: usize = 64 * 1024;

/// What the UI shows about the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

/// What the link tells the UI thread, in the order it happened. Nothing from the brain comes
/// before `State(Connected)`, and none of a connection's messages after its next state.
#[derive(Debug)]
pub(crate) enum LinkEvent {
    State(LinkState),
    /// A run's phase changed, from the control pipe.
    RunState {
        run: RunId,
        thread: ThreadId,
        phase: Phase,
    },
    /// A data frame, with its size on the pipe, header included, for the credit it earns once
    /// the UI has taken it.
    Data {
        message: DataToShell,
        bytes: u32,
    },
}

/// Why a connection attempt failed.
#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("no brain pipe")]
    NoBrain,
    #[error("the brain pipe is busy")]
    Busy,
    #[error("can't start the brain")]
    Launch(#[source] std::io::Error),
    #[error("the pipe is served by {0:?}, not the brain beside the shell")]
    NotOurBrain(PathBuf),
    #[error("no capability yet")]
    Capability(#[from] CredentialError),
    #[error("the brain closed the connection during the handshake")]
    Rejected,
    #[error("the brain is from another build")]
    BuildMismatch,
    #[error("quitting")]
    Quitting,
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(std::io::Error),
}

/// An authenticated connection. Dropping it closes both pipes.
#[derive(Debug)]
struct Connection {
    control: Arc<PipeClient>,
    data: Arc<PipeClient>,
    incarnation: Incarnation,
}

/// What the data writer sends.
enum DataOut {
    Submit(DataToBrain),
    /// Credit is waiting in [`Outbox::credit`].
    Credit,
}

/// Where the UI's messages go while connected.
#[derive(Debug)]
struct Outbox {
    control: Sender<ControlToBrain>,
    data: Sender<DataOut>,
    /// Credit granted and not yet sent; the data writer sends it after each message it writes.
    credit: Arc<AtomicU32>,
}

/// What the UI thread and the link thread share.
#[derive(Debug, Default)]
struct Shared {
    /// Set once the user chose Quit, so a closing brain isn't restarted.
    quitting: AtomicBool,
    /// The control pipe while connected, so Quit can bound its writes.
    control: Mutex<Option<Arc<PipeClient>>>,
    outbox: Mutex<Option<Outbox>>,
}

impl Shared {
    fn outbox<T>(&self, use_it: impl FnOnce(&Outbox) -> T) -> Option<T> {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(use_it)
    }
}

/// The UI's handle on the link.
#[derive(Clone, Debug)]
pub(crate) struct LinkHandle {
    shared: Arc<Shared>,
    retry: Sender<()>,
    /// Closes when the link thread ends.
    done: Receiver<()>,
}

impl LinkHandle {
    /// Marks Quit, so the link starts no brain from here on. Cheap enough for the UI thread.
    /// Returns false when Quit was already under way.
    pub(crate) fn begin_quit(&self) -> bool {
        let first = !self.shared.quitting.swap(true, Ordering::SeqCst);
        // Wakes a link waiting for Retry, so it sees Quit and ends.
        let _woken = self.retry.try_send(());
        first
    }

    /// Tells a connected brain the user quit, so it ends its helpers and exits instead of
    /// relaunching the shell, then waits for the link to end. A link that's still connecting
    /// sends `Quitting` itself once connected. Blocks for up to [`CONNECT_TIMEOUT`] plus
    /// [`QUIT_TIMEOUT`], so call it off the UI thread.
    pub(crate) fn finish_quit(&self) {
        let control = self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(control) = control {
            // A stalled brain can't hold the shell open.
            control.set_timeout(Some(QUIT_TIMEOUT));
            if !self.send_control(ControlToBrain::Quitting) {
                tracing::warn!("couldn't tell the brain about Quit");
            }
        }
        // A brain this link already started must connect before it can be told.
        let deadline = Instant::now() + CONNECT_TIMEOUT + QUIT_TIMEOUT;
        // shortcut: polls for the link's end; a joinable handle would avoid it, once Quit
        // needs more than this one wait.
        while !self.done.is_closed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !self.done.is_closed() {
            tracing::warn!("the brain didn't close the pipe after Quit");
        }
    }

    /// Tries to reach the brain again after `Failed`.
    pub(crate) fn retry(&self) {
        if self.retry.try_send(()).is_err() {
            tracing::debug!("a retry is already pending");
        }
    }

    /// Queues a control message for the brain. False while no brain is connected, or when it has
    /// stopped reading. Never blocks.
    pub(crate) fn send_control(&self, message: ControlToBrain) -> bool {
        self.shared
            .outbox(|outbox| outbox.control.try_send(message).is_ok())
            .unwrap_or(false)
    }

    /// Queues a typed turn for the brain, like [`LinkHandle::send_control`].
    pub(crate) fn send_data(&self, message: DataToBrain) -> bool {
        self.shared
            .outbox(|outbox| outbox.data.try_send(DataOut::Submit(message)).is_ok())
            .unwrap_or(false)
    }

    /// Grants the brain `bytes` more of data frames, once the UI has taken a frame that size.
    /// Grants add up until the data writer sends them. Never blocks.
    pub(crate) fn grant(&self, bytes: u32) {
        self.shared.outbox(|outbox| {
            // The writer sends whatever has added up after each message, so only the first grant
            // since the last send needs to wake it; a full queue wakes it anyway.
            if outbox.credit.fetch_add(bytes, Ordering::SeqCst) == 0 {
                let _woken = outbox.data.try_send(DataOut::Credit);
            }
        });
    }
}

/// Where to find the brain.
#[derive(Debug)]
struct Link {
    brain: PathBuf,
    session: u32,
    shared: Arc<Shared>,
}

/// A link that hasn't started connecting, so the UI can arm the keyboard first. Dropping it
/// unstarted ends the link, and Quit then waits for nothing.
#[derive(Debug)]
pub(crate) struct LinkStart {
    link: Link,
    events: Sender<LinkEvent>,
    retries: Receiver<()>,
    alive: Sender<()>,
}

/// Prepares the link. It reports on `events` once started, and ends when the UI drops the
/// receiver or the user quits.
///
/// # Errors
/// Fails when the shell can't find its own path or session.
pub(crate) fn prepare(events: Sender<LinkEvent>) -> std::io::Result<(LinkHandle, LinkStart)> {
    let link = Link {
        brain: std::env::current_exe()?.with_file_name(BRAIN_EXE),
        session: platform::session_id()?,
        shared: Arc::new(Shared::default()),
    };
    Ok(start(link, events))
}

fn start(link: Link, events: Sender<LinkEvent>) -> (LinkHandle, LinkStart) {
    let (retry, retries) = async_channel::bounded(1);
    let (alive, done) = async_channel::bounded::<()>(1);
    let handle = LinkHandle {
        shared: Arc::clone(&link.shared),
        retry,
        done,
    };
    let start = LinkStart {
        link,
        events,
        retries,
        alive,
    };
    (handle, start)
}

impl LinkStart {
    /// Starts connecting on the link thread, which may start the brain.
    ///
    /// # Errors
    /// Fails when the thread can't start.
    pub(crate) fn spawn(self) -> std::io::Result<()> {
        let Self {
            link,
            events,
            retries,
            alive,
        } = self;
        thread::Builder::new()
            .name("link".to_owned())
            .spawn(move || {
                link.run(&events, &retries);
                drop(alive);
            })?;
        Ok(())
    }
}

impl Link {
    /// Connects, attends, and reconnects after a loss. The first loss relaunches the brain on
    /// its own; another within [`REPEAT_WINDOW`], or a failed connect, waits for Retry.
    fn run(&self, events: &Sender<LinkEvent>, retries: &Receiver<()>) {
        let mut state = LinkState::Connecting;
        let mut last_loss: Option<Instant> = None;
        loop {
            if self.quitting() || events.send_blocking(LinkEvent::State(state)).is_err() {
                return;
            }
            let connection = match self.connect() {
                Ok(connection) => connection,
                Err(_) if self.quitting() => return,
                Err(error) => {
                    tracing::error!(%error, "can't reach the brain");
                    if !wait_for_retry(events, retries) {
                        return;
                    }
                    (state, last_loss) = (LinkState::Connecting, None);
                    continue;
                }
            };
            tracing::info!(incarnation = ?connection.incarnation, "connected to the brain");
            if !self.attend(connection, events) || self.quitting() {
                return;
            }
            tracing::warn!("lost the brain");
            let now = Instant::now();
            if last_loss.is_some_and(|at| now.duration_since(at) < REPEAT_WINDOW) {
                if !wait_for_retry(events, retries) {
                    return;
                }
                (state, last_loss) = (LinkState::Connecting, None);
                continue;
            }
            (state, last_loss) = (LinkState::Reconnecting, Some(now));
        }
    }

    fn quitting(&self) -> bool {
        self.shared.quitting.load(Ordering::SeqCst)
    }

    /// Serves one connection until it ends: starts the writers, says Connected, then reads data
    /// on its own thread and control here. False when the UI has gone.
    fn attend(&self, connection: Connection, events: &Sender<LinkEvent>) -> bool {
        let Connection { control, data, .. } = connection;
        let (control_out, control_queue) = async_channel::bounded(CONTROL_QUEUE);
        let (data_out, data_queue) = async_channel::bounded(DATA_QUEUE);
        let credit = Arc::new(AtomicU32::new(0));
        let mut helpers = Vec::new();
        let spawned = spawn_helper("link-control-writer", &mut helpers, {
            let control = Arc::clone(&control);
            move || write_control(&control, &control_queue)
        })
        .and_then(|()| {
            spawn_helper("link-data-writer", &mut helpers, {
                let (data, credit) = (Arc::clone(&data), Arc::clone(&credit));
                move || write_data(&data, &data_queue, &credit)
            })
        });
        *self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&control));
        *self
            .shared
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Outbox {
            control: control_out.clone(),
            data: data_out,
            credit,
        });
        // Quit may have come while connecting, when there was no pipe to send it on.
        if self.quitting() {
            control.set_timeout(Some(QUIT_TIMEOUT));
            let _queued = control_out.try_send(ControlToBrain::Quitting);
        }
        let open = events
            .send_blocking(LinkEvent::State(LinkState::Connected))
            .is_ok();
        let read = spawned.and_then(|()| {
            // Only now, so nothing from the brain reaches the UI before Connected.
            spawn_helper("link-data-reader", &mut helpers, {
                let (data, control, events) =
                    (Arc::clone(&data), Arc::clone(&control), events.clone());
                move || read_data(&data, &control, &events)
            })
        });
        match read {
            Ok(()) if open => watch(&control, &control_out, events),
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "can't start the link's threads"),
        }
        *self
            .shared
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        drop(control_out);
        // The writers and the reader return once their pipes fail.
        control.abort();
        data.abort();
        for helper in helpers {
            if helper.join().is_err() {
                tracing::error!("a link thread panicked");
            }
        }
        open
    }

    /// Connects, starting the brain once if none answers, until [`CONNECT_TIMEOUT`].
    fn connect(&self) -> Result<Connection, LinkError> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut launched = false;
        loop {
            let error = match self.handshake() {
                Ok(connection) => return Ok(connection),
                Err(LinkError::NoBrain) if self.quitting() && !launched => {
                    return Err(LinkError::Quitting);
                }
                Err(LinkError::NoBrain) if !launched => {
                    platform::launch_detached(&self.brain).map_err(LinkError::Launch)?;
                    launched = true;
                    continue;
                }
                // That brain exits by itself; start ours once its pipe is gone.
                Err(LinkError::BuildMismatch) => {
                    launched = false;
                    LinkError::BuildMismatch
                }
                Err(error @ LinkError::NotOurBrain(_)) => return Err(error),
                Err(error) => error,
            };
            // A brain we started is kept so it can be told to quit; otherwise nothing waits on us.
            if self.quitting() && !launched {
                return Err(LinkError::Quitting);
            }
            if Instant::now() >= deadline {
                return Err(error);
            }
            tracing::debug!(%error, "retrying");
            thread::sleep(RETRY_DELAY);
        }
    }

    fn handshake(&self) -> Result<Connection, LinkError> {
        let control = open(&Pipe::Control.name(self.session), &self.brain)?;
        // A brain that hangs mid-handshake must not hang the shell with it.
        control.set_timeout(Some(HANDSHAKE_TIMEOUT));
        let hello = Hello {
            build_id: BUILD_ID.to_owned(),
            capability: credential::read(self.session)?,
        };
        frame::write(
            &mut &control,
            &ControlToBrain::Hello(hello.clone()),
            Pipe::Control,
        )?;
        let incarnation = match frame::read::<ControlToShell>(&mut &control, Pipe::Control)? {
            None | Some(ControlToShell::RunState { .. } | ControlToShell::Ping) => {
                return Err(LinkError::Rejected);
            }
            Some(ControlToShell::BuildMismatch { brain_build }) => {
                tracing::warn!(
                    brain_build,
                    shell_build = BUILD_ID,
                    "brain from another build"
                );
                return Err(LinkError::BuildMismatch);
            }
            Some(ControlToShell::Welcome { incarnation }) => incarnation,
        };
        let data = open(&Pipe::Data.name(self.session), &self.brain)?;
        frame::write(&mut &data, &DataToBrain::Hello(hello), Pipe::Data)?;
        control.set_timeout(None);
        Ok(Connection {
            control: Arc::new(control),
            data: Arc::new(data),
            incarnation,
        })
    }
}

fn spawn_helper(
    name: &str,
    helpers: &mut Vec<JoinHandle<()>>,
    run: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    helpers.push(thread::Builder::new().name(name.to_owned()).spawn(run)?);
    Ok(())
}

/// Shows `Failed` and waits for the user's Retry; false when the UI has gone.
fn wait_for_retry(events: &Sender<LinkEvent>, retries: &Receiver<()>) -> bool {
    events
        .send_blocking(LinkEvent::State(LinkState::Failed))
        .is_ok()
        && retries.recv_blocking().is_ok()
}

/// Opens a pipe and checks that the brain beside the shell serves it.
fn open(name: &str, brain: &Path) -> Result<PipeClient, LinkError> {
    let pipe = PipeClient::open(name).map_err(|error| match error.raw_os_error() {
        _ if error.kind() == std::io::ErrorKind::NotFound => LinkError::NoBrain,
        Some(PIPE_BUSY) => LinkError::Busy,
        _ => LinkError::Io(error),
    })?;
    let server = pipe.server_image().map_err(LinkError::Io)?;
    if !server.as_os_str().eq_ignore_ascii_case(brain.as_os_str()) {
        return Err(LinkError::NotOurBrain(server));
    }
    Ok(pipe)
}

/// Reads the control pipe until the connection ends: answers pings and passes run states on. A
/// repeated handshake ends the connection, as the brain treats a protocol violation.
fn watch(control: &PipeClient, out: &Sender<ControlToBrain>, events: &Sender<LinkEvent>) {
    loop {
        match frame::read::<ControlToShell>(&mut &*control, Pipe::Control) {
            Ok(None) => return,
            Ok(Some(ControlToShell::Ping)) => {
                if out.try_send(ControlToBrain::Pong).is_err() {
                    tracing::warn!("the control queue is full; the brain isn't reading");
                }
            }
            Ok(Some(ControlToShell::RunState { run, thread, phase })) => {
                if events
                    .send_blocking(LinkEvent::RunState { run, thread, phase })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Welcome { .. } | ControlToShell::BuildMismatch { .. })) => {
                tracing::warn!("the brain repeated its handshake; ending the connection");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "control pipe failed");
                return;
            }
        }
    }
}

/// Counts the bytes read through it, so each frame's size is known for its credit.
struct Counted<R> {
    inner: R,
    bytes: usize,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes += read;
        Ok(read)
    }
}

/// Reads data frames and passes them on until the pipe ends. A malformed frame, or the data pipe
/// ending, ends the whole connection, so the link reconnects instead of leaving the shell
/// connected with nothing arriving.
fn read_data(data: &PipeClient, control: &PipeClient, events: &Sender<LinkEvent>) {
    let mut reader = Counted {
        inner: BufReader::with_capacity(READ_BUFFER, data),
        bytes: 0,
    };
    loop {
        reader.bytes = 0;
        match frame::read::<DataToShell>(&mut reader, Pipe::Data) {
            Ok(Some(message)) => {
                // At most the data cap and a header.
                let bytes = u32::try_from(reader.bytes).unwrap_or(u32::MAX);
                if events
                    .send_blocking(LinkEvent::Data { message, bytes })
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => break,
            Err(FrameError::Io(error)) => {
                tracing::debug!(%error, "data pipe closed");
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "the brain broke the data pipe's protocol");
                break;
            }
        }
    }
    control.abort();
}

/// Writes queued control messages in order until the queue closes or the pipe fails.
fn write_control(control: &PipeClient, queue: &Receiver<ControlToBrain>) {
    while let Ok(message) = queue.recv_blocking() {
        if let Err(error) = frame::write(&mut &*control, &message, Pipe::Control) {
            tracing::debug!(%error, "control pipe write failed");
            return;
        }
    }
}

/// Writes queued turns in order, and after each wake-up any credit that has added up, until the
/// queue closes or the pipe fails.
fn write_data(data: &PipeClient, queue: &Receiver<DataOut>, credit: &AtomicU32) {
    while let Ok(out) = queue.recv_blocking() {
        let submitted = match out {
            DataOut::Submit(message) => frame::write(&mut &*data, &message, Pipe::Data),
            DataOut::Credit => Ok(()),
        };
        let bytes = credit.swap(0, Ordering::SeqCst);
        let credited = if bytes > 0 {
            frame::write(&mut &*data, &DataToBrain::Credit { bytes }, Pipe::Data)
        } else {
            Ok(())
        };
        match submitted.and(credited) {
            Ok(()) => {}
            Err(FrameError::Io(error)) => {
                tracing::debug!(%error, "data pipe write failed");
                return;
            }
            // Never one the shell sends: turns are checked against the cap before they're queued.
            Err(error) => tracing::error!(%error, "a data message couldn't be encoded"),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use async_channel::Receiver;
    use capsula_protocol::frame::{self, Pipe};
    use capsula_protocol::message::{
        ControlToBrain, ControlToShell, DataToBrain, DataToShell, Incarnation, Phase, RunId,
        ThreadId,
    };

    use super::{Connection, Link, LinkEvent, LinkHandle, LinkState, Shared, start};
    use crate::platform::{PipeClient, test_pair};

    /// Bytes of text each data sample streams.
    const TOTAL: usize = 960_000;
    /// How long a test waits for anything before it fails.
    const PATIENCE: Duration = Duration::from_secs(5);

    fn pair(label: &str) -> (PipeClient, PipeClient) {
        let name = format!(r"\\.\pipe\capsula-test-{}-{label}", std::process::id());
        test_pair(&name).expect("a test pipe")
    }

    /// A connection the link attends on its own thread, and the brain's ends of its pipes.
    struct Attended {
        handle: LinkHandle,
        events: Receiver<LinkEvent>,
        control: PipeClient,
        data: Arc<PipeClient>,
        /// Returns whether the UI was still there when the connection ended.
        link: JoinHandle<bool>,
    }

    fn attended(label: &str) -> Attended {
        let (control, shell_control) = pair(&format!("{label}-control"));
        let (data, shell_data) = pair(&format!("{label}-data"));
        control.set_timeout(Some(PATIENCE));
        data.set_timeout(Some(PATIENCE));
        let (sender, events) = async_channel::unbounded();
        let link = Link {
            brain: PathBuf::new(),
            session: 0,
            shared: Arc::new(Shared::default()),
        };
        let (handle, start) = start(link, sender);
        let connection = Connection {
            control: Arc::new(shell_control),
            data: Arc::new(shell_data),
            incarnation: Incarnation(1),
        };
        let link = thread::spawn(move || start.link.attend(connection, &start.events));
        Attended {
            handle,
            events,
            control,
            data: Arc::new(data),
            link,
        }
    }

    /// The link's events with a timeout on each.
    fn patient(events: Receiver<LinkEvent>) -> mpsc::Receiver<LinkEvent> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(event) = events.recv_blocking() {
                if sender.send(event).is_err() {
                    return;
                }
            }
        });
        receiver
    }

    fn next(events: &mpsc::Receiver<LinkEvent>) -> LinkEvent {
        events.recv_timeout(PATIENCE).expect("an event in time")
    }

    fn text(run: u64, delta: &str) -> DataToShell {
        DataToShell::AssistantText {
            run: RunId(run),
            delta: delta.to_owned(),
        }
    }

    #[test]
    fn nothing_from_the_brain_comes_before_connected_and_frames_carry_their_size() {
        let link = attended("order");
        let sent = text(1, "early");
        frame::write(&mut &*link.data, &sent, Pipe::Data).expect("written");
        let events = patient(link.events);
        assert!(matches!(
            next(&events),
            LinkEvent::State(LinkState::Connected)
        ));
        let LinkEvent::Data { message, bytes } = next(&events) else {
            panic!("data next");
        };
        assert_eq!(message, sent);
        let size = frame::encode(&sent, Pipe::Data).expect("encodes").len();
        assert_eq!(usize::try_from(bytes).expect("small"), size);
        drop(link.control);
        assert!(link.link.join().expect("joined"));
    }

    #[test]
    fn pings_are_answered_and_run_states_passed_on() {
        let link = attended("ping");
        let events = patient(link.events);
        assert!(matches!(
            next(&events),
            LinkEvent::State(LinkState::Connected)
        ));
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
        let pong = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control);
        assert_eq!(pong.expect("read"), Some(ControlToBrain::Pong));
        let state = ControlToShell::RunState {
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Answering,
        };
        frame::write(&mut &link.control, &state, Pipe::Control).expect("state");
        assert!(matches!(
            next(&events),
            LinkEvent::RunState {
                run: RunId(2),
                thread: ThreadId(3),
                phase: Phase::Answering
            }
        ));
        drop(link.control);
        assert!(link.link.join().expect("joined"));
    }

    #[test]
    fn the_ui_s_messages_go_out_on_their_pipes_and_credit_adds_up() {
        let link = attended("outbox");
        let events = patient(link.events);
        assert!(matches!(
            next(&events),
            LinkEvent::State(LinkState::Connected)
        ));
        let stop = ControlToBrain::Stop {
            run: RunId(3),
            revealed: vec![0],
        };
        assert!(link.handle.send_control(stop.clone()));
        let read = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control);
        assert_eq!(read.expect("read"), Some(stop));
        let submit = DataToBrain::TextSubmit {
            thread: None,
            text: "typed".to_owned(),
        };
        assert!(link.handle.send_data(submit.clone()));
        link.handle.grant(100);
        link.handle.grant(28);
        let read = frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data);
        assert_eq!(read.expect("read"), Some(submit));
        let mut credit = 0;
        while credit < 128 {
            let read = frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data);
            let Ok(Some(DataToBrain::Credit { bytes })) = read else {
                panic!("credit, got {read:?}");
            };
            credit += bytes;
        }
        assert_eq!(credit, 128);
        drop(link.control);
        assert!(link.link.join().expect("joined"));
        assert!(
            !link.handle.send_control(ControlToBrain::Pong),
            "not connected"
        );
    }

    #[test]
    fn a_malformed_data_frame_ends_the_connection() {
        let link = attended("malformed");
        let events = patient(link.events);
        assert!(matches!(
            next(&events),
            LinkEvent::State(LinkState::Connected)
        ));
        // A three-byte payload naming no `DataToShell` variant.
        std::io::Write::write_all(&mut &*link.data, &[3, 0, 0, 0, 0xff, 0xff, 0xff])
            .expect("written");
        // The shell closes the control pipe too; this read times out if it doesn't.
        assert!(matches!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control),
            Ok(None)
        ));
        assert!(link.link.join().expect("joined"));
        assert!(
            !link.handle.send_control(ControlToBrain::Pong),
            "not connected"
        );
    }

    #[test]
    fn a_repeated_handshake_ends_the_connection() {
        let link = attended("rehandshake");
        let events = patient(link.events);
        assert!(matches!(
            next(&events),
            LinkEvent::State(LinkState::Connected)
        ));
        let welcome = ControlToShell::Welcome {
            incarnation: Incarnation(2),
        };
        frame::write(&mut &link.control, &welcome, Pipe::Control).expect("written");
        assert!(matches!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control),
            Ok(None)
        ));
        assert!(link.link.join().expect("joined"));
    }

    /// Plays the brain's control side: answers each Stop with `Interrupted`.
    fn answer_stops(server: &PipeClient) {
        while let Ok(Some(message)) = frame::read::<ControlToBrain>(&mut &*server, Pipe::Control) {
            if let ControlToBrain::Stop { run, .. } = message {
                let state = ControlToShell::RunState {
                    run,
                    thread: ThreadId(1),
                    phase: Phase::Interrupted,
                };
                if frame::write(&mut &*server, &state, Pipe::Control).is_err() {
                    return;
                }
            }
        }
    }

    /// Plays the brain's data side: streams [`TOTAL`] bytes in `chunk`-byte text frames, one
    /// allocation per frame as the brain does, while another thread drains what the shell sends.
    fn stream_text(server: &Arc<PipeClient>, chunk: usize) {
        let drain = Arc::clone(server);
        let drainer = thread::spawn(move || {
            let mut sink = [0; 4096];
            while std::io::Read::read(&mut &*drain, &mut sink).is_ok_and(|n| n > 0) {}
        });
        let mut sent = 0;
        while sent < TOTAL {
            let delta = "x".repeat(chunk.min(TOTAL - sent));
            sent += delta.len();
            let text = DataToShell::AssistantText {
                run: RunId(1),
                delta,
            };
            if frame::write(&mut &**server, &text, Pipe::Data).is_err() {
                break;
            }
        }
        // The shell closes its end once the connection ends, which ends the drain.
        let _ = drainer.join();
    }

    #[expect(clippy::print_stderr, reason = "a measurement's output")]
    fn report(name: &str, samples: &mut [Duration]) {
        samples.sort_unstable();
        eprintln!(
            "{name}: p50 {:?} p95 {:?} max {:?} (n={})",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1],
            samples.len()
        );
    }

    /// Rez measurement for the pull request, not a check: run it with
    /// `cargo test --release -p capsula-shell -- --ignored --nocapture measure_`. The shell side
    /// is the link as the UI uses it: a Stop queued for the control writer and its answer taken
    /// from the event channel, and text taken from the event channel, granting credit for each
    /// frame.
    #[test]
    #[ignore = "a measurement, run by hand on Rez"]
    fn measure_control_round_trip_and_data_throughput() {
        let link = attended("measure-control");
        let control = Arc::new(link.control);
        let brain = thread::spawn({
            let control = Arc::clone(&control);
            move || answer_stops(&control)
        });
        assert!(matches!(
            link.events.recv_blocking(),
            Ok(LinkEvent::State(LinkState::Connected))
        ));
        let mut stops = Vec::new();
        for round in 0..200 {
            let run = RunId(round);
            let sent = Instant::now();
            assert!(link.handle.send_control(ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            }));
            while !matches!(
                link.events.recv_blocking(),
                Ok(LinkEvent::RunState { run: got, .. }) if got == run
            ) {}
            stops.push(sent.elapsed());
        }
        control.abort();
        let _ = brain.join();
        // Closing the brain's end ends the connection.
        drop(control);
        assert!(link.link.join().expect("joined"));
        report("Stop -> Interrupted round trip", &mut stops);

        for chunk in [240_000, 40] {
            let mut streams = Vec::new();
            for round in 0..20 {
                let link = attended(&format!("measure-data-{chunk}-{round}"));
                link.data.set_timeout(None);
                let brain = thread::spawn({
                    let data = Arc::clone(&link.data);
                    move || stream_text(&data, chunk)
                });
                assert!(matches!(
                    link.events.recv_blocking(),
                    Ok(LinkEvent::State(LinkState::Connected))
                ));
                let mut started = None;
                let mut bytes = 0;
                while bytes < TOTAL {
                    let Ok(LinkEvent::Data {
                        message: DataToShell::AssistantText { delta, .. },
                        bytes: size,
                    }) = link.events.recv_blocking()
                    else {
                        panic!("text only");
                    };
                    started.get_or_insert_with(Instant::now);
                    bytes += delta.len();
                    link.handle.grant(size);
                }
                streams.push(started.expect("text arrived").elapsed());
                drop(link.control);
                assert!(link.link.join().expect("joined"));
                let _ = brain.join();
            }
            report(
                &format!("960 kB in {chunk}-byte frames, first frame to last"),
                &mut streams,
            );
        }
    }
}
