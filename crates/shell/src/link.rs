//! The shell's connection to the brain. Connecting and watching the control pipe run on the link
//! thread, because pipe reads block. Once connected, each pipe gets its own writer thread, fed by
//! channels the UI thread sends on without blocking, and the data pipe gets its own reader
//! thread, so no read waits behind a write and the UI never waits on the brain. Everything the
//! brain sends reaches the UI thread as [`LinkEvent`]s on one channel.
//!
//! Connecting starts the brain when no brain pipe answers, authenticates with the capability
//! from Credential Manager, and checks that the pipe's server is the brain beside the shell.

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender, WeakSender};
use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::BRAIN_EXE;
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AccountView, ConnectionsView, ControlToBrain, ControlToShell, DataToBrain, DataToShell,
    Deleted, GestureId, GitHubView, Hello, HoldStatus, Incarnation, MAX_PENDING_TURNS,
    MAX_VISIBLE_SCENES, Phase, RunId, RunOrigin, SceneDescriptor, SceneDescriptorV1, SettingsOp,
    SettingsView, ThreadId, ThreadOp, UserError,
};
use kumochi_protocol::voice::VoicePart;

use kumochi_scene::deferred::Deferred;
use zeroize::{Zeroize, Zeroizing};

use crate::credential::{self, CredentialError};
use crate::platform::microphone::CollectionGate;
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
/// sends a handful at a time: Stop, `ClearDrawing`, `Pong`, up to four deletes and `Quitting`.
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
/// before `Connected`, and none of a connection's messages after its next state.
pub(crate) enum LinkEvent {
    VoiceHold {
        generation: u64,
        gesture: GestureId,
        status: HoldStatus,
    },
    CaptureDone {
        generation: u64,
        capture_for: kumochi_protocol::message::CaptureFor,
    },
    CaptureFailed {
        generation: u64,
        capture_for: kumochi_protocol::message::CaptureFor,
        reason: kumochi_protocol::message::CaptureFailure,
    },
    KeyStage {
        generation: u64,
        reply: ControlToShell,
    },
    /// Any state but `Connected`, which comes as [`LinkEvent::Connected`].
    State(LinkState),
    /// A new connection, and the session the UI sends and grants credit through.
    Connected(Session),
    Settings {
        generation: u64,
        view: SettingsView,
        answer: bool,
    },
    Account {
        generation: u64,
        view: AccountView,
    },
    Voice {
        generation: u64,
        speech_input: kumochi_protocol::message::PartState,
    },
    Connections {
        generation: u64,
        view: ConnectionsView,
    },
    GitHub {
        generation: u64,
        view: GitHubView,
    },
    GitHubError {
        generation: u64,
        error: UserError,
    },
    ActionState {
        generation: u64,
        draft: kumochi_protocol::message::DraftId,
        version: kumochi_protocol::message::StateVersion,
        state: kumochi_protocol::message::ActionState,
        approval: Option<kumochi_protocol::message::ApprovalRev>,
    },
    /// A run's phase changed, from the control pipe.
    RunState {
        generation: u64,
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        error: Option<UserError>,
        origin: Option<RunOrigin>,
    },
    /// A data frame, with its size on the pipe, header included, for the credit it earns once
    /// the UI has taken it, and the generation of the connection it came on.
    Data {
        generation: u64,
        message: DataToShell,
        bytes: u32,
    },
    /// A scene/removal frame protected from intake through final disposal.
    SceneData {
        generation: u64,
        frame: Deferred<DataToShell>,
        bytes: u32,
    },
    /// The connection's visible scenes, from the control pipe.
    Scenes(Vec<SceneDescriptor>),
    ScenesV1 {
        generation: u64,
        scenes: kumochi_scene::text::Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
    },
    /// A delete's answer, from the control pipe.
    ThreadDeleted {
        thread: ThreadId,
        outcome: Deleted,
        last_run: Option<RunId>,
    },
    /// The brain refused this connection's identified turn, from the control pipe.
    TurnRefused {
        generation: u64,
        turn: u32,
        thread: Option<ThreadId>,
    },
    /// The brain changed a listed thread, from the control pipe.
    ThreadsChanged,
    /// A run asked to move the typed input, from connection `generation`'s control pipe.
    Focus {
        generation: u64,
        run: RunId,
        thread: ThreadId,
    },
    /// Something the user should be told, from the control pipe.
    Error {
        generation: u64,
        error: UserError,
    },
}

impl fmt::Debug for LinkEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkEvent({:?})", std::mem::discriminant(self))
    }
}

/// Link events applied in one update at most [guess]: a fast stream is applied in batches
/// rather than one UI update per frame.
const EVENT_BATCH: usize = 256;

pub(crate) enum Batch {
    Plain(Vec<LinkEvent>),
    Scene(Deferred<Vec<LinkEvent>>),
}

impl fmt::Debug for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("events", &self.len())
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for Batch {
    type Target = Vec<LinkEvent>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Plain(events) => events,
            Self::Scene(events) => events,
        }
    }
}

impl std::ops::DerefMut for Batch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Plain(events) => events,
            Self::Scene(events) => events,
        }
    }
}

impl Batch {
    pub(crate) fn attribute(&mut self, bytes: usize) {
        if let Self::Scene(events) = self {
            events.attribute(bytes);
        }
    }
}

/// Protects scene batches through lost delivery; ordinary batches free in place.
pub(crate) fn batch(first: LinkEvent, rest: &Receiver<LinkEvent>) -> Batch {
    let mut events = vec![first];
    while events.len() < EVENT_BATCH
        && let Ok(event) = rest.try_recv()
    {
        events.push(event);
    }
    if !events
        .iter()
        .any(|event| matches!(event, LinkEvent::SceneData { .. }))
    {
        return Batch::Plain(events);
    }
    let payload = events
        .iter()
        .map(|event| match event {
            LinkEvent::Data { bytes, .. } | LinkEvent::SceneData { bytes, .. } => {
                usize::try_from(*bytes).unwrap_or(usize::MAX)
            }
            _ => 0,
        })
        .fold(0, usize::saturating_add);
    let bytes = (events.capacity() * size_of::<LinkEvent>()).saturating_add(payload);
    Batch::Scene(Deferred::new(events, bytes))
}

/// One connection as the UI sees it, from its `Connected` on. Sends and grants through the
/// session of a connection that has ended do nothing, so nothing meant for one connection
/// reaches the next.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    alive: Arc<AtomicBool>,
    /// Counts connections in this shell; the brain's incarnation can't, since the same brain may
    /// be reconnected to.
    generation: u64,
    credit: Arc<Credit>,
    /// The brain it reached.
    incarnation: Incarnation,
}

impl Session {
    pub(crate) fn alive(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn incarnation(&self) -> Incarnation {
        self.incarnation
    }

    /// Grants this connection's brain `bytes` more of data frames, once the UI has taken a frame
    /// that size. Grants add up until the data writer sends them. Never blocks or locks.
    pub(crate) fn grant(&self, bytes: u32) {
        // The writer sends whatever has added up after each message, so only the first grant
        // since the last send needs to wake it; a full queue wakes it anyway.
        if self.credit.bytes.fetch_add(bytes, Ordering::SeqCst) == 0
            && let Some(wake) = self.credit.wake.upgrade()
        {
            let _woken = wake.try_send(DataOut::Credit);
        }
    }
}

/// A connection's credit granted and not yet sent.
#[derive(Debug)]
struct Credit {
    bytes: AtomicU32,
    /// The data writer's queue, weak so a session the UI still holds keeps no writer alive.
    wake: WeakSender<DataOut>,
}

#[derive(Clone)]
struct EndSession(Arc<AtomicBool>);

impl Drop for EndSession {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
    /// Credit is waiting in the connection's [`Credit`].
    Credit,
}

impl Drop for DataOut {
    fn drop(&mut self) {
        if let Self::Submit(DataToBrain::MicPcm(packet)) = self {
            packet.samples.zeroize();
        }
    }
}

impl fmt::Debug for DataOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DataOut({:?})", std::mem::discriminant(self))
    }
}

/// Where the UI's messages go while connected.
#[derive(Debug)]
struct Outbox {
    generation: u64,
    control: Sender<ControlToBrain>,
    data: Sender<DataOut>,
    /// The control pipe, so the UI can end a connection that broke the protocol.
    pipe: Arc<PipeClient>,
    /// The `TextSubmit`s queued on this connection, which a delete's `after_turns` counts.
    submits: AtomicU32,
}

/// A delete the user confirmed, until the brain answers it.
#[derive(Debug)]
struct Held {
    thread: ThreadId,
    /// The connection it was last queued on.
    sent_on: Option<u64>,
    /// It reached an earlier connection, whose brain may already have answered it.
    resend: bool,
}

/// Queues every held delete not yet queued on `outbox`'s connection, in order, each with the
/// turns queued before it. False when the control queue is full.
fn queue_deletes(outbox: &Outbox, deletes: &mut [Held]) -> bool {
    let after_turns = outbox.submits.load(Ordering::SeqCst);
    queue_on(&outbox.control, outbox.generation, after_turns, deletes)
}

/// [`queue_deletes`] on connection `generation`'s control queue. A delete that reached an
/// earlier connection stays a resend through every retry.
fn queue_on(
    control: &Sender<ControlToBrain>,
    generation: u64,
    after_turns: u32,
    deletes: &mut [Held],
) -> bool {
    for held in deletes
        .iter_mut()
        .filter(|held| held.sent_on != Some(generation))
    {
        held.resend |= held.sent_on.is_some();
        let delete = ControlToBrain::DeleteThread {
            thread: held.thread,
            after_turns,
            resend: held.resend,
        };
        if control.try_send(delete).is_err() {
            return false;
        }
        held.sent_on = Some(generation);
    }
    true
}

/// What the UI thread and the link thread share.
#[derive(Debug, Default)]
struct Shared {
    microphone: CollectionGate,
    input: AtomicU32,
    choice: AtomicU64,
    /// Set once the user chose Quit, so a closing brain isn't restarted.
    quitting: AtomicBool,
    /// The control pipe while connected, so Quit can bound its writes.
    control: Mutex<Option<Arc<PipeClient>>>,
    outbox: Mutex<Option<Outbox>>,
    /// The last connection's generation.
    generations: AtomicU64,
    /// Locked only after `outbox`, so a delete and a turn are counted in the order they're
    /// queued.
    deletes: Mutex<Vec<Held>>,
}

impl Shared {
    fn revoke_input(&self) -> Option<GestureId> {
        let gesture = GestureId(self.input.swap(0, Ordering::AcqRel));
        if gesture.0 == 0 {
            return None;
        }
        self.microphone.cancel(gesture);
        Some(gesture)
    }

    /// Uses the outbox if it's still `session`'s connection's.
    fn outbox<T>(&self, session: &Session, use_it: impl FnOnce(&Outbox) -> T) -> Option<T> {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|outbox| {
                outbox.generation == session.generation && session.alive.load(Ordering::Acquire)
            })
            .map(use_it)
    }

    fn deletes(&self) -> std::sync::MutexGuard<'_, Vec<Held>> {
        self.deletes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues the held deletes and then `Quitting` on the current connection, so a delete the
    /// user confirmed reaches the brain before Quit. False when there's no connection or its
    /// control queue is full.
    fn tell_quit(&self) -> bool {
        let outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(outbox) = outbox.as_ref() else {
            return false;
        };
        queue_deletes(outbox, &mut self.deletes())
            && outbox.control.try_send(ControlToBrain::Quitting).is_ok()
    }
}

/// The UI's handle on the link.
#[derive(Clone, Debug)]
pub(crate) struct LinkHandle {
    shared: Arc<Shared>,
    retry: Sender<()>,
    /// Closes when the link thread ends.
    done: Receiver<()>,
    #[cfg(test)]
    recording: Option<Sender<DataToBrain>>,
    #[cfg(test)]
    control_recording: Option<Sender<ControlToBrain>>,
}

impl LinkHandle {
    pub(crate) fn microphone_gate(&self) -> CollectionGate {
        self.shared.microphone.clone()
    }
    pub(crate) fn choice_revision(&self) -> u64 {
        self.shared.choice.load(Ordering::Acquire)
    }
    pub(crate) fn link_generation(&self) -> u64 {
        self.shared.generations.load(Ordering::Acquire)
    }
    pub(crate) fn track_input(&self, gesture: GestureId) {
        self.shared.input.store(gesture.0, Ordering::Release);
    }
    pub(crate) fn input_is(&self, gesture: GestureId) -> bool {
        gesture.0 != 0 && self.shared.input.load(Ordering::Acquire) == gesture.0
    }
    pub(crate) fn clear_input(&self, gesture: GestureId) {
        let input = &self.shared.input;
        let _changed = input.compare_exchange(gesture.0, 0, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(crate) fn cancel_input(&self) -> bool {
        self.cancel_input_gesture().is_some()
    }
    pub(crate) fn cancel_input_gesture(&self) -> Option<GestureId> {
        let shared = &self.shared;
        let gesture = shared.revoke_input()?;
        let cancel = ControlToBrain::GestureCancel { gesture };
        #[cfg(test)]
        if let Some(recording) = &self.control_recording {
            let _queued = recording.try_send(cancel);
            return Some(gesture);
        }
        let outbox = shared.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(outbox) = outbox.as_ref()
            && outbox.control.try_send(cancel).is_err()
        {
            outbox.pipe.abort();
        }
        Some(gesture)
    }

    /// Marks Quit, so the link starts no brain from here on. Cheap enough for the UI thread.
    /// Returns false when Quit was already under way.
    pub(crate) fn begin_quit(&self) -> bool {
        self.cancel_input();
        let first = !self.shared.quitting.swap(true, Ordering::SeqCst);
        // Wakes a link waiting for Retry, so it sees Quit and ends.
        let _woken = self.retry.try_send(());
        first
    }

    /// Tells a connected brain the user quit, after every delete the user confirmed that it
    /// hasn't been sent, so it ends its helpers and exits instead of relaunching the shell; then
    /// waits for the link to end. A link that's still connecting tells it once connected, and
    /// a full control queue is tried again. Blocks for up to [`CONNECT_TIMEOUT`] plus
    /// [`QUIT_TIMEOUT`], so call it off the UI thread; if the brain never takes them, the
    /// deletes and Quit aren't sent.
    pub(crate) fn finish_quit(&self) {
        let deadline = Instant::now() + CONNECT_TIMEOUT + QUIT_TIMEOUT;
        let mut told = false;
        // shortcut: polls for the link's end; a joinable handle would avoid it, once Quit
        // needs more than this one wait.
        while !self.done.is_closed() && Instant::now() < deadline {
            if !told {
                let control = self
                    .shared
                    .control
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                // A stalled brain can't hold the shell open.
                if let Some(control) = control {
                    control.set_timeout(Some(QUIT_TIMEOUT));
                }
                told = self.shared.tell_quit();
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !told {
            tracing::warn!("couldn't tell the brain about Quit");
        }
        if !self.done.is_closed() {
            tracing::warn!("the brain didn't close the pipe after Quit");
        }
    }

    /// Holds a delete the user confirmed until [`LinkHandle::settle_delete`]; it's sent by
    /// [`LinkHandle::send_deletes`], and by Quit.
    pub(crate) fn delete(&self, thread: ThreadId) {
        self.cancel_input();
        self.shared.deletes().push(Held {
            thread,
            sent_on: None,
            resend: false,
        });
    }

    /// Queues the held deletes `session`'s connection hasn't been sent. False when some
    /// couldn't be: that connection has ended or its control queue is full. Never blocks.
    pub(crate) fn send_deletes(&self, session: &Session) -> bool {
        self.shared
            .outbox(session, |outbox| {
                queue_deletes(outbox, &mut self.shared.deletes())
            })
            .unwrap_or(false)
    }

    /// The brain answered `thread`'s delete, so it's no longer sent.
    pub(crate) fn settle_delete(&self, thread: ThreadId) {
        self.shared.deletes().retain(|held| held.thread != thread);
    }

    /// Tries to reach the brain again after `Failed`.
    pub(crate) fn retry(&self) {
        if self.retry.try_send(()).is_err() {
            tracing::debug!("a retry is already pending");
        }
    }

    /// Queues a control message for `session`'s brain. False once that connection has ended, or
    /// when the brain has stopped reading. Never blocks.
    pub(crate) fn send_control(&self, session: &Session, message: ControlToBrain) -> bool {
        if !session.alive.load(Ordering::Acquire) {
            return false;
        }
        let shared = &self.shared;
        let mutation = matches!(
            &message,
            ControlToBrain::ThreadOp(ThreadOp::SetModel { .. })
                | ControlToBrain::Settings(
                    SettingsOp::SetDefaultModel(_)
                        | SettingsOp::SetHoldToTalk(false)
                        | SettingsOp::RemoveVoice(VoicePart::SpeechInput)
                )
                | ControlToBrain::AccountOp(_)
                | ControlToBrain::Connection(_)
        );
        let send = |out: &Sender<ControlToBrain>| {
            if mutation {
                let cancelled = shared.revoke_input();
                let choice = &shared.choice;
                let revision = choice.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    old.checked_add(1)
                });
                if revision.is_err()
                    || cancelled.is_some_and(|gesture| {
                        out.try_send(ControlToBrain::GestureCancel { gesture })
                            .is_err()
                    })
                {
                    return false;
                }
            }
            out.try_send(message).is_ok()
        };
        #[cfg(test)]
        if let Some(recording) = &self.control_recording {
            let queued = send(recording);
            if !queued && mutation {
                session.alive.store(false, Ordering::Release);
            }
            return queued;
        }
        shared
            .outbox(session, |outbox| {
                let queued = send(&outbox.control);
                if !queued && mutation {
                    session.alive.store(false, Ordering::Release);
                    outbox.pipe.abort();
                }
                queued
            })
            .unwrap_or(false)
    }

    /// Ends `session`'s connection, as a malformed frame does, so the link reconnects. Nothing
    /// happens once that connection has ended. Never blocks.
    pub(crate) fn end(&self, session: &Session) {
        #[cfg(test)]
        if let Some(recording) = &self.control_recording
            && recording.capacity().is_some()
        {
            recording.close();
        }
        self.shared.outbox(session, |outbox| outbox.pipe.abort());
        session.alive.store(false, Ordering::Release);
    }

    /// Queues a typed turn or a page request for `session`'s brain, like
    /// [`LinkHandle::send_control`]. A turn is counted once queued; one past the count ends the
    /// connection instead.
    pub(crate) fn send_data(&self, session: &Session, message: DataToBrain) -> bool {
        let out = DataOut::Submit(message);
        if !session.alive.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(test)]
        if let Some(recording) = &self.recording {
            let mut out = out;
            let DataOut::Submit(message) = &mut out else {
                return false;
            };
            return recording
                .try_send(std::mem::replace(message, DataToBrain::Credit { bytes: 0 }))
                .map_err(|error| DataOut::Submit(error.into_inner()))
                .is_ok();
        }
        self.shared
            .outbox(session, |outbox| {
                let turn = matches!(
                    &out,
                    DataOut::Submit(DataToBrain::TextSubmit { .. } | DataToBrain::AskAbout { .. })
                );
                if turn && outbox.submits.load(Ordering::SeqCst) == u32::MAX {
                    session.alive.store(false, Ordering::Release);
                    outbox.pipe.abort();
                    return false;
                }
                let queued = outbox.data.try_send(out).is_ok();
                if queued && turn {
                    outbox.submits.fetch_add(1, Ordering::SeqCst);
                }
                queued
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
impl LinkHandle {
    /// A handle with no link thread, pipe or outbox, so every send reports the connection gone.
    pub(crate) fn detached() -> Self {
        let (retry, _retries) = async_channel::bounded(1);
        let (_alive, done) = async_channel::bounded(1);
        Self {
            shared: Arc::new(Shared::default()),
            retry,
            done,
            recording: None,
            control_recording: None,
        }
    }

    pub(crate) fn recording() -> (Self, Receiver<DataToBrain>) {
        let (recording, requests) = async_channel::unbounded();
        let mut link = Self::detached();
        link.recording = Some(recording);
        link.shared.generations.store(1, Ordering::Release);
        (link, requests)
    }

    pub(crate) fn record_controls(&mut self) -> Receiver<ControlToBrain> {
        let (sender, receiver) = async_channel::unbounded();
        self.control_recording = Some(sender);
        receiver
    }

    pub(crate) fn record_controls_bounded(&mut self) -> Receiver<ControlToBrain> {
        let (sender, receiver) = async_channel::bounded(CONTROL_QUEUE);
        self.control_recording = Some(sender);
        receiver
    }
}

#[cfg(test)]
impl Session {
    /// A session with no connection behind it: its grants only add up here.
    pub(crate) fn detached(generation: u64, incarnation: Incarnation) -> Self {
        let (wake, _sleeping) = async_channel::bounded::<DataOut>(1);
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            generation,
            credit: Arc::new(Credit {
                bytes: AtomicU32::new(0),
                wake: wake.downgrade(),
            }),
            incarnation,
        }
    }

    /// The credit granted and not yet sent.
    pub(crate) fn granted(&self) -> u32 {
        self.credit.bytes.load(Ordering::SeqCst)
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
        #[cfg(test)]
        recording: None,
        #[cfg(test)]
        control_recording: None,
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
    #[expect(
        clippy::too_many_lines,
        reason = "one connection keeps its scoped readers and writers together"
    )]
    fn attend(&self, connection: Connection, events: &Sender<LinkEvent>) -> bool {
        let Connection {
            control,
            data,
            incarnation,
        } = connection;
        let (control_out, control_queue) = async_channel::bounded(CONTROL_QUEUE);
        let (data_out, data_queue) = async_channel::bounded(DATA_QUEUE);
        let Ok(previous) =
            self.shared
                .generations
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_add(1)
                })
        else {
            let _notified = events.send_blocking(LinkEvent::State(LinkState::Failed));
            return false;
        };
        let generation = previous + 1;
        let ended = EndSession(Arc::new(AtomicBool::new(true)));
        let credit = Arc::new(Credit {
            bytes: AtomicU32::new(0),
            wake: data_out.downgrade(),
        });
        let mut helpers = Vec::new();
        let spawned = spawn_helper("link-control-writer", &mut helpers, {
            let control = Arc::clone(&control);
            let ended = ended.clone();
            move || {
                let _ended = ended;
                write_control(&control, &control_queue);
            }
        })
        .and_then(|()| {
            spawn_helper("link-data-writer", &mut helpers, {
                let (data, credit) = (Arc::clone(&data), Arc::clone(&credit));
                let ended = ended.clone();
                move || {
                    let _ended = ended;
                    write_data(&data, &data_queue, &credit);
                }
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
            generation,
            control: control_out.clone(),
            data: data_out,
            pipe: Arc::clone(&control),
            submits: AtomicU32::new(0),
        });
        // Quit may have come while connecting, when there was no pipe to send it on; held
        // deletes go first. A full queue is retried by Quit's wait.
        if self.quitting() {
            control.set_timeout(Some(QUIT_TIMEOUT));
            let _told = self.shared.tell_quit();
        }
        let open = events
            .send_blocking(LinkEvent::Connected(Session {
                alive: Arc::clone(&ended.0),
                generation,
                credit,
                incarnation,
            }))
            .is_ok();
        let read = spawned.and_then(|()| {
            // Only now, so nothing from the brain reaches the UI before Connected.
            spawn_helper("link-data-reader", &mut helpers, {
                let (data, control, events) =
                    (Arc::clone(&data), Arc::clone(&control), events.clone());
                let ended = ended.clone();
                move || {
                    let _ended = ended;
                    read_data(&data, &control, &events, generation);
                }
            })
        });
        match read {
            Ok(()) if open => watch(&control, &control_out, events, generation, &self.shared),
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "can't start the link's threads"),
        }
        drop(ended);
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
        let incarnation = welcome(&control)?;
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

/// The brain's answer to Hello: its incarnation, or why this brain can't be used.
fn welcome(control: &PipeClient) -> Result<Incarnation, LinkError> {
    match frame::read::<ControlToShell>(&mut &*control, Pipe::Control)? {
        None
        | Some(
            ControlToShell::RunState { .. }
            | ControlToShell::Ping
            | ControlToShell::Scenes { .. }
            | ControlToShell::ThreadDeleted { .. }
            | ControlToShell::TurnRefused { .. }
            | ControlToShell::Settings { .. }
            | ControlToShell::Error(_)
            | ControlToShell::ScenesV1 { .. }
            | ControlToShell::NoVisual { .. }
            | ControlToShell::Account(_)
            | ControlToShell::ThreadsChanged
            | ControlToShell::Focus { .. }
            | ControlToShell::CaptureDone { .. }
            | ControlToShell::CaptureFailed { .. }
            | ControlToShell::ThreadModelSet { .. }
            | ControlToShell::GitHub(_)
            | ControlToShell::GitHubError(_)
            | ControlToShell::ActionState { .. }
            | ControlToShell::IssueRefused { .. }
            | ControlToShell::Connections(_)
            | ControlToShell::AnthropicKeyStage { .. }
            | ControlToShell::AnthropicKeyStageDone { .. }
            | ControlToShell::SpeechRevoked { .. }
            | ControlToShell::VoiceHoldStatus { .. }
            | ControlToShell::Voice(_),
        ) => Err(LinkError::Rejected),
        Some(ControlToShell::BuildMismatch { brain_build }) => {
            tracing::warn!(
                brain_build,
                shell_build = BUILD_ID,
                "brain from another build"
            );
            Err(LinkError::BuildMismatch)
        }
        Some(ControlToShell::Welcome { incarnation }) => Ok(incarnation),
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

/// Reads the control pipe until the connection ends: answers pings and passes run states and
/// scenes on. A repeated handshake ends the connection, as the brain treats a protocol violation.
#[expect(clippy::too_many_lines, reason = "one arm per control message")]
fn watch(
    control: &PipeClient,
    out: &Sender<ControlToBrain>,
    events: &Sender<LinkEvent>,
    generation: u64,
    shared: &Shared,
) {
    loop {
        match frame::read::<ControlToShell>(&mut &*control, Pipe::Control) {
            Ok(None) => return,
            Ok(Some(ControlToShell::Ping)) => {
                if out.try_send(ControlToBrain::Pong).is_err() {
                    tracing::warn!("the control queue is full; the brain isn't reading");
                }
            }
            Ok(Some(ControlToShell::Settings { view, answer })) => {
                if events
                    .send_blocking(LinkEvent::Settings {
                        generation,
                        view,
                        answer,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Voice(view))) => {
                if events
                    .send_blocking(LinkEvent::Voice {
                        generation,
                        speech_input: view.speech_input,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::RunState {
                run,
                thread,
                phase,
                error,
                origin,
            })) => {
                if events
                    .send_blocking(LinkEvent::RunState {
                        generation,
                        run,
                        thread,
                        phase,
                        error,
                        origin,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Scenes { scenes })) => {
                if events.send_blocking(LinkEvent::Scenes(scenes)).is_err() {
                    return;
                }
            }
            Ok(Some(ControlToShell::ScenesV1 { scenes })) => {
                if events
                    .send_blocking(LinkEvent::ScenesV1 { generation, scenes })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::ThreadDeleted {
                thread,
                outcome,
                last_run,
            })) => {
                let deleted = LinkEvent::ThreadDeleted {
                    thread,
                    outcome,
                    last_run,
                };
                if events.send_blocking(deleted).is_err() {
                    return;
                }
            }
            Ok(Some(ControlToShell::TurnRefused { turn, thread })) => {
                if events
                    .send_blocking(LinkEvent::TurnRefused {
                        generation,
                        turn,
                        thread,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::ThreadsChanged)) => {
                if events.send_blocking(LinkEvent::ThreadsChanged).is_err() {
                    return;
                }
            }
            Ok(Some(ControlToShell::Focus { run, thread })) => {
                let focus = LinkEvent::Focus {
                    generation,
                    run,
                    thread,
                };
                if events.send_blocking(focus).is_err() {
                    return;
                }
            }
            Ok(Some(ControlToShell::Error(error))) => {
                if events
                    .send_blocking(LinkEvent::Error { generation, error })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Account(view))) => {
                if events
                    .send_blocking(LinkEvent::Account { generation, view })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::GitHub(view))) => {
                if events
                    .send_blocking(LinkEvent::GitHub { generation, view })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::GitHubError(error))) => {
                if events
                    .send_blocking(LinkEvent::GitHubError { generation, error })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::VoiceHoldStatus { gesture, status })) => {
                if generation == shared.generations.load(Ordering::Acquire)
                    && matches!(
                        status,
                        HoldStatus::Empty | HoldStatus::Cancelled | HoldStatus::Failed(_)
                    )
                {
                    shared.microphone.cancel(gesture);
                    let input = &shared.input;
                    let _cleared =
                        input.compare_exchange(gesture.0, 0, Ordering::AcqRel, Ordering::Acquire);
                }
                let event = LinkEvent::VoiceHold {
                    generation,
                    gesture,
                    status,
                };
                if events.send_blocking(event).is_err() {
                    return;
                }
            }
            Ok(Some(
                reply @ (ControlToShell::AnthropicKeyStage { .. }
                | ControlToShell::AnthropicKeyStageDone { .. }),
            )) => {
                if events
                    .send_blocking(LinkEvent::KeyStage { generation, reply })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Connections(view))) => {
                if events
                    .send_blocking(LinkEvent::Connections { generation, view })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::ActionState {
                draft,
                version,
                state,
                approval,
            })) => {
                if events
                    .send_blocking(LinkEvent::ActionState {
                        generation,
                        draft,
                        version,
                        state,
                        approval,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::CaptureDone { capture_for })) => {
                if events
                    .send_blocking(LinkEvent::CaptureDone {
                        generation,
                        capture_for,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::CaptureFailed {
                capture_for,
                reason,
            })) => {
                if events
                    .send_blocking(LinkEvent::CaptureFailed {
                        generation,
                        capture_for,
                        reason,
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Slice 7's PR 6 stops playback on SpeechRevoked; nothing plays yet.
            Ok(Some(
                ControlToShell::IssueRefused { .. }
                | ControlToShell::SpeechRevoked { .. }
                | ControlToShell::ThreadModelSet { .. }
                | ControlToShell::NoVisual { .. },
            )) => {}
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
fn read_data(data: &PipeClient, control: &PipeClient, events: &Sender<LinkEvent>, generation: u64) {
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
                let event = if matches!(
                    &message,
                    DataToShell::SceneSnapshotV1 { .. }
                        | DataToShell::SceneBegin { .. }
                        | DataToShell::SceneStepCommit { .. }
                        | DataToShell::SceneCutApplied { .. }
                        | DataToShell::SceneEnd { .. }
                        | DataToShell::ScenesRemoved { .. }
                ) {
                    LinkEvent::SceneData {
                        generation,
                        frame: Deferred::new(message, reader.bytes),
                        bytes,
                    }
                } else {
                    LinkEvent::Data {
                        generation,
                        message,
                        bytes,
                    }
                };
                if events.send_blocking(event).is_err() {
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
fn write_data(data: &PipeClient, queue: &Receiver<DataOut>, credit: &Credit) {
    let mut encoded = Zeroizing::new(Vec::new());
    while let Ok(out) = queue.recv_blocking() {
        let submitted = match &out {
            DataOut::Submit(message) => write_data_frame(&mut &*data, message, &mut encoded),
            DataOut::Credit => Ok(()),
        };
        drop(out);
        let bytes = credit.bytes.swap(0, Ordering::SeqCst);
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

fn write_data_frame(
    writer: &mut impl Write,
    message: &DataToBrain,
    encoded: &mut Zeroizing<Vec<u8>>,
) -> Result<(), FrameError> {
    let result = frame::encode_into(message, Pipe::Data, encoded)
        .and_then(|()| writer.write_all(encoded).map_err(FrameError::from));
    encoded.zeroize();
    result
}

#[cfg(all(test, windows))]
#[path = "link_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "link_delete_tests.rs"]
mod delete_tests;

#[cfg(all(test, windows))]
#[path = "link_measure_tests.rs"]
mod measure_tests;
