//! The messages on each pipe, as closed enums. The shell's first frame on every pipe is a
//! [`Hello`], and the brain decodes nothing else until the capability in it checks out.
//!
//! postcard encodes a variant by its position, so a new variant goes at the end of its enum and
//! the handshake's bytes stay readable by other builds.

use serde::{Deserialize, Serialize};

use crate::Secret;

mod pages;
mod settings;
pub use pages::{EventId, PageEvent, PageEventKind, PageOf, ThreadCursor, ThreadRow};
pub use settings::{AssistantName, MAX_NAME_CHARS, NameError, SettingsOp, SettingsView, UserName};

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
}

/// Shell to brain, on the data pipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataToBrain {
    /// The handshake, repeated so the data pipe is authenticated on its own.
    Hello(Hello),
    /// Typed chat: a new turn in `thread`, or in a new thread when it's `None`.
    TextSubmit {
        /// The thread to continue.
        thread: Option<ThreadId>,
        /// What the user typed.
        text: String,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataToShell {
    /// More of the assistant's answer.
    AssistantText {
        /// The run it belongs to.
        run: RunId,
        /// The new text.
        delta: String,
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
}

#[cfg(test)]
mod tests {
    use super::{
        CAPABILITY_LEN, ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain, DataToShell,
        Deleted, Hello, Incarnation, MAX_STEP_LABEL, MAX_STEPS, MAX_VISIBLE_SCENES, RunId,
        SceneDescriptor, SceneId, Secret, Step, ThreadId,
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
            thread: Some(thread),
        };
        assert_eq!(payload(&refused, Pipe::Control), 1 + 1 + 10);
        let removed = DataToShell::ScenesRemoved {
            scenes: vec![SCENE; MAX_VISIBLE_SCENES],
        };
        assert_eq!(payload(&removed, Pipe::Data), 2 + 4 * 10);
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
}
