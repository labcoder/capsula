//! The shell's connection to the brain. Connecting and watching the control pipe run on the link
//! thread, because pipe reads block. Once connected, each pipe gets its own writer thread, fed by
//! channels the UI thread sends on without blocking, and the data pipe gets its own reader
//! thread, so no read waits behind a write and the UI never waits on the brain. Everything the
//! brain sends reaches the UI thread as [`LinkEvent`]s on one channel.
//!
//! The shell's platform connector finds or starts the brain, then checks each pipe's server
//! image before sending that leg's greeting. The link starts attendance after the handshake.

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender, WeakSender};
use kumochi_protocol::constants::BRAIN_EXE;
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AccountView, AudioSession, ConnectionsView, ControlToBrain, ControlToShell, DataToBrain,
    DataToShell, Deleted, ErrorKey, GestureId, GitHubView, HoldStatus, Incarnation, IssueSession,
    MAX_PENDING_TURNS, MAX_VISIBLE_SCENES, Phase, RunId, RunOrigin, SceneDescriptor,
    SceneDescriptorV1, SettingsOp, SettingsView, TextMetricsV1, ThreadId, ThreadOp, UserError,
};
use kumochi_protocol::voice::VoicePart;

use kumochi_scene::deferred::Deferred;
use zeroize::{Zeroize, Zeroizing};

use crate::platform::microphone::CollectionGate;
use crate::platform::{self, Connection, HANDSHAKE_TIMEOUT, PipeClient, RETRY_DELAY};
#[cfg(any(test, feature = "test-support"))]
use crate::speaker;

mod microphone;
mod narrated;
pub mod output;
mod queued;

pub use narrated::Wake as NarrationWake;

/// How long one connection attempt may take, starting the brain included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long Quit waits for the brain to take `Quitting` and close the pipe.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);
/// A second brain loss within this long needs the user's Retry [guess, v1 §2.3].
const REPEAT_WINDOW: Duration = Duration::from_secs(60);
/// Control messages waiting for the brain to read them before a send fails [guess]. The shell
/// sends a handful at a time: Stop, `ClearDrawing`, `Pong`, up to four deletes and `Quitting`.
const CONTROL_QUEUE: usize = 64;
const QUIT_INPUT: u64 = 1 << 32;
/// Marks the watched recovery gesture as revoked.
const REVOKED: u64 = 1 << 32;
const DEFERRED_CANCEL: u64 = 1 << 33;
/// Data messages waiting for the brain to read them: the turns the shell may leave unanswered,
/// and room for credit.
const DATA_QUEUE: usize = MAX_PENDING_TURNS + 8;
/// What the data reader buffers, so a small frame doesn't cost a read of its own [guess].
const READ_BUFFER: usize = 64 * 1024;

/// What the UI shows about the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
    OutputUnavailable,
}

/// What the link tells the UI thread, in the order it happened. Nothing from the brain comes
/// before `Connected`, and none of a connection's messages after its next state.
pub enum LinkEvent {
    DrawingNotice {
        generation: u64,
        key: ErrorKey,
        thread: ThreadId,
        run: RunId,
    },
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
    /// Whether a hold also shows the model a window, from the control pipe.
    ScreenHolds {
        generation: u64,
        state: kumochi_protocol::message::ScreenHolds,
    },
    /// What a screen hold carried into its turn, from the control pipe.
    HoldScreen {
        generation: u64,
        gesture: GestureId,
        outcome: kumochi_protocol::message::HeldScreen,
    },
    KeyStage {
        generation: u64,
        reply: ControlToShell,
    },
    /// Any state but `Connected`, which comes as [`LinkEvent::Connected`].
    State(LinkState),
    /// A new connection, and the session the UI sends and grants credit through.
    Connected(Session),
    /// Coalesced drawing evidence from the exact narrated pair and run.
    NarrationReady(NarrationWake),
    Settings {
        generation: u64,
        view: SettingsView,
        answer: bool,
    },
    SpokenRepliesDefaulted {
        generation: u64,
        view: SettingsView,
    },
    Account {
        generation: u64,
        view: AccountView,
    },
    AccountUnsupported {
        generation: u64,
        view: AccountView,
    },
    Voice {
        generation: u64,
        view: kumochi_protocol::message::VoiceView,
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
        request: Option<u64>,
        draft: kumochi_protocol::message::DraftId,
        version: kumochi_protocol::message::StateVersion,
        state: kumochi_protocol::message::ActionState,
        approval: Option<kumochi_protocol::message::ApprovalRev>,
    },
    IssueRefused {
        generation: u64,
        request: u64,
        draft: kumochi_protocol::message::DraftId,
        version: kumochi_protocol::message::StateVersion,
        approval: Option<kumochi_protocol::message::ApprovalRev>,
        reason: kumochi_protocol::message::IssueRefused,
    },
    IssueUnavailable {
        generation: u64,
        request: u64,
        draft: kumochi_protocol::message::DraftId,
        kind: kumochi_protocol::message::IssueUnavailableKind,
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

pub enum Batch {
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
    pub fn attribute(&mut self, bytes: usize) {
        if let Self::Scene(events) = self {
            events.attribute(bytes);
        }
    }
}

/// Protects scene batches through lost delivery; ordinary batches free in place.
pub fn batch(first: LinkEvent, rest: &Receiver<LinkEvent>) -> Batch {
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
pub struct Session {
    alive: Arc<AtomicBool>,
    /// Set once its end is under way elsewhere: nothing more is sent on it.
    closing: Arc<AtomicBool>,
    /// Counts connections in this shell; the brain's incarnation can't, since the same brain may
    /// be reconnected to.
    generation: u64,
    io: Arc<SessionIo>,
    /// The brain and authenticated connection it reached.
    issue_origin: IssueSession,
}

impl Session {
    pub fn alive(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// Refuses every later send on this connection at once, without waiting, while its end runs
    /// elsewhere.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Signals the matching queued worker without waiting on the output owner.
    pub fn stop_playback(&self, run: RunId) {
        if let Some((_, stop)) = self
            .io
            .playback_stop
            .load()
            .as_deref()
            .filter(|(id, _)| *id == run)
        {
            stop.request();
        }
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub fn playback_stop_requested(&self, run: RunId) -> Option<bool> {
        self.io
            .playback_stop
            .load()
            .as_deref()
            .filter(|(id, _)| *id == run)
            .map(|(_, stop)| stop.requested())
    }

    /// Alive and not closing: what sends and capture admission require.
    pub fn open(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.closing.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn incarnation(&self) -> Incarnation {
        self.issue_origin.incarnation
    }

    pub fn issue_session(&self) -> IssueSession {
        self.issue_origin
    }

    /// The local drawing font selected with this pair's metrics, even if sampling failed.
    pub fn drawing_font_family(&self) -> Option<&Arc<str>> {
        self.io.drawing_font_family.as_ref()
    }

    #[inline]
    pub fn has_narration(&self) -> bool {
        self.io.narrated.is_some()
    }

    pub fn attach_narration(&self, presenter: &mut crate::presenter::Presenter) {
        if let Some(bridge) = &self.io.narrated {
            bridge.attach(presenter);
        }
    }

    #[must_use]
    pub fn accepts_narration(&self, wake: NarrationWake) -> bool {
        self.open() && wake.take(&self.io)
    }

    /// Reconciles current evidence without waiting for either the reader or speaker.
    #[must_use]
    pub fn update_narration(
        &self,
        link: &LinkHandle,
        presenter: &mut crate::presenter::Presenter,
        now: Instant,
    ) -> bool {
        self.io.narrated.as_ref().is_none_or(|bridge| {
            if !self.open() || bridge.update(link, self, presenter, now).is_err() {
                return false;
            }
            bridge.handoff(self, presenter);
            true
        })
    }

    pub fn retry_narration(&self) -> Option<impl FnOnce() + Send + use<>> {
        self.io.narrated.as_ref()?.retry(self)
    }

    /// Resumes an interrupted scene pass only on the same pair, not by generation alone.
    #[must_use]
    pub fn resumes_narration(&self, waiting: &Self) -> bool {
        self.open()
            && Arc::ptr_eq(&self.io, &waiting.io)
            && self
                .io
                .narrated
                .as_ref()
                .is_some_and(|bridge| bridge.resume())
    }

    /// Grants this connection's brain `bytes` more of data frames, once the UI has taken a frame
    /// that size. Grants add up until the data writer sends them. Never blocks or locks.
    pub fn grant(&self, bytes: u32) {
        if self.open() {
            self.io.grant(bytes);
        }
    }
}

impl SessionIo {
    fn grant(&self, bytes: u32) {
        // The writer sends whatever has added up after each message, so only the first grant
        // since the last send needs to wake it; a full queue wakes it anyway.
        if self.bytes.fetch_add(bytes, Ordering::SeqCst) == 0
            && let Some(wake) = self.wake.upgrade()
        {
            let _woken = wake.try_send(DataOut::Credit);
        }
    }
}

/// One connection's send endpoints, weak so a session the UI holds keeps no writer alive.
#[derive(Debug)]
struct SessionIo {
    queued: bool,
    narrated: Option<Arc<narrated::Narrated>>,
    control: Option<WeakSender<ControlToBrain>>,
    playback_stop: Arc<output::PlaybackStop>,
    bytes: AtomicU32,
    wake: WeakSender<DataOut>,
    drawing_font_family: Option<Arc<str>>,
}

#[derive(Clone)]
struct EndSession {
    alive: Arc<AtomicBool>,
    shared: Arc<Shared>,
    queued: Option<u64>,
}

impl Drop for EndSession {
    fn drop(&mut self) {
        if let Some(session) = self.queued {
            self.shared.output.end(session);
        }
        self.shared.lose(&self.alive);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attendance {
    Ended,
    Lost,
    Retry,
    TalkBackChanged,
}

/// What the data writer sends.
enum DataOut {
    Submit(DataToBrain),
    /// Credit is waiting in the connection's [`SessionIo`].
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
    queued_input: QueuedInput,
    /// Its session's [`Session::close`].
    closing: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct QueuedInput {
    hold: Option<QueuedHold>,
    settings_answers: u32,
    consent_answers: u32,
}

fn changes_hold_consent(op: &SettingsOp) -> bool {
    matches!(
        op,
        SettingsOp::SetTalkBack(_)
            | SettingsOp::SetShowScreen(_)
            | SettingsOp::SetScreenHolds(_)
            | SettingsOp::InstallVoice(VoicePart::SpokenReplies)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuedHold {
    Recognized(GestureId),
    Listening(GestureId),
    Released(GestureId),
    Cancelled(GestureId),
}

impl QueuedHold {
    fn cancelled(self, gesture: GestureId) -> Option<Self> {
        matches!(self, Self::Recognized(id) | Self::Listening(id) if id == gesture)
            .then_some(Self::Cancelled(gesture))
    }

    fn gesture(self) -> GestureId {
        match self {
            Self::Recognized(id)
            | Self::Listening(id)
            | Self::Released(id)
            | Self::Cancelled(id) => id,
        }
    }
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
    microphone_stop: Mutex<Option<microphone::Stop>>,
    input: AtomicU64,
    // Only the UI thread reads or writes this stamp, so Relaxed is enough.
    input_generation: AtomicU64,
    /// The gesture whose held words the UI keeps, with `REVOKED` once its input was revoked.
    recovery: AtomicU64,
    recovery_generation: AtomicU64,
    choice: AtomicU64,
    quit_sent: AtomicBool,
    /// The control pipe while connected, so Quit can bound its writes.
    control: Mutex<Option<Arc<PipeClient>>>,
    outbox: Mutex<Option<Outbox>>,
    audio: OnceLock<AudioSession>,
    /// The last connection's generation.
    generations: AtomicU64,
    output: output::OutputOwner,
    narrated: bool,
    #[cfg(any(test, feature = "test-support"))]
    hold_control_writer: std::sync::atomic::AtomicU8,
    #[cfg(any(test, feature = "test-support"))]
    hold_pcm_writer: std::sync::atomic::AtomicU8,
    #[cfg(any(test, feature = "test-support"))]
    hold_text_writer: std::sync::atomic::AtomicU8,
    #[cfg(any(test, feature = "test-support"))]
    fake_playback: Option<std::sync::mpsc::SyncSender<speaker::queued::FixturePlayback>>,
    #[cfg(any(test, feature = "test-support"))]
    prime_fake_playback: AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    #[cfg(windows)]
    refused_retirement: Mutex<Option<kumochi_protocol::message::SpeechRetirement>>,
    /// Locked only after `outbox`, so a delete and a turn are counted in the order they're
    /// queued.
    deletes: Mutex<Vec<Held>>,
    /// Orders Quit against an owned brain child's spawn.
    #[cfg(target_os = "macos")]
    spawn: Arc<platform::SpawnWord>,
}

impl Shared {
    /// Ends a session's permit; the output owner sees it before any later acceptance.
    fn lose(&self, alive: &AtomicBool) {
        alive.store(false, Ordering::Release);
        self.output.cancelled();
    }

    fn revoke_input(&self) -> Option<GestureId> {
        let gesture = self
            .input
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |input| {
                (input != 0 && input < QUIT_INPUT).then_some(0)
            })
            .ok()?;
        let _marked = self.recovery.compare_exchange(
            gesture,
            gesture | REVOKED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let gesture = GestureId(u32::try_from(gesture).expect("gesture fits in the input word"));
        self.microphone.cancel(gesture);
        Some(gesture)
    }

    fn quitting(&self) -> bool {
        self.input.load(Ordering::SeqCst) & QUIT_INPUT != 0
    }

    fn take_quit_input(&self) -> Option<GestureId> {
        let gesture = self.input.fetch_and(QUIT_INPUT, Ordering::SeqCst) & !QUIT_INPUT;
        (gesture != 0)
            .then(|| GestureId(u32::try_from(gesture).expect("gesture fits in the input word")))
    }

    fn input_is(&self, gesture: GestureId) -> bool {
        gesture.0 != 0 && self.input.load(Ordering::Acquire) == u64::from(gesture.0)
    }

    fn clear_input(&self, gesture: GestureId) {
        let _cleared = self.input.compare_exchange(
            u64::from(gesture.0),
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Uses the outbox to send on `session`, only while it is open (alive and not closing).
    fn outbox<T>(&self, session: &Session, use_it: impl FnOnce(&mut Outbox) -> T) -> Option<T> {
        if !session.open() {
            return None;
        }
        self.connection(session, use_it)
    }

    /// Uses the outbox if it's still `session`'s connection's, closing or not.
    fn connection<T>(&self, session: &Session, use_it: impl FnOnce(&mut Outbox) -> T) -> Option<T> {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .filter(|outbox| {
                !outbox.pipe.is_aborted()
                    && outbox.generation == session.generation
                    && session.alive.load(Ordering::Acquire)
            })
            .map(use_it)
    }

    fn with_queued_input<T>(
        &self,
        generation: u64,
        update: impl FnOnce(&mut QueuedInput) -> T,
    ) -> Option<T> {
        self.outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .filter(|outbox| outbox.generation == generation)
            .map(|outbox| update(&mut outbox.queued_input))
    }

    fn queue_control(
        &self,
        session: &Session,
        out: &Sender<ControlToBrain>,
        input: &mut QueuedInput,
        message: ControlToBrain,
    ) -> bool {
        if !session.io.queued {
            return self.queue_voice_control(out, message);
        }
        let generation = session.generation;
        if let ControlToBrain::Settings(op) = &message {
            let Some(answers) = input.settings_answers.checked_add(1) else {
                return false;
            };
            let consent = changes_hold_consent(op);
            if out.try_send(message).is_err() {
                return false;
            }
            input.settings_answers = answers;
            if consent {
                input.consent_answers = answers;
            }
            return true;
        }
        let hold = &mut input.hold;
        let replacement = match &message {
            ControlToBrain::KeyDown { .. } => {
                return input.consent_answers == 0
                    && self.output.queued_hold_ready(generation)
                    && out.try_send(message).is_ok();
            }
            ControlToBrain::VoiceHoldRecognized { gesture, .. }
            | ControlToBrain::ScreenHoldRecognized { gesture, .. } => {
                if hold.is_some()
                    || input.consent_answers != 0
                    || !self.input_is(*gesture)
                    || !self.output.can_reserve_hold(generation)
                {
                    return false;
                }
                Some(QueuedHold::Recognized(*gesture))
            }
            ControlToBrain::KeyUp { gesture, .. }
            | ControlToBrain::ScreenRelease { gesture, .. } => {
                if *hold != Some(QueuedHold::Listening(*gesture)) || !self.input_is(*gesture) {
                    return false;
                }
                matches!(message, ControlToBrain::KeyUp { .. })
                    .then_some(QueuedHold::Released(*gesture))
            }
            ControlToBrain::GestureCancel { gesture } => {
                hold.and_then(|hold| hold.cancelled(*gesture))
            }
            _ => None,
        };
        // Published before KeyUp can reach the Brain, whose early promise reads it.
        let released = matches!(replacement, Some(QueuedHold::Released(_)));
        if released {
            self.output.hold_released.store(true, Ordering::Release);
        }
        if !self.queue_voice_control(out, message) {
            if released {
                self.output.hold_released.store(false, Ordering::Release);
            }
            return false;
        }
        if let Some(replacement) = replacement {
            *hold = Some(replacement);
        }
        true
    }

    fn deletes(&self) -> std::sync::MutexGuard<'_, Vec<Held>> {
        self.deletes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues the held deletes and then `Quitting` on the current connection, so a delete the
    /// user confirmed reaches the brain before Quit. False when there's no connection or its
    /// control queue is full.
    fn tell_quit(&self) -> bool {
        // The one send that skips the closed check: a brain never told Quit relaunches the shell.
        let outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
        if self.quit_sent.load(Ordering::Acquire) {
            return true;
        }
        let Some(outbox) = outbox.as_ref().filter(|outbox| !outbox.pipe.is_aborted()) else {
            return false;
        };
        if let Some(gesture) = self.take_quit_input()
            && outbox
                .control
                .try_send(ControlToBrain::GestureCancel { gesture })
                .is_err()
        {
            outbox.pipe.abort();
            return false;
        }
        if !queue_deletes(outbox, &mut self.deletes())
            || outbox.control.try_send(ControlToBrain::Quitting).is_err()
            || outbox.pipe.is_aborted()
        {
            return false;
        }
        self.quit_sent.store(true, Ordering::Release);
        true
    }
}

/// A locally revoked gesture whose wire cancellation belongs to one link generation.
#[derive(Debug)]
pub struct InputCancel {
    generation: u64,
    gesture: GestureId,
}

impl InputCancel {
    /// The revoked gesture and the link generation that tracked it.
    pub fn target(&self) -> (GestureId, u64) {
        (self.gesture, self.generation)
    }
}

/// The UI's handle on the link.
#[derive(Clone, Debug)]
pub struct LinkHandle {
    shared: Arc<Shared>,
    worker: Option<LinkWorker>,
    #[cfg(any(test, feature = "test-support"))]
    recording: Option<Sender<DataToBrain>>,
    #[cfg(any(test, feature = "test-support"))]
    control_recording: Option<Sender<ControlToBrain>>,
}

#[derive(Clone, Debug)]
struct LinkWorker {
    retry: Sender<()>,
    /// Closes when the link thread ends.
    done: Receiver<()>,
}

impl LinkHandle {
    pub fn audio_session(&self) -> AudioSession {
        self.shared
            .audio
            .get()
            .copied()
            .unwrap_or(AudioSession::Legacy)
    }

    pub fn admits_text(&self, session: Option<&Session>) -> bool {
        if self.worker.is_none() {
            return false;
        }
        self.audio_session() == AudioSession::Legacy
            || session.is_some_and(|session| {
                self.shared
                    .outbox(session, |outbox| {
                        outbox.queued_input.hold.is_none()
                            && self
                                .shared
                                .output
                                .queued_turn(session.generation, None, || session.open())
                    })
                    .unwrap_or(false)
            })
    }

    // shortcut: A contended first edge refuses the press; if it shows in use, publish eligibility as one atomic word.
    pub fn can_start_voice_gesture(&self, generation: u64) -> bool {
        if self.worker.is_none() {
            return false;
        }
        self.audio_session() == AudioSession::Legacy
            || self.shared.outbox.try_lock().is_ok_and(|outbox| {
                outbox.as_ref().is_some_and(|outbox| {
                    outbox.generation == generation
                        && outbox.queued_input.consent_answers == 0
                        && self.shared.output.try_queued_hold_ready(generation)
                })
            })
    }

    /// Checks the exact pair before presence changes voice panels or attempts the offer.
    pub fn admits_voice(&self, generation: u64) -> bool {
        if self.worker.is_none() {
            return false;
        }
        self.audio_session() == AudioSession::Legacy
            || self
                .shared
                .outbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|outbox| {
                    outbox.generation == generation
                        && !outbox.pipe.is_aborted()
                        && outbox.queued_input.consent_answers == 0
                        && self.shared.output.queued_hold_ready(generation)
                })
    }

    pub fn output(&self) -> &output::OutputOwner {
        &self.shared.output
    }
    pub fn microphone_gate(&self) -> CollectionGate {
        self.shared.microphone.clone()
    }
    pub fn choice_revision(&self) -> u64 {
        self.shared.choice.load(Ordering::Acquire)
    }
    pub fn link_generation(&self) -> u64 {
        self.shared.generations.load(Ordering::Acquire)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn track_input(&self, gesture: GestureId) {
        self.track_input_on(gesture, self.link_generation());
    }

    /// Tracks a gesture with the link generation sampled during recognition.
    pub fn track_input_on(&self, gesture: GestureId, generation: u64) {
        self.shared
            .input_generation
            .store(generation, Ordering::Relaxed);
        let _tracked =
            self.shared
                .input
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |input| {
                    (input < QUIT_INPUT).then_some(u64::from(gesture.0))
                });
    }
    pub fn input_is(&self, gesture: GestureId) -> bool {
        self.shared.input_is(gesture)
    }
    pub fn clear_input(&self, gesture: GestureId) {
        self.shared.clear_input(gesture);
    }
    /// Watches `gesture`, whose held words the UI keeps, for a revoke of its input.
    pub fn watch_revoke(&self, gesture: GestureId, generation: u64) {
        self.shared
            .recovery_generation
            .store(generation, Ordering::Relaxed);
        self.shared
            .recovery
            .store(u64::from(gesture.0), Ordering::Release);
    }
    /// Whether the watched `gesture`'s input was revoked since it was watched.
    pub fn was_revoked(&self, gesture: GestureId) -> bool {
        self.shared.recovery.load(Ordering::Acquire) & !DEFERRED_CANCEL
            == u64::from(gesture.0) | REVOKED
    }
    pub fn cancel_input(&self) -> bool {
        self.cancel_input_gesture().is_some()
    }
    pub fn cancel_input_gesture(&self) -> Option<GestureId> {
        let gesture = self.shared.revoke_input()?;
        self.send_input_cancel(gesture, None);
        Some(gesture)
    }

    /// Revokes local input without touching the outbox. The caller owns dispatch even if the
    /// UI ends the same hold first.
    pub fn revoke_input_for_dispatch(&self) -> Option<InputCancel> {
        let cancel = self.revoke_input_cancel()?;
        if self.shared.recovery_generation.load(Ordering::Relaxed) == cancel.generation {
            let revoked = u64::from(cancel.gesture.0) | REVOKED;
            let _marked = self.shared.recovery.compare_exchange(
                revoked,
                revoked | DEFERRED_CANCEL,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        Some(cancel)
    }

    /// The matching cancel is owned by deferred dispatch, even before its outbox send.
    pub fn deferred_input_cancel(
        &self,
        gesture: GestureId,
        generation: u64,
    ) -> Option<InputCancel> {
        let watched = self.shared.recovery.load(Ordering::Acquire);
        (watched == u64::from(gesture.0) | REVOKED | DEFERRED_CANCEL
            && self.shared.recovery_generation.load(Ordering::Relaxed) == generation)
            .then_some(InputCancel {
                generation,
                gesture,
            })
    }

    pub(crate) fn revoke_input_cancel(&self) -> Option<InputCancel> {
        let generation = self.shared.input_generation.load(Ordering::Relaxed);
        let gesture = self.shared.revoke_input()?;
        Some(InputCancel {
            generation,
            gesture,
        })
    }

    /// Sends the revoked gesture's cancel on its original outbox, before any replacement offer.
    pub fn dispatch_input_cancel(&self, cancel: &InputCancel) -> bool {
        self.send_input_cancel(cancel.gesture, Some(cancel.generation))
    }

    fn send_input_cancel(&self, gesture: GestureId, generation: Option<u64>) -> bool {
        let cancel = ControlToBrain::GestureCancel { gesture };
        #[cfg(any(test, feature = "test-support"))]
        if let Some(recording) = &self.control_recording {
            return generation.is_none_or(|generation| generation == self.link_generation())
                && recording.try_send(cancel).is_ok();
        }
        let mut outbox = self
            .shared
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(outbox) = outbox.as_mut().filter(|outbox| {
            generation.is_none_or(|generation| {
                generation == outbox.generation && !outbox.pipe.is_aborted()
            })
        }) else {
            return false;
        };
        if outbox.control.try_send(cancel).is_ok() {
            if generation.is_some()
                && let Some(cancelled) = outbox
                    .queued_input
                    .hold
                    .and_then(|hold| hold.cancelled(gesture))
            {
                outbox.queued_input.hold = Some(cancelled);
            }
            true
        } else {
            outbox.pipe.abort();
            false
        }
    }

    /// Marks Quit, so the link starts no brain from here on. Does not wait on the output owner,
    /// outbox or pipe.
    /// Returns false when Quit was already under way.
    pub fn begin_quit(&self) -> bool {
        let first = self.shared.input.fetch_or(QUIT_INPUT, Ordering::SeqCst) & QUIT_INPUT == 0;
        #[cfg(target_os = "macos")]
        self.shared.spawn.quit();
        self.shared.microphone.cancel_current();
        // Wakes a link waiting for Retry, so it sees Quit and ends.
        if let Some(worker) = &self.worker {
            let _woken = worker.retry.try_send(());
        }
        first
    }

    /// Whether Quit has revoked input and reserved its last tracked gesture for shutdown.
    pub fn is_quitting(&self) -> bool {
        self.shared.quitting()
    }

    /// Tells a connected brain the user quit, after every delete the user confirmed that it
    /// hasn't been sent, so it ends its helpers and exits instead of relaunching the shell; then
    /// waits for the link to end. A link that's still connecting tells it once connected, and
    /// a full control queue is tried again. Blocks for up to [`CONNECT_TIMEOUT`] plus
    /// [`QUIT_TIMEOUT`], so call it off the UI thread; if the brain never takes them, the
    /// deletes and Quit aren't sent.
    pub fn finish_quit(&self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let deadline = Instant::now() + CONNECT_TIMEOUT + QUIT_TIMEOUT;
        let mut told = false;
        // shortcut: polls for the link's end; a joinable handle would avoid it, once Quit
        // needs more than this one wait.
        while !worker.done.is_closed() && Instant::now() < deadline {
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
        if !told && !self.shared.quit_sent.load(Ordering::Acquire) {
            tracing::warn!("couldn't tell the brain about Quit");
        }
        if !worker.done.is_closed() {
            tracing::warn!("the brain didn't close the pipe after Quit");
        }
    }

    /// Holds a delete the user confirmed until [`LinkHandle::settle_delete`]; it's sent by
    /// [`LinkHandle::send_deletes`], and by Quit.
    pub fn delete(&self, thread: ThreadId) {
        self.cancel_input();
        self.shared.deletes().push(Held {
            thread,
            sent_on: None,
            resend: false,
        });
    }

    /// Queues the held deletes `session`'s connection hasn't been sent. False when some
    /// couldn't be: that connection has ended or its control queue is full. Never blocks.
    pub fn send_deletes(&self, session: &Session) -> bool {
        self.shared
            .outbox(session, |outbox| {
                queue_deletes(outbox, &mut self.shared.deletes())
            })
            .unwrap_or(false)
    }

    /// The brain answered `thread`'s delete, so it's no longer sent.
    pub fn settle_delete(&self, thread: ThreadId) {
        self.shared.deletes().retain(|held| held.thread != thread);
    }

    /// Requests another attempt after `Failed`, or returns false if there is no retry listener.
    pub fn retry(&self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        match worker.retry.try_send(()) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!("a retry is already pending");
                error.is_full()
            }
        }
    }

    /// Queues a control message for `session`'s brain. False when the connection ended or the
    /// mailbox refused. Stop, drawing clears and queued scene refusals don't take the outbox lock.
    /// shortcut: control Debug prints physical points for assertions; use only redacted kinds if adding diagnostics here.
    pub fn send_control(&self, session: &Session, message: ControlToBrain) -> bool {
        if self.worker.is_none()
            || session.io.queued
                && matches!(
                    &message,
                    ControlToBrain::HoldRecognized { .. }
                        | ControlToBrain::Warm { .. }
                        | ControlToBrain::Settings(SettingsOp::SetTalkBack(_))
                )
        {
            return false;
        }
        if let ControlToBrain::VoiceHoldStopped { gesture } = &message {
            if let Some(retained) = self.retain_microphone_stop(session, *gesture) {
                return retained;
            }
            if session.io.queued {
                return true;
            }
        }
        if !session.open() {
            return false;
        }
        if matches!(
            &message,
            ControlToBrain::Stop { .. }
                | ControlToBrain::StopScenes { .. }
                | ControlToBrain::ClearDrawing { .. }
        ) || session.io.queued && matches!(&message, ControlToBrain::SceneFailed { .. })
        {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(recording) = &self.control_recording {
                return recording.try_send(message).is_ok();
            }
            return session
                .io
                .control
                .as_ref()
                .and_then(WeakSender::upgrade)
                .is_some_and(|out| session.open() && out.try_send(message).is_ok());
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
                | ControlToBrain::Connection(_)
        ) || matches!(&message, ControlToBrain::AccountOp(op) if op.changes_hold_route())
            || session.io.queued
                && matches!(&message, ControlToBrain::Settings(op) if changes_hold_consent(op));
        if session.io.queued
            && session.generation == self.link_generation()
            && let ControlToBrain::GestureCancel { gesture } = &message
        {
            shared.clear_input(*gesture);
            shared.microphone.cancel(*gesture);
        }
        let send = |out: &Sender<ControlToBrain>, input: &mut QueuedInput| {
            if mutation {
                let cancelled = shared.revoke_input();
                let choice = &shared.choice;
                let revision = choice.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    old.checked_add(1)
                });
                if revision.is_err()
                    || cancelled.is_some_and(|gesture| {
                        !shared.queue_control(
                            session,
                            out,
                            input,
                            ControlToBrain::GestureCancel { gesture },
                        )
                    })
                {
                    return false;
                }
            }
            shared.queue_control(session, out, input, message)
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Some(recording) = &self.control_recording {
            let queued = send(recording, &mut QueuedInput::default());
            if !queued && mutation {
                shared.lose(&session.alive);
            }
            return queued;
        }
        shared
            .outbox(session, |outbox| {
                let queued = send(&outbox.control, &mut outbox.queued_input);
                if !queued && mutation {
                    shared.lose(&session.alive);
                    outbox.pipe.abort();
                }
                queued
            })
            .unwrap_or(false)
    }

    /// Offers the preference without interrupting a queued pair's work.
    ///
    /// # Errors
    /// Busy if the pair is not idle; otherwise Stale or Lost if it cannot take the offer.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the wire preference is a boolean"
    )]
    pub fn send_talk_back(&self, session: &Session, on: bool) -> Result<(), output::Denied> {
        use output::Denied;

        let message = ControlToBrain::Settings(SettingsOp::SetTalkBack(on));
        if !session.io.queued {
            return self
                .send_control(session, message)
                .then_some(())
                .ok_or(Denied::Lost);
        }
        let shared = &self.shared;
        shared
            .outbox(session, |outbox| {
                let input = &mut outbox.queued_input;
                if input.hold.is_some() || input.settings_answers != 0 || self.voice_stop_pending()
                {
                    return Err(Denied::Busy);
                }
                shared.output.switch_talk_back(session.generation, on, || {
                    session.open()
                        && !shared.quitting()
                        && shared.queue_control(session, &outbox.control, input, message)
                })
            })
            .unwrap_or(Err(Denied::Stale))
    }

    /// Ends `session`'s connection, as a malformed frame does, so the link reconnects. Nothing
    /// happens once that connection has ended. It waits on no I/O or join, only briefly on the
    /// output owner's mutex. On return the output owner holds no command sender for that
    /// generation's speaker, however late the link thread's teardown is.
    pub fn end(&self, session: &Session) {
        if self.worker.is_none() {
            return;
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(recording) = &self.control_recording
            && recording.capacity().is_some()
        {
            recording.close();
        }
        if session.io.queued {
            session.close();
            session.alive.store(false, Ordering::Release);
        } else {
            self.shared
                .connection(session, |outbox| outbox.pipe.abort());
            self.shared.lose(&session.alive);
        }
        self.shared.output.end(session.generation);
    }

    /// Queues a typed turn or a page request for `session`'s brain, like
    /// [`LinkHandle::send_control`]. A turn is counted once queued; one past the count ends the
    /// connection instead.
    pub fn send_data(&self, session: &Session, message: DataToBrain) -> bool {
        use kumochi_protocol::message::ScreenChoice;
        let mic = if let DataToBrain::MicPcm(packet) = &message {
            Some(packet.gesture)
        } else {
            None
        };
        let plain_turn = match &message {
            DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                ..
            } => Some(*turn),
            DataToBrain::TextSubmit { .. } | DataToBrain::AskAbout { .. } if session.io.queued => {
                return false;
            }
            _ => None,
        };
        let out = DataOut::Submit(message);
        if !session.open() || (mic.is_some() && self.is_quitting()) {
            return false;
        }
        #[cfg(any(test, feature = "test-support"))]
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
                if mic.is_some() && self.is_quitting() {
                    return false;
                }
                let turn = matches!(
                    &out,
                    DataOut::Submit(DataToBrain::TextSubmit { .. } | DataToBrain::AskAbout { .. })
                );
                if turn && outbox.submits.load(Ordering::SeqCst) == u32::MAX {
                    self.shared.lose(&session.alive);
                    outbox.pipe.abort();
                    return false;
                }
                let enqueue = || outbox.data.try_send(out).is_ok();
                let queued = if session.io.queued && plain_turn.is_some() {
                    outbox.queued_input.hold.is_none()
                        && self
                            .shared
                            .output
                            .queued_turn(session.generation, plain_turn, || {
                                session.open() && enqueue()
                            })
                } else if let Some(gesture) = mic.filter(|_| session.io.queued) {
                    outbox.queued_input.hold == Some(QueuedHold::Listening(gesture))
                        && self.input_is(gesture)
                        && enqueue()
                } else {
                    enqueue()
                };
                if queued && turn {
                    outbox.submits.fetch_add(1, Ordering::SeqCst);
                }
                queued
            })
            .unwrap_or(false)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl LinkHandle {
    /// A handle with no link thread, pipe or outbox, so every send reports the connection gone.
    pub fn detached() -> Self {
        let (retry, _retries) = async_channel::bounded(1);
        let (_alive, done) = async_channel::bounded(1);
        Self {
            shared: Arc::new(Shared::default()),
            worker: Some(LinkWorker { retry, done }),
            recording: None,
            control_recording: None,
        }
    }

    pub fn recording() -> (Self, Receiver<DataToBrain>) {
        let (recording, requests) = async_channel::unbounded();
        let mut link = Self::detached();
        link.recording = Some(recording);
        link.shared.generations.store(1, Ordering::Release);
        // Session 1's output, as a normal launch has it: none, so a hold's claim can be granted.
        link.shared.output.begin_quiet(1);
        (link, requests)
    }

    pub fn set_recorded_generation(&self, generation: u64) {
        assert!(
            self.control_recording.is_some(),
            "only a recording can simulate link generations"
        );
        self.shared.generations.store(generation, Ordering::Release);
    }

    pub fn record_controls(&mut self) -> Receiver<ControlToBrain> {
        let (sender, receiver) = async_channel::unbounded();
        self.control_recording = Some(sender);
        receiver
    }

    pub fn record_controls_bounded(&mut self) -> Receiver<ControlToBrain> {
        let (sender, receiver) = async_channel::bounded(CONTROL_QUEUE);
        self.control_recording = Some(sender);
        receiver
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Session {
    /// A session with no connection behind it: its grants only add up here.
    pub fn detached(generation: u64, incarnation: Incarnation) -> Self {
        let (wake, _sleeping) = async_channel::bounded::<DataOut>(1);
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            closing: Arc::default(),
            generation,
            io: Arc::new(SessionIo {
                queued: false,
                narrated: None,
                control: None,
                playback_stop: Arc::default(),
                bytes: AtomicU32::new(0),
                wake: wake.downgrade(),
                drawing_font_family: None,
            }),
            issue_origin: IssueSession {
                incarnation,
                epoch: generation,
            },
        }
    }

    /// The credit granted and not yet sent.
    pub fn granted(&self) -> u32 {
        self.io.bytes.load(Ordering::SeqCst)
    }
}

/// Where to find the brain.
#[derive(Debug)]
struct Link {
    brain: PathBuf,
    session: u32,
    audio: AudioSession,
    shared: Arc<Shared>,
}

/// A link that hasn't started connecting, so the UI can arm the keyboard first. Dropping it
/// unstarted ends the link, and Quit then waits for nothing.
#[derive(Debug)]
pub struct LinkStart {
    link: Link,
    events: Sender<LinkEvent>,
    retries: Receiver<()>,
    alive: Sender<()>,
}

/// One attempt's drawing snapshot. Only the numeric profile goes over the pipe.
#[derive(Default)]
pub struct DrawingText {
    pub profile: Option<TextMetricsV1>,
    pub font_family: Option<Arc<str>>,
}

/// Prepares the link. It reports on `events` once started, and ends when the UI drops the
/// receiver or the user quits.
///
/// # Errors
/// Fails when the shell can't find its own path or session.
pub fn prepare(
    events: Sender<LinkEvent>,
    audio: AudioSession,
) -> std::io::Result<(LinkHandle, LinkStart)> {
    let link = Link {
        brain: std::env::current_exe()?.with_file_name(BRAIN_EXE),
        session: platform::session_id()?,
        audio,
        shared: Arc::new(Shared::default()),
    };
    Ok(start(link, events))
}

/// Prepares the shell's link when the platform has no admitted brain transport.
/// Reports `Failed` immediately and leaves Retry unavailable. There is no worker or session.
/// # Errors
/// Returns `BrokenPipe` if the event receiver is closed or `WouldBlock` if its queue is full.
pub fn prepare_disconnected(events: &Sender<LinkEvent>) -> std::io::Result<LinkHandle> {
    events
        .try_send(LinkEvent::State(LinkState::Failed))
        .map_err(|error| match error {
            async_channel::TrySendError::Closed(_) => std::io::ErrorKind::BrokenPipe,
            async_channel::TrySendError::Full(_) => std::io::ErrorKind::WouldBlock,
        })?;
    Ok(LinkHandle {
        shared: Arc::new(Shared::default()),
        worker: None,
        #[cfg(any(test, feature = "test-support"))]
        recording: None,
        #[cfg(any(test, feature = "test-support"))]
        control_recording: None,
    })
}

fn start(link: Link, events: Sender<LinkEvent>) -> (LinkHandle, LinkStart) {
    let (retry, retries) = async_channel::bounded(1);
    let (alive, done) = async_channel::bounded::<()>(1);
    link.shared.audio.get_or_init(|| link.audio);
    let handle = LinkHandle {
        shared: Arc::clone(&link.shared),
        worker: Some(LinkWorker { retry, done }),
        #[cfg(any(test, feature = "test-support"))]
        recording: None,
        #[cfg(any(test, feature = "test-support"))]
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
    /// The host selects its drawing font and samples it, with a bounded wait, on each attempt.
    ///
    /// # Errors
    /// Fails when the thread can't start.
    pub fn spawn(
        self,
        text_metrics: impl FnMut(Instant) -> DrawingText + Send + 'static,
    ) -> std::io::Result<()> {
        let Self {
            link,
            events,
            retries,
            alive,
        } = self;
        thread::Builder::new()
            .name("link".to_owned())
            .spawn(move || {
                link.run(&events, &retries, text_metrics);
                drop(alive);
            })?;
        Ok(())
    }
}

impl Link {
    /// Connects, attends, and reconnects after a loss. The first loss relaunches the brain on
    /// its own; another within [`REPEAT_WINDOW`], or a failed connect, waits for Retry.
    fn run(
        &self,
        events: &Sender<LinkEvent>,
        retries: &Receiver<()>,
        mut text_metrics: impl FnMut(Instant) -> DrawingText,
    ) {
        let mut state = LinkState::Connecting;
        let mut last_loss: Option<Instant> = None;
        let mut repair_of = None;
        loop {
            if self.quitting() || events.send_blocking(LinkEvent::State(state)).is_err() {
                return;
            }
            let reconnect_by = Instant::now() + CONNECT_TIMEOUT;
            let connection = match platform::connect_brain(
                &self.brain,
                self.session,
                self.audio,
                repair_of,
                reconnect_by,
                || self.quitting(),
                |control| self.quit_parked(control),
                &mut text_metrics,
            ) {
                Ok(connection) => connection,
                Err(_) if self.quitting() => return,
                Err(error) => {
                    tracing::error!(%error, "can't reach the brain");
                    if !wait_for_retry(events, retries) {
                        return;
                    }
                    (state, last_loss, repair_of) = (LinkState::Connecting, None, None);
                    continue;
                }
            };
            tracing::info!(incarnation = ?connection.issue_origin.incarnation, "connected to the brain");
            let previous = connection.issue_origin;
            let attended = self.attend_until(connection, events, reconnect_by);
            if attended == Attendance::Ended || self.quitting() {
                return;
            }
            if attended != Attendance::TalkBackChanged {
                tracing::warn!("lost the brain");
            }
            let Some(next) = self.after_loss(events, retries, attended, last_loss, previous) else {
                return;
            };
            (state, last_loss, repair_of) = next;
        }
    }

    fn after_loss(
        &self,
        events: &Sender<LinkEvent>,
        retries: &Receiver<()>,
        attended: Attendance,
        last_loss: Option<Instant>,
        previous: IssueSession,
    ) -> Option<(LinkState, Option<Instant>, Option<IssueSession>)> {
        let refused_tail = if self.audio == AudioSession::QueuedRequired {
            self.shared.output.wait_for_refused_tail()
        } else {
            None
        };
        if self.quitting() {
            return None;
        }
        if refused_tail == Some(output::RefusedTail::Unavailable) {
            let _notified = events.send_blocking(LinkEvent::State(LinkState::OutputUnavailable));
            return None;
        }
        if attended == Attendance::TalkBackChanged && refused_tail.is_none() {
            return Some((LinkState::Reconnecting, last_loss, Some(previous)));
        }
        let now = Instant::now();
        if attended == Attendance::Retry
            || refused_tail == Some(output::RefusedTail::Quiet)
            || last_loss.is_some_and(|at| now.duration_since(at) < REPEAT_WINDOW)
        {
            return wait_for_retry(events, retries).then_some((LinkState::Connecting, None, None));
        }
        Some((LinkState::Reconnecting, Some(now), None))
    }

    fn quitting(&self) -> bool {
        self.shared.quitting()
    }

    fn quit_parked(&self, control: &PipeClient) {
        self.shared.take_quit_input();
        let generation = 0;
        let (sender, pending) = async_channel::bounded(CONTROL_QUEUE);
        if !queue_on(&sender, generation, 0, &mut self.shared.deletes())
            || sender.try_send(ControlToBrain::Quitting).is_err()
        {
            return;
        }
        drop(sender);
        let deadline = Instant::now() + QUIT_TIMEOUT;
        while let Ok(message) = pending.try_recv() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            control.set_timeout(Some(left));
            if let Err(error) = frame::write(&mut &*control, &message, Pipe::Control) {
                tracing::debug!(%error, "parked control pipe shutdown write failed");
                return;
            }
        }
        self.shared.quit_sent.store(true, Ordering::Release);
    }

    fn queued_start_refusal(
        &self,
        control: &PipeClient,
        events: &Sender<LinkEvent>,
        reconnect_by: Instant,
    ) -> Option<Attendance> {
        if self.quitting() {
            self.quit_parked(control);
            Some(Attendance::Ended)
        } else if events.is_closed() {
            Some(Attendance::Ended)
        } else {
            (Instant::now() >= reconnect_by).then_some(Attendance::Retry)
        }
    }

    #[cfg(windows)]
    #[cfg(any(test, feature = "test-support"))]
    fn attend(&self, connection: Connection, events: &Sender<LinkEvent>) -> Attendance {
        self.attend_until(connection, events, Instant::now() + CONNECT_TIMEOUT)
    }

    /// Serves one connection until it ends: starts the writers, says Connected, then reads data
    /// on its own thread and control here. A refused output needs explicit retry.
    #[expect(
        clippy::too_many_lines,
        reason = "one connection keeps its scoped readers and writers together"
    )]
    fn attend_until(
        &self,
        connection: Connection,
        events: &Sender<LinkEvent>,
        reconnect_by: Instant,
    ) -> Attendance {
        let Connection {
            control,
            data,
            issue_origin,
            drawing_font_family,
        } = connection;
        let queued = self.audio == AudioSession::QueuedRequired;
        if queued && let Some(refused) = self.queued_start_refusal(&control, events, reconnect_by) {
            return refused;
        }
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
            return Attendance::Ended;
        };
        let generation = previous + 1;
        let ended = EndSession {
            alive: Arc::new(AtomicBool::new(true)),
            shared: Arc::clone(&self.shared),
            queued: queued.then_some(generation),
        };
        let narrated =
            (queued && self.shared.narrated).then(|| Arc::new(narrated::Narrated::new()));
        let credit = Arc::new(SessionIo {
            queued,
            narrated,
            control: Some(control_out.downgrade()),
            playback_stop: Arc::default(),
            bytes: AtomicU32::new(0),
            wake: data_out.downgrade(),
            drawing_font_family,
        });
        let mut helpers = Vec::new();
        let closing = Arc::<AtomicBool>::default();
        let reader = queued.then(Arc::<OnceLock<Thread>>::default);
        let registration = if let Some(reader) = &reader {
            let (ack, started) = std::sync::mpsc::sync_channel(1);
            let pipes = [Arc::downgrade(&control), Arc::downgrade(&data)];
            let (close, c, d) = (
                Arc::clone(&closing),
                control_queue.clone(),
                data_queue.clone(),
            );
            let wake = Arc::clone(reader);
            let abort: output::Abort = Arc::new(move || {
                close.store(true, Ordering::Release);
                c.close();
                d.close();
                for pipe in pipes.iter().filter_map(std::sync::Weak::upgrade) {
                    pipe.abort();
                }
                if let Some(reader) = wake.get() {
                    reader.unpark();
                }
            });
            if let Some(mode) = &credit.narrated
                && !mode.register_pair(&credit, &self.shared, &closing, reader, events)
            {
                return Attendance::Ended;
            }
            let cancelled = || self.quitting() || events.is_closed();
            let Some(deadline) = self.shared.output.begin_queued(
                generation,
                abort,
                reconnect_by,
                cancelled,
                |deadline| {
                    output::Queued::new(
                        deadline,
                        ack,
                        Arc::clone(reader),
                        (Arc::clone(&closing), Arc::clone(&credit.playback_stop)),
                        credit.narrated.clone(),
                    )
                },
            ) else {
                return self
                    .queued_start_refusal(&control, events, reconnect_by)
                    .unwrap_or(Attendance::Retry);
            };
            Some((started, deadline))
        } else {
            None
        };
        let connected = LinkEvent::Connected(Session {
            alive: Arc::clone(&ended.alive),
            closing: Arc::clone(&closing),
            generation,
            io: Arc::clone(&credit),
            issue_origin,
        });
        let mut control_slot = self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut outbox = self
            .shared
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let legacy_connected = if !queued {
            Some(connected)
        } else if Instant::now() >= reconnect_by
            || self.quitting()
            || events.try_send(connected).is_err()
        {
            drop(outbox);
            drop(control_slot);
            return self
                .queued_start_refusal(&control, events, reconnect_by)
                .unwrap_or(Attendance::Retry);
        } else {
            None
        };
        *control_slot = Some(Arc::clone(&control));
        *outbox = Some(Outbox {
            generation,
            control: control_out.clone(),
            data: data_out,
            pipe: Arc::clone(&control),
            submits: AtomicU32::new(0),
            queued_input: QueuedInput::default(),
            closing: Arc::clone(&closing),
        });
        drop(outbox);
        drop(control_slot);
        self.shared.replay_microphone_stop();
        // Quit may have come while connecting, when there was no pipe to send it on; held
        // deletes go first. A full queue is retried by Quit's wait.
        if self.quitting() {
            control.set_timeout(Some(QUIT_TIMEOUT));
            let _told = self.shared.tell_quit();
        }
        let spawned = (|| {
            if let Some((started, deadline)) = registration {
                let shared = Arc::clone(&self.shared);
                spawn_helper("queued-start-deadline", &mut helpers, move || {
                    if started
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_err()
                    {
                        shared.output.end(generation);
                    }
                })?;
            }
            spawn_helper("link-control-writer", &mut helpers, {
                let control = Arc::clone(&control);
                let ended = ended.clone();
                move || {
                    write_control(
                        &control,
                        &control_queue,
                        #[cfg(any(test, feature = "test-support"))]
                        &ended.shared.hold_control_writer,
                    );
                    drop(ended);
                }
            })?;
            spawn_helper("link-data-writer", &mut helpers, {
                let (data, credit) = (Arc::clone(&data), Arc::clone(&credit));
                let ended = ended.clone();
                let closing = Arc::clone(&closing);
                move || {
                    write_data(&data, &data_queue, &credit, &ended.shared, || {
                        ended.alive.load(Ordering::Acquire) && !closing.load(Ordering::Acquire)
                    });
                    drop(ended);
                }
            })
        })();
        // Before Connected, so nothing can claim the session's output before its owner knows it.
        if !queued {
            self.shared.output.begin_quiet(generation);
        }
        let open = legacy_connected.is_none_or(|connected| events.send_blocking(connected).is_ok());
        let read = spawned.and_then(|()| {
            // Only now, so nothing from the brain reaches the UI before Connected.
            spawn_helper("link-data-reader", &mut helpers, {
                let (data, control, events) =
                    (Arc::clone(&data), Arc::clone(&control), events.clone());
                let (ended, credit) = (ended.clone(), Arc::clone(&credit));
                let reader = reader.clone();
                move || {
                    if let Some(reader) = &reader {
                        let _first = reader.set(thread::current());
                    }
                    read_data(
                        &data,
                        || {
                            if !queued {
                                control.abort();
                            }
                        },
                        &events,
                        generation,
                        &credit,
                        queued.then_some((&ended.shared.output, credit.narrated.as_deref())),
                    );
                }
            })
        });
        let mut attendance = Attendance::Lost;
        match read {
            Ok(()) if open => watch(
                &control,
                &control_out,
                events,
                generation,
                &self.shared,
                queued.then_some(&credit),
                &mut attendance,
            ),
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "can't start the link's threads"),
        }
        self.shared.output.end(generation);
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
        let _closed = control_out.close();
        drop(control_out);
        // The writers and the reader return once their pipes fail.
        if !queued {
            control.abort();
            data.abort();
        }
        for helper in helpers {
            if helper.join().is_err() {
                tracing::error!("a link thread panicked");
            }
        }
        if open { attendance } else { Attendance::Ended }
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

/// Reads the control pipe until the connection ends: answers pings and passes run states and
/// scenes on. A repeated handshake ends the connection, as the brain treats a protocol violation.
#[expect(clippy::too_many_lines, reason = "one arm per control message")]
fn watch(
    control: &PipeClient,
    out: &Sender<ControlToBrain>,
    events: &Sender<LinkEvent>,
    generation: u64,
    shared: &Arc<Shared>,
    queued: Option<&Arc<SessionIo>>,
    attendance: &mut Attendance,
) {
    let mut named = (RunId(0), ThreadId(0));
    loop {
        match frame::read::<ControlToShell>(&mut &*control, Pipe::Control) {
            Ok(None) => return,
            Ok(Some(ControlToShell::SpeechDecisionTaken(key))) => {
                if !queued
                    .and_then(|io| io.narrated.as_ref())
                    .is_some_and(|mode| mode.taken(generation, key))
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::QueuedAudioRegistered(output))) => {
                if queued.is_none() || !shared.output.acknowledge_queued(generation, output) {
                    return;
                }
            }
            Ok(Some(ControlToShell::Ping)) => {
                if queued.is_some() && shared.output.run_end_overdue(generation, Instant::now()) {
                    return;
                }
                if out.try_send(ControlToBrain::Pong).is_err() {
                    tracing::warn!("the control queue is full; the brain isn't reading");
                }
            }
            Ok(Some(ControlToShell::Settings { view, answer })) => {
                if queued.is_some() {
                    if shared.output.show_screen_changes(generation, &view) {
                        let _revoked = shared.revoke_input();
                    }
                    if let Err(reason) = shared
                        .output
                        .register_queued(generation, &view, answer, out)
                    {
                        *attendance = reason;
                        return;
                    }
                    if answer {
                        shared.with_queued_input(generation, |input| {
                            input.settings_answers = input.settings_answers.saturating_sub(1);
                            input.consent_answers = input.consent_answers.saturating_sub(1);
                        });
                    }
                }
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
            Ok(Some(ControlToShell::SpokenRepliesDefaulted(view))) => {
                if queued.is_none() || !shared.output.defer_spoken_replies(generation, &view) {
                    return;
                }
                if events
                    .send_blocking(LinkEvent::SpokenRepliesDefaulted { generation, view })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::Voice(view))) => {
                if events
                    .send_blocking(LinkEvent::Voice { generation, view })
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
                // The speaker plays a run's frames only once control has named it.
                if run > named.0 {
                    named = (run, thread);
                    if let Some(credit) = queued {
                        let Some(origin) = origin else {
                            return;
                        };
                        if let RunOrigin::Hold { gesture } = origin
                            && shared.with_queued_input(generation, |input| {
                                input.hold == Some(QueuedHold::Released(gesture))
                            }) != Some(true)
                        {
                            return;
                        }
                        let credit = Arc::clone(credit);
                        if !shared.output.bind_queued(
                            generation,
                            run,
                            origin,
                            out,
                            move |bytes| credit.grant(bytes),
                            Arc::downgrade(shared),
                        ) {
                            return;
                        }
                        if matches!(origin, RunOrigin::Hold { .. }) {
                            shared.with_queued_input(generation, |input| input.hold = None);
                        }
                    }
                }
                if queued.is_some()
                    && matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed)
                {
                    shared.output.queued_terminal(generation, run);
                }
                if queued.is_some()
                    && matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed)
                {
                    shared.output.queued_run_ended(generation, run);
                }
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
                if queued.is_some()
                    && matches!(outcome, Deleted::Hidden | Deleted::Removed)
                    && let Some(run) = last_run
                    && named == (run, thread)
                {
                    shared.output.queued_terminal(generation, run);
                }
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
                if queued.is_some() {
                    shared.output.queued_refused(generation, turn);
                }
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
                if queued.is_some()
                    && matches!(
                        error.key(),
                        ErrorKey::SettingsRefused | ErrorKey::ConfigUnreadable
                    )
                {
                    shared.output.refuse_talk_back(generation);
                }
                if events
                    .send_blocking(LinkEvent::Error { generation, error })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::DrawingReplacementFailed { thread, run })) => {
                if events
                    .send_blocking(LinkEvent::DrawingNotice {
                        generation,
                        key: ErrorKey::DrawingReplacementFailed,
                        thread,
                        run,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::DrawingNotReturned { thread, run })) => {
                if events
                    .send_blocking(LinkEvent::DrawingNotice {
                        generation,
                        key: ErrorKey::DrawingNotReturned,
                        thread,
                        run,
                    })
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
            Ok(Some(ControlToShell::AccountUnsupported(view))) => {
                if events
                    .send_blocking(LinkEvent::AccountUnsupported { generation, view })
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
                    shared.clear_input(gesture);
                    if queued.is_some()
                        && shared.with_queued_input(generation, |input| {
                            input
                                .hold
                                .take_if(|hold| hold.gesture() == gesture)
                                .is_some()
                        }) == Some(true)
                    {
                        shared.output.hold_released.store(false, Ordering::Release);
                    }
                }
                if queued.is_some() && status == HoldStatus::Admitted {
                    shared.with_queued_input(generation, |input| {
                        if input.hold == Some(QueuedHold::Recognized(gesture)) {
                            input.hold = Some(QueuedHold::Listening(gesture));
                        }
                    });
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
            Ok(Some(ControlToShell::VoiceHoldStopAccepted { gesture })) => {
                shared.accept_microphone_stop(generation, gesture);
            }
            Ok(Some(
                reply @ (ControlToShell::AnthropicKeyStage { .. }
                | ControlToShell::AnthropicKeyStageDone { .. }
                | ControlToShell::AnthropicKeyRemoved { .. }
                | ControlToShell::AnthropicKeyReconciled { .. }),
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
                request,
                draft,
                version,
                state,
                approval,
            })) => {
                if events
                    .send_blocking(LinkEvent::ActionState {
                        generation,
                        request,
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
            Ok(Some(ControlToShell::IssueRefused {
                request,
                draft,
                version,
                approval,
                reason,
            })) => {
                if events
                    .send_blocking(LinkEvent::IssueRefused {
                        generation,
                        request,
                        draft,
                        version,
                        approval,
                        reason,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::IssueUnavailable {
                request,
                draft,
                kind,
            })) => {
                if events
                    .send_blocking(LinkEvent::IssueUnavailable {
                        generation,
                        request,
                        draft,
                        kind,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::ScreenHolds(state))) => {
                if events
                    .send_blocking(LinkEvent::ScreenHolds { generation, state })
                    .is_err()
                {
                    return;
                }
            }
            Ok(Some(ControlToShell::HoldScreen { gesture, outcome })) => {
                if events
                    .send_blocking(LinkEvent::HoldScreen {
                        generation,
                        gesture,
                        outcome,
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Without a speaker nothing was announced, so nothing plays.
            Ok(Some(ControlToShell::SpeechRevoked { run })) => {
                if queued.is_some() && !shared.output.revoke_queued(generation, run) {
                    return;
                }
            }
            Ok(Some(ControlToShell::ThreadModelSet { .. } | ControlToShell::NoVisual { .. })) => {}
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
fn read_data(
    data: &PipeClient,
    finish: impl FnOnce(),
    events: &Sender<LinkEvent>,
    generation: u64,
    credit: &SessionIo,
    queued: Option<(&output::OutputOwner, Option<&narrated::Narrated>)>,
) {
    let mut ingress = queued::Reader::default();
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
                let message = if let Some((owner, narrated)) = queued {
                    match ingress.frame(owner, generation, message, bytes, credit, narrated) {
                        Ok(Some(message)) => message,
                        Ok(None) => continue,
                        Err(()) => break,
                    }
                } else {
                    message
                };
                // No output was announced on Legacy. Unexpected PCM is never shown.
                let message = match message {
                    DataToShell::SpeechPcm(mut pcm) => {
                        pcm.samples.zeroize();
                        credit.grant(bytes);
                        continue;
                    }
                    message => message,
                };
                let event = if matches!(
                    &message,
                    DataToShell::SceneSnapshotV1 { .. }
                        | DataToShell::SceneBegin { .. }
                        | DataToShell::SceneStepCommit { .. }
                        | DataToShell::SceneCutApplied { .. }
                        | DataToShell::SceneEnd { .. }
                        | DataToShell::ScenesRemoved { .. }
                        | DataToShell::ScenesReplaced { .. }
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
    finish();
}

/// Writes queued control messages in order until the queue closes or the pipe fails.
fn write_control(
    control: &PipeClient,
    queue: &Receiver<ControlToBrain>,
    #[cfg(any(test, feature = "test-support"))] hold: &std::sync::atomic::AtomicU8,
) {
    while let Ok(message) = queue.recv_blocking() {
        #[cfg(any(test, feature = "test-support"))]
        if hold.load(Ordering::Acquire) != 0 {
            hold.store(2, Ordering::Release);
            while !queue.is_closed() {
                thread::park_timeout(Duration::from_millis(2));
            }
            return;
        }
        if let Err(error) = frame::write(&mut &*control, &message, Pipe::Control) {
            tracing::debug!(%error, "control pipe write failed");
            return;
        }
    }
}

/// Writes queued turns in order, and after each wake-up any credit that has added up, until the
/// queue closes or the pipe fails.
fn write_data(
    data: &PipeClient,
    queue: &Receiver<DataOut>,
    credit: &SessionIo,
    shared: &Shared,
    open: impl Fn() -> bool,
) {
    let mut encoded = Zeroizing::new(Vec::new());
    while let Ok(out) = queue.recv_blocking() {
        let submitted = match &out {
            DataOut::Submit(DataToBrain::MicPcm(_)) if shared.quitting() => Ok(()),
            DataOut::Submit(message) => {
                write_data_frame(&mut &*data, message, &mut encoded, || {
                    let DataToBrain::MicPcm(packet) = message else {
                        #[cfg(any(test, feature = "test-support"))]
                        if matches!(message, DataToBrain::TextSubmit { .. })
                            && shared.hold_text_writer.load(Ordering::Acquire) != 0
                        {
                            shared.hold_text_writer.store(2, Ordering::Release);
                            while shared.hold_text_writer.load(Ordering::Acquire) != 0
                                && open()
                                && !queue.is_closed()
                            {
                                thread::park_timeout(Duration::from_millis(1));
                            }
                            return open() && !queue.is_closed();
                        }
                        return true;
                    };
                    #[cfg(any(test, feature = "test-support"))]
                    if shared.hold_pcm_writer.load(Ordering::Acquire) != 0 {
                        shared.hold_pcm_writer.store(2, Ordering::Release);
                        while shared.hold_pcm_writer.load(Ordering::Acquire) != 0
                            && !queue.is_closed()
                        {
                            thread::park_timeout(Duration::from_millis(1));
                        }
                    }
                    !shared.quitting()
                        && (!credit.queued || (open() && shared.input_is(packet.gesture)))
                })
            }
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
    admitted: impl FnOnce() -> bool,
) -> Result<(), FrameError> {
    let result = frame::encode_into(message, Pipe::Data, encoded).and_then(|()| {
        if admitted() {
            writer.write_all(encoded).map_err(FrameError::from)
        } else {
            Ok(())
        }
    });
    encoded.as_mut_slice().zeroize();
    encoded.clear();
    result
}

#[cfg(windows)]
#[cfg(any(test, feature = "test-support"))]
#[path = "link_fixture.rs"]
pub mod fixture;

#[cfg(all(test, windows))]
#[path = "link_tests.rs"]
mod tests;

#[cfg(test)]
mod disconnected_tests;

#[cfg(test)]
#[path = "link_delete_tests.rs"]
mod delete_tests;

#[cfg(all(test, windows))]
#[path = "link_measure_tests.rs"]
mod measure_tests;
