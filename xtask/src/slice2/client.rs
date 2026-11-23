//! The scripted client: it plays the shell on both pipes, answers `Ping`, grants credit, and
//! records every frame with the instant its reader took it.

#[cfg(test)]
mod owned_tests;

use std::io::Read;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{INSTANCE_SERVICE, instance_entry};
use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    Capability, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Hello, IssueSession,
};
use kumochi_protocol::{BUILD_ID, Secret};
use serde::de::DeserializeOwned;

use crate::platform::gate_pipe::{self, PipeClient};

/// How long a brain that's starting gets to accept the client.
const CONNECT: Duration = Duration::from_secs(15);

/// A frame the brain sent, or the end of one of its pipes.
#[derive(Debug)]
pub(crate) enum Frame {
    Control(ControlToShell),
    /// A data frame and its size on the pipe, header included.
    Data(DataToShell, u32),
    /// A pipe ended.
    Closed,
    /// A frame or pipe operation failed.
    BadRead,
}

/// A frame and when its reader took it.
#[derive(Debug)]
pub(crate) struct Seen<F = Frame> {
    pub(crate) at: Instant,
    pub(crate) frame: F,
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

    fn abort(&self) {
        self.flow().closed = true;
        self.flow_changed.notify_all();
        self.control.abort();
        self.data.abort();
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
pub(crate) struct Client<F = Frame> {
    shared: Arc<Shared>,
    frames: Receiver<Seen<F>>,
    readers: Vec<JoinHandle<()>>,
    /// Every frame taken so far, in the order the client took them.
    pub(crate) log: Vec<Seen>,
}

impl Client {
    /// Connects and authenticates both pipes, retrying while the brain starts.
    pub(crate) fn connect(pid: u32) -> Result<Self> {
        Self::connect_with(pid, None)
    }

    /// Connects with the data reader paused, so recovery's chunks can be taken one at a time.
    pub(super) fn connect_paused(pid: u32) -> Result<Self> {
        Self::connect_with(pid, Some(0))
    }

    fn connect_with(pid: u32, allowed: Option<usize>) -> Result<Self> {
        let session = gate_pipe::session_id()?;
        let mut shared = owned_connect(
            &Pipe::Control.name(session),
            &Pipe::Data.name(session),
            pid,
            Instant::now() + CONNECT,
            || capability(session),
        )
        .context("the brain didn't accept the client")?;
        shared.control.clear_deadline();
        shared.data.clear_deadline();
        shared.flow().allowed = allowed;
        let (sender, frames) = mpsc::channel();
        let send = move |seen: Option<Seen>, _| {
            seen.is_none_or(|mut seen| {
                if matches!(seen.frame, Frame::BadRead) {
                    seen.frame = Frame::Closed;
                }
                sender.send(seen).is_ok()
            })
        };
        Client::readers(Arc::new(shared), frames, send.clone(), send)
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
                Frame::Control(ControlToShell::Settings { answer, .. }) => {
                    format!("Settings(answer {answer})")
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
                Frame::Closed | Frame::BadRead => "closed".to_owned(),
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

impl<F> Drop for Client<F> {
    fn drop(&mut self) {
        self.shared.abort();
        for reader in self.readers.drain(..) {
            let _ended = reader.join();
        }
    }
}

fn hello(capability: Capability) -> Hello {
    Hello {
        build_id: BUILD_ID.to_owned(),
        capability,
    }
}

fn welcome(control: &PipeClient, hello: &Hello) -> Result<Option<IssueSession>> {
    frame::write(
        &mut &*control,
        &ControlToBrain::Hello(hello.clone()),
        Pipe::Control,
    )?;
    match read::<ControlToShell>(control, Pipe::Control)? {
        Some((
            ControlToShell::Welcome {
                incarnation,
                issue_epoch,
            },
            _,
        )) => Ok(Some(IssueSession {
            incarnation,
            epoch: issue_epoch,
        })),
        Some((ControlToShell::BuildMismatch { .. }, _)) => {
            bail!("the brain is from another build; rebuild both")
        }
        None => Ok(None),
        _ => bail!("no welcome"),
    }
}

fn paired(
    control: PipeClient,
    data: PipeClient,
    hello: Hello,
    origin: IssueSession,
) -> Result<Shared> {
    frame::write(&mut &data, &DataToBrain::Hello(hello), Pipe::Data)?;
    frame::write(&mut &data, &DataToBrain::TextMetrics(None), Pipe::Data)?;
    ensure!(
        matches!(
            frame::read(&mut &control, Pipe::Control)?,
            Some(ControlToShell::ShellTuning { session, tuning })
                if session == origin && tuning.valid()
        ),
        "missing or invalid shell tuning"
    );
    Ok(Shared {
        control,
        data,
        control_writer: Mutex::new(()),
        data_writer: Mutex::new(()),
        flow: Mutex::default(),
        flow_changed: Condvar::new(),
    })
}

fn owned_connect(
    control_name: &str,
    data_name: &str,
    pid: u32,
    deadline: Instant,
    mut published: impl FnMut() -> Result<Option<Capability>>,
) -> Result<Shared> {
    let mut refused: Option<Capability> = None;
    loop {
        let control = loop {
            ensure!(
                Instant::now() < deadline,
                "owned pipes did not become ready"
            );
            match PipeClient::open(control_name) {
                Ok(pipe) => break pipe,
                Err(error) if gate_pipe::is_starting(&error) => startup_pause(deadline),
                Err(error) => return Err(error.into()),
            }
        };
        control.require_server(pid)?;
        let control = control.until(deadline);
        // Keep the first handle open until the capability appears: the server hasn't
        // necessarily called ConnectNamedPipe yet. After a clean refusal, never resend
        // that capability; only a different published value is evidence of startup.
        let hello = loop {
            ensure!(
                Instant::now() < deadline,
                "owned capability did not become ready"
            );
            if let Some(capability) = published()?
                && refused
                    .as_ref()
                    .is_none_or(|old| old.expose() != capability.expose())
            {
                break hello(capability);
            }
            startup_pause(deadline);
        };
        if let Some(origin) = welcome(&control, &hello)? {
            let data = PipeClient::open(data_name)?;
            data.require_server(pid)?;
            return paired(control, data.until(deadline), hello, origin);
        }
        ensure!(
            refused.is_none(),
            "owned pipe authentication was refused with a changed capability"
        );
        refused = Some(hello.capability);
    }
}

fn startup_pause(deadline: Instant) {
    thread::sleep(
        Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
    );
}

impl<F> Client<F> {
    fn readers(
        shared: Arc<Shared>,
        frames: Receiver<Seen<F>>,
        control: impl FnMut(Option<Seen>, u32) -> bool + Send + 'static,
        data: impl FnMut(Option<Seen>, u32) -> bool + Send + 'static,
    ) -> Result<Self> {
        let mut client = Self {
            shared,
            frames,
            readers: Vec::with_capacity(2),
            log: Vec::new(),
        };
        client.readers.push(spawn(
            "gate-control",
            control_reader(Arc::clone(&client.shared), control),
        )?);
        client.readers.push(spawn(
            "gate-data",
            data_reader(Arc::clone(&client.shared), data),
        )?);
        Ok(client)
    }

    pub(crate) fn control(&self, message: &ControlToBrain) -> Result<Instant> {
        self.shared.write_control(message)
    }

    pub(crate) fn data(&self, message: &DataToBrain) -> Result<Instant> {
        self.shared.write_data(message)
    }
}

impl<F> Client<Option<F>> {
    fn next_projected(&self, until: Instant) -> Result<Seen<Option<F>>> {
        let frame = self
            .frames
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow::anyhow!("projected pipe wait ended"))?;
        ensure!(
            !self.shared.flow().closed && Instant::now() < until,
            "projected pipe closed or expired"
        );
        Ok(frame)
    }

    pub(crate) fn next_fact(&self, until: Instant) -> Result<Seen<F>> {
        let Seen { at, frame } = self.next_projected(until)?;
        Ok(Seen {
            at,
            frame: frame.context("projected pipe ended early")?,
        })
    }

    pub(crate) fn finish_projected(&self, sent: Instant, until: Instant) -> Result<()> {
        for _ in 0..2 {
            let Seen { at, frame } = self.next_projected(until)?;
            ensure!(
                at >= sent && frame.is_none(),
                "unexpected frame or early pipe end"
            );
        }
        Ok(())
    }
}

struct Projection<F> {
    sender: SyncSender<Seen<Option<F>>>,
    deadline: Instant,
    // Admission caps: a reader can have decoded one more frame before it checks these limits,
    // bounded by its per-pipe frame maximum. Data credit still precedes admission.
    left: Option<(u32, u32)>,
}

impl<F> Projection<F> {
    fn accept(
        &mut self,
        seen: Option<Seen>,
        bytes: u32,
        project: &mut impl FnMut(Frame) -> std::result::Result<Option<F>, ()>,
    ) -> std::result::Result<(), ()> {
        let (frames, budget) = self.left.take().ok_or(())?;
        if Instant::now() >= self.deadline {
            return Err(());
        }
        if let Some(Seen {
            at,
            frame: Frame::Closed,
        }) = seen
        {
            self.sender
                .try_send(Seen { at, frame: None })
                .map_err(|_| ())?;
            self.left = Some((frames, budget));
            return Ok(());
        }
        let remaining = (
            frames.checked_sub(1).ok_or(())?,
            budget.checked_sub(bytes).ok_or(())?,
        );
        if let Some(Seen { at, frame }) = seen {
            if matches!(frame, Frame::BadRead) {
                return Err(());
            }
            if let Some(frame) = project(frame)? {
                self.sender
                    .try_send(Seen {
                        at,
                        frame: Some(frame),
                    })
                    .map_err(|_| ())?;
            }
        }
        self.left = Some(remaining);
        Ok(())
    }
}

impl<F: Send + 'static> Client<Option<F>> {
    pub(crate) fn project(
        pid: u32,
        deadline: Instant,
        mut project: impl FnMut(Frame) -> std::result::Result<Option<F>, ()> + Send + 'static,
    ) -> Result<Self> {
        let session = gate_pipe::session_id()?;
        let shared = Arc::new(owned_connect(
            &Pipe::Control.name(session),
            &Pipe::Data.name(session),
            pid,
            deadline,
            || capability(session),
        )?);
        let (sender, frames) = mpsc::sync_channel(128);
        let mut intake = Projection {
            sender,
            deadline,
            left: Some((32_768, 64_u32 << 20)),
        };
        let owner = Arc::clone(&shared);
        let send = Arc::new(Mutex::new(move |seen, bytes| {
            if !owner.flow().closed && intake.accept(seen, bytes, &mut project).is_ok() {
                return true;
            }
            owner.abort();
            false
        }));
        let control = {
            let send = Arc::clone(&send);
            move |seen, bytes| lock(&send)(seen, bytes)
        };
        Client::readers(shared, frames, control, move |seen, bytes| {
            lock(&send)(seen, bytes)
        })
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .context("starting a reader")
}

fn capability(session: u32) -> Result<Option<Capability>> {
    let account = instance_entry(session);
    let bytes = match keyring::Entry::new(INSTANCE_SERVICE, &account)?.get_secret() {
        Ok(bytes) => bytes,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("the stored capability has the wrong length"))?;
    Ok(Some(Secret::new(bytes)))
}

fn control_reader(
    shared: Arc<Shared>,
    mut send: impl FnMut(Option<Seen>, u32) -> bool + Send,
) -> impl FnOnce() + Send {
    move || {
        loop {
            let (frame, bytes) = match read::<ControlToShell>(&shared.control, Pipe::Control) {
                Ok(Some((message, bytes))) => match frame_of(message) {
                    Some(frame) => (frame, bytes),
                    None if !send(None, bytes) => return,
                    None if shared.write_control(&ControlToBrain::Pong).is_ok() => continue,
                    None => (Frame::BadRead, 0),
                },
                Ok(None) => (Frame::Closed, 0),
                Err(_) => (Frame::BadRead, 0),
            };
            let closed = matches!(frame, Frame::Closed | Frame::BadRead);
            let seen = Seen {
                at: Instant::now(),
                frame,
            };
            if !send(Some(seen), bytes) || closed {
                return;
            }
        }
    }
}

/// The frame a control message becomes; `None` for a `Ping`, which the reader answers itself.
fn frame_of(message: ControlToShell) -> Option<Frame> {
    match message {
        ControlToShell::Ping => None,
        message => Some(Frame::Control(message)),
    }
}

fn data_reader(
    shared: Arc<Shared>,
    mut send: impl FnMut(Option<Seen>, u32) -> bool + Send,
) -> impl FnOnce() + Send {
    move || {
        while shared.take_turn() {
            let read = read::<DataToShell>(&shared.data, Pipe::Data);
            let read_at = shared.data.read_stamp();
            let (seen, bytes) = credited_data(read, read_at, |bytes| shared.credit(bytes));
            let closed = matches!(seen.frame, Frame::Closed | Frame::BadRead);
            if !send(Some(seen), bytes) || closed {
                return;
            }
        }
    }
}

fn credited_data(
    read: Result<Option<(DataToShell, u32)>>,
    read_at: Option<Instant>,
    credit: impl FnOnce(u32) -> Result<()>,
) -> (Seen, u32) {
    let (frame, bytes) = match read {
        Ok(Some((message, bytes))) if credit(bytes).is_ok() => (Frame::Data(message, bytes), bytes),
        Ok(None) => (Frame::Closed, 0),
        Ok(Some(_)) | Err(_) => (Frame::BadRead, 0),
    };
    (
        Seen {
            at: read_at.unwrap_or_else(Instant::now),
            frame,
        },
        bytes,
    )
}

/// Reads one frame and its size on the pipe; `None` when the brain closed it.
fn read<T: DeserializeOwned>(mut reader: impl Read, which: Pipe) -> Result<Option<(T, u32)>> {
    let mut header = [0; HEADER_LEN];
    let prefix = reader.read(&mut header)?;
    if prefix == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[prefix..])?;
    let len = frame::payload_len(header, which)?;
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(Some((
        frame::decode(&payload)?,
        u32::try_from(HEADER_LEN + len)?,
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use kumochi_protocol::frame::{self, Pipe};
    use kumochi_protocol::message::{
        AssistantName, ControlToShell, DEFAULT_HOLD_TO_TALK, DEFAULT_SCREEN_HOLDS,
        DEFAULT_SHOW_SCREEN, DockPosition, MaxFrameRate, OnboardingStep, SettingsView, SttUnload,
        ThreadId,
    };

    use super::{Frame, Projection, Seen, frame_of, read};

    #[test]
    fn a_clean_close_is_empty_input_not_a_truncated_frame() {
        let pipe = Pipe::Control;
        let mut bytes = Vec::new();
        frame::write(&mut bytes, &ControlToShell::Ping, pipe).expect("one fixture frame");
        assert_eq!(
            read::<ControlToShell>(&[][..], pipe).expect("empty stream"),
            None
        );
        for n in 1..bytes.len() {
            assert!(
                read::<ControlToShell>(&bytes[..n], pipe).is_err(),
                "prefix of {n} bytes"
            );
        }
        let (message, size) = read::<ControlToShell>(&bytes[..], pipe)
            .expect("full frame")
            .expect("not EOF");
        assert_eq!(message, ControlToShell::Ping);
        assert_eq!(usize::try_from(size).expect("bounded frame"), bytes.len());
    }

    #[test]
    fn projected_intake_counts_ignored_frames_and_bytes_before_queuing() {
        let deadline = Instant::now() + Duration::from_secs(5);
        for (bounds, sizes) in [((1, 40), [11, 13]), ((4, 24), [24, 1])] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let mut intake = Projection {
                sender,
                deadline,
                left: Some(bounds),
            };
            let stored = Seen {
                at: Instant::now(),
                frame: Frame::Control(ControlToShell::Ping),
            };
            assert_eq!(intake.accept(None, sizes[0], &mut |_| Ok(Some(17))), Ok(()));
            assert_eq!(
                intake.accept(Some(stored), sizes[1], &mut |_| Ok(Some(31))),
                Err(())
            );
            assert_eq!(intake.accept(None, 0, &mut |_| Ok(Some(17))), Err(()));
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn projected_intake_preserves_time_and_fails_closed_on_a_full_queue() {
        let at = Instant::now();
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut intake = Projection {
            sender,
            deadline: at + Duration::from_secs(5),
            left: Some((10, 40)),
        };
        let seen = || Seen {
            at,
            frame: Frame::Control(ControlToShell::Ping),
        };
        assert_eq!(
            intake.accept(Some(seen()), 13, &mut |_| Ok(Some(31))),
            Ok(())
        );
        assert_eq!(
            intake.accept(Some(seen()), 13, &mut |_| Ok(Some(17))),
            Err(())
        );
        let held = receiver.try_recv().expect("exactly one projected fact");
        assert_eq!((held.at, held.frame), (at, Some(31)));
        assert_eq!(
            intake.accept(Some(seen()), 13, &mut |_| Ok(Some(90))),
            Err(())
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn projected_intake_distinguishes_clean_eof_from_partial_reads_and_enforces_deadline() {
        let at = Instant::now();
        for (frame, deadline, expected) in [
            (Frame::Closed, at + Duration::from_secs(5), Ok(())),
            (Frame::BadRead, at + Duration::from_secs(5), Err(())),
            (Frame::Closed, at, Err(())),
        ] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let mut intake = Projection::<u8> {
                sender,
                deadline,
                left: Some((1, 3)),
            };
            assert_eq!(
                intake.accept(Some(Seen { at, frame }), 0, &mut |_| panic!(
                    "no frame to project"
                )),
                expected
            );
            if expected == Ok(()) {
                let eof = receiver.try_recv().expect("clean EOF");
                assert_eq!((eof.at, eof.frame), (at, None));
            } else {
                assert!(matches!(
                    receiver.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
            }
        }
    }

    #[test]
    fn the_connections_first_settings_reaches_the_client_before_its_scenes() {
        let view = SettingsView {
            assistant_name: AssistantName::new("Kumo").expect("a valid name"),
            user_name: None,
            onboarded: true,
            onboarding_from: OnboardingStep::Welcome,
            main_thread: ThreadId(1),
            max_frame_rate: MaxFrameRate::default(),
            default_model: None,
            dock_position: DockPosition::TopCenter,
            show_in_taskbar: false,
            talk_back: false,
            stt_unload: SttUnload::Kept,
            hold_to_talk: DEFAULT_HOLD_TO_TALK,
            screen_holds: DEFAULT_SCREEN_HOLDS,
            show_screen: DEFAULT_SHOW_SCREEN,
        };
        let sent = [
            ControlToShell::Settings {
                view,
                answer: false,
            },
            ControlToShell::Ping,
            ControlToShell::Scenes { scenes: Vec::new() },
        ];
        let taken: Vec<&str> = sent
            .into_iter()
            .filter_map(frame_of)
            .map(|frame| match frame {
                Frame::Control(ControlToShell::Settings { answer: false, .. }) => "settings",
                Frame::Control(ControlToShell::Scenes { .. }) => "scenes",
                _ => "other",
            })
            .collect();
        assert_eq!(taken, ["settings", "scenes"]);
    }
}
