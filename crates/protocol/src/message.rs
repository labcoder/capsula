//! The messages on each pipe, as closed enums. The shell's first frame on every pipe is a
//! [`Hello`], and the brain decodes nothing else until the capability in it checks out.
//!
//! postcard encodes a variant by its position, so a new variant goes at the end of its enum and
//! the handshake's bytes stay readable by other builds.

use serde::{Deserialize, Serialize};

use crate::Secret;
use crate::geometry::{DisplayRef, PhysicalRect, WindowRef};

mod account;
mod connections;
mod content;
mod error;
mod pages;
mod scenes;
#[macro_use]
mod settings;
mod threads;
pub use account::{
    AccountEmail, AccountOp, AccountRecovery, AccountView, Cleanup, EmailError, MANAGE_USAGE_URL,
    MAX_EMAIL_BYTES, PlanState,
};
pub use connections::{
    AuditFailed, ClaudeCodeView, ClaudeVersion, ConnectionKind, ConnectionOp, ConnectionsView,
    KeyRejection, KeyView, PolicyFailed, Unsupported, VersionError, validate_anthropic_key,
};
pub use content::Content;
pub use error::{
    Action, ErrorArg, ErrorKey, MAX_ERROR_ACTIONS, MAX_ERROR_ARGS, TooMany, UserError,
};
pub use pages::{EventId, PageEvent, PageEventKind, PageOf, ThreadCursor, ThreadRow};
pub use scenes::{
    MAX_SCENE_LOG_ENTRIES, MAX_SCENE_SNAPSHOT_BYTES, MAX_SCENE_STAGING_BYTES, SceneCut, SceneCuts,
    SceneDataError, SceneDescriptorV1, SceneFailure, SceneHeader, SceneSnapshotChunk,
};
pub use settings::{
    AssistantName, MAX_NAME_CHARS, MaxFrameRate, NameError, NotACap, SettingsOp, SettingsView,
    UserName,
};
pub use threads::{
    Effort, LISTED_MODELS_CALL_TOOLS, MAX_MODEL_NAME_CHARS, MAX_MODELS, MAX_SLUG_BYTES,
    MAX_TITLE_CHARS, ModelChoice, ModelInfo, ModelName, ModelSlug, SlugError, ThreadKind, ThreadOp,
    ThreadTitle,
};

/// The most rows one page carries [guess].
pub const MAX_PAGE_ROWS: usize = 50;
/// The largest encoded page frame, header included [guess].
pub const MAX_PAGE_BYTES: usize = 32 * 1024;
/// Unanswered page requests before the brain ends the connection [guess].
pub const MAX_PENDING_PAGES: usize = 4;

/// Length of the instance capability, in bytes.
pub const CAPABILITY_LEN: usize = 32;

/// The random secret the brain rotates on every start and stores in Credential Manager. A shell
/// proves it runs as the same user by reading it back and sending it in [`Hello`].
pub type Capability = Secret<[u8; CAPABILITY_LEN]>;

/// One run of the brain, random per start. A reconnect to a new incarnation discards everything
/// in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incarnation(pub u64);

/// One thread of conversation, as the store numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub u64);

/// One user turn, and the epoch every run-scoped message carries. Ids only grow, even across
/// restarts, so a stale message can never pass for a new one. The default, 0, is below every id
/// the brain hands out.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct RunId(pub u64);

/// A hold gesture, counted up by the shell on each connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GestureId(pub u32);

/// An exclusion lease generation, counted up by the shell on each connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGeneration(pub u32);

/// The user's preferred focus within a captured display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusHint {
    /// A selected region in physical desktop pixels.
    Region(PhysicalRect),
    /// The native window the user selected.
    Window(WindowRef),
}

/// The input that requested a capture, scoped to one shell connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureFor {
    /// A recognized hold.
    Gesture(GestureId),
    /// A typed submit.
    Turn(u32),
}

/// A content-free reason a capture could not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureFailure {
    /// No live exclusion lease protects the capture.
    ExclusionUnavailable,
    /// The requesting gesture or turn was cancelled.
    Cancelled,
    /// The capture worker or platform failed.
    Unavailable,
}

/// The input that admitted a run, present on its first state only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOrigin {
    /// A typed submit's connection-local number.
    Typed {
        /// The submit number.
        turn: u32,
    },
    /// A recognized hold's connection-local identifier.
    Hold {
        /// The recognized gesture.
        gesture: GestureId,
    },
}

/// Whether a typed submit requests screen context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenChoice {
    /// Text only.
    None,
    /// Include the screen under this exclusion lease.
    Include {
        /// The exclusion lease protecting capture.
        lease: LeaseGeneration,
    },
}

/// The shell's first frame on each pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The shell's [`crate::BUILD_ID`].
    pub build_id: String,
    /// The capability the shell read from Credential Manager.
    pub capability: Capability,
}

/// The most steps one run may draw (architecture v1 §5.2).
pub const MAX_STEPS: usize = 64;

/// New scene v1 steps committed by one run, independently of a scene's lifetime step cap.
pub const MAX_DRAW_STEPS_PER_RUN: usize = 64;

/// The longest placeholder step label, in bytes [guess].
pub const MAX_STEP_LABEL: usize = 256;

/// The most scenes on screen at once. A fifth scene evicts the oldest.
pub const MAX_VISIBLE_SCENES: usize = 4;

/// One drawing: the id of the store event that began it. Ids only grow, even across deletions
/// and restarts, and none leaves the brain before its first step is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SceneId(pub u64);

/// A visible scene, as [`ControlToShell::Scenes`] names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneDescriptor {
    /// The scene.
    pub scene: SceneId,
    /// Every step committed to it, kept or not; it never goes back.
    pub revision: u32,
    /// The steps it keeps after any Stop, which its snapshot carries.
    pub committed: u32,
}

/// Bytes of data-pipe frames the brain may send before the shell grants more with
/// [`DataToBrain::Credit`] [guess]. It holds the largest frame the data pipe allows, so no frame
/// is too big to send, and several of the 64 KiB text frames runs stream, so a reading shell
/// rarely waits on a grant.
pub const DATA_WINDOW: u32 = 512 * 1024;

/// Deletes the shell holds unanswered, or answered while a turn sent before them still is
/// [guess]. The brain keeps twice as many in flight: those that outlived an earlier connection,
/// whose shell may have restarted and forgotten them, and the current shell's.
pub const MAX_PENDING_DELETES: usize = 4;

/// The most typed turns the brain holds before it has taken them. One more ends the connection
/// as broken, so the shell keeps fewer than this many `TextSubmit`s unanswered [guess].
pub const MAX_PENDING_TURNS: usize = 64;

/// One drawing step. shortcut: a labelled placeholder so runs, recovery and Esc can be built
/// and tested; slice 4 replaces it with the `scene` v1 step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// What the step shows, at most [`MAX_STEP_LABEL`] bytes.
    pub label: String,
}

/// Where a run is, as the shell shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Accepted; nothing has streamed yet.
    Thinking,
    /// Text or steps are arriving.
    Answering,
    /// Finished.
    Done,
    /// Stopped before it finished.
    Interrupted,
    /// Ended by an error.
    Failed,
    /// A recognized hold is waiting for input.
    Listening,
}

/// Shell to brain, on the control pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlToBrain {
    /// The handshake; first and only once.
    Hello(Hello),
    /// The user chose Quit: the brain ends its helpers, deletes the capability and exits, and
    /// doesn't relaunch the shell.
    Quitting,
    /// The first Esc, after the shell has already stopped locally. `revealed` lists the steps
    /// whose entrance had finished, so the thread records what stayed; the brain keeps only
    /// indices of steps the run produced, and the control cap bounds the list.
    Stop {
        /// The run the shell stopped.
        run: RunId,
        /// Indices of the steps that stayed on screen.
        revealed: Vec<u32>,
    },
    /// The answer to [`ControlToShell::Ping`].
    Pong,
    /// The second Esc, after the shell has already cleared the drawing locally. The thread keeps
    /// the drawing for replay; the brain records the clear so it doesn't come back.
    ClearDrawing {
        /// The scenes cleared, at most [`MAX_VISIBLE_SCENES`]; more ends the connection.
        scene_ids: Vec<SceneId>,
    },
    /// The user confirmed deleting a thread. The shell never targets it again while the delete
    /// is unanswered.
    DeleteThread {
        /// The thread.
        thread: ThreadId,
        /// How many `TextSubmit`s the shell sent on this connection before this delete, so the
        /// brain refuses any of them that names the thread.
        after_turns: u32,
        /// A resend, at a connection's start, of a delete the shell sent on an earlier one.
        resend: bool,
    },
    /// Change a setting; answered by exactly one [`ControlToShell::Settings`].
    Settings(SettingsOp),
    /// Rename a task or set a thread's model; nothing answers it.
    ThreadOp(ThreadOp),
    /// A scene-wide Stop, applied even when a later run appended to one of its scenes.
    StopScenes {
        /// The run fenced locally; scene cuts are not restricted to this run's commits.
        run: RunId,
        /// Every known visible v1 scene's activated revision and finished objects.
        cuts: SceneCuts,
    },
    /// The first paint containing a step; ordered before later cuts on this control pipe.
    StepShown {
        /// The run that committed the step.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Zero-based lifetime commit index, not an index into the kept steps.
        index: u32,
    },
    /// Preparation failed; the entire scene stops at this cut, through the ordinary cut FIFO.
    StepFailed {
        /// The run that committed the failed step.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// The failed step's zero-based lifetime index.
        index: u32,
        /// A content-free reason; never a parser diagnostic or model text.
        reason: SceneFailure,
        /// The activated revision and finished objects; zero activation clears the scene.
        cut: kumochi_scene::StopCut,
    },
    /// Something the user asked of the account; answered by one [`ControlToShell::Account`].
    AccountOp(AccountOp),
    /// The shell recognized a hold while its exclusion lease was live.
    HoldRecognized {
        /// The connection-local gesture.
        gesture: GestureId,
        /// The display containing the pointer at recognition.
        pointer_display: DisplayRef,
        /// The thread to continue, or a new thread when absent.
        thread: Option<ThreadId>,
        /// The exclusion lease protecting capture.
        lease: LeaseGeneration,
    },
    /// The hold key was released.
    KeyUp {
        /// The released gesture.
        gesture: GestureId,
    },
    /// The shell cancelled the hold.
    GestureCancel {
        /// The cancelled gesture.
        gesture: GestureId,
    },
    /// A region or window selected for this hold's context.
    FocusHint {
        /// The gesture whose context is being refined.
        gesture: GestureId,
        /// The selected region or window.
        hint: FocusHint,
    },
    /// A capture exclusion lease stopped protecting the screen.
    ExclusionLapsed {
        /// The lease that stopped protecting capture.
        lease: LeaseGeneration,
    },
    /// Something the user asked of a Claude connection; answered by one
    /// [`ControlToShell::Connections`].
    Connection(ConnectionOp),
    /// The user started typing or holding in `thread`, so a Claude Code launch may warm up.
    Warm {
        /// The thread.
        thread: ThreadId,
    },
}

/// What a thread's delete came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deleted {
    /// Nothing changed: the thread is live and whole.
    NotHidden,
    /// The thread is hidden and takes no new work, but its rows remain until the next start
    /// removes them.
    Hidden,
    /// The thread's rows are gone, or it was already absent.
    Removed,
}

/// Brain to shell, on the control pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlToShell {
    /// The handshake succeeded.
    Welcome {
        /// This brain's incarnation.
        incarnation: Incarnation,
    },
    /// The shell is from another build. The brain exits after sending this, so the shell can
    /// start the brain from its own folder.
    BuildMismatch {
        /// The brain's [`crate::BUILD_ID`].
        brain_build: String,
    },
    /// A run's phase changed.
    RunState {
        /// The run.
        run: RunId,
        /// Its thread.
        thread: ThreadId,
        /// Its new phase.
        phase: Phase,
        /// Why it failed, sent only with [`Phase::Failed`] and only when there's something to
        /// tell. shortcut: it isn't stored, so a reloaded page shows the generic failure.
        error: Option<UserError>,
        /// The admitting input, on the first state only; later states carry `None`.
        origin: Option<RunOrigin>,
    },
    /// The watchdog, sent only while a run is active; the shell answers [`ControlToBrain::Pong`].
    Ping,
    /// The visible scenes, once per connection before any turn is taken. It's authoritative: a
    /// scene it doesn't name is no longer visible. Their snapshots follow on the data pipe.
    Scenes {
        /// At most [`MAX_VISIBLE_SCENES`], each named once.
        scenes: Vec<SceneDescriptor>,
    },
    /// The answer to [`ControlToBrain::DeleteThread`]. A thread's drawing leaves on the data
    /// pipe's [`DataToShell::ScenesRemoved`], not with this.
    ThreadDeleted {
        /// The thread.
        thread: ThreadId,
        /// What the delete came to.
        outcome: Deleted,
        /// On `Hidden` and `Removed`, at least the newest run of the thread the brain named; the
        /// shell fences every run up to it.
        last_run: Option<RunId>,
    },
    /// A `TextSubmit` the brain refused before giving it a run: its thread is being or was
    /// deleted, or it was blank. It answers one submit, in order with the `Thinking` of
    /// admitted ones.
    TurnRefused {
        /// The refused submit's connection-local number.
        turn: u32,
        /// The thread the submit named.
        thread: Option<ThreadId>,
    },
    /// The settings: once per connection before `Scenes`, once per
    /// [`ControlToBrain::Settings`] from this connection, in order, and after a change an earlier
    /// connection sent is saved.
    Settings {
        /// The settings as saved.
        view: SettingsView,
        /// It answers this connection's oldest unanswered change. Otherwise it's the settled
        /// settings, sent on its own.
        answer: bool,
    },
    /// Something the user should know went wrong. A refused settings change from this
    /// connection sends it just before that change's answer.
    Error(UserError),
    /// Authoritative v1 scene membership before snapshots, replacing `Scenes` at the B3 switch.
    ScenesV1 {
        /// At most four descriptors; each full recovery log has its own advertised length.
        scenes: kumochi_scene::text::Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
    },
    /// A requested drawing never produced a visual outcome; separate from preparation errors.
    NoVisual {
        /// The run that requested a drawing.
        run: RunId,
    },
    /// The account: once per connection after `Settings` and before `Scenes`, after every
    /// change, and once per [`ControlToBrain::AccountOp`]. It carries the display email, so
    /// the tap never records it.
    Account(AccountView),
    /// The brain changed a listed thread on its own, such as a task's title, so the shell reads
    /// its list again. It carries nothing.
    ThreadsChanged,
    /// Capture finished; the shell may release the input's exclusion lease.
    CaptureDone {
        /// The input whose capture completed.
        capture_for: CaptureFor,
    },
    /// Capture failed; the shell may release the input's exclusion lease.
    CaptureFailed {
        /// The input whose capture failed.
        capture_for: CaptureFor,
        /// A content-free failure category.
        reason: CaptureFailure,
    },
    /// Both Claude connections: once per connection after `Settings` and `Account` and before
    /// `Scenes`, after every change, and once per [`ControlToBrain::Connection`].
    Connections(ConnectionsView),
}

/// Shell to brain, on the data pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataToBrain {
    /// The handshake, repeated so the data pipe is authenticated on its own.
    Hello(Hello),
    /// Typed chat: a new turn in `thread`, or in a new thread when it's `None`.
    TextSubmit {
        /// Counted up from one by the shell on each connection.
        turn: u32,
        /// Whether this turn requests screen context.
        screen: ScreenChoice,
        /// The thread to continue.
        thread: Option<ThreadId>,
        /// What the user typed.
        text: Content,
    },
    /// The shell has taken `bytes` more of the brain's data frames, header included, and the
    /// brain may send that many more.
    Credit {
        /// How many bytes.
        bytes: u32,
    },
    /// Read one bounded page, without changing any run.
    PageRequest {
        /// Echoed in the reply so a superseded request can be dropped.
        request: u32,
        /// The list and its continuation.
        page: PageOf,
    },
}

/// Brain to shell, on the data pipe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DataToShell {
    /// More of the assistant's answer.
    AssistantText {
        /// The run it belongs to.
        run: RunId,
        /// The new text.
        delta: Content,
    },
    /// One step, sent only once the brain has committed it.
    StepCommit {
        /// The run it belongs to.
        run: RunId,
        /// The scene it draws in.
        scene: SceneId,
        /// Its place in its scene, from 0.
        index: u32,
        /// The step.
        step: Step,
    },
    /// The run's last data frame. The pipes aren't ordered with each other, so a terminal
    /// `RunState` on the control pipe doesn't mean the run's data has arrived; this does.
    RunEnd {
        /// The run that ended.
        run: RunId,
    },
    /// A bounded page of live threads.
    ThreadList {
        /// The request this answers.
        request: u32,
        /// Rows in list order.
        threads: Vec<ThreadRow>,
        /// Continue after this key, or stop when absent.
        next: Option<ThreadCursor>,
    },
    /// A bounded page of a thread's stored events, newest first.
    Page {
        /// The request this answers.
        request: u32,
        /// The thread read.
        thread: ThreadId,
        /// Stored events; empty for a missing or deleting thread.
        events: Vec<PageEvent>,
        /// Continue before this id, or stop when absent.
        before: Option<EventId>,
    },
    /// The read for `request` failed; the brain logged why.
    PageFailed {
        /// The request this answers.
        request: u32,
    },
    /// Part of a visible scene's kept steps, in order, after [`ControlToShell::Scenes`] named it.
    SceneSnapshot {
        /// The scene.
        scene: SceneId,
        /// The revision its descriptor named.
        revision: u32,
        /// Whole steps, never empty.
        chunk: Vec<Step>,
        /// The scene's last chunk; the shell installs the snapshot only then.
        last: bool,
    },
    /// A deleted thread's scenes left the visible set; nothing of them installs after this.
    ScenesRemoved {
        /// At most [`MAX_VISIBLE_SCENES`].
        scenes: Vec<SceneId>,
    },
    /// A live scene's immutable header, on the same FIFO before its first typed step.
    SceneBegin {
        /// The scene, allocated by the brain.
        scene: SceneId,
        /// Typed header and layout version used for live and recovery preparation.
        header: SceneHeader,
    },
    /// One durable v1 step. Scene ownership preserves it across Stop's run revocation.
    SceneStepCommit {
        /// The run that committed the step.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// Zero-based lifetime commit index.
        index: u32,
        /// Original input, never a normalized or pre-expanded replacement.
        step: kumochi_scene::Step,
    },
    /// A bounded chunk of the complete ordered step/cut log; install only on the last chunk.
    SceneSnapshotV1 {
        /// Chunk zero includes the header; later chunks omit it.
        chunk: SceneSnapshotChunk,
    },
    /// The brain accepted this scene-wide cut, after delivering every prior durable step.
    SceneCutApplied {
        /// The scene whose outstanding-cut hold may now be released.
        scene: SceneId,
        /// The last committed revision before the cut, not the cut's activated bound.
        revision: u32,
        /// The authoritative cut applied to the mirror at exactly that revision.
        cut: kumochi_scene::StopCut,
    },
    /// A draw call ended; committed steps remain even when its suffix was incomplete.
    SceneEnd {
        /// The originating run.
        run: RunId,
        /// The scene.
        scene: SceneId,
        /// False after malformed, truncated, invalid or differing final arguments.
        complete: bool,
    },
    /// The account's model catalog, whole: sent when a read succeeds and to each shell that
    /// connects after one, and empty once the account stops being usable. Nothing is sent before a
    /// read succeeds.
    Models {
        /// The connection whose catalog this is.
        connection: ConnectionKind,
        /// At most [`MAX_MODELS`], in the catalog's order.
        models: Vec<ModelInfo>,
    },
}

#[cfg(test)]
#[path = "message/scene_frame_tests.rs"]
mod scene_frame_tests;

#[cfg(test)]
#[path = "message/hold_frame_tests.rs"]
mod hold_frame_tests;

#[cfg(test)]
#[path = "message/content_tests.rs"]
mod content_tests;

#[cfg(test)]
mod tests {
    use super::{
        CAPABILITY_LEN, ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain, DataToShell,
        Deleted, Effort, Hello, Incarnation, MAX_MODEL_NAME_CHARS, MAX_MODELS, MAX_PAGE_BYTES,
        MAX_PAGE_ROWS, MAX_SLUG_BYTES, MAX_STEP_LABEL, MAX_STEPS, MAX_TITLE_CHARS,
        MAX_VISIBLE_SCENES, ModelChoice, ModelInfo, ModelName, ModelSlug, RunId, SceneDescriptor,
        SceneId, Secret, Step, ThreadCursor, ThreadId, ThreadKind, ThreadRow,
    };
    use crate::frame::{self, HEADER_LEN, Pipe};

    /// The bytes another build must be able to read: the handshake and its answers. A new
    /// variant goes at the end of its enum, or these tags move.
    #[test]
    fn handshake_frames_keep_their_bytes_across_builds() {
        let hello = Hello {
            build_id: "b".to_owned(),
            capability: Secret::new([7; CAPABILITY_LEN]),
        };
        let mut hello_bytes = vec![0, 1, b'b'];
        hello_bytes.extend([7; CAPABILITY_LEN]);
        let encode = |bytes: postcard::Result<Vec<u8>>| bytes.expect("encodes");
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToBrain::Hello(hello.clone()))),
            hello_bytes
        );
        assert_eq!(
            encode(postcard::to_allocvec(&DataToBrain::Hello(hello))),
            hello_bytes
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToBrain::Quitting)),
            [1]
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToShell::Welcome {
                incarnation: Incarnation(5)
            })),
            [0, 5]
        );
        assert_eq!(
            encode(postcard::to_allocvec(&ControlToShell::BuildMismatch {
                brain_build: "b".to_owned()
            })),
            [1, 1, b'b']
        );
    }

    /// Worst-case ids and revisions: the largest varints postcard writes.
    const SCENE: SceneId = SceneId(u64::MAX);

    /// The payload a frame carries, checked against the pipe's cap by encoding it.
    fn payload<T: serde::Serialize>(message: &T, pipe: Pipe) -> usize {
        frame::encode(message, pipe)
            .expect("fits the pipe's cap")
            .len()
            - HEADER_LEN
    }

    #[test]
    fn four_worst_case_descriptors_fit_the_control_pipe() {
        let descriptor = SceneDescriptor {
            scene: SCENE,
            revision: u32::MAX,
            committed: u32::MAX,
        };
        let scenes = ControlToShell::Scenes {
            scenes: vec![descriptor; MAX_VISIBLE_SCENES],
        };
        // Tag and length, then 10 + 5 + 5 bytes per descriptor.
        assert_eq!(payload(&scenes, Pipe::Control), 2 + 4 * 20);
    }

    #[test]
    fn worst_case_delete_frames_fit_their_pipes() {
        let thread = ThreadId(u64::MAX);
        let delete = ControlToBrain::DeleteThread {
            thread,
            after_turns: u32::MAX,
            resend: true,
        };
        assert_eq!(payload(&delete, Pipe::Control), 1 + 10 + 5 + 1);
        let deleted = ControlToShell::ThreadDeleted {
            thread,
            outcome: Deleted::Removed,
            last_run: Some(RunId(u64::MAX)),
        };
        assert_eq!(payload(&deleted, Pipe::Control), 1 + 10 + 1 + 1 + 10);
        let refused = ControlToShell::TurnRefused {
            turn: u32::MAX,
            thread: Some(thread),
        };
        assert_eq!(payload(&refused, Pipe::Control), 1 + 5 + 1 + 10);
        let removed = DataToShell::ScenesRemoved {
            scenes: vec![SCENE; MAX_VISIBLE_SCENES],
        };
        assert_eq!(payload(&removed, Pipe::Data), 2 + 4 * 10);
    }

    /// Titles are at most 80 characters when written, so only an older or planted one ever needs
    /// the page budget's shortening.
    #[test]
    fn a_full_page_of_the_longest_titles_and_choices_fits_the_page_cap() {
        let row = ThreadRow {
            id: ThreadId(u64::MAX),
            title: "🌥".repeat(MAX_TITLE_CHARS),
            updated_ms: i64::MIN,
            kind: ThreadKind::Thread,
            choice: Some(ModelChoice {
                model: ModelSlug::new(&"a".repeat(MAX_SLUG_BYTES)).expect("valid"),
                effort: Effort::XHigh,
                connection: super::ConnectionKind::ChatGpt,
            }),
        };
        let list = DataToShell::ThreadList {
            request: u32::MAX,
            threads: vec![row; MAX_PAGE_ROWS],
            next: Some(ThreadCursor {
                updated_ms: i64::MIN,
                id: ThreadId(u64::MAX),
                rank_bits: u64::MAX,
            }),
        };
        // Per row: id, a 2-byte length and 320 bytes of title, time, kind, Some, a 1-byte length
        // and 64 bytes of slug, effort, connection.
        let row_len = 10 + 2 + 320 + 10 + 1 + 1 + 1 + 64 + 1 + 1;
        let payload = payload(&list, Pipe::Data);
        assert_eq!(payload, 1 + 5 + 1 + MAX_PAGE_ROWS * row_len + 1 + 30);
        assert!(payload + HEADER_LEN <= MAX_PAGE_BYTES, "{payload} bytes");
    }

    #[test]
    fn a_clear_of_four_worst_case_ids_fits_the_control_pipe() {
        let clear = ControlToBrain::ClearDrawing {
            scene_ids: vec![SCENE; MAX_VISIBLE_SCENES],
        };
        assert_eq!(payload(&clear, Pipe::Control), 2 + 4 * 10);
    }

    #[test]
    fn a_chunk_of_every_step_at_the_longest_label_fits_a_data_frame_and_the_window() {
        let chunk = DataToShell::SceneSnapshot {
            scene: SCENE,
            revision: u32::MAX,
            chunk: vec![
                Step {
                    label: "é".repeat(MAX_STEP_LABEL / 2),
                };
                MAX_STEPS
            ],
            last: true,
        };
        let payload = payload(&chunk, Pipe::Data);
        // Tag, id, revision, length, then a 2-byte length and 256 bytes per label, then last.
        assert_eq!(
            payload,
            1 + 10 + 5 + 1 + MAX_STEPS * (2 + MAX_STEP_LABEL) + 1
        );
        let window = usize::try_from(DATA_WINDOW).expect("the window fits in usize");
        assert!(payload + HEADER_LEN <= window, "{payload} bytes");
    }

    #[test]
    fn the_longest_catalog_fits_a_data_frame() {
        let model = ModelInfo {
            slug: ModelSlug::new(&"a".repeat(MAX_SLUG_BYTES)).expect("valid"),
            display_name: ModelName::new(&"\u{1F600}".repeat(MAX_MODEL_NAME_CHARS)).expect("valid"),
        };
        let models = DataToShell::Models {
            connection: super::ConnectionKind::ClaudeCode,
            models: vec![model; MAX_MODELS],
        };
        // Tag, the connection's tag and a two-byte count, then 200 times a one-byte slug length,
        // 64 bytes of slug, a two-byte name length and 80 four-byte characters.
        assert_eq!(payload(&models, Pipe::Data), 77_404);
    }
}
