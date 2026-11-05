//! The scripted client: it plays the shell on both pipes, answers `Ping`, grants credit, and
//! records every frame with the instant its reader took it.

use std::io::{self, Read as _};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kumochi_protocol::constants::{INSTANCE_SERVICE, instance_entry};
use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    CAPABILITY_LEN, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Hello,
};
use kumochi_protocol::{BUILD_ID, Secret};
use serde::de::DeserializeOwned;

use crate::platform::gate_pipe::{self, PipeClient};

/// How long a brain that's starting gets to accept the client.
const CONNECT: Duration = Duration::from_secs(15);
/// How long the brain gets to answer `Hello`.
const WELCOME: Duration = Duration::from_secs(5);

/// A frame the brain sent, or the end of one of its pipes.
#[derive(Debug)]
pub(crate) enum Frame {
    Control(ControlToShell),
    /// A data frame and its size on the pipe, header included.
    Data(DataToShell, u32),
    /// A pipe ended.
    Closed,
}

/// A frame and when its reader took it.
#[derive(Debug)]
pub(crate) struct Seen {
    pub(super) at: Instant,
    pub(crate) frame: Frame,
}

/// Whether the data reader reads, and whether it grants what it read as credit.
#[derive(Debug, Default)]
struct Flow {
    /// Frames the reader may still take; `None` reads freely.
    allowed: Option<usize>,
    withhold: bool,
    owed: u32,
    closed: bool,
}

#[derive(Debug)]
struct Shared {
    control: PipeClient,
    data: PipeClient,
    control_writer: Mutex<()>,
    data_writer: Mutex<()>,
    flow: Mutex<Flow>,
    flow_changed: Condvar,
}

impl Shared {
    fn write_control(&self, message: &ControlToBrain) -> Result<Instant> {
        let _turn = lock(&self.control_writer);
        let at = Instant::now();
        frame::write(&mut &self.control, message, Pipe::Control)?;
        Ok(at)
    }

    fn write_data(&self, message: &DataToBrain) -> Result<Instant> {
        let _turn = lock(&self.data_writer);
        let at = Instant::now();
        frame::write(&mut &self.data, message, Pipe::Data)?;
        Ok(at)
    }

    fn flow(&self) -> MutexGuard<'_, Flow> {
        lock(&self.flow)
    }

    /// Waits until the reader may take a frame; false once the client is closing.
    fn take_turn(&self) -> bool {
        let mut flow = self.flow();
        while !flow.closed && flow.allowed == Some(0) {
            flow = self
                .flow_changed
                .wait(flow)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(allowed) = flow.allowed.as_mut() {
            *allowed -= 1;
        }
        !flow.closed
    }

    /// Grants `bytes` read, with any held back, unless credit is withheld.
    fn credit(&self, bytes: u32) -> Result<()> {
        let mut flow = self.flow();
        flow.owed = flow.owed.saturating_add(bytes);
        if flow.withhold || flow.owed == 0 {
            return Ok(());
        }
        let bytes = std::mem::take(&mut flow.owed);
        drop(flow);
        self.write_data(&DataToBrain::Credit { bytes })?;
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One connection to the brain, as the shell would hold it.
#[derive(Debug)]
pub(crate) struct Client {
    shared: Arc<Shared>,
    frames: Receiver<Seen>,
    readers: Vec<JoinHandle<()>>,
    /// Every frame taken so far, in the order the client took them.
    pub(super) log: Vec<Seen>,
}

impl Client {
    /// Connects and authenticates both pipes, retrying while the brain starts.
    pub(crate) fn connect() -> Result<Self> {
        Self::connect_with(None)
    }

    /// Connects with the data reader paused, so recovery's chunks can be taken one at a time.
    pub(super) fn connect_paused() -> Result<Self> {
        Self::connect_with(Some(0))
    }

    fn connect_with(allowed: Option<usize>) -> Result<Self> {
        let session = gate_pipe::session_id()?;
        let end = Instant::now() + CONNECT;
        loop {
            match attempt(session, allowed) {
                Ok(client) => return Ok(client),
                Err(error) if Instant::now() >= end => {
                    return Err(error.context("the brain didn't accept the client"));
                }
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    pub(crate) fn control(&self, message: &ControlToBrain) -> Result<Instant> {
        self.shared.write_control(message)
    }

    pub(super) fn data(&self, message: &DataToBrain) -> Result<Instant> {
        self.shared.write_data(message)
    }

    /// Stops reading the data pipe after the frame being read, if any.
    pub(super) fn pause_data(&self) {
        self.shared.flow().allowed = Some(0);
    }

    /// Lets the paused data reader take `frames` more.
    pub(super) fn allow_data(&self, frames: usize) {
        let mut flow = self.shared.flow();
        flow.allowed = Some(flow.allowed.unwrap_or(0) + frames);
        self.shared.flow_changed.notify_all();
    }

    pub(super) fn resume_data(&self) {
        self.shared.flow().allowed = None;
        self.shared.flow_changed.notify_all();
    }

    /// Keeps reading data but grants no credit until [`Client::grant`].
    pub(super) fn withhold_credit(&self) {
        self.shared.flow().withhold = true;
    }

    /// Grants everything read since credit was withheld, and credit freely from then on.
    pub(super) fn grant(&self) -> Result<()> {
        self.shared.flow().withhold = false;
        self.shared.credit(0)
    }

    /// Like [`Client::until`], but a frame already taken counts too, for frames that may have come
    /// on the other pipe before the one waited on.
    pub(super) fn taken_or_until<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&Seen) -> Option<T>,
    ) -> Result<T> {
        if let Some(picked) = self.log.iter().find_map(&mut pick) {
            return Ok(picked);
        }
        self.until(limit, pick)
    }

    /// Takes frames until `pick` chooses one, failing after `limit` or when a pipe ends.
    pub(crate) fn until<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&Seen) -> Option<T>,
    ) -> Result<T> {
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let seen = match self.frames.recv_timeout(left) {
                Ok(seen) => seen,
                Err(RecvTimeoutError::Timeout) => {
                    bail!(
                        "nothing expected arrived in {limit:?}; last: {}",
                        self.tail()
                    );
                }
                Err(RecvTimeoutError::Disconnected) => bail!("both pipes ended"),
            };
            let picked = pick(&seen);
            let closed = matches!(seen.frame, Frame::Closed);
            self.log.push(seen);
            if let Some(picked) = picked {
                return Ok(picked);
            }
            if closed {
                bail!("a pipe ended before the frame expected");
            }
        }
    }

    /// The last frames taken, by kind, run and size; never their text.
    fn tail(&self) -> String {
        let start = self.log.len().saturating_sub(6);
        let kinds: Vec<String> = self.log[start..]
            .iter()
            .map(|seen| match &seen.frame {
                Frame::Control(ControlToShell::RunState { run, phase, .. }) => {
                    format!("RunState({}, {phase:?})", run.0)
                }
                Frame::Control(other) => format!("{other:?}"),
                Frame::Data(DataToShell::AssistantText { run, .. }, bytes) => {
                    format!("AssistantText({}, {bytes} B)", run.0)
                }
                Frame::Data(DataToShell::StepCommit { run, index, .. }, _) => {
                    format!("StepCommit({}, {index})", run.0)
                }
                Frame::Data(
                    DataToShell::SceneSnapshot {
                        scene, chunk, last, ..
                    },
                    _,
                ) => {
                    format!(
                        "SceneSnapshot({}, {} steps, last {last})",
                        scene.0,
                        chunk.len()
                    )
                }
                Frame::Data(DataToShell::RunEnd { run }, _) => format!("RunEnd({})", run.0),
                Frame::Data(_, bytes) => format!("data({bytes} B)"),
                Frame::Closed => "closed".to_owned(),
            })
            .collect();
        format!("{} frames, then {}", self.log.len(), kinds.join(", "))
    }

    /// Takes whatever arrives for `period`.
    pub(super) fn drain(&mut self, period: Duration) {
        let end = Instant::now() + period;
        while let Ok(seen) = self
            .frames
            .recv_timeout(end.saturating_duration_since(Instant::now()))
        {
            self.log.push(seen);
        }
    }

    /// Quits as the shell does, then closes the pipes.
    pub(super) fn quit(self) -> Result<()> {
        self.control(&ControlToBrain::Quitting)?;
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.flow().closed = true;
        self.shared.flow_changed.notify_all();
        self.shared.control.abort();
        self.shared.data.abort();
        for reader in self.readers.drain(..) {
            let _ended = reader.join();
        }
    }
}

fn attempt(session: u32, allowed: Option<usize>) -> Result<Client> {
    let control = PipeClient::open(&Pipe::Control.name(session))?;
    control.set_read_timeout(Some(WELCOME));
    let hello = Hello {
        build_id: BUILD_ID.to_owned(),
        capability: capability(session)?,
    };
    frame::write(
        &mut &control,
        &ControlToBrain::Hello(hello.clone()),
        Pipe::Control,
    )?;
    match read::<ControlToShell>(&control, Pipe::Control)? {
        Some((ControlToShell::Welcome { .. }, _)) => {}
        Some((ControlToShell::BuildMismatch { .. }, _)) => {
            bail!("the brain is from another build; rebuild both")
        }
        other => bail!("no welcome: {other:?}"),
    }
    control.set_read_timeout(None);
    let data = PipeClient::open(&Pipe::Data.name(session))?;
    frame::write(&mut &data, &DataToBrain::Hello(hello), Pipe::Data)?;
    let shared = Arc::new(Shared {
        control,
        data,
        control_writer: Mutex::new(()),
        data_writer: Mutex::new(()),
        flow: Mutex::new(Flow {
            allowed,
            ..Flow::default()
        }),
        flow_changed: Condvar::new(),
    });
    let (sender, frames) = mpsc::channel();
    let readers = vec![
        spawn(
            "gate-control",
            control_reader(Arc::clone(&shared), sender.clone()),
        )?,
        spawn("gate-data", data_reader(Arc::clone(&shared), sender))?,
    ];
    Ok(Client {
        shared,
        frames,
        readers,
        log: Vec::new(),
    })
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .context("starting a reader")
}

fn capability(session: u32) -> Result<Secret<[u8; CAPABILITY_LEN]>> {
    let account = instance_entry(session);
    let bytes = keyring::Entry::new(INSTANCE_SERVICE, &account)?.get_secret()?;
    let bytes: [u8; CAPABILITY_LEN] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("the stored capability has the wrong length"))?;
    Ok(Secret::new(bytes))
}

fn control_reader(shared: Arc<Shared>, frames: Sender<Seen>) -> impl FnOnce() + Send {
    move || {
        loop {
            let frame = match read::<ControlToShell>(&shared.control, Pipe::Control) {
                Ok(Some((ControlToShell::Ping, _))) => {
                    if shared.write_control(&ControlToBrain::Pong).is_err() {
                        Frame::Closed
                    } else {
                        continue;
                    }
                }
                // The slice 2 gate checks runs and deletes, not settings.
                Ok(Some((ControlToShell::Settings { .. }, _))) => continue,
                Ok(Some((message, _))) => Frame::Control(message),
                Ok(None) | Err(_) => Frame::Closed,
            };
            let closed = matches!(frame, Frame::Closed);
            let seen = Seen {
                at: Instant::now(),
                frame,
            };
            if frames.send(seen).is_err() || closed {
                return;
            }
        }
    }
}

fn data_reader(shared: Arc<Shared>, frames: Sender<Seen>) -> impl FnOnce() + Send {
    move || {
        while shared.take_turn() {
            let frame = match read::<DataToShell>(&shared.data, Pipe::Data) {
                Ok(Some((message, bytes))) if shared.credit(bytes).is_ok() => {
                    Frame::Data(message, bytes)
                }
                Ok(_) | Err(_) => Frame::Closed,
            };
            let closed = matches!(frame, Frame::Closed);
            let seen = Seen {
                at: Instant::now(),
                frame,
            };
            if frames.send(seen).is_err() || closed {
                return;
            }
        }
    }
}

/// Reads one frame and its size on the pipe; `None` when the brain closed it.
fn read<T: DeserializeOwned>(pipe: &PipeClient, which: Pipe) -> Result<Option<(T, u32)>> {
    let mut reader = pipe;
    let mut header = [0; HEADER_LEN];
    match reader.read_exact(&mut header) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result?,
    }
    let len = frame::payload_len(header, which)?;
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(Some((
        frame::decode(&payload)?,
        u32::try_from(HEADER_LEN + len)?,
    )))
}
