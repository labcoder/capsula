use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, MAX_PENDING_TURNS, Phase, RunId,
    SceneDescriptor, SceneId, SettingsView, Step, ThreadId,
};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::data::DataQueue;
use super::{Brain, Ended, Link, MAX_ENDING, window};
use crate::coordinator::Input;
use crate::engines::Engine;
use crate::engines::fixture::Fixture;
use crate::ipc;
#[cfg(windows)]
use crate::platform::{self, PipeClient, PipeServer};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

#[path = "account_tests.rs"]
mod accounts;
#[path = "ask_tests.rs"]
mod asks;
#[path = "claude_publish_tests.rs"]
mod claude_publish;
#[path = "delete_tests.rs"]
mod deletes;
#[path = "drawing_tests.rs"]
mod drawings;
#[path = "job_tests.rs"]
mod jobs;
#[path = "lease_tests.rs"]
mod leases;
#[path = "loop_tests.rs"]
mod loops;
#[path = "page_tests.rs"]
mod pages;
#[path = "scene_tests.rs"]
mod scenes;
#[path = "settings_tests.rs"]
mod settings;
#[path = "speech_tests.rs"]
mod speech;
#[path = "thread_op_tests.rs"]
mod thread_ops;

#[test]
fn staged_hold_controls_do_not_admit_or_change_a_run() {
    use kumochi_protocol::geometry::{DisplayRef, PhysicalRect, WindowRef};
    use kumochi_protocol::message::{FocusHint, GestureId, LeaseGeneration};
    let gesture = GestureId(9);
    let lease = LeaseGeneration(4);
    for message in [
        ControlToBrain::HoldRecognized {
            gesture,
            pointer_display: DisplayRef(PhysicalRect {
                origin: [-1, 2],
                size: [3, 4],
            }),
            thread: Some(ThreadId(1)),
            lease,
            t_qpc: u64::MAX,
        },
        ControlToBrain::KeyDown { gesture, t_qpc: 1 },
        ControlToBrain::KeyUp {
            gesture,
            t_qpc: 2,
            chunks: 3,
        },
        ControlToBrain::GestureCancel { gesture },
        ControlToBrain::FocusHint {
            gesture,
            hint: FocusHint::Window(WindowRef(17)),
        },
        ControlToBrain::ExclusionLapsed { lease },
    ] {
        assert_eq!(super::controls::control_input(message), Ok(None));
    }
}

#[test]
fn staged_github_and_issue_controls_do_not_admit_or_change_a_run() {
    use kumochi_protocol::message::{DraftId, GitHubOp, IssueOp};

    for message in [
        ControlToBrain::GitHub(GitHubOp::Connect),
        ControlToBrain::Issue(IssueOp::Discard { draft: DraftId(9) }),
    ] {
        assert_eq!(super::controls::control_input(message), Ok(None));
    }
}

#[test]
fn speech_controls_reach_the_coordinator_and_an_output_rate_out_of_bounds_ends_the_connection() {
    use kumochi_protocol::message::{Cursor, OutputGeneration, SegmentId};
    let (run, segment) = (RunId(1), SegmentId(2));
    for (rate, ok) in [
        (7_999, false),
        (8_000, true),
        (192_000, true),
        (192_001, false),
    ] {
        let output = ControlToBrain::AudioOutput {
            rate,
            generation: OutputGeneration(1),
        };
        let expected = if ok {
            Ok(Some(Input::AudioOutput {
                rate,
                generation: OutputGeneration(1),
            }))
        } else {
            Err(super::Ended::Broken("an output rate out of bounds"))
        };
        assert_eq!(
            super::controls::control_input(output),
            expected,
            "{rate} Hz"
        );
    }
    let cursor = Some(Cursor { segment, sample: 5 });
    for (message, input) in [
        (
            ControlToBrain::SpeechProgress {
                run,
                segment,
                sample: 3,
                t_qpc: 4,
            },
            Input::SpeechProgress {
                run,
                segment,
                sample: 3,
            },
        ),
        (
            ControlToBrain::SpeechStopped { run, cursor: None },
            Input::SpeechStopped { run, cursor: None },
        ),
        (
            ControlToBrain::SpeechStopped { run, cursor },
            Input::SpeechStopped { run, cursor },
        ),
        (
            ControlToBrain::SegmentPlayed {
                run,
                segment,
                samples: 6,
            },
            Input::SegmentPlayed {
                run,
                segment,
                samples: 6,
            },
        ),
    ] {
        assert_eq!(super::controls::control_input(message), Ok(Some(input)));
    }
}

#[test]
fn scene_v1_control_reaches_the_canonical_drawing_consumer() {
    use kumochi_protocol::message::{SceneCuts, SceneFailure};
    let cut = kumochi_protocol::message::SceneCut {
        scene: SceneId(1),
        cut: serde_json::from_str(r#"{"activated":0,"revealed":[]}"#).expect("empty cut"),
    };
    let messages = [
        ControlToBrain::StopScenes {
            run: RunId(1),
            cuts: SceneCuts::new(vec![cut.clone()]).expect("one cut"),
        },
        ControlToBrain::StepShown {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
        },
        ControlToBrain::StepFailed {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
            reason: SceneFailure::Timeout,
            cut: cut.cut.clone(),
        },
    ];
    let expected = [
        Input::StopScenes {
            run: RunId(1),
            cuts: SceneCuts::new(vec![cut.clone()]).expect("one cut"),
        },
        Input::Drawing(crate::drawing::Input::StepShown {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
        }),
        Input::Drawing(crate::drawing::Input::StepFailed {
            run: RunId(1),
            scene: SceneId(1),
            index: 0,
            cut: cut.cut,
        }),
    ];
    for (message, expected) in messages.into_iter().zip(expected) {
        assert_eq!(super::controls::control_input(message), Ok(Some(expected)));
    }
}

/// How long a test waits for something that should happen at once before failing: far above a
/// slow CI runner's worst case, so only a hang runs it out.
const PATIENCE: Duration = Duration::from_secs(60);

thread_local! {
    /// The brain's close and `Scenes` waits in this test, `PATIENCE` unless it checks one expires.
    static WAITS: Cell<(Duration, Duration)> = const { Cell::new((PATIENCE, PATIENCE)) };
}

pub(super) fn close_wait() -> Duration {
    WAITS.get().0
}

pub(super) fn scenes_wait() -> Duration {
    WAITS.get().1
}

/// Gives this test the product's close and `Scenes` waits, for a test that checks one expires.
fn product_waits() {
    WAITS.set((super::CLOSE_WAIT, super::SCENES_WAIT));
}

/// What a duplex pipe holds before its writer blocks.
const DUPLEX_BUFFER: usize = 4096;

/// The test's side of a shell: it reads control, and data only when asked.
struct Shell<S> {
    control_in: ReadHalf<S>,
    control_out: WriteHalf<S>,
    data_in: ReadHalf<S>,
    data_out: WriteHalf<S>,
    /// The thread of the last run state read.
    thread: Option<ThreadId>,
    /// The last `Scenes` read.
    scenes: Option<Vec<SceneDescriptor>>,
    /// Every `Settings` read, in order.
    views: Vec<SettingsView>,
    /// Control frames other than pings that arrived while it waited on data, for the next
    /// control read.
    held: VecDeque<ControlToShell>,
}

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    fn new(control: S, data: S) -> Self {
        let (control_in, control_out) = tokio::io::split(control);
        let (data_in, data_out) = tokio::io::split(data);
        Self {
            control_in,
            control_out,
            data_in,
            data_out,
            thread: None,
            scenes: None,
            views: Vec::new(),
            held: VecDeque::new(),
        }
    }

    async fn submit(&mut self, text: &str) {
        let message = DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: None,
            text: text.into(),
        };
        let frame = frame::encode(&message, Pipe::Data).expect("encodes");
        self.data_out.write_all(&frame).await.expect("sent");
    }

    async fn credit(&mut self, bytes: usize) {
        let message = DataToBrain::Credit {
            bytes: u32::try_from(bytes).expect("small"),
        };
        let frame = frame::encode(&message, Pipe::Data).expect("encodes");
        self.data_out.write_all(&frame).await.expect("sent");
    }

    async fn stop(&mut self, run: RunId) {
        self.send(&ControlToBrain::Stop {
            run,
            revealed: Vec::new(),
        })
        .await;
    }

    /// The next control frame other than an account view, which every connection sends.
    async fn unaccounted(&mut self) -> std::io::Result<Option<ControlToShell>> {
        loop {
            match self.next_control().await {
                Ok(Some(ControlToShell::Account(_) | ControlToShell::Connections(_))) => {}
                other => return other.map_err(std::io::Error::other),
            }
        }
    }

    async fn send(&mut self, message: &ControlToBrain) {
        let frame = frame::encode(message, Pipe::Control).expect("encodes");
        self.control_out.write_all(&frame).await.expect("sent");
    }

    /// The next control frame: one held while it waited on data, or the next one read.
    async fn next_control(&mut self) -> Result<Option<ControlToShell>, frame::FrameError> {
        match self.held.pop_front() {
            Some(held) => Ok(Some(held)),
            None => ipc::read(&mut self.control_in, Pipe::Control).await,
        }
    }

    /// The next control frame, answering pings on the way; `None` once the brain hangs up.
    async fn control(&mut self) -> Option<ControlToShell> {
        loop {
            match timeout(PATIENCE, self.next_control())
                .await
                .expect("in time")
                .expect("readable")
            {
                Some(ControlToShell::Ping) => {
                    let pong =
                        frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
                    self.control_out.write_all(&pong).await.expect("sent");
                }
                Some(ControlToShell::Settings { view, .. }) => self.views.push(view),
                Some(ControlToShell::Account(_) | ControlToShell::Connections(_)) => {}
                other => return other,
            }
        }
    }

    /// The connection's `Scenes`, which comes before any run state.
    async fn scenes(&mut self) -> Vec<SceneDescriptor> {
        match self.control().await {
            Some(ControlToShell::Scenes { scenes }) => scenes,
            other => panic!("expected Scenes, got {other:?}"),
        }
    }

    /// The next run state, answering pings on the way.
    async fn state(&mut self) -> (RunId, Phase) {
        loop {
            let message = self.next_control().await.expect("readable").expect("open");
            match message {
                ControlToShell::RunState {
                    run, thread, phase, ..
                } => {
                    self.thread = Some(thread);
                    return (run, phase);
                }
                ControlToShell::Ping => {
                    let pong =
                        frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
                    self.control_out.write_all(&pong).await.expect("sent");
                }
                ControlToShell::Scenes { scenes } => self.scenes = Some(scenes),
                ControlToShell::Settings { view, .. } => self.views.push(view),
                ControlToShell::Account(_) | ControlToShell::Connections(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// The next data frame. Meanwhile it answers pings, as a shell's control reader does, so a
    /// slow store never reads as a shell that stopped answering; other control frames are held.
    async fn data(&mut self) -> DataToShell {
        let Self {
            control_in,
            control_out,
            data_in,
            held,
            ..
        } = self;
        let next = ipc::read::<DataToShell>(data_in, Pipe::Data);
        tokio::pin!(next);
        let mut control_open = true;
        let mut first = [0; 1];
        let frame = async {
            loop {
                tokio::select! {
                    biased;
                    frame = &mut next => return frame,
                    // One byte, so a control frame is never cut short when the data frame wins.
                    read = control_in.read(&mut first), if control_open => {
                        if !matches!(read, Ok(1)) {
                            control_open = false;
                            continue;
                        }
                        let mut rest = first.as_slice().chain(&mut *control_in);
                        let read = ipc::read::<ControlToShell>(&mut rest, Pipe::Control).await;
                        match read.expect("readable") {
                            Some(ControlToShell::Ping) => {
                                let pong = frame::encode(&ControlToBrain::Pong, Pipe::Control)
                                    .expect("encodes");
                                control_out.write_all(&pong).await.expect("sent");
                            }
                            Some(other) => held.push_back(other),
                            None => control_open = false,
                        }
                    }
                }
            }
        };
        timeout(PATIENCE, frame)
            .await
            .expect("data arrives")
            .expect("readable")
            .expect("open")
    }

    /// Reads only the next data frame's header, so its writer can't finish the frame once it's
    /// larger than the pipe holds; the payload's length.
    async fn start_frame(&mut self) -> usize {
        let mut header = [0; frame::HEADER_LEN];
        let read = timeout(PATIENCE, self.data_in.read_exact(&mut header)).await;
        read.expect("data arrives").expect("readable");
        frame::payload_len(header, Pipe::Data).expect("under the cap")
    }

    /// Reads `run`'s text up to its `RunEnd`, granting credit as a shell would; the text's
    /// length.
    async fn read_to_end(&mut self, run: RunId) -> usize {
        let mut bytes = 0;
        loop {
            match self.data().await {
                DataToShell::AssistantText { run: of, delta } if of == run => {
                    bytes += delta.len();
                    self.credit(delta.len() + 64).await;
                }
                DataToShell::RunEnd { run: ended } if ended == run => return bytes,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

/// Pipes with small buffers, so a writer the shell doesn't drain blocks at once.
fn duplex() -> ((DuplexStream, DuplexStream), Shell<DuplexStream>) {
    let (control, control_client) = tokio::io::duplex(DUPLEX_BUFFER);
    let (data, data_client) = tokio::io::duplex(DUPLEX_BUFFER);
    ((control, data), Shell::new(control_client, data_client))
}

/// The real named pipes.
#[cfg(windows)]
async fn pipes(name: &str) -> ((PipeServer, PipeServer), Shell<PipeClient>) {
    let (control, control_client) = platform::test_pair(&format!("{name}-control"))
        .await
        .expect("control pipe");
    let (data, data_client) = platform::test_pair(&format!("{name}-data"))
        .await
        .expect("data pipe");
    ((control, data), Shell::new(control_client, data_client))
}

/// A brain whose store waits up to 10 s on a lock, and the connection holding that lock.
fn locked_brain(scratch: &Scratch, script: &str) -> (Brain, rusqlite::Connection) {
    let brain = patient_brain(scratch, script);
    (brain, hold(scratch))
}

/// A brain whose store waits on a lock as long as a test waits.
fn patient_brain(scratch: &Scratch, script: &str) -> Brain {
    patient_brain_with(scratch, Box::new(Fixture::parse(script).expect("script")))
}

/// A brain whose store waits up to 10 s on a lock, answering with `engine`.
fn patient_brain_with(scratch: &Scratch, engine: Box<dyn Engine>) -> Brain {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main_thread = store.main_thread().expect("main thread");
    let high_water = store.high_water().expect("high water");
    store
        .conn_for_tests()
        .busy_timeout(PATIENCE)
        .expect("busy timeout");
    let scenes = store.scenes().expect("scenes");
    Brain::new(
        store,
        main_thread,
        high_water,
        scenes,
        crate::runtime::Selected::Fixture(engine),
        scratch.config(),
        crate::test_support::accounts(scratch),
    )
    .expect("brain")
}

/// A connection holding the store's write lock, so the brain's writes wait until it ends.
fn hold(scratch: &Scratch) -> rusqlite::Connection {
    let lock = rusqlite::Connection::open(scratch.db()).expect("lock");
    lock.busy_timeout(PATIENCE).expect("busy timeout");
    lock.execute_batch("BEGIN IMMEDIATE").expect("locked");
    lock
}

/// Every run's state and reason, oldest first.
fn outcomes(scratch: &Scratch) -> Vec<(String, Option<String>)> {
    let store = Store::open(&scratch.db()).expect("reopen");
    let mut query = store
        .conn_for_tests()
        .prepare("SELECT state, reason FROM runs ORDER BY id")
        .expect("query");
    query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read")
}

fn outcome(state: &str, reason: &str) -> (String, Option<String>) {
    (state.to_owned(), Some(reason.to_owned()))
}

#[tokio::test]
async fn submit_numbers_reach_refusals_and_only_the_first_run_state_in_fifo_order() {
    use kumochi_protocol::message::{RunOrigin, ScreenChoice};

    let scratch = Scratch::new("submit-origins");
    let mut brain = test_support::brain(&scratch, "5000 text late");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        for (turn, text) in [(73, " "), (91, "accepted")] {
            let message = DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                thread: None,
                text: text.into(),
            };
            shell
                .data_out
                .write_all(&frame::encode(&message, Pipe::Data).expect("encodes"))
                .await
                .expect("sent");
        }
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::TurnRefused {
                turn: 73,
                thread: None
            })
        );
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::RunState {
                run: RunId(1),
                thread: ThreadId(2),
                phase: Phase::Thinking,
                error: None,
                origin: Some(RunOrigin::Typed { turn: 91 }),
            })
        );
        shell.stop(RunId(1)).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::RunState {
                run: RunId(1),
                thread: ThreadId(2),
                phase: Phase::Interrupted,
                error: None,
                origin: None,
            })
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn stop_is_answered_while_a_data_write_is_blocked() {
    let scratch = Scratch::new("blocked-write");
    let mut brain = test_support::brain(&scratch, "0 fill 240000\n5000 step late");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        // The writer has started a frame many times what the duplex holds, and nothing reads
        // the rest, so that write can't finish before the Stop is answered.
        let len = shell.start_frame().await;
        assert!(len > 4 * DUPLEX_BUFFER, "{len} bytes");
        shell.stop(run).await;
        let interrupted = timeout(PATIENCE, shell.state()).await;
        assert_eq!(
            interrupted.expect("Stop is answered while the data write is blocked"),
            (run, Phase::Interrupted)
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    // The stall timer would also end the run, as view_stalled, if the Stop were ignored.
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "stop")]);
}

/// Proves Stop works over real pipes while the shell reads no data; the blocked write itself is
/// established over the duplex above.
#[cfg(windows)]
#[tokio::test]
async fn stop_is_served_over_real_pipes_while_the_shell_reads_no_data() {
    let scratch = Scratch::new("saturated");
    let script = "0 fill 240000\n0 fill 240000\n5000 step late";
    let mut brain = test_support::brain(&scratch, script);
    let ((control, data), mut shell) = pipes("saturated").await;
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, thinking) = shell.state().await;
        assert_eq!(thinking, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        shell.stop(run).await;
        let interrupted = timeout(PATIENCE, shell.state()).await;
        assert_eq!(
            interrupted.expect("Stop is answered"),
            (run, Phase::Interrupted)
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_step_finished_after_stop_never_reaches_the_shell_or_the_store() {
    let scratch = Scratch::new("late");
    let mut brain = test_support::brain(
        &scratch,
        "0 text a\n0 step one\n3000 step never\nlate closed",
    );
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        let first_thread = shell.thread.expect("a run state names its thread");
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run,
                delta: "a".into()
            }
        );
        assert_eq!(
            shell.data().await,
            DataToShell::StepCommit {
                run,
                // Events 1 and 2 are the turn and the text before the step.
                scene: SceneId(3),
                index: 0,
                step: Step {
                    label: "one".to_owned()
                }
            }
        );
        shell.stop(run).await;
        // Answering came with the text; Interrupted follows the Stop.
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        // A replacement run right after: the old run's late step must not show up before it.
        shell.submit("again").await;
        let (next, phase) = shell.state().await;
        assert!(next > run, "{next:?} after {run:?}");
        assert_eq!(phase, Phase::Thinking);
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run: next,
                delta: "a".into()
            }
        );
        drop(shell);
        first_thread
    };
    let (ended, first_thread) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen");
    assert!(
        store
            .pages()
            .search_ids("closed")
            .expect("search")
            .is_empty()
    );
    // The replacement run may or may not commit its own step before the shell hangs up.
    assert!(
        store
            .pages()
            .search_ids("one")
            .expect("search")
            .contains(&first_thread)
    );
}

#[tokio::test]
async fn a_runs_data_is_complete_at_its_run_end_not_at_its_done() {
    let scratch = Scratch::new("runend");
    let mut brain = test_support::brain(&scratch, "0 text a\n0 step one\n0 text b");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        // Done can arrive on control before any of the data is read: the pipes aren't ordered.
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        let mut frames = Vec::new();
        loop {
            let frame = shell.data().await;
            let last = frame == DataToShell::RunEnd { run };
            frames.push(frame);
            if last {
                break;
            }
        }
        let text = |delta: &str| DataToShell::AssistantText {
            run,
            delta: delta.into(),
        };
        assert_eq!(
            frames,
            [
                text("a"),
                DataToShell::StepCommit {
                    run,
                    scene: SceneId(3),
                    index: 0,
                    step: Step {
                        label: "one".to_owned()
                    }
                },
                text("b"),
                DataToShell::RunEnd { run },
            ]
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn new_turns_wait_while_the_shell_is_behind_on_data() {
    let scratch = Scratch::new("turn-gate");
    let mut brain = test_support::brain(&scratch, "0 fill 960000");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("one").await;
        let (first, _) = shell.state().await;
        // Answering comes with the text, which leaves far more queued than the cap.
        assert_eq!(shell.state().await, (first, Phase::Answering));
        shell.submit("two").await;
        // The replacement waits until reading drains the queue under the cap, so a good part
        // of the first run arrives before its RunEnd; taken at once, at most the frame already
        // being written would.
        let read = shell.read_to_end(first).await;
        assert!((300_000..960_000).contains(&read), "{read} bytes");
        assert_eq!(shell.state().await, (first, Phase::Interrupted));
        assert_eq!(shell.state().await.1, Phase::Thinking);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn turns_beyond_the_queue_while_the_store_is_behind_end_the_connection() {
    let scratch = Scratch::new("turn-flood");
    let (mut brain, lock) = locked_brain(&scratch, "0 text a");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        // Each turn replaces the last, whose writes wait on the lock, until MAX_ENDING runs
        // wait on the store and turns stop being taken.
        for _ in 0..=MAX_ENDING {
            shell.submit("go").await;
            while shell.state().await.1 != Phase::Thinking {}
        }
        // The reader never waits for the core: a full turn queue ends the connection rather
        // than leaving credit unread behind it.
        for _ in 0..=MAX_PENDING_TURNS {
            shell.submit("more").await;
        }
        shell
    };
    let (ended, shell) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("the connection ends");
    assert_eq!(ended, Ended::Broken("turns sent faster than taken"));
    drop(shell);
    lock.execute_batch("ROLLBACK").expect("unlocked");
    brain.close().await;
}

#[tokio::test]
async fn a_full_control_queue_still_applies_store_effects() {
    let scratch = Scratch::new("full-control");
    let mut brain = test_support::brain(&scratch, "");
    let offline = |brain: &mut Brain, input| {
        let effects = brain.coordinator.handle(input).expect("accepted");
        brain.apply(effects, None).expect("no shell to fill");
    };
    offline(
        &mut brain,
        Input::Turn {
            turn: 1,
            thread: None,
            text: "go".into(),
        },
    );
    let stored = brain.inputs.recv().await.expect("a reply");
    let Input::TurnStored { run } = stored else {
        panic!("unexpected {stored:?}");
    };
    offline(&mut brain, stored);
    // A shell whose control queue is already full.
    let (out, _unread) = mpsc::channel(1);
    out.try_send(Vec::new()).expect("room for one");
    let queue = RefCell::new(DataQueue::new(window()));
    let (ready, mut tap) = (Notify::new(), Tap::from_env());
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let stop = Input::Stop {
        run,
        revealed: Vec::new(),
    };
    let effects = brain.coordinator.handle(stop).expect("current");
    assert!(brain.apply(effects, Some(&mut link)).is_err(), "full");
    // The outcome after the refused Interrupted still went to the store.
    // The build the stored turn started may answer first, in either order; Stop made it stale.
    let reply = timeout(PATIENCE, async {
        loop {
            match brain.inputs.recv().await {
                Some(Input::ContextBuilt(_)) => {}
                other => break other,
            }
        }
    })
    .await;
    assert_eq!(reply.expect("answered"), Some(Input::RunStored { run }));
    brain.offline(Input::RunStored { run });
    brain.close().await;
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "stop")]);
}

/// A lost shell leaves a replaced run whose turn is still being written, and a held turn behind
/// it; the store is locked until the shell has gone.
async fn lose_the_shell_mid_write(scratch: &Scratch) -> Brain {
    let (mut brain, lock) = locked_brain(scratch, "0 text a");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("one").await;
        let (first, _) = shell.state().await;
        shell.submit("two").await;
        assert_eq!(shell.state().await, (first, Phase::Interrupted));
        assert_eq!(shell.state().await.1, Phase::Thinking);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    lock.execute_batch("ROLLBACK").expect("unlocked");
    brain
}

#[tokio::test]
async fn runs_a_lost_shell_left_reach_the_store_before_another_connects() {
    let scratch = Scratch::new("idle");
    let mut brain = lose_the_shell_mid_write(&scratch).await;
    let expected = [
        outcome("interrupted", "replaced"),
        outcome("interrupted", "shell_lost"),
    ];
    let stored = async {
        while outcomes(&scratch) != expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        () = brain.idle() => unreachable!("idle never returns"),
        stored = timeout(PATIENCE, stored) => stored.expect("stored with no shell connected"),
    }
    brain.close().await;
}

#[tokio::test]
async fn closing_stores_what_a_lost_shell_left() {
    let scratch = Scratch::new("close");
    let brain = lose_the_shell_mid_write(&scratch).await;
    brain.close().await;
    assert_eq!(
        outcomes(&scratch),
        [
            outcome("interrupted", "replaced"),
            outcome("interrupted", "shell_lost"),
        ]
    );
}

#[tokio::test]
async fn engine_events_wait_while_the_shell_is_behind_on_data() {
    let scratch = Scratch::new("behind");
    let fill = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000";
    let mut brain = test_support::brain(&scratch, fill);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("one").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        // Past the cap the engine's last text and its Done wait, so the unread data stalls the
        // run instead of finishing it.
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "view_stalled")]);
}

#[tokio::test]
async fn a_stale_stop_never_drops_a_finished_runs_data() {
    let scratch = Scratch::new("stale-stop");
    let mut brain = test_support::brain(&scratch, "0 fill 240000\n0 fill 240000");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        // Esc after Done: stale, so the text still queued behind the unread pipe stays.
        shell.stop(run).await;
        assert_eq!(shell.read_to_end(run).await, 480_000);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

// Real time: paused time would jump ahead whenever the runtime waits on the store's own thread.
#[tokio::test]
async fn a_shell_that_stops_reading_data_ends_the_run_as_stalled() {
    let scratch = Scratch::new("stalled");
    let mut brain = test_support::brain(&scratch, "0 fill 240000\n0 fill 240000\n5000 step late");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let started = tokio::time::Instant::now();
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        let after = started.elapsed();
        assert!(after >= super::data::STALL, "stalled after {after:?}");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "view_stalled")]);
}

/// Rez measurement for the pull request, not a check: run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_`.
#[cfg(windows)]
#[tokio::test]
#[ignore = "a measurement, run by hand on Rez"]
async fn measure_stop_under_a_saturated_writer_and_throughput() {
    let script = "0 fill 240000\n0 fill 240000\n0 fill 240000\n0 fill 240000\n5000 step late";
    let mut stops = Vec::new();
    let mut streams = Vec::new();
    for round in 0..20 {
        let scratch = Scratch::new("measure");
        let mut brain = test_support::brain(&scratch, script);
        let ((control, data), mut shell) = pipes(&format!("measure-{round}")).await;
        let mut tap = Tap::from_env();
        let client = async {
            // Throughput: one run read to its last byte, granting credit as a shell would.
            shell.submit("read").await;
            let (run, _) = shell.state().await;
            let mut started = None;
            let mut bytes = 0;
            while bytes < 960_000 {
                let DataToShell::AssistantText { delta, .. } = shell.data().await else {
                    panic!("text only");
                };
                started.get_or_insert_with(tokio::time::Instant::now);
                bytes += delta.len();
                shell.credit(delta.len() + 64).await;
            }
            streams.push(started.expect("text arrived").elapsed());
            shell.stop(run).await;
            while shell.state().await != (run, Phase::Interrupted) {}
            // Stop: a second run that nothing reads.
            shell.submit("ignore").await;
            let (run, _) = shell.state().await;
            assert_eq!(shell.state().await, (run, Phase::Answering));
            tokio::time::sleep(Duration::from_millis(100)).await;
            let sent = tokio::time::Instant::now();
            shell.stop(run).await;
            assert_eq!(shell.state().await, (run, Phase::Interrupted));
            stops.push(sent.elapsed());
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        brain.close().await;
    }
    let report = |name: &str, samples: &mut Vec<Duration>| {
        samples.sort_unstable();
        eprintln!(
            "{name}: p50 {:?} p95 {:?} max {:?} (n={})",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1],
            samples.len()
        );
    };
    report("Stop -> Interrupted, shell reading no data", &mut stops);
    report(
        "960 kB of text from its first frame to its last",
        &mut streams,
    );
}

/// Rez measurement for the pull request, not a check: the first step's latency from the turn, and
/// the second's after it. Run it with
/// `cargo test --release -p kumochi-brain -- --ignored --nocapture measure_first_step`.
#[cfg(windows)]
#[tokio::test]
#[ignore = "a measurement, run by hand on Rez"]
async fn measure_first_step_latency() {
    let mut firsts = Vec::new();
    let mut seconds = Vec::new();
    for round in 0..30 {
        let scratch = Scratch::new("measure-step");
        let mut brain = test_support::brain(&scratch, "0 step first\n0 step second");
        let ((control, data), mut shell) = pipes(&format!("measure-step-{round}")).await;
        let mut tap = Tap::from_env();
        let client = async {
            let sent = tokio::time::Instant::now();
            shell.submit("draw").await;
            let mut steps = Vec::new();
            while steps.len() < 2 {
                if let DataToShell::StepCommit { .. } = shell.data().await {
                    steps.push(sent.elapsed());
                }
            }
            firsts.push(steps[0]);
            seconds.push(steps[1].saturating_sub(steps[0]));
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        brain.close().await;
    }
    for (name, samples) in [
        ("turn -> first StepCommit", &mut firsts),
        ("first -> second StepCommit", &mut seconds),
    ] {
        samples.sort_unstable();
        eprintln!(
            "{name}: p50 {:?} p95 {:?} max {:?} (n={})",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1],
            samples.len()
        );
    }
}
