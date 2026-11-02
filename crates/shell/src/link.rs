//! The shell's connection to the brain. It runs on its own thread, because pipe reads block, and
//! reports [`LinkState`] changes to the UI thread over a channel.
//!
//! Connecting starts the brain when no brain pipe answers, authenticates with the capability
//! from Credential Manager, and checks that the pipe's server is the brain beside the shell.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use capsula_protocol::BUILD_ID;
use capsula_protocol::constants::BRAIN_EXE;
use capsula_protocol::frame::{self, FrameError, Pipe};
use capsula_protocol::message::{ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation};

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

/// What the UI shows about the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
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
    #[expect(dead_code, reason = "held open; slice 2 streams on it")]
    data: PipeClient,
    incarnation: Incarnation,
}

/// What the UI thread and the link thread share.
#[derive(Debug, Default)]
struct Shared {
    /// Set once the user chose Quit, so a closing brain isn't restarted.
    quitting: AtomicBool,
    /// The control pipe while connected, for sending `Quitting` beside the link's reads.
    control: Mutex<Option<Arc<PipeClient>>>,
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
            send_quitting(&control);
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
}

/// Sends `Quitting`, bounded so a stalled brain can't hold the shell open.
fn send_quitting(control: &PipeClient) {
    control.set_timeout(Some(QUIT_TIMEOUT));
    if let Err(error) = frame::write(&mut &*control, &ControlToBrain::Quitting, Pipe::Control) {
        tracing::warn!(%error, "couldn't tell the brain about Quit");
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
    states: Sender<LinkState>,
    retries: Receiver<()>,
    alive: Sender<()>,
}

/// Prepares the link. It reports every state change on `states` once started, and ends when
/// the UI drops the receiver or the user quits.
///
/// # Errors
/// Fails when the shell can't find its own path or session.
pub(crate) fn prepare(states: Sender<LinkState>) -> std::io::Result<(LinkHandle, LinkStart)> {
    let shared = Arc::new(Shared::default());
    let link = Link {
        brain: std::env::current_exe()?.with_file_name(BRAIN_EXE),
        session: platform::session_id()?,
        shared: Arc::clone(&shared),
    };
    let (retry, retries) = async_channel::bounded(1);
    let (alive, done) = async_channel::bounded::<()>(1);
    let handle = LinkHandle {
        shared,
        retry,
        done,
    };
    let start = LinkStart {
        link,
        states,
        retries,
        alive,
    };
    Ok((handle, start))
}

impl LinkStart {
    /// Starts connecting on the link thread, which may start the brain.
    ///
    /// # Errors
    /// Fails when the thread can't start.
    pub(crate) fn spawn(self) -> std::io::Result<()> {
        let Self {
            link,
            states,
            retries,
            alive,
        } = self;
        thread::Builder::new()
            .name("link".to_owned())
            .spawn(move || {
                link.run(&states, &retries);
                drop(alive);
            })?;
        Ok(())
    }
}

impl Link {
    /// Connects, watches, and reconnects after a loss. The first loss relaunches the brain on
    /// its own; another within [`REPEAT_WINDOW`], or a failed connect, waits for Retry.
    fn run(&self, states: &Sender<LinkState>, retries: &Receiver<()>) {
        let mut state = LinkState::Connecting;
        let mut last_loss: Option<Instant> = None;
        loop {
            if self.quitting() || states.send_blocking(state).is_err() {
                return;
            }
            let connection = match self.connect() {
                Ok(connection) => connection,
                Err(_) if self.quitting() => return,
                Err(error) => {
                    tracing::error!(%error, "can't reach the brain");
                    if !wait_for_retry(states, retries) {
                        return;
                    }
                    (state, last_loss) = (LinkState::Connecting, None);
                    continue;
                }
            };
            tracing::info!(incarnation = ?connection.incarnation, "connected to the brain");
            self.set_control(Some(Arc::clone(&connection.control)));
            // Quit may have come while connecting, when there was no pipe to send it on.
            if self.quitting() {
                send_quitting(&connection.control);
                watch(&connection);
                return;
            }
            if states.send_blocking(LinkState::Connected).is_err() {
                return;
            }
            watch(&connection);
            self.set_control(None);
            if self.quitting() {
                return;
            }
            tracing::warn!("lost the brain");
            let now = Instant::now();
            if last_loss.is_some_and(|at| now.duration_since(at) < REPEAT_WINDOW) {
                if !wait_for_retry(states, retries) {
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

    fn set_control(&self, control: Option<Arc<PipeClient>>) {
        *self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = control;
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
            data,
            incarnation,
        })
    }
}

/// Shows `Failed` and waits for the user's Retry; false when the UI has gone.
fn wait_for_retry(states: &Sender<LinkState>, retries: &Receiver<()>) -> bool {
    states.send_blocking(LinkState::Failed).is_ok() && retries.recv_blocking().is_ok()
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

/// Blocks until the brain goes away. Slice 0's brain sends nothing after `Welcome`.
fn watch(connection: &Connection) {
    loop {
        match frame::read::<ControlToShell>(&mut &*connection.control, Pipe::Control) {
            Ok(None) => return,
            Ok(Some(message)) => tracing::warn!(?message, "unexpected message"),
            Err(error) => {
                tracing::warn!(%error, "control pipe failed");
                return;
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use capsula_protocol::frame::{self, Pipe};
    use capsula_protocol::message::{
        ControlToBrain, ControlToShell, DataToShell, Phase, RunId, ThreadId,
    };

    use crate::platform::{PipeClient, test_pair};

    /// Bytes of text each data sample streams.
    const TOTAL: usize = 960_000;

    fn pair(label: &str) -> (PipeClient, PipeClient) {
        let name = format!(r"\\.\pipe\capsula-test-{}-{label}", std::process::id());
        test_pair(&name).expect("a test pipe")
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
    fn stream_text(server: PipeClient, chunk: usize) {
        let server = std::sync::Arc::new(server);
        let drain = std::sync::Arc::clone(&server);
        let drainer = thread::spawn(move || {
            let mut sink = [0; 4096];
            while std::io::Read::read(&mut &*drain, &mut sink).is_ok_and(|n| n > 0) {}
        });
        let mut sent = 0;
        while sent < TOTAL {
            let delta = "x".repeat(chunk.min(TOTAL - sent));
            sent += delta.len();
            let text = DataToShell::AssistantText { run: RunId(1), delta };
            if frame::write(&mut &*server, &text, Pipe::Data).is_err() {
                break;
            }
        }
        // The shell closes its end once it has read everything, which ends the drain.
        let _ = drainer.join();
    }

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
    /// here is the plain pipe client, read and written on one thread.
    #[test]
    #[ignore = "a measurement, run by hand on Rez"]
    fn measure_control_round_trip_and_data_throughput() {
        let (server, client) = pair("control");
        let brain = thread::spawn(move || answer_stops(&server));
        let mut stops = Vec::new();
        for round in 0..200 {
            let run = RunId(round);
            let sent = Instant::now();
            let stop = ControlToBrain::Stop { run, revealed: Vec::new() };
            frame::write(&mut &client, &stop, Pipe::Control).expect("Stop sent");
            loop {
                let state = frame::read::<ControlToShell>(&mut &client, Pipe::Control);
                if let Ok(Some(ControlToShell::RunState { run: got, .. })) = state
                    && got == run
                {
                    break;
                }
            }
            stops.push(sent.elapsed());
        }
        drop(client);
        let _ = brain.join();
        report("Stop -> Interrupted round trip", &mut stops);

        for chunk in [240_000, 40] {
            let mut streams = Vec::new();
            for round in 0..20 {
                let (server, client) = pair(&format!("data-{chunk}-{round}"));
                let brain = thread::spawn(move || stream_text(server, chunk));
                let mut started = None;
                let mut bytes = 0;
                while bytes < TOTAL {
                    let Ok(Some(DataToShell::AssistantText { delta, .. })) =
                        frame::read::<DataToShell>(&mut &client, Pipe::Data)
                    else {
                        panic!("text only");
                    };
                    started.get_or_insert_with(Instant::now);
                    bytes += delta.len();
                }
                streams.push(started.expect("text arrived").elapsed());
                drop(client);
                let _ = brain.join();
            }
            report(
                &format!("960 kB in {chunk}-byte frames, first frame to last"),
                &mut streams,
            );
        }
    }
}
