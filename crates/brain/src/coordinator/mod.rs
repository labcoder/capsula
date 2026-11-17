//! The coordinator: the brain's core loop, sans I/O (architecture v1 §5.1).
//!
//! It takes inputs (a typed turn, engine events, `Stop`, store acknowledgements, shell loss,
//! stalls, deadlines) and returns effects (store operations, frames for the shell, engine start
//! and cancel). It owns run ids, the one current run and its budgets, and the runs still ending,
//! and nothing else; the systems around it do the I/O and report back. An engine input or a
//! timer that names a run other than the current one is [`Stale`] and changes nothing, which is
//! how a late completion after `Stop` is refused. A run that ended stays accountable to the
//! store until its outcome is written, so a failed write is still reported. The streaming half,
//! which turns an engine round's events into text frames, steps and the run's end, is in `run`.

mod admission;
mod ask;
mod availability;
mod calls;
mod cards;
mod delete;
mod drawing;
mod hold;
mod inline;
mod job;
mod lease;
mod provider_draw;
mod run;
mod scenes;
mod speech;
mod store_op;
#[cfg(test)]
mod tests;
mod thread_op;
mod tools;

use std::collections::VecDeque;
use std::time::Duration;

use kumochi_protocol::message::{
    Content, ControlToShell, Cursor, DataToShell, Deleted, ModelChoice, OutputGeneration, PageOf,
    Phase, RunId, SceneId, SegmentId, Step, ThreadId, ThreadOp, UserError,
};

use crate::engines::{RunRequest, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd, SceneState};

pub use admission::{BuildContext, BuildKey, ContextBuilt, Unbuilt};
pub(crate) use availability::model_unavailable;
pub use availability::{Availability, Ready};
pub use cards::{CardRead, Cards};
pub use hold::{HoldKey, HoldRead, HoldSnapshot};
pub use inline::{CompactBuilt, CompactContext};
pub(crate) use job::JobAsk;
pub use job::{JobBuilt, JobKey, JobKind, JobOutput, ReadJob};
pub use lease::{LeaseId, MAX_HOLD, SEND_DEADLINE, SendKey};
pub use speech::{DRAIN_STALL, QUIET_TIMEOUT, Synthesized};
pub use store_op::{StoreOp, SummaryText};
pub use tools::{MAX_CALLS, MAX_ROUNDS, RoundItems};
mod page;
pub use page::PageKey;

/// A run's whole wall-clock budget [S3's total cap].
pub const WALL_CLOCK: Duration = Duration::from_secs(120);
/// How long a typed turn's first build waits for Claude Code's next report while it's being found
/// (OSC-753) [guess].
const FINDING_WAIT: Duration = Duration::from_secs(30);
/// The most visible text one run may stream [S3].
pub const MAX_TEXT: usize = 1024 * 1024;
/// The largest piece of text one `AssistantText` frame carries, well under the data pipe's cap.
pub const MAX_DELTA: usize = 64 * 1024;
/// How many characters of a new thread's first turn become its title [guess].
const TITLE_CHARS: usize = 80;

/// Something that happened, for the coordinator to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A recognition-time snapshot; intercepted by the runtime's hold owner.
    HoldPrepared {
        /// The exact pending gesture.
        key: HoldKey,
        /// Pinned admission data, or a content-free read failure.
        result: Result<HoldSnapshot, ()>,
    },
    /// The store answered an operation owned and routed by the runtime issue system.
    IssueWritten {
        /// The issue owner's outstanding operation identity.
        serial: u64,
        /// The durable action or a content-free store failure.
        reply: Result<crate::issues::Answer, ()>,
    },
    /// Fence a run immediately and apply scene-wide activated cuts in control-pipe order.
    StopScenes {
        /// The locally fenced run.
        run: RunId,
        /// Cuts independent of whichever run last appended to each scene.
        cuts: kumochi_protocol::message::SceneCuts,
    },
    /// Direct v1 records and observations; the fixture producer switches in B3.
    Drawing(crate::drawing::Input),
    /// The store answered the drawing reducer's one outstanding operation.
    DrawingWritten {
        /// The outstanding operation identity.
        serial: u64,
        /// A typed result, or a content-free failure.
        reply: Result<crate::drawing::Reply, ()>,
    },
    /// Read a page for one shell connection, without changing any run.
    ReadPage {
        /// The admitted read.
        key: PageKey,
        /// The page to read.
        page: PageOf,
    },
    /// Read one admitted batch without changing a run.
    ReadCards {
        /// The admitted batch and bounded source identities.
        cards: std::sync::Arc<Cards>,
    },
    /// A card read finished, before output capacity or deletion checks.
    CardsRead {
        /// The original admission.
        cards: std::sync::Arc<Cards>,
        /// At most sixteen cards, or a fixed read failure.
        reply: Result<Vec<CardRead>, ()>,
    },
    /// A page read finished.
    PageRead {
        /// The admitted read.
        key: PageKey,
        /// The page, or `PageFailed` when the read failed.
        reply: DataToShell,
    },
    /// The user typed a turn, in `thread` or in a new thread.
    Turn {
        /// The submit's connection-local number.
        turn: u32,
        /// The thread to continue, or `None` for a new one.
        thread: Option<ThreadId>,
        /// What the user typed.
        text: Content,
    },
    /// The user clicked a labelled 3D part: a turn about it, in its drawing's thread.
    Ask {
        /// The ask's connection-local number, shared with typed turns.
        turn: u32,
        /// The visible scene.
        scene: SceneId,
        /// The solid's host identity.
        element: kumochi_scene::Handle,
    },
    /// The shell stopped `run` locally; `revealed` are the steps that stayed on screen.
    Stop {
        /// The stopped run.
        run: RunId,
        /// Indices of the steps whose entrance had finished.
        revealed: Vec<u32>,
    },
    /// The engine answering an admitted build produced something.
    Engine {
        /// The exact run, round and attempt that started this engine.
        key: BuildKey,
        /// What it produced.
        event: TurnEvent,
    },
    /// A round's context was built, or failed to be.
    ContextBuilt(ContextBuilt),
    /// The transport is about to send this build's or job's body and waits for `Grant` or
    /// `Rebuild`.
    SendReady {
        /// The build or job.
        key: SendKey,
        /// How long its lease may go unsettled, for a send that can go out again until a process
        /// tree ends; capped at `MAX_HOLD`. `None` is `SEND_DEADLINE`.
        hold: Option<Duration>,
    },
    /// An inline compaction's read finished.
    CompactBuilt(CompactBuilt),
    /// A background job's read finished.
    JobBuilt(JobBuilt),
    /// A background job's request ended: the cleaned title or summary, or `None` when it failed.
    JobDone {
        /// The job.
        key: JobKey,
        /// What it came to.
        output: Option<JobOutput>,
    },
    /// A `SetTitle` finished.
    TitleSet {
        /// The job that wrote it; `None` for the user's rename.
        job: Option<JobKey>,
        /// It changed the title.
        applied: bool,
    },
    /// A thread's model write finished.
    ModelSet {
        /// The requested thread.
        thread: ThreadId,
        /// The requested choice.
        choice: ModelChoice,
        /// A live row matched the committed write.
        applied: bool,
    },
    /// A `StoreSummary` finished.
    SummaryStored {
        /// The job or build that wrote it.
        owner: SendKey,
        /// It was stored.
        applied: bool,
    },
    /// An account can send model requests, as account generation `generation`.
    SignedIn {
        /// The account generation.
        generation: u64,
    },
    /// The account signed in as `generation` can't send any more.
    SignedOut {
        /// The account generation.
        generation: u64,
    },
    /// A granted send stopped: it finished, failed, or was dropped.
    SendSettled {
        /// Its lease.
        lease: LeaseId,
    },
    /// A granted send stopped before any write of its request was attempted.
    SendUnsent {
        /// Its lease.
        lease: LeaseId,
    },
    /// A granted send's deadline passed.
    SendDeadline {
        /// Its lease.
        lease: LeaseId,
    },
    /// The store committed a turn.
    TurnStored {
        /// The run the turn started.
        run: RunId,
    },
    /// The store committed a step.
    StepStored {
        /// The run it belongs to.
        run: RunId,
        /// Its index.
        index: u32,
        /// The step.
        step: Step,
        /// Its scene; a run's first step learns its new scene's id here.
        scene: SceneId,
    },
    /// The store committed a round's items and results.
    RoundStored {
        /// The run.
        run: RunId,
        /// The round.
        round: u32,
    },
    /// The store committed a run's outcome.
    RunStored {
        /// The run.
        run: RunId,
    },
    /// The oldest unanswered store operation for `run` failed.
    StoreFailed {
        /// The run it belonged to.
        run: RunId,
    },
    /// The shell went away.
    ShellLost,
    /// The shell took no data for too long while `run` streamed.
    Stalled {
        /// The streaming run.
        run: RunId,
    },
    /// `run`'s wall-clock budget ran out.
    Deadline {
        /// The run.
        run: RunId,
    },
    /// The shell cleared these scenes locally.
    Clear {
        /// The scenes, at most four.
        scenes: Vec<SceneId>,
    },
    /// A `ClearScenes` write finished.
    ScenesCleared {
        /// The scenes it named.
        scenes: Vec<SceneId>,
        /// Whether it committed.
        ok: bool,
    },
    /// The shell quit: every visible scene is cleared, so only a crash brings drawings back.
    ClearVisible,
    /// A `KeepScene` write finished.
    SceneKept {
        /// The scene it named.
        scene: SceneId,
        /// Whether it committed.
        ok: bool,
    },
    /// The timer for owed clears and selections fired.
    Retry,
    /// [`crate::drawing::READY_WAIT`] passed while a draw call waited for the shell.
    ReadyWait,
    /// The user confirmed deleting `thread`.
    Delete {
        /// The thread.
        thread: ThreadId,
        /// The submits the shell sent on this connection before it.
        after_turns: u32,
        /// A resend of a delete the shell sent on an earlier connection.
        resend: bool,
    },
    /// A `Delete` op finished.
    Deleted {
        /// The thread.
        thread: ThreadId,
        /// What it came to.
        outcome: Deleted,
        /// The thread's newest stored run, read before its rows went.
        last_run: Option<RunId>,
    },
    /// The shell renamed a task or set a thread's model.
    ThreadOp(ThreadOp),
    /// Whether runs on a connection kind can be answered changed.
    Engines(kumochi_protocol::message::ConnectionKind, Availability),
    /// A `CommitCall` finished.
    CallCommitted {
        /// The call.
        key: crate::dispatch::LedgerKey,
        /// How the store settled it.
        outcome: crate::dispatch::Committed,
        /// The call's follow-up, as the dispatcher built it.
        then: Option<crate::dispatch::Then>,
    },
    /// A stamped history read finished without retaining its source text.
    ReadFinished(crate::dispatch::ReadFinished),
    /// The shell's output device: its rate and generation.
    AudioOutput {
        /// The rate in Hz, already checked.
        rate: u32,
        /// The device's generation.
        generation: OutputGeneration,
    },
    /// Spoken replies were loaded or unloaded; a run admitted while they're loaded, with an output
    /// device, speaks its answer.
    SpokenReplies {
        /// They're loaded.
        loaded: bool,
    },
    /// The user turned screen holds off: every run's window image is withdrawn.
    ScreenHoldsOff,
    /// The user turned showing the screen off: every run's screen picture is withdrawn.
    ShowScreenOff,
    /// The pump forwarded what TTS made of `run`'s segment.
    Tts {
        /// The run.
        run: RunId,
        /// The segment.
        segment: SegmentId,
        /// What TTS made.
        made: Synthesized,
    },
    /// The shell played `sample` samples of `run`'s playing segment.
    SpeechProgress {
        /// The run.
        run: RunId,
        /// The segment.
        segment: SegmentId,
        /// Samples of it played.
        sample: u32,
    },
    /// The last of a segment's `samples` played.
    SegmentPlayed {
        /// The run.
        run: RunId,
        /// The segment.
        segment: SegmentId,
        /// Its total samples.
        samples: u32,
    },
    /// The shell's output for `run` went quiet after a Stop or a revoke.
    SpeechStopped {
        /// The run.
        run: RunId,
        /// Where playback stopped; none when none of the run's audio played.
        cursor: Option<Cursor>,
    },
    /// `run`'s drain stall timer armed with `epoch` fired.
    DrainTimeout {
        /// The run.
        run: RunId,
        /// The timer's epoch; only the latest counts.
        epoch: u32,
    },
    /// `run`'s speech was revoked [`QUIET_TIMEOUT`] ago.
    QuietTimeout {
        /// The run.
        run: RunId,
    },
}

/// Something for the systems around the coordinator to do, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Run a store operation in order, with one acknowledgement for every read or write.
    Store(StoreOp),
    /// Send on the control pipe.
    Control(ControlToShell),
    /// Send on the data pipe.
    Data(DataToShell),
    /// Tell the shell a held recognition's turn is stored, only on the hold's own connection.
    HoldStored(HoldKey),
    /// Tell the shell what a hold's screen carried into its turn, only on the hold's own
    /// connection.
    HoldScreen {
        /// The hold.
        hold: HoldKey,
        /// What its turn carried.
        outcome: kumochi_protocol::message::HeldScreen,
    },
    /// Deliver an admitted page without losing terminal ownership.
    Page {
        /// The admitted read.
        key: PageKey,
        /// Its terminal reply.
        reply: DataToShell,
    },
    /// Deliver a bounded card result under its original admission.
    Cards {
        /// The original admission.
        cards: std::sync::Arc<Cards>,
        /// A bounded result, checked again before queueing.
        reply: Result<Vec<CardRead>, ()>,
    },
    /// Start the engine's round with an admitted build.
    StartEngine {
        /// What the engine answers.
        request: RunRequest,
    },
    /// Let the transport send this lease's body; its guard reports `SendSettled` when dropped.
    Grant(LeaseId),
    /// Tell the transport to drop this build's or job's body unsent.
    Rebuild(SendKey),
    /// Start a background job's request; it reports `JobDone`.
    StartJob {
        /// The job.
        job: JobKey,
        /// What it does, which decides how its output is cleaned.
        kind: JobKind,
        /// What it sends.
        request: RunRequest,
    },
    /// Drop a background job's request and whatever it still produces.
    CancelJob(JobKey),
    /// Report `SendDeadline` for `lease` after `after`.
    ArmSendDeadline {
        /// The lease.
        lease: LeaseId,
        /// How long from now.
        after: Duration,
    },
    /// Stop the engine for `run` and drop whatever it still produces.
    CancelEngine {
        /// The run.
        run: RunId,
    },
    /// Drop `run`'s data not yet sent to the shell, which no longer shows it.
    Revoke {
        /// The run.
        run: RunId,
    },
    /// Lay a drawing step out off this thread; the job's answer is a drawing input.
    PrepareDrawing(crate::drawing::PrepareJob),
    /// Drop the snapshots of `scenes` not yet sent to the shell, which cleared them.
    RevokeScenes {
        /// The scenes.
        scenes: Vec<SceneId>,
    },
    /// Report `Retry` after `after`, replacing any earlier `ArmRetry`.
    ArmRetry {
        /// How long from now.
        after: Duration,
    },
    /// Forget the armed `ArmRetry`: nothing owed is left unwritten.
    CancelRetry,
    /// Report `ReadyWait` after [`crate::drawing::READY_WAIT`], replacing any earlier one.
    ArmReadyWait,
    /// Forget the armed `ArmReadyWait`: no draw call waits for the shell.
    CancelReadyWait,
    /// Report `Deadline` for `run` after `after`.
    ArmDeadline {
        /// The run.
        run: RunId,
        /// How long from now.
        after: Duration,
    },
    /// The shell broke the protocol: end its connection.
    Broken(&'static str),
    /// Hand TTS `run`'s segment, to be resampled to `rate`; the pump forwards its events as
    /// [`Input::Tts`].
    Speak {
        /// The run.
        run: RunId,
        /// The segment.
        segment: SegmentId,
        /// The output rate.
        rate: u32,
        /// What to say.
        text: Content,
    },
    /// Let the pump forward `samples` more of `run`'s audio.
    Credit {
        /// The run.
        run: RunId,
        /// How many samples.
        samples: u64,
    },
    /// Stop `run`'s current Speak; its speech goes on.
    CancelSpeech {
        /// The run.
        run: RunId,
    },
    /// Stop `run`'s speech for good.
    RevokeSpeech {
        /// The run.
        run: RunId,
    },
    /// Report `DrainTimeout` for `run` and `epoch` after `after`, replacing an earlier one.
    ArmDrain {
        /// The run.
        run: RunId,
        /// The timer's epoch.
        epoch: u32,
        /// How long from now.
        after: Duration,
    },
    /// Report `QuietTimeout` for `run` after `after`.
    ArmQuiet {
        /// The run.
        run: RunId,
        /// How long from now.
        after: Duration,
    },
}

/// The input named a run that isn't current, or an acknowledgement nothing is waiting for, so
/// it changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

/// Whether ending a run tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notify {
    Shell,
    Nobody,
}

/// The run in flight, or one that ended and is waiting on the store.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own fact about the run, not one state spread over flags"
)]
struct Run {
    control: crate::recall::control::CancelOnDrop,
    id: RunId,
    thread: ThreadId,
    turn_stored: bool,
    /// The turn's write, held until every older run's outcome is queued.
    begin: Option<StoreOp>,
    answering: bool,
    /// Streamed text not yet stored; it's stored with the next step or the outcome.
    unstored: String,
    text_bytes: usize,
    /// Steps the run produced, and how many the store acknowledged, in order.
    steps: u32,
    acked: u32,
    /// The run's scene, once its first step is committed.
    scene: Option<SceneId>,
    /// The scene the first step's write evicts, until it's acknowledged.
    evict: Option<SceneId>,
    /// Steps after the first, with the text streamed before each, until the scene has its id.
    waiting: VecDeque<(Option<Content>, u32, Step)>,
    /// One entry per pending step: the frames the engine produced after it, sent once it's
    /// acknowledged, so the shell sees the engine's order.
    held: VecDeque<Vec<DataToShell>>,
    /// What the run read when its first build began, reused by every later build.
    snapshot: Option<crate::context::RunSnapshot>,
    hold: Option<HoldSnapshot>,
    /// The hold's screen, which every request of the run carries.
    screen: Option<crate::context::Screen>,
    /// The run took up a screen hold's picture, so while marks are on every round stores its
    /// native items without boxes, even after the picture is withdrawn or can't be drawn on.
    pictured: bool,
    /// The hold's gesture while its screen is still to be told to the shell, which happens once:
    /// an image once a granted send carrying it may have written it, anything else at the run's
    /// first request. A granted send holds it until it settles.
    untold: Option<kumochi_protocol::message::GestureId>,
    /// The round's current build; the engine starts once it's admitted.
    build: Option<admission::Build>,
    /// The sign-in each kind had ready when the run's turn arrived; a kind's entry goes when that
    /// kind changes before the first build.
    arrived: availability::Arrived,
    /// What the run answers on, from its first admitted build on.
    route: Option<crate::context::Route>,
    /// A typed turn that arrived while Claude Code was being found, until it binds or is released.
    finding: Option<availability::Finding>,
    /// The round that compacted inline since the run's last admitted build.
    compacted: Option<u32>,
    /// The draw call the engine's steps stream in, once it starts.
    draw: Option<run::DrawArgs>,
    draw_calls: Vec<provider_draw::DrawCall>,
    provider_calls: Vec<crate::engines::CallId>,
    /// Why the run failed, for the shell.
    error: Option<UserError>,
    /// The run's tool calls, one entry per key.
    ledger: crate::dispatch::Ledger,
    /// The Claude Code launch authorized to call tools for this run; C4b-2 sets it.
    mcp: Option<crate::bridge::LaunchId>,
    /// Calls answered on their write's acknowledgement, for the tool loop to take.
    answered: Vec<(crate::dispatch::Caller, crate::dispatch::ToolOutput)>,
    /// The run's calls and the current round's native items.
    tools: tools::Loop,
    /// The engine's round completed while steps or its items' write were pending; the round
    /// ends with the last of them. A spoken run also stays here until its speech is final.
    finishing: bool,
    ending: Option<Ending>,
    /// The run's speech, when it was admitted with spoken replies loaded and an output device.
    reply: Option<crate::voice::reply::Reply>,
}

/// How an ended run is being written.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the independent store and delivery fences can each settle separately"
)]
struct Ending {
    outcome: RunEnd,
    revealed: Option<Vec<u32>>,
    notify: Notify,
    /// A store operation of the run failed, so only `failed(store)` is written.
    failed: bool,
    /// The outcome was sent to the store.
    written: bool,
    /// The scene a Stop kept nothing of, cleared in the outcome's transaction.
    clear: Option<SceneId>,
    /// The Stop's selection was applied to the visible set; it stays for every outcome write.
    trimmed: bool,
    /// V1 durable scene frames must precede this interrupted run's final data frame.
    deferred_data: bool,
    /// The run's speech was revoked, so the shell hears how it ended only once the outcome is
    /// stored and the shell's output is quiet.
    quieting: bool,
    /// The store has the outcome.
    stored: bool,
    /// It called `draw` and none of its steps survived, told once `Done` is stored.
    drew_nothing: bool,
}

impl Run {
    /// Steps sent to the store; the ones waiting for the scene's id aren't.
    fn sent(&self) -> u32 {
        self.steps - u32::try_from(self.waiting.len()).expect("at most MAX_STEPS wait")
    }

    /// The engine started: the round's build was admitted.
    fn started(&self) -> bool {
        self.build.as_ref().is_some_and(admission::Build::admitted)
    }

    fn outstanding(&self) -> bool {
        !self.turn_stored || self.acked < self.sent() || self.tools.writing()
    }
}

/// The core loop's state.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own fact about the session, not one state spread over flags"
)]
pub struct Coordinator {
    drawing: crate::drawing::Drawing,
    drawing_held: Vec<(ThreadId, DataToShell, Option<RunId>)>,
    /// Whether `draw` may mark the user's screen: [`crate::drawing::SCREEN_MARKS`].
    screen_marks: crate::drawing::ScreenMarks,
    /// Never deleted or renamed.
    main_thread: ThreadId,
    last_thread: ThreadId,
    last_run: RunId,
    current: Option<Run>,
    /// Runs that ended, until the store answers for their outcome.
    ending: Vec<Run>,
    /// The visible set, oldest first, at most four.
    scenes: Vec<SceneState>,
    /// Cleared scenes whose `scene_cleared` isn't committed yet, and whether a write for each
    /// is in the store; a failed one is retried with the next store operation.
    clearing: Vec<(SceneId, bool)>,
    /// Visible scenes whose kept selection no outcome recorded, the same way.
    keeping: Vec<(SceneId, Vec<u32>, bool)>,
    /// The first run the connection being served could start; older runs were drawn by a
    /// shell that's gone, so this one's Stop can't name them.
    session_start: RunId,
    /// The wait before owed writes are tried again, doubling while they keep failing.
    retry_after: Duration,
    /// The wait of the `ArmRetry` that's out and hasn't come back as `Retry`.
    retry_armed: Option<Duration>,
    /// An `ArmReadyWait` is out and hasn't come back as `ReadyWait`.
    ready_armed: bool,
    /// Deletes sent to the store and not answered yet, from any connection.
    deleting: Vec<delete::Deleting>,
    /// Threads hidden or removed on this connection while a submit sent before their delete is
    /// still unread, with how many submits came before it.
    fenced: Vec<(ThreadId, u32)>,
    /// Threads whose delete, sent on an earlier connection, was answered on this one before the
    /// shell's resend of it was read; that resend needs no second op.
    crossed: Vec<ThreadId>,
    /// The turns taken on this connection.
    turns_read: u32,
    /// A delete on this connection was refused, so its thread wasn't hidden.
    refused_delete: bool,
    /// Build attempts started, so each has its own serial.
    attempts: u64,
    /// Granted sends that haven't settled, and failed ones, which fence their sources for good.
    leases: Vec<lease::Lease>,
    /// Grants made, so each lease has its own serial.
    sends: u64,
    /// A lease that wasn't held failed closed: nothing is granted again until the brain restarts.
    transport_failed: bool,
    /// Whether `ChatGPT` runs can be answered, and on which sign-in.
    availability: Availability,
    /// The other connections, which no engine answers yet, so they stay unavailable.
    anthropic_key: Availability,
    claude_code: Availability,
    /// The one background job.
    job: Option<job::Job>,
    /// The signed-in account's generation, while one can send.
    account: Option<u64>,
    /// The newest account generation that signed out.
    signed_out: Option<u64>,
    /// The shell's output device on this connection: its rate and generation.
    output: Option<(u32, OutputGeneration)>,
    /// Spoken replies are loaded.
    spoken: bool,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator")
            .field("current_run", &self.current_run())
            .field("ending", &self.ending.len())
            .field("scenes", &self.scenes.len())
            .field("drawing", &self.drawing)
            .field("drawing_held", &self.drawing_held.len())
            .field("clearing", &self.clearing.len())
            .field("keeping", &self.keeping.len())
            .field("deleting", &self.deleting.len())
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    /// Starts from the store's main thread and high-water marks, so ids keep growing across
    /// restarts, and from the visible set the store recovered.
    pub fn new(
        main_thread: ThreadId,
        last_thread: ThreadId,
        last_run: RunId,
        scenes: Vec<SceneState>,
    ) -> Self {
        Self {
            drawing: crate::drawing::Drawing::default(),
            drawing_held: Vec::new(),
            screen_marks: crate::drawing::SCREEN_MARKS,
            main_thread,
            last_thread,
            last_run,
            current: None,
            ending: Vec::new(),
            scenes,
            clearing: Vec::new(),
            keeping: Vec::new(),
            session_start: RunId(last_run.0 + 1),
            retry_after: scenes::FIRST_RETRY,
            retry_armed: None,
            ready_armed: false,
            deleting: Vec::new(),
            fenced: Vec::new(),
            crossed: Vec::new(),
            turns_read: 0,
            refused_delete: false,
            attempts: 0,
            leases: Vec::new(),
            sends: 0,
            transport_failed: false,
            availability: Availability::Unavailable(availability::sign_in_needed()),
            anthropic_key: Availability::Unavailable(availability::model_unavailable()),
            claude_code: Availability::Unavailable(availability::model_unavailable()),
            job: None,
            account: None,
            signed_out: None,
            output: None,
            spoken: false,
        }
    }

    /// A coordinator the dev fixture answers for, as most tests need.
    #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
    pub fn ready_for_tests(
        main_thread: ThreadId,
        last_thread: ThreadId,
        last_run: RunId,
        scenes: Vec<SceneState>,
    ) -> Self {
        let mut core = Self::new(main_thread, last_thread, last_run, scenes);
        core.availability = Availability::Ready(Ready::fixture());
        core
    }

    /// The run in flight, if any.
    pub fn current_run(&self) -> Option<RunId> {
        self.current.as_ref().map(|run| run.id)
    }

    /// How many ended runs still wait on the store.
    pub fn ending_runs(&self) -> usize {
        self.ending.len()
    }

    /// Acts on one input.
    ///
    /// # Errors
    /// [`Stale`] when the input names a run that isn't current or answers nothing that is
    /// pending; nothing changed.
    ///
    /// # Panics
    /// Only if a run in the ending list has no ending record, which `end` never allows.
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive input router; handlers live in dedicated modules"
    )]
    pub fn handle(&mut self, input: Input) -> Result<Vec<Effect>, Stale> {
        self.fence_hold_input(&input);
        let mut effects = Vec::new();
        match input {
            Input::HoldPrepared { .. } | Input::IssueWritten { .. } => return Err(Stale),
            Input::StopScenes { run, .. } if run < self.session_start || run > self.last_run => {
                return Err(Stale);
            }
            Input::StopScenes { run, cuts } => self.stop_drawings(run, cuts, &mut effects)?,
            Input::Drawing(input) => self.drawing_input(input, &mut effects)?,
            Input::DrawingWritten { serial, reply } => {
                self.drawing_input(
                    crate::drawing::Input::Written { serial, reply },
                    &mut effects,
                )?;
            }
            Input::ReadPage { key, page } => {
                let op = StoreOp::ReadPage { key, page };
                return Ok(vec![Effect::Store(op)]);
            }
            Input::PageRead { key, reply } => return Ok(vec![Effect::Page { key, reply }]),
            Input::ReadCards { cards } => {
                return Ok(vec![Effect::Store(StoreOp::ReadCards { cards })]);
            }
            Input::CardsRead { cards, reply } => return Ok(vec![Effect::Cards { cards, reply }]),
            Input::Turn { turn, thread, text } => self.typed(turn, thread, text, &mut effects),
            Input::Ask {
                turn,
                scene,
                element,
            } => self.asked(turn, scene, element, &mut effects),
            Input::Stop { run, .. } if run < self.session_start => return Err(Stale),
            Input::Stop { run, revealed } if self.current_run() != Some(run) => {
                self.late_stop(run, revealed, &mut effects)?;
            }
            Input::Stop { run, mut revealed } => {
                let mut current = self.take(run)?;
                speech::revoke(
                    &mut current,
                    crate::voice::reply::Revoker::Shell,
                    &mut effects,
                );
                revealed.retain(|&index| index < current.acked);
                revealed.sort_unstable();
                revealed.dedup();
                let stopped = RunEnd::Interrupted(Interruption::Stop);
                self.end(
                    current,
                    stopped,
                    Some(revealed),
                    Notify::Shell,
                    &mut effects,
                );
            }
            Input::Engine { key, event } => self.engine(key, event, &mut effects)?,
            Input::ContextBuilt(built) => self.context_built(built, &mut effects)?,
            Input::SendReady {
                key: SendKey::Build(key),
                hold,
            } => self.send_ready(key, hold, &mut effects),
            Input::SendReady {
                key: SendKey::Job(key),
                hold,
            } => self.job_ready(key, hold, &mut effects),
            Input::CompactBuilt(built) => self.compact_built(built, &mut effects)?,
            Input::JobBuilt(built) => self.job_built(built, &mut effects)?,
            Input::JobDone { key, output } => self.job_done(key, output, &mut effects)?,
            Input::TitleSet { job, applied } => {
                // A committed title is news to the shell, whoever wrote it.
                if applied {
                    effects.push(Effect::Control(ControlToShell::ThreadsChanged));
                }
                self.job_written(job, &mut effects);
            }
            Input::ModelSet {
                thread,
                choice,
                applied,
            } => {
                effects.push(Effect::Control(ControlToShell::ThreadModelSet {
                    thread,
                    choice,
                    applied,
                }));
            }
            Input::SummaryStored {
                owner: SendKey::Job(key),
                ..
            } => self.job_written(Some(key), &mut effects),
            Input::SummaryStored {
                owner: SendKey::Build(key),
                ..
            } => self.inline_written(key, &mut effects)?,
            Input::SignedIn { generation } => self.signed_in(generation, &mut effects)?,
            Input::SignedOut { generation } => self.signed_out(generation, &mut effects)?,
            Input::SendSettled { lease } => self.send_settled(lease, false, &mut effects)?,
            Input::SendUnsent { lease } => self.send_settled(lease, true, &mut effects)?,
            Input::SendDeadline { lease } => self.send_deadline(lease, &mut effects)?,
            Input::TurnStored { run } => self.turn_stored(run, &mut effects)?,
            Input::StepStored {
                run,
                index,
                step,
                scene,
            } => {
                self.step_stored(run, index, step, scene, &mut effects)?;
            }
            Input::RoundStored { run, round } => self.round_stored(run, round, &mut effects)?,
            Input::RunStored { run } => self.run_stored(run, &mut effects)?,
            Input::StoreFailed { run } => self.store_failed(run, &mut effects)?,
            Input::ShellLost => {
                for (_, _, end) in &mut self.drawing_held {
                    *end = None;
                }
                for run in &mut self.ending {
                    if let Some(ending) = &mut run.ending {
                        ending.notify = Notify::Nobody;
                    }
                }
                if let Some(current) = self.current.take() {
                    let lost = RunEnd::Interrupted(Interruption::ShellLost);
                    self.end(current, lost, None, Notify::Nobody, &mut effects);
                }
                self.output = None;
                let runs: Vec<_> = self.ending.iter().map(|run| run.id).collect();
                for run in runs {
                    self.release(run, &mut effects);
                }
            }
            Input::Stalled { run } => {
                let current = self.take(run)?;
                let stalled = RunEnd::Interrupted(Interruption::ViewStalled);
                self.end(current, stalled, None, Notify::Shell, &mut effects);
            }
            Input::Deadline { run } => {
                // The engine finished within its budget; only its last writes are being stored.
                let current = self.current(run)?;
                if current.finishing && !current.tools.calling() {
                    return Err(Stale);
                }
                if current.finding == Some(availability::Finding::Waiting) {
                    let reason = model_unavailable();
                    self.fail_with(run, Failure::Engine, reason, &mut effects)?;
                } else {
                    let current = self.take(run)?;
                    let over = RunEnd::Failed(Failure::Budget);
                    self.end(current, over, None, Notify::Shell, &mut effects);
                }
            }
            Input::Clear { scenes } => {
                self.drawing_clear(scenes.clone(), &mut effects);
                self.clear(&scenes, &mut effects);
            }
            Input::ScenesCleared { scenes, ok: true } => self.cleared(&scenes),
            Input::ScenesCleared { scenes, ok: false } => self.clear_failed(&scenes),
            Input::ClearVisible => {
                self.drawing_input(crate::drawing::Input::ClearVisible, &mut effects)?;
                self.clear_visible(&mut effects);
            }
            Input::SceneKept { scene, ok: true } => self.kept(scene),
            Input::SceneKept { scene, ok: false } => self.keep_failed(scene),
            Input::Retry => self.retry_now(&mut effects),
            Input::ReadyWait => {
                self.ready_armed = false;
                self.drawing_input(crate::drawing::Input::ReadyWait, &mut effects)?;
            }
            input @ (Input::Delete { .. } | Input::Deleted { .. }) => {
                self.deletion(&input, &mut effects)?;
                self.provider_sources_changed(&mut effects);
            }
            Input::ThreadOp(op) => self.thread_op(op, &mut effects),
            Input::Engines(kind, now) => self.available(kind, now, &mut effects),
            Input::ReadFinished(done) => self.read_finished(done, &mut effects)?,
            Input::CallCommitted { key, outcome, then } => match outcome {
                crate::dispatch::Committed::Done => {
                    // The task exists whether or not its run is still current.
                    let refresh = matches!(
                        then,
                        Some(
                            crate::dispatch::Then::Created { .. }
                                | crate::dispatch::Then::IssueDrafted
                        )
                    );
                    if refresh {
                        effects.push(Effect::Control(ControlToShell::ThreadsChanged));
                    }
                    match self.call_committed(&key, then, &mut effects) {
                        Err(Stale) if refresh => {}
                        settled => settled?,
                    }
                }
                crate::dispatch::Committed::Refused(error) => {
                    self.call_refused(&key, error, &mut effects)?;
                }
                crate::dispatch::Committed::Failed => self.call_failed(&key, &mut effects)?,
            },
            Input::ScreenHoldsOff => self.screen_holds_off(
                |screen| matches!(screen, crate::context::Screen::Image(_)),
                &mut effects,
            ),
            Input::ShowScreenOff => {
                self.screen_holds_off(
                    |screen| matches!(screen, crate::context::Screen::Picture(..)),
                    &mut effects,
                );
                if self.screen_marks == crate::drawing::ScreenMarks::On {
                    self.drawing_input(crate::drawing::Input::ScreenOff, &mut effects)?;
                }
            }
            input @ (Input::AudioOutput { .. }
            | Input::SpokenReplies { .. }
            | Input::Tts { .. }
            | Input::SpeechProgress { .. }
            | Input::SegmentPlayed { .. }
            | Input::SpeechStopped { .. }
            | Input::DrainTimeout { .. }
            | Input::QuietTimeout { .. }) => self.speech_input(input, &mut effects)?,
        }
        self.drawing_settle(&mut effects);
        self.begin_turns(&mut effects);
        self.retry_with(&mut effects);
        self.arm_retry(&mut effects);
        Ok(effects)
    }

    /// A run's turn is stored: its first round's context is built, unless it already ended.
    fn turn_stored(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let found = self.find(run)?;
        if found.begin.is_some() || found.turn_stored {
            return Err(Stale);
        }
        found.turn_stored = true;
        if let Some(hold) = &found.hold {
            effects.push(Effect::HoldStored(hold.key));
        }
        if found.ending.is_some() {
            self.settle(run, effects);
            return Ok(());
        }
        self.build(run, 1, effects);
        Ok(())
    }

    /// A run's outcome is stored: it leaves the ending list, and the shell hears Done, unless its
    /// revoked speech isn't quiet yet.
    fn run_stored(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let at = self.ending_at(run)?;
        let ending = self.ending[at]
            .ending
            .as_mut()
            .expect("an ending run has an Ending");
        if !ending.written || ending.stored {
            return Err(Stale);
        }
        ending.stored = true;
        self.release(run, effects);
        Ok(())
    }

    /// Lets a stored run go once the shell is quiet or gone, and tells the shell how it ended.
    fn release(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        let Ok(at) = self.ending_at(run) else {
            return;
        };
        let ended = &self.ending[at];
        let ending = ended.ending.as_ref().expect("an ending run has an Ending");
        let quiet = ending.notify == Notify::Nobody
            || ended
                .reply
                .as_ref()
                .is_none_or(crate::voice::reply::Reply::is_final);
        if !ending.stored || !quiet {
            return;
        }
        let done = self.ending.swap_remove(at);
        let ending = done.ending.as_ref().expect("an ending run has an Ending");
        // shortcut: a run whose store write failed keeps no heard point, since its stored text
        // may lack what was heard; track the committed text prefix if that point is ever wanted.
        if !ending.failed
            && let Some(heard) = done
                .reply
                .as_ref()
                .and_then(crate::voice::reply::Reply::heard)
        {
            effects.push(Effect::Store(StoreOp::Heard {
                run: done.id,
                heard,
            }));
        }
        if ending.quieting && ending.notify == Notify::Shell {
            let phase = if ending.failed {
                Phase::Failed
            } else {
                terminal(ending.outcome)
            };
            self.told(&done, phase, effects);
        }
        if let Some(Ending {
            outcome: RunEnd::Done,
            failed: false,
            ..
        }) = done.ending
        {
            self.after_run(done.thread, done.id, done.route, effects);
        }
        if let Some(Ending {
            outcome: RunEnd::Done,
            notify: Notify::Shell,
            failed: false,
            drew_nothing,
            ..
        }) = done.ending
        {
            if drew_nothing {
                effects.push(Effect::Control(ControlToShell::DrawingNotReturned {
                    thread: done.thread,
                    run: done.id,
                }));
            }
            self.told(&done, Phase::Done, effects);
        }
    }

    fn turn(
        &mut self,
        turn: u32,
        thread: Option<ThreadId>,
        text: Content,
        effects: &mut Vec<Effect>,
    ) -> RunId {
        self.start_turn(
            kumochi_protocol::message::RunOrigin::Typed { turn },
            thread,
            text,
            None,
            None,
            effects,
        )
    }

    fn start_turn(
        &mut self,
        origin: kumochi_protocol::message::RunOrigin,
        thread: Option<ThreadId>,
        text: Content,
        mut hold: Option<HoldSnapshot>,
        screen: Option<crate::context::Screen>,
        effects: &mut Vec<Effect>,
    ) -> RunId {
        self.cancel_job(effects);
        if let Some(current) = self.current.take() {
            let replaced = RunEnd::Interrupted(Interruption::Replaced);
            self.end(current, replaced, None, Notify::Shell, effects);
        }
        self.last_run = RunId(self.last_run.0 + 1);
        let run = self.last_run;
        let (thread, title) = if let Some(thread) = thread {
            (thread, None)
        } else {
            self.last_thread = ThreadId(self.last_thread.0 + 1);
            (self.last_thread, Some(title(&text).into()))
        };
        let begin = StoreOp::BeginTurn {
            thread,
            title,
            run,
            text,
            screen: screen.as_ref().map(crate::context::Screen::held),
        };
        if let Some(hold) = &mut hold {
            hold.defaults.origin = admission::origin(thread, self.main_thread);
        }
        let mut control =
            crate::recall::control::Control::new(std::time::Instant::now() + WALL_CLOCK);
        if let Some(hold) = &hold {
            control = control.observe(std::sync::Arc::clone(&hold.cancel));
        }
        let untold = screen
            .as_ref()
            .and(hold.as_ref())
            .map(|hold| hold.key.gesture);
        let arrived = hold
            .as_ref()
            .map_or_else(|| self.arrived_now(), |hold| hold.arrived);
        let finding = (hold.is_none() && self.claude_code == Availability::Finding)
            .then_some(availability::Finding::Due);
        self.current = Some(Run {
            control: crate::recall::control::CancelOnDrop(control),
            id: run,
            thread,
            turn_stored: false,
            begin: Some(begin),
            answering: false,
            unstored: String::new(),
            text_bytes: 0,
            steps: 0,
            acked: 0,
            scene: None,
            evict: None,
            waiting: VecDeque::new(),
            held: VecDeque::new(),
            snapshot: hold.as_ref().map(|hold| hold.defaults.clone()),
            untold,
            hold,
            pictured: matches!(screen, Some(crate::context::Screen::Picture(..))),
            screen,
            build: None,
            arrived,
            route: None,
            finding,
            compacted: None,
            draw: None,
            draw_calls: Vec::new(),
            provider_calls: Vec::new(),
            error: None,
            ledger: crate::dispatch::Ledger::default(),
            mcp: None,
            answered: Vec::new(),
            tools: tools::Loop::default(),
            finishing: false,
            ending: None,
            reply: self
                .output
                .filter(|_| self.spoken)
                .map(|(rate, generation)| crate::voice::reply::Reply::new(run, rate, generation)),
        });
        self.begin_turns(effects);
        effects.push(Effect::Control(ControlToShell::RunState {
            run,
            thread,
            phase: Phase::Thinking,
            error: None,
            origin: Some(origin),
        }));
        effects.push(Effect::ArmDeadline {
            run,
            after: WALL_CLOCK,
        });
        run
    }

    /// Queues held turns whose older runs all have their outcomes queued, so the store keeps a
    /// thread's events in run order and a replacement's engine starts only after its predecessor
    /// is stored.
    fn begin_turns(&mut self, effects: &mut Vec<Effect>) {
        let oldest_unwritten = self
            .ending
            .iter()
            .filter(|ended| ended.ending.as_ref().is_some_and(|ending| !ending.written))
            .map(|ended| ended.id)
            .min();
        for run in self.ending.iter_mut().chain(self.current.as_mut()) {
            if oldest_unwritten.is_none_or(|oldest| run.id <= oldest)
                && let Some(begin) = run.begin.take()
            {
                effects.push(Effect::Store(begin));
            }
        }
    }

    /// The oldest unanswered operation of `run` failed: the turn, a step, or the outcome.
    fn store_failed(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        if self.find(run)?.begin.is_some() {
            return Err(Stale);
        }
        if let Ok(current) = self.current(run) {
            if !current.outstanding() {
                return Err(Stale);
            }
            let current = self.take(run)?;
            let failed = RunEnd::Failed(Failure::Store);
            self.end(current, failed, None, Notify::Shell, effects);
        }
        let at = self.ending_at(run)?;
        let ended = &mut self.ending[at];
        let (id, thread) = (ended.id, ended.thread);
        let ending = ended.ending.as_mut().expect("an ending run has an Ending");
        if ending.stored {
            return Err(Stale);
        }
        // A failed outcome was already told; any other ending now turns out failed. A quieting
        // one tells it once it's released.
        let tell_end = ending.notify == Notify::Shell
            && !ending.quieting
            && !ending.failed
            && !matches!(ending.outcome, RunEnd::Failed(_));
        if tell_end {
            effects.push(Effect::Control(ControlToShell::RunState {
                run: id,
                thread,
                phase: Phase::Failed,
                error: None,
                origin: None,
            }));
        }
        if tell_end && ending.outcome == RunEnd::Done {
            self.data_end(id, thread, effects);
        }
        let ended = &mut self.ending[at];
        let ending = ended.ending.as_mut().expect("an ending run has an Ending");
        if !ended.turn_stored || ending.written && ending.failed {
            // The turn never stored, so there's no row, or the failed(store) write failed too;
            // a run left active is marked failed(crash) at the next start. Its scene's clear or
            // kept selection is still owed, so it's retried on its own.
            let kept = ended
                .scene
                .zip(ending.revealed.take().filter(|_| ending.trimmed));
            let clear = ending.clear;
            // shortcut: the store gave up, so a quieting run is told without waiting for quiet.
            let tell = ending.quieting && ending.notify == Notify::Shell;
            let gone = self.ending.swap_remove(at);
            if tell {
                self.told(&gone, Phase::Failed, effects);
            }
            self.owe(clear, kept);
            return Ok(());
        }
        // The oldest pending write failed: the round's, the outcome, or the next step.
        if !ended.tools.write_failed() {
            if ending.written {
                ending.written = false;
            } else {
                ended.acked += 1;
            }
        }
        ending.failed = true;
        self.settle(run, effects);
        Ok(())
    }

    /// Ends a run: the engine is cancelled first, so it produces nothing more, and data it
    /// still held is dropped. Every ending but `Done` tells the shell at once; `Done` waits
    /// until the store has it. The outcome is written once the run's earlier writes are answered.
    fn end(
        &mut self,
        mut run: Run,
        outcome: RunEnd,
        revealed: Option<Vec<u32>>,
        notify: Notify,
        effects: &mut Vec<Effect>,
    ) {
        run.control.0.cancel();
        // No send carrying the hold's picture was granted, or none wrote any of it, so its turn
        // mustn't say it was sent.
        if run.untold.is_some()
            && run.screen.as_ref().is_some_and(|screen| {
                matches!(
                    screen.held(),
                    kumochi_protocol::message::HeldScreen::Attached
                        | kumochi_protocol::message::HeldScreen::Screen
                )
            })
        {
            match &mut run.begin {
                Some(StoreOp::BeginTurn { screen, .. }) => *screen = None,
                _ => effects.push(Effect::Store(StoreOp::ForgetScreen { run: run.id })),
            }
        }
        if outcome != RunEnd::Done {
            speech::revoke(&mut run, crate::voice::reply::Revoker::Brain, effects);
        }
        let drew_nothing = outcome == RunEnd::Done && self.drawing.drew_nothing(run.id);
        let loss = match outcome {
            RunEnd::Interrupted(
                Interruption::Stop | Interruption::Replaced | Interruption::Deleted,
            ) => crate::drawing::Loss::Drop,
            RunEnd::Done
            | RunEnd::Interrupted(Interruption::ShellLost | Interruption::ViewStalled)
            | RunEnd::Failed(Failure::Budget | Failure::Engine | Failure::Store) => {
                crate::drawing::Loss::Tell
            }
        };
        self.drawing_closed(run.id, loss, effects);
        run.tools.drop_round();
        if run.turn_stored && outcome != RunEnd::Done {
            effects.push(Effect::CancelEngine { run: run.id });
        }
        // Steps waiting for the scene's id were never sent, so no ack is owed for them.
        run.steps = run.sent();
        run.waiting.clear();
        if loss == crate::drawing::Loss::Drop {
            effects.push(Effect::Revoke { run: run.id });
        }
        run.held.clear();
        let quieting = notify == Notify::Shell
            && outcome != RunEnd::Done
            && run.reply.as_ref().is_some_and(|reply| !reply.is_final());
        let deferred_data = notify == Notify::Shell
            && outcome != RunEnd::Done
            && !quieting
            && self.drawing.pending_for(run.id);
        if notify == Notify::Shell && outcome != RunEnd::Done && !quieting {
            let phase = terminal(outcome);
            if deferred_data {
                effects.push(run_state(&run, phase));
            } else {
                self.told(&run, phase, effects);
            }
        }
        run.ending = Some(Ending {
            outcome,
            revealed,
            notify,
            failed: outcome == RunEnd::Failed(Failure::Store),
            written: false,
            clear: None,
            trimmed: false,
            deferred_data,
            quieting,
            stored: false,
            drew_nothing,
        });
        let id = run.id;
        self.ending.push(run);
        self.settle(id, effects);
    }

    /// Writes an ended run's outcome once nothing earlier of it is pending. A Stop's revealed
    /// set becomes its scene's kept set here, once every step write has settled; a scene left
    /// with nothing kept is cleared in the same transaction.
    fn settle(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        if self.drawing.pending_for(run) {
            return;
        }
        let Some(ended) = self.ending.iter_mut().find(|ended| ended.id == run) else {
            return;
        };
        if ended.outstanding() {
            return;
        }
        let (thread, text, scene) = (ended.thread, unstored(ended), ended.scene);
        let Some(ending) = ended.ending.as_mut().filter(|ending| !ending.written) else {
            return;
        };
        ending.written = true;
        let tell_end = std::mem::take(&mut ending.deferred_data) && ending.notify == Notify::Shell;
        if let (Some(revealed), Some(scene)) = (&ending.revealed, scene)
            && !ending.trimmed
        {
            ending.trimmed = true;
            ending.clear = scenes::keep(&mut self.scenes, scene, revealed);
        }
        let op = if ending.failed {
            StoreOp::EndRun {
                thread,
                run,
                text: None,
                revealed: ending.revealed.clone(),
                clear: ending.clear,
                end: RunEnd::Failed(Failure::Store),
            }
        } else {
            StoreOp::EndRun {
                thread,
                run,
                text,
                revealed: ending.revealed.clone(),
                clear: ending.clear,
                end: ending.outcome,
            }
        };
        if tell_end {
            self.data_end(run, thread, effects);
        }
        effects.push(Effect::Store(op));
    }

    /// Tells the shell a run ended: its terminal phase on the control pipe, and the end of its
    /// produced data on the data pipe, after its run-bearing drawing frames.
    fn told(&mut self, run: &Run, phase: Phase, effects: &mut Vec<Effect>) {
        effects.push(run_state(run, phase));
        self.data_end(run.id, run.thread, effects);
    }

    fn current(&mut self, run: RunId) -> Result<&mut Run, Stale> {
        self.current
            .as_mut()
            .filter(|current| current.id == run)
            .ok_or(Stale)
    }

    fn take(&mut self, run: RunId) -> Result<Run, Stale> {
        self.current(run)?;
        self.current.take().ok_or(Stale)
    }

    fn ending_at(&self, run: RunId) -> Result<usize, Stale> {
        self.ending
            .iter()
            .position(|ended| ended.id == run)
            .ok_or(Stale)
    }

    /// The current run or one still ending.
    fn find(&mut self, run: RunId) -> Result<&mut Run, Stale> {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.id == run)
        {
            return self.current(run);
        }
        let at = self.ending_at(run)?;
        Ok(&mut self.ending[at])
    }
}

/// The phase that tells the shell a run ended other than `Done`.
fn terminal(outcome: RunEnd) -> Phase {
    match outcome {
        RunEnd::Interrupted(_) => Phase::Interrupted,
        RunEnd::Done | RunEnd::Failed(_) => Phase::Failed,
    }
}

fn run_state(run: &Run, phase: Phase) -> Effect {
    Effect::Control(ControlToShell::RunState {
        run: run.id,
        thread: run.thread,
        phase,
        error: run.error.clone().filter(|_| phase == Phase::Failed),
        origin: None,
    })
}

/// The text streamed since the last write, taken for the next one.
fn unstored(run: &mut Run) -> Option<Content> {
    (!run.unstored.is_empty()).then(|| std::mem::take(&mut run.unstored).into())
}

/// A new thread's title: the first line of its first turn, cut to [`TITLE_CHARS`].
fn title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(TITLE_CHARS)
        .collect()
}
